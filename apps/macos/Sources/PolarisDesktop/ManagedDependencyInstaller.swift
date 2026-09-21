import CryptoKit
import Foundation

enum ManagedDependencyID: String, CaseIterable, Sendable {
  case llamaCpp = "llama_cpp"
  case rust
  case embeddingRuntime = "embedding_runtime"
  case multilingualE5Small = "multilingual_e5_small"
  case multilingualE5Base = "multilingual_e5_base"

  static let preset: [ManagedDependencyID] = [
    .embeddingRuntime, .multilingualE5Small, .multilingualE5Base, .llamaCpp, .rust,
  ]

  var title: String {
    switch self {
    case .llamaCpp: "llama.cpp 実行環境"
    case .rust: "Rust 1.96.0"
    case .embeddingRuntime: "e5 埋め込み実行環境"
    case .multilingualE5Small: "multilingual-e5-small（長期記憶）"
    case .multilingualE5Base: "multilingual-e5-base（事前選定）"
    }
  }
}

enum ManagedDependencyStatus: Equatable, Sendable {
  case notInstalled
  case checking
  case downloading
  case installing
  case installed(version: String)
  case failed(String)

  var title: String {
    switch self {
    case .notInstalled: "未導入"
    case .checking: "確認中"
    case .downloading: "ダウンロード中"
    case .installing: "検証・導入中"
    case .installed(let version): "導入済み（\(version)）"
    case .failed: "確認できませんでした"
    }
  }

  var isBusy: Bool {
    self == .checking || self == .downloading || self == .installing
  }
}

enum ManagedDependencyError: Error, LocalizedError, Equatable {
  case manifest
  case unsupportedPlatform
  case invalidResponse
  case insufficientSpace
  case integrity
  case archive
  case conflict
  case cancelled
  case install

  var errorDescription: String? {
    switch self {
    case .manifest: "依存関係の固定情報を確認できませんでした。"
    case .unsupportedPlatform: "このMac向けの依存関係がありません。"
    case .invalidResponse: "固定した取得先から正しい応答を受け取れませんでした。"
    case .insufficientSpace: "ダウンロードと展開に必要な空き容量がありません。"
    case .integrity: "取得したファイルのサイズまたはSHA-256が一致しません。"
    case .archive: "取得したアーカイブを安全に展開できません。"
    case .conflict: "同じ版の導入先に別の内容があります。"
    case .cancelled: "導入を取り消しました。"
    case .install: "依存関係を導入できませんでした。"
    }
  }
}

struct ManagedGGUFPlan: Equatable, Sendable {
  let sourceURL: URL
  let downloadURL: URL
  let filename: String
  let size: Int64
}

struct ManagedGGUFModel: Codable, Equatable, Identifiable, Sendable {
  var id: String { filename }
  let filename: String
  let sourceURL: URL
  let size: Int64
  let sha256: String
  let installedAt: String

  enum CodingKeys: String, CodingKey {
    case filename, size, sha256
    case sourceURL = "source_url"
    case installedAt = "installed_at"
  }
}

enum ManagedGGUFStatus: Equatable, Sendable {
  case idle
  case inspecting
  case ready
  case downloading
  case installed
  case failed(String)

  var title: String {
    switch self {
    case .idle: "リンク未確認"
    case .inspecting: "リンクを確認中"
    case .ready: "ダウンロード準備完了"
    case .downloading: "モデルをダウンロード中"
    case .installed: "モデルを導入しました"
    case .failed: "モデルを導入できませんでした"
    }
  }
}

private struct ManagedGGUFCatalog: Codable {
  let schemaVersion: Int
  var models: [ManagedGGUFModel]

  enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", models }
}

private struct DependencyManifest: Decodable {
  let schemaVersion: Int
  let dependencies: [DependencyEntry]

  enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", dependencies }
}

private struct DependencyEntry: Decodable {
  let id: String
  let version: String
  let platform: String
  let architecture: String
  let url: URL?
  let sha256: String?
  let compressedSize: Int64?
  let requiredSpace: Int64?
  let archive: String?
  let archiveRoot: String?
  let installer: String?
  let installKind: String?
  let requiredExecutable: String?
  let destination: String?
  let files: [DependencyFile]?
  let pythonVersion: String?
  let pythonPackages: [String: String]?
  let excludeNewer: String?

  enum CodingKeys: String, CodingKey {
    case id, version, platform, architecture, url, sha256, archive, installer, destination, files
    case compressedSize = "compressed_size"
    case requiredSpace = "required_space"
    case archiveRoot = "archive_root"
    case requiredExecutable = "required_executable"
    case installKind = "install_kind"
    case pythonVersion = "python_version"
    case pythonPackages = "python_packages"
    case excludeNewer = "exclude_newer"
  }
}

private struct DependencyFile: Decodable {
  let path: String
  let url: URL
  let sha256: String
  let size: Int64
}

private struct DependencyReceipt: Codable {
  let schemaVersion: Int
  let id: String
  let version: String
  let manifestSHA256: String
  let installedAt: String

