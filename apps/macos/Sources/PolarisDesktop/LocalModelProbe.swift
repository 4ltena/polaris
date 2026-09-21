import Foundation
import PolarisSettings

enum LocalProbeStatus: Equatable, Sendable {
  case idle
  case checking
  case available(Int)
  case unreachable
  case invalidResponse
  case failed

  var title: String {
    switch self {
    case .idle: "未確認"
    case .checking: "接続を確認中"
    case .available(let count): "到達を確認（モデル \(count)件、推論は未確認）"
    case .unreachable: "到達できませんでした"
    case .invalidResponse: "応答形式を確認できませんでした"
    case .failed: "接続確認を完了できませんでした"
    }
  }
}

struct LocalModelProbeAccess: Sendable {
  let probe: @Sendable (LocalRuntimePreference, String) async throws -> LocalInventory

  static let live = LocalModelProbeAccess { runtime, endpoint in
    try await LocalModelProbe.run(runtime: runtime, endpoint: endpoint)
  }
}

private final class OwnedProbeProcess: @unchecked Sendable {
  private let lock = NSLock()
  private var process: Process?

  func set(_ process: Process?) {
    lock.lock()
    self.process = process
    lock.unlock()
  }

  func cancel() {
    lock.lock()
    let process = process
    lock.unlock()
    if process?.isRunning == true { process?.terminate() }
  }
}

enum LocalModelProbe {
  static func run(runtime: LocalRuntimePreference, endpoint: String) async throws -> LocalInventory {
    let provider: String
    switch runtime {
    case .ollama: provider = "ollama"
    case .lmstudio: provider = "lmstudio"
    case .llamacpp: throw ServiceError.schema
    }
    try ExecutionBinding.validateLocalEndpoint(endpoint)
    let helper = try DesktopWorkspaceOwner.packagedHelper()
    let owner = OwnedProbeProcess()
    return try await withTaskCancellationHandler {
      try await Task.detached(priority: .userInitiated) {
        try Task.checkCancellation()
        let process = Process()
        let output = Pipe()
        process.executableURL = helper
        process.arguments = ["--probe-local-models", provider, "--endpoint", endpoint]
        process.environment = [:]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        owner.set(process)
        defer { owner.set(nil) }
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        try Task.checkCancellation()
        guard process.terminationReason == .exit, process.terminationStatus == 0,
              !data.isEmpty, data.count <= ServiceCodec.maxFrameBytes
        else { throw ServiceError.launch }
        return try LocalInventory(ServiceCodec.json(data))
      }.value
    } onCancel: {
      owner.cancel()
    }
  }
}