  enum CodingKeys: String, CodingKey {
    case schemaVersion = "schema_version"
    case id, version
    case manifestSHA256 = "manifest_sha256"
    case installedAt = "installed_at"
  }
}

private struct InstalledModelManifest: Decodable {
  let revision: String
  let files: [String: String]
}

private final class HTTPSRedirectDelegate: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  func urlSession(_ session: URLSession, task: URLSessionTask,
                  willPerformHTTPRedirection response: HTTPURLResponse,
                  newRequest request: URLRequest) async -> URLRequest? {
    request.url?.scheme == "https" && request.url?.user == nil && request.url?.password == nil
      ? request : nil
  }
}

private final class ManagedProcessControl: @unchecked Sendable {
  private let lock = NSLock()
  private var process: Process?
  private var cancelled = false

  func start(_ process: Process) throws {
    lock.lock()
    defer { lock.unlock() }
    guard !cancelled else { throw CancellationError() }
    self.process = process
    do { try process.run() }
    catch {
      self.process = nil
      throw error
    }
  }

  func finish() -> Bool {
    lock.lock()
    defer { lock.unlock() }
    process = nil
    return cancelled
  }

  func cancel() {
    lock.lock()
    cancelled = true
    let running = process?.isRunning == true ? process : nil
    lock.unlock()
    running?.terminate()
  }
}

protocol ManagedDependencyInstalling: Sendable {
  func statuses() async throws -> [ManagedDependencyID: ManagedDependencyStatus]
  func status(_ id: ManagedDependencyID) async throws -> ManagedDependencyStatus
  func install(_ id: ManagedDependencyID) async throws -> String
  func activatePreset() async throws
  func inspectGGUF(_ source: String) async throws -> ManagedGGUFPlan
  func installedGGUFModels() async throws -> [ManagedGGUFModel]
  func installGGUF(_ plan: ManagedGGUFPlan) async throws -> ManagedGGUFModel
}

actor ManagedDependencyInstaller: ManagedDependencyInstalling {
  private let manifestURL: URL
  private let home: URL
  private let fileManager: FileManager

  init(manifestURL: URL? = Bundle.module.url(forResource: "runtime-dependencies", withExtension: "json"),
       home: URL = FileManager.default.homeDirectoryForCurrentUser,
       fileManager: FileManager = .default) {
    self.manifestURL = manifestURL ?? URL(fileURLWithPath: "/invalid-runtime-dependencies.json")
    self.home = home
    self.fileManager = fileManager
  }

  func statuses() throws -> [ManagedDependencyID: ManagedDependencyStatus] {
    let (manifest, digest) = try loadManifest()
    return try Dictionary(uniqueKeysWithValues: ManagedDependencyID.allCases.map { id in
      let entry = try selectedEntry(id, manifest: manifest)
      return (id, try status(entry: entry, id: id, manifestDigest: digest))
    })
  }

  func status(_ id: ManagedDependencyID) throws -> ManagedDependencyStatus {
    let (manifest, digest) = try loadManifest()
    let entry = try selectedEntry(id, manifest: manifest)
    return try status(entry: entry, id: id, manifestDigest: digest)
  }

  func install(_ id: ManagedDependencyID) async throws -> String {
    do {
      try Task.checkCancellation()
      let (manifest, digest) = try loadManifest()
      let entry = try selectedEntry(id, manifest: manifest)
      if case .installed(let version) = try status(entry: entry, id: id, manifestDigest: digest) {
        return version
      }
      let destination = try destinationURL(entry: entry, id: id)
      if fileManager.fileExists(atPath: destination.path) { throw ManagedDependencyError.conflict }
      try ensurePrivateDirectory(destination.deletingLastPathComponent())
      let polaris = home.appendingPathComponent(".polaris", isDirectory: true)
      try ensurePrivateDirectory(polaris)
      let staging = polaris.appendingPathComponent(".install-\(UUID().uuidString)", isDirectory: true)
      try ensurePrivateDirectory(staging)
      defer { try? fileManager.removeItem(at: staging) }
      try checkFreeSpace(required: requiredSpace(entry), at: polaris)

      let payload: URL
      if let files = entry.files {
        payload = try await installFiles(files, entry: entry, staging: staging)
      } else {
        payload = try await installArchive(entry, id: id, staging: staging)
      }
      try Task.checkCancellation()
      try validatePayload(payload, entry: entry, id: id)
      try fileManager.moveItem(at: payload, to: destination)
      do {
        try writeReceipt(entry: entry, id: id, manifestDigest: digest, destination: destination)
      } catch {
        try? fileManager.removeItem(at: destination)
        throw error
      }
      return entry.version
    } catch is CancellationError {
      throw ManagedDependencyError.cancelled
    }
  }

  func activatePreset() throws {
    let (manifest, digest) = try loadManifest()
    var selected: [ManagedDependencyID: DependencyEntry] = [:]
    for id in ManagedDependencyID.preset {
      let entry = try selectedEntry(id, manifest: manifest)
      guard case .installed = try status(entry: entry, id: id, manifestDigest: digest) else {
        throw ManagedDependencyError.install
      }
      selected[id] = entry
    }
    guard let runtime = selected[.embeddingRuntime],
          let small = selected[.multilingualE5Small],
          let base = selected[.multilingualE5Base],
          let llama = selected[.llamaCpp],
          let runtimeExecutable = runtime.requiredExecutable,
          let llamaExecutable = llama.requiredExecutable else {
      throw ManagedDependencyError.manifest
    }
    let runtimeRoot = try destinationURL(entry: runtime, id: .embeddingRuntime)
    let smallRoot = try destinationURL(entry: small, id: .multilingualE5Small)
    let baseRoot = try destinationURL(entry: base, id: .multilingualE5Base)
    let llamaRoot = try destinationURL(entry: llama, id: .llamaCpp)
    let llamaServer = llamaRoot.appendingPathComponent(llamaExecutable)
    let values = [
      "runtime": runtimeRoot.appendingPathComponent(runtimeExecutable).path,
      "model_path": smallRoot.path,
      "revision": small.version,
      "preselect_model_path": baseRoot.path,
      "preselect_revision": base.version,
      "llama_server": llamaServer.path,
      "llama_models": ggufRoot().path,
      "llama_build": llama.version,
    ]
    let body = try """
    # Generated by Polaris. Explicit config.toml values take precedence.
    [embedding]
    runtime = \(tomlString(values["runtime"]!))
    model_path = \(tomlString(values["model_path"]!))
    revision = \(tomlString(values["revision"]!))
    preselect_model_path = \(tomlString(values["preselect_model_path"]!))
    preselect_revision = \(tomlString(values["preselect_revision"]!))

    [llama_cpp]
    server = \(tomlString(values["llama_server"]!))
    models_dir = \(tomlString(values["llama_models"]!))
    build = \(tomlString(values["llama_build"]!))
    """
    try writeManagedResources(Data(body.utf8))
  }

  func inspectGGUF(_ source: String) async throws -> ManagedGGUFPlan {
    let trimmed = source.trimmingCharacters(in: .whitespacesAndNewlines)
    guard trimmed == source, let sourceURL = URL(string: source), validDownloadURL(sourceURL) else {
      throw ManagedDependencyError.manifest
    }
    let downloadURL = try resolvedGGUFURL(sourceURL)
    let filename = try ggufFilename(downloadURL)
    let (size, finalURL) = try await metadata(downloadURL)
    guard size > 0, size <= 64 * 1024 * 1024 * 1024, finalURL.scheme == "https" else {
      throw ManagedDependencyError.invalidResponse
    }
    return ManagedGGUFPlan(sourceURL: sourceURL, downloadURL: downloadURL,
                           filename: filename, size: size)
  }

  func installedGGUFModels() throws -> [ManagedGGUFModel] {
    let root = ggufRoot()
    let catalogURL = root.appendingPathComponent("catalog.json")
    guard fileManager.fileExists(atPath: catalogURL.path) else { return [] }
    let metadata = try fileManager.attributesOfItem(atPath: catalogURL.path)
    guard (metadata[.type] as? FileAttributeType) == .typeRegular,
          (metadata[.size] as? NSNumber)?.intValue ?? Int.max <= 1024 * 1024
    else { throw ManagedDependencyError.integrity }
    let catalog = try JSONDecoder().decode(
      ManagedGGUFCatalog.self, from: Data(contentsOf: catalogURL, options: [.mappedIfSafe]))
    guard catalog.schemaVersion == 1, catalog.models.count <= 128,
          Set(catalog.models.map(\.filename)).count == catalog.models.count
    else { throw ManagedDependencyError.integrity }
    return try catalog.models.sorted { $0.filename < $1.filename }.map { model in
      guard try ggufFilename(URL(fileURLWithPath: model.filename)) == model.filename,
            validDownloadURL(model.sourceURL), validDigest(model.sha256), model.size > 0
      else { throw ManagedDependencyError.integrity }
      let file = root.appendingPathComponent(model.filename)
      let values = try fileManager.attributesOfItem(atPath: file.path)
      guard (values[.type] as? FileAttributeType) == .typeRegular,
            (values[.size] as? NSNumber)?.int64Value == model.size
      else { throw ManagedDependencyError.integrity }
      return model
    }
  }

  func installGGUF(_ plan: ManagedGGUFPlan) async throws -> ManagedGGUFModel {
    do {
      guard try resolvedGGUFURL(plan.sourceURL) == plan.downloadURL,
            try ggufFilename(plan.downloadURL) == plan.filename, plan.size > 0
      else { throw ManagedDependencyError.manifest }
      let root = ggufRoot()
      try ensurePrivateDirectory(root)
      let destination = root.appendingPathComponent(plan.filename)
      guard !fileManager.fileExists(atPath: destination.path) else {
        throw ManagedDependencyError.conflict
      }
      try checkFreeSpace(required: plan.size * 2 + 256 * 1024 * 1024, at: root)
      let polaris = home.appendingPathComponent(".polaris", isDirectory: true)
      let staging = polaris.appendingPathComponent(".model-install-\(UUID().uuidString)", isDirectory: true)
      try ensurePrivateDirectory(staging)
      defer { try? fileManager.removeItem(at: staging) }
      let temporary = try await download(plan.downloadURL, expectedSize: plan.size, staging: staging)
      let digest = try fileDigest(temporary)
      try Task.checkCancellation()
      try fileManager.moveItem(at: temporary, to: destination)
      try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
      let installed = ManagedGGUFModel(
        filename: plan.filename, sourceURL: plan.sourceURL, size: plan.size,
        sha256: digest, installedAt: ISO8601DateFormatter().string(from: Date()))
      do {
        var models = (try? installedGGUFModels()) ?? []
        models.append(installed)
        let catalog = ManagedGGUFCatalog(schemaVersion: 1, models: models.sorted { $0.filename < $1.filename })
        let data = try JSONEncoder().encode(catalog)
        try data.write(to: root.appendingPathComponent("catalog.json"), options: [.atomic])
      } catch {
        try? fileManager.removeItem(at: destination)
        throw error
      }
      return installed
    } catch is CancellationError {
      throw ManagedDependencyError.cancelled
    }
  }

  private func loadManifest() throws -> (DependencyManifest, String) {
    let data = try Data(contentsOf: manifestURL, options: [.mappedIfSafe])
    guard data.count <= 512 * 1024 else { throw ManagedDependencyError.manifest }
    let manifest = try JSONDecoder().decode(DependencyManifest.self, from: data)
    guard manifest.schemaVersion == 1, !manifest.dependencies.isEmpty else {
      throw ManagedDependencyError.manifest
    }
    try manifest.dependencies.forEach(validateEntry)
    return (manifest, hexDigest(data))
  }

  private func selectedEntry(_ id: ManagedDependencyID,
                             manifest: DependencyManifest) throws -> DependencyEntry {
    let architecture: String
#if arch(arm64)
    architecture = "arm64"
#elseif arch(x86_64)
    architecture = "x86_64"
#else
    throw ManagedDependencyError.unsupportedPlatform
#endif
    let matches = manifest.dependencies.filter {
      $0.id == id.rawValue && $0.platform == "macos"
        && ($0.architecture == architecture || $0.architecture == "universal")
    }
    guard matches.count == 1, let entry = matches.first else {
      throw ManagedDependencyError.unsupportedPlatform
    }
    return entry
  }

  private func validateEntry(_ entry: DependencyEntry) throws {
    guard !entry.version.isEmpty, entry.version.utf8.count <= 128,
          !entry.version.contains("/"), !entry.version.contains(".."),
          entry.requiredSpace.map({ $0 > 0 && $0 <= 32 * 1024 * 1024 * 1024 }) ?? true
    else { throw ManagedDependencyError.manifest }
    if let files = entry.files {
      guard entry.url == nil, entry.archive == nil, entry.installKind == nil,
            entry.pythonVersion == nil, entry.pythonPackages == nil,
            !files.isEmpty, files.count <= 16 else {
        throw ManagedDependencyError.manifest
      }
      var paths = Set<String>()
      for file in files {
        guard validRelative(file.path), paths.insert(file.path).inserted,
              validDownloadURL(file.url), validDigest(file.sha256), file.size > 0
        else { throw ManagedDependencyError.manifest }
      }
    } else {
      guard let url = entry.url, validDownloadURL(url),
            let digest = entry.sha256, validDigest(digest),
            let size = entry.compressedSize, size > 0,
            entry.archive == "tar_gzip" || entry.archive == "tar_xz",
            entry.archiveRoot.map(validRelative) == true,
            entry.requiredExecutable.map(validRelative) == true
      else { throw ManagedDependencyError.manifest }
      if let installer = entry.installer, !validRelative(installer) {
        throw ManagedDependencyError.manifest
      }
      if entry.installKind == "uv_embedding" {
        guard entry.id == ManagedDependencyID.embeddingRuntime.rawValue,
              entry.installer == nil,
              entry.pythonVersion == "3.11.9",
              let packages = entry.pythonPackages,
              Set(packages.keys) == Set(["torch", "transformers", "tokenizers", "safetensors", "numpy"]),
              packages.values.allSatisfy({ validVersion($0) }),
              entry.excludeNewer == "2026-09-19T00:00:00Z"
        else { throw ManagedDependencyError.manifest }
      } else if entry.installKind != nil || entry.pythonVersion != nil
                  || entry.pythonPackages != nil || entry.excludeNewer != nil {
        throw ManagedDependencyError.manifest
      }
    }
  }

  private func status(entry: DependencyEntry, id: ManagedDependencyID,
                      manifestDigest: String) throws -> ManagedDependencyStatus {
    let destination = try destinationURL(entry: entry, id: id)
    var directory: ObjCBool = false
    guard fileManager.fileExists(atPath: destination.path, isDirectory: &directory) else {
      return .notInstalled
    }
    guard directory.boolValue else { return .failed("導入先がフォルダではありません。") }
    let receiptMatches: Bool = {
      guard let data = try? Data(contentsOf: receiptURL(for: destination)),
            let receipt = try? JSONDecoder().decode(DependencyReceipt.self, from: data)
      else { return false }
      return receipt.schemaVersion == 1 && receipt.id == id.rawValue
        && receipt.version == entry.version && receipt.manifestSHA256 == manifestDigest
    }()
    let existingPinnedModel = (id == .multilingualE5Small || id == .multilingualE5Base)
      && modelManifestMatches(destination, entry: entry)
    guard receiptMatches || existingPinnedModel else {
      return .failed("導入情報が固定manifestと一致しません。")
    }
    do {
      try validatePayload(destination, entry: entry, id: id, verifyHashes: false)
      return .installed(version: entry.version)
    } catch {
      return .failed("導入済みファイルを確認できませんでした。")
    }
  }

  private func modelManifestMatches(_ destination: URL, entry: DependencyEntry) -> Bool {
    guard let expected = entry.files,
          let data = try? Data(contentsOf: destination.appendingPathComponent("manifest.json")),
          data.count <= 1024 * 1024,
          let manifest = try? JSONDecoder().decode(InstalledModelManifest.self, from: data),
          manifest.revision == entry.version,
          manifest.files == Dictionary(uniqueKeysWithValues: expected.map { ($0.path, $0.sha256) })
    else { return false }
    return expected.allSatisfy { file in
      let path = destination.appendingPathComponent(file.path).path
      guard let attributes = try? fileManager.attributesOfItem(atPath: path) else { return false }
      return (attributes[.type] as? FileAttributeType) == .typeRegular
        && (attributes[.size] as? NSNumber)?.int64Value == file.size
    }
  }

  private func destinationURL(entry: DependencyEntry, id: ManagedDependencyID) throws -> URL {
    let root = home.appendingPathComponent(".polaris", isDirectory: true)
    switch id {
    case .llamaCpp:
      return root.appendingPathComponent("runtimes/llama.cpp/\(entry.version)", isDirectory: true)
    case .rust:
      return root.appendingPathComponent("toolchains/rust/\(entry.version)", isDirectory: true)
    case .embeddingRuntime:
      return root.appendingPathComponent("runtimes/embedding/\(entry.version)", isDirectory: true)
    case .multilingualE5Small:
      guard entry.destination == "models/multilingual-e5-small" else {
        throw ManagedDependencyError.manifest
      }
      return root.appendingPathComponent("models/multilingual-e5-small/\(entry.version)", isDirectory: true)
    case .multilingualE5Base:
      guard entry.destination == "models/multilingual-e5-base" else {
        throw ManagedDependencyError.manifest
      }
      return root.appendingPathComponent("models/multilingual-e5-base/\(entry.version)", isDirectory: true)
    }
  }

  private func installFiles(_ files: [DependencyFile], entry: DependencyEntry,
                            staging: URL) async throws -> URL {
    let payload = staging.appendingPathComponent("payload", isDirectory: true)
    try ensurePrivateDirectory(payload)
    for file in files {
      try Task.checkCancellation()
      let temporary = try await download(file.url, expectedSize: file.size, staging: staging)
      guard try fileDigest(temporary) == file.sha256 else { throw ManagedDependencyError.integrity }
      let destination = payload.appendingPathComponent(file.path)
      try fileManager.moveItem(at: temporary, to: destination)
    }
    let fileMap = Dictionary(uniqueKeysWithValues: files.map { ($0.path, $0.sha256) })
    let modelManifest = try JSONSerialization.data(
      withJSONObject: ["revision": entry.version, "files": fileMap], options: [.prettyPrinted, .sortedKeys])
    try modelManifest.write(to: payload.appendingPathComponent("manifest.json"), options: .atomic)
    return payload
  }

  private func installArchive(_ entry: DependencyEntry, id: ManagedDependencyID,
                              staging: URL) async throws -> URL {
    guard let url = entry.url, let expectedSize = entry.compressedSize,
          let expectedDigest = entry.sha256, let archive = entry.archive,
          let archiveRoot = entry.archiveRoot else { throw ManagedDependencyError.manifest }
    let downloaded = try await download(url, expectedSize: expectedSize, staging: staging)
    guard try fileDigest(downloaded) == expectedDigest else { throw ManagedDependencyError.integrity }
    let names = try await runProcess("/usr/bin/tar", ["-tf", downloaded.path], capture: true)
    let entries = String(decoding: names, as: UTF8.self).split(separator: "\n", omittingEmptySubsequences: true)
    guard !entries.isEmpty, entries.count <= 4096,
          entries.allSatisfy({ raw in
            let name = String(raw).hasSuffix("/") ? String(raw.dropLast()) : String(raw)
            return validRelative(name) && (name == archiveRoot || name.hasPrefix(archiveRoot + "/"))
          })
    else { throw ManagedDependencyError.archive }
    let extracted = staging.appendingPathComponent("extracted", isDirectory: true)
    try ensurePrivateDirectory(extracted)
    let mode = archive == "tar_gzip" ? "-xzf" : "-xJf"
    try await runProcess("/usr/bin/tar", [mode, downloaded.path, "-C", extracted.path], capture: false)
    let root = extracted.appendingPathComponent(archiveRoot, isDirectory: true)
    if id == .rust {
      guard let installer = entry.installer else { throw ManagedDependencyError.manifest }
      let payload = staging.appendingPathComponent("payload", isDirectory: true)
      try ensurePrivateDirectory(payload)
      try await runProcess(
        "/bin/sh", [root.appendingPathComponent(installer).path,
                    "--prefix=\(payload.path)", "--disable-ldconfig"],
        capture: false, environment: ["PATH": "/usr/bin:/bin", "LANG": "C"])
      return payload
    }
    if id == .embeddingRuntime {
      return try await installEmbeddingRuntime(uvRoot: root, entry: entry, staging: staging)
    }
    return root
  }

  private func installEmbeddingRuntime(uvRoot: URL, entry: DependencyEntry,
                                       staging: URL) async throws -> URL {
    guard entry.installKind == "uv_embedding", let pythonVersion = entry.pythonVersion,
          let packages = entry.pythonPackages, let excludeNewer = entry.excludeNewer else {
      throw ManagedDependencyError.manifest
    }
    let uv = uvRoot.appendingPathComponent("uv")
    guard fileManager.isExecutableFile(atPath: uv.path) else { throw ManagedDependencyError.install }
    let payload = staging.appendingPathComponent("payload", isDirectory: true)
    let pythonStore = payload.appendingPathComponent("python", isDirectory: true)
    let runtime = payload.appendingPathComponent("runtime", isDirectory: true)
    let isolatedHome = staging.appendingPathComponent("home", isDirectory: true)
    try ensurePrivateDirectory(payload)
    try ensurePrivateDirectory(isolatedHome)
    let environment = ["HOME": isolatedHome.path, "PATH": "/usr/bin:/bin", "LANG": "C",
                       "LC_ALL": "C", "UV_NO_CONFIG": "1"]
    try await runProcess(
      uv.path,
      ["python", "install", pythonVersion, "--install-dir", pythonStore.path, "--no-bin",
       "--no-registry", "--managed-python", "--no-cache", "--no-progress", "--no-config"],
      capture: false, environment: environment)
    let children = try fileManager.contentsOfDirectory(
      at: pythonStore, includingPropertiesForKeys: [.isDirectoryKey], options: [.skipsHiddenFiles])
    guard children.count == 1,
          (try children[0].resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true else {
      throw ManagedDependencyError.install
    }
    try fileManager.moveItem(at: children[0], to: runtime)
    try fileManager.removeItem(at: pythonStore)
    guard let required = entry.requiredExecutable else { throw ManagedDependencyError.manifest }
    let python = payload.appendingPathComponent(required)
    guard fileManager.isExecutableFile(atPath: python.path) else { throw ManagedDependencyError.install }
    let requirements = packages.keys.sorted().map { "\($0)==\(packages[$0]!)" }
    var arguments = ["pip", "install", "--python", python.path, "--exact", "--no-cache",
                     "--no-progress", "--no-config", "--no-python-downloads",
                     "--only-binary", ":all:", "--exclude-newer", excludeNewer]
    arguments.append(contentsOf: requirements)
    try await runProcess(uv.path, arguments, capture: false, environment: environment)
    let packageExpression = packages.keys.sorted().map { "\"\($0)\"" }.joined(separator: ",")
    let receipt = try await runProcess(
      python.path,
      ["-I", "-c", "import importlib.metadata,json; n=[\(packageExpression)]; print(json.dumps({k:importlib.metadata.version(k) for k in n},sort_keys=True,separators=(',',':')))"],
      capture: true, environment: ["HOME": isolatedHome.path, "PATH": "/usr/bin:/bin",
                                   "LANG": "C", "LC_ALL": "C", "PYTHONNOUSERSITE": "1"])
    let observed = try JSONDecoder().decode([String: String].self, from: receipt)
    guard observed == packages else { throw ManagedDependencyError.integrity }
    let encoded = try JSONEncoder().encode(observed)
    try encoded.write(to: payload.appendingPathComponent("embedding-packages.json"), options: [.atomic])
    return payload
  }

  private func download(_ url: URL, expectedSize: Int64, staging: URL) async throws -> URL {
    try Task.checkCancellation()
    let configuration = URLSessionConfiguration.ephemeral
    configuration.urlCache = nil
    configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
    configuration.timeoutIntervalForRequest = 60
    configuration.timeoutIntervalForResource = 60 * 60
    let session = URLSession(configuration: configuration, delegate: HTTPSRedirectDelegate(), delegateQueue: nil)
    defer { session.invalidateAndCancel() }
    var request = URLRequest(url: url)
    request.httpMethod = "HEAD"
    let (_, metadataResponse) = try await session.data(for: request)
    guard let metadata = metadataResponse as? HTTPURLResponse, metadata.statusCode == 200,
          metadata.url?.scheme == "https", metadata.expectedContentLength == expectedSize
    else { throw ManagedDependencyError.invalidResponse }
    try Task.checkCancellation()
    let (temporary, response) = try await session.download(from: url)
    guard let response = response as? HTTPURLResponse, response.statusCode == 200,
          response.url?.scheme == "https", response.expectedContentLength == expectedSize
    else { throw ManagedDependencyError.invalidResponse }
    let attributes = try fileManager.attributesOfItem(atPath: temporary.path)
    guard (attributes[.type] as? FileAttributeType) == .typeRegular,
          (attributes[.size] as? NSNumber)?.int64Value == expectedSize
    else { throw ManagedDependencyError.integrity }
    let owned = staging.appendingPathComponent("download-\(UUID().uuidString)")
    try fileManager.moveItem(at: temporary, to: owned)
    return owned
  }

  private func metadata(_ url: URL) async throws -> (Int64, URL) {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.urlCache = nil
    configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
    configuration.timeoutIntervalForRequest = 60
    let session = URLSession(configuration: configuration, delegate: HTTPSRedirectDelegate(), delegateQueue: nil)
    defer { session.invalidateAndCancel() }
    var request = URLRequest(url: url)
    request.httpMethod = "HEAD"
    let (_, response) = try await session.data(for: request)
    guard let http = response as? HTTPURLResponse, http.statusCode == 200,
          let finalURL = http.url, finalURL.scheme == "https",
          http.expectedContentLength > 0
    else { throw ManagedDependencyError.invalidResponse }
    return (http.expectedContentLength, finalURL)
  }

  private func resolvedGGUFURL(_ source: URL) throws -> URL {
    guard validDownloadURL(source) else { throw ManagedDependencyError.manifest }
    if source.host?.lowercased() != "huggingface.co" {
      _ = try ggufFilename(source)
      return source
    }
    var parts = source.path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
    guard parts.count >= 5, parts[2] == "blob" || parts[2] == "resolve" else {
      throw ManagedDependencyError.manifest
    }
    parts[2] = "resolve"
    var components = URLComponents(url: source, resolvingAgainstBaseURL: false)
    components?.path = "/" + parts.joined(separator: "/")
    components?.fragment = nil
    guard let result = components?.url else { throw ManagedDependencyError.manifest }
    _ = try ggufFilename(result)
    return result
  }

  private func ggufFilename(_ url: URL) throws -> String {
    let filename = url.lastPathComponent.removingPercentEncoding ?? url.lastPathComponent
    guard !filename.isEmpty, filename.utf8.count <= 255,
          filename.lowercased().hasSuffix(".gguf"), filename != ".", filename != "..",
          !filename.contains("/"), !filename.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    else { throw ManagedDependencyError.manifest }
    return filename
  }

  private func ggufRoot() -> URL {
    home.appendingPathComponent(".polaris/models/llama.cpp", isDirectory: true)
  }

  private func validatePayload(_ payload: URL, entry: DependencyEntry, id: ManagedDependencyID,
                               verifyHashes: Bool = true) throws {
    var directory: ObjCBool = false
    guard fileManager.fileExists(atPath: payload.path, isDirectory: &directory), directory.boolValue else {
      throw ManagedDependencyError.install
    }
    if let files = entry.files {
      for file in files {
        let url = payload.appendingPathComponent(file.path)
        let attributes = try fileManager.attributesOfItem(atPath: url.path)
        let hashMatches: Bool
        if verifyHashes { hashMatches = try fileDigest(url) == file.sha256 }
        else { hashMatches = true }
        guard (attributes[.type] as? FileAttributeType) == .typeRegular,
              (attributes[.size] as? NSNumber)?.int64Value == file.size,
              hashMatches
        else { throw ManagedDependencyError.integrity }
      }
      guard fileManager.fileExists(atPath: payload.appendingPathComponent("manifest.json").path) else {
        throw ManagedDependencyError.integrity
      }
      return
    }
    guard let required = entry.requiredExecutable else { throw ManagedDependencyError.manifest }
    let executable = payload.appendingPathComponent(required)
    guard fileManager.isExecutableFile(atPath: executable.path),
          (try fileManager.attributesOfItem(atPath: executable.path)[.type] as? FileAttributeType) == .typeRegular
    else { throw ManagedDependencyError.install }
    if id == .llamaCpp {
      let resolved = executable.resolvingSymlinksInPath().path
      guard resolved == executable.path else { throw ManagedDependencyError.install }
    }
    if id == .embeddingRuntime {
      guard let expected = entry.pythonPackages,
            let data = try? Data(contentsOf: payload.appendingPathComponent("embedding-packages.json")),
            data.count <= 16 * 1024,
            (try? JSONDecoder().decode([String: String].self, from: data)) == expected
      else { throw ManagedDependencyError.integrity }
    }
  }

  private func writeReceipt(entry: DependencyEntry, id: ManagedDependencyID,
                            manifestDigest: String, destination: URL) throws {
    let receipt = DependencyReceipt(
      schemaVersion: 1, id: id.rawValue, version: entry.version,
      manifestSHA256: manifestDigest, installedAt: ISO8601DateFormatter().string(from: Date()))
    let data = try JSONEncoder().encode(receipt)
    try data.write(to: receiptURL(for: destination), options: [.atomic])
  }

  private func receiptURL(for destination: URL) -> URL {
    destination.deletingLastPathComponent().appendingPathComponent(".\(destination.lastPathComponent).receipt.json")
  }

  private func requiredSpace(_ entry: DependencyEntry) -> Int64 {
    if let required = entry.requiredSpace { return required }
    let bytes = entry.files?.reduce(0, { $0 + $1.size }) ?? entry.compressedSize ?? 0
    return max(512 * 1024 * 1024, bytes * 3 + 256 * 1024 * 1024)
  }

  private func checkFreeSpace(required: Int64, at url: URL) throws {
    let attributes = try fileManager.attributesOfFileSystem(forPath: url.path)
    guard let free = (attributes[.systemFreeSize] as? NSNumber)?.int64Value, free >= required else {
      throw ManagedDependencyError.insufficientSpace
    }
  }

  private func ensurePrivateDirectory(_ url: URL) throws {
    try fileManager.createDirectory(at: url, withIntermediateDirectories: true,
                                    attributes: [.posixPermissions: 0o700])
    try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
  }

  private func tomlString(_ value: String) throws -> String {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.withoutEscapingSlashes]
    let data = try encoder.encode(value)
    guard let encoded = String(data: data, encoding: .utf8) else {
      throw ManagedDependencyError.install
    }
    return encoded
  }

  private func writeManagedResources(_ data: Data) throws {
    let directory = home.appendingPathComponent(".polaris", isDirectory: true)
    try ensurePrivateDirectory(directory)
    let destination = directory.appendingPathComponent("managed-resources.toml")
    if fileManager.fileExists(atPath: destination.path) {
      let values = try destination.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
      guard values.isRegularFile == true, values.isSymbolicLink != true else {
        throw ManagedDependencyError.conflict
      }
    }
    let temporary = directory.appendingPathComponent(".managed-resources-\(UUID().uuidString).tmp")
    defer { try? fileManager.removeItem(at: temporary) }
    try data.write(to: temporary, options: [.withoutOverwriting])
    try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: temporary.path)
    if fileManager.fileExists(atPath: destination.path) {
      _ = try fileManager.replaceItemAt(destination, withItemAt: temporary)
    } else {
      try fileManager.moveItem(at: temporary, to: destination)
    }
    try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
  }

  private func fileDigest(_ url: URL) throws -> String {
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var digest = SHA256()
    while let chunk = try handle.read(upToCount: 1024 * 1024), !chunk.isEmpty {
      try Task.checkCancellation()
      digest.update(data: chunk)
    }
    return digest.finalize().map { String(format: "%02x", $0) }.joined()
  }

  private func hexDigest(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
  }

  private func validDigest(_ value: String) -> Bool {
    value.count == 64 && value.utf8.allSatisfy { ($0 >= 48 && $0 <= 57) || ($0 >= 97 && $0 <= 102) }
  }

  private func validVersion(_ value: String) -> Bool {
    !value.isEmpty && value.utf8.count <= 64
      && value.utf8.allSatisfy { byte in
        (byte >= 48 && byte <= 57) || (byte >= 65 && byte <= 90) || (byte >= 97 && byte <= 122)
          || byte == 45 || byte == 46 || byte == 43
      }
  }

  private func validDownloadURL(_ url: URL) -> Bool {
    url.scheme == "https" && url.user == nil && url.password == nil && url.fragment == nil
      && url.host != nil
  }

  private func validRelative(_ value: String) -> Bool {
    !value.isEmpty && value.utf8.count <= 1024 && !value.hasPrefix("/")
      && value.split(separator: "/", omittingEmptySubsequences: false).allSatisfy {
        !$0.isEmpty && $0 != "." && $0 != ".."
      }
  }

  @discardableResult
  private func runProcess(_ executable: String, _ arguments: [String], capture: Bool,
                          environment: [String: String]? = nil) async throws -> Data {
    let control = ManagedProcessControl()
    return try await withTaskCancellationHandler {
      try await Task.detached(priority: .utility) {
        let process = Process()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.environment = environment ?? [:]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = capture ? output : FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try control.start(process)
        let data = capture ? output.fileHandleForReading.readDataToEndOfFile() : Data()
        process.waitUntilExit()
        if control.finish() { throw CancellationError() }
        guard process.terminationReason == .exit, process.terminationStatus == 0,
              data.count <= 2 * 1024 * 1024 else { throw ManagedDependencyError.archive }
        return data
      }.value
    } onCancel: {
      control.cancel()
    }
  }
}
