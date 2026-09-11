import Combine
import Foundation
import PolarisSettings

/// Startup status only. No endpoint, credential, or inference readiness is retained.
@MainActor
final class StartupConnections: ObservableObject {
  enum Connection: Sendable, CaseIterable { case gpt, local }
  enum State: Sendable, Equatable {
    case notConfigured, unavailable, checking, reachable, unconfirmed, failed, timedOut, cancelled
  }
  enum Outcome: Sendable { case reachable, unconfirmed, failed }
  struct Result: Sendable, Equatable {
    let state: State
    let measuredAt: Date?
  }
  struct Results: Sendable, Equatable {
    let generation: UInt64
    var gpt: Result
    var local: Result
  }
  /// Injected status probes share the production transport lifecycle contract.
  /// Operations must suspend without blocking the main actor, and own transport cleanup.
  /// Cancellation signals intent; it does not prove remote work stopped.
  typealias Operation = @Sendable (Connection) async -> Outcome

  @Published private(set) var results = Results(
    generation: 0, gpt: Result(state: .notConfigured, measuredAt: nil),
    local: Result(state: .notConfigured, measuredAt: nil))
  @Published private(set) var cleanupPending = false
  private let operation: Operation?
  private let usesProductionProbes: Bool
  private let makeSessionConfiguration: @Sendable () -> URLSessionConfiguration
  private let deadline: Duration
  private var workers: [Connection: Task<Void, Never>] = [:]
  private var timer: Task<Void, Never>?
  private var completion: (@MainActor (Results) -> Void)?

  init(operation: Operation? = nil, deadline: Duration = .seconds(3), usesProductionProbes: Bool = false,
       makeSessionConfiguration: @escaping @Sendable () -> URLSessionConfiguration = { .ephemeral }) {
    self.operation = operation
    self.usesProductionProbes = usesProductionProbes
    self.makeSessionConfiguration = makeSessionConfiguration
    self.deadline = min(max(deadline, .zero), .seconds(3))
  }

  /// False means the existing generation still owns its slots. No retry is queued.
  /// Completion may run synchronously when no connection needs checking.
  @discardableResult
  func start(
    savedPreferences: Preferences?, completion: @escaping @MainActor (Results) -> Void
  ) -> Bool {
    guard self.completion == nil, workers.isEmpty, timer == nil else {
      completion(results)
      return false
    }
    let generation = results.generation &+ 1
    let operation = self.operation ?? (usesProductionProbes ? Self.networkOperation(savedPreferences, makeSessionConfiguration: makeSessionConfiguration) : nil)
    let gptSelected = savedPreferences.map { $0.gpt != .later } ?? false
    let localSelected = savedPreferences.map { $0.local != .later } ?? false
    func initial(_ selected: Bool) -> Result {
      Result(
        state: selected ? (operation == nil ? .unavailable : .checking) : .notConfigured,
        measuredAt: nil)
    }
    results = Results(
      generation: generation, gpt: initial(gptSelected), local: initial(localSelected))
    guard let operation, gptSelected || localSelected else {
      completion(results)
      return true
    }
    self.completion = completion
    for connection in Connection.allCases {
      guard connection == .gpt ? gptSelected : localSelected else { continue }
      workers[connection] = Task { [weak self] in
        let outcome = await operation(connection)
        self?.finished(connection, generation: generation, outcome: outcome)
      }
    }
    timer = Task { [weak self, deadline] in
      do { try await Task.sleep(for: deadline) } catch { return }
      self?.settlePending(as: .timedOut)
    }
    return true
  }

  /// Credential-free HTTP reachability only; never claims authentication or inference readiness.
  nonisolated private static func networkOperation(_ preferences: Preferences?,
    makeSessionConfiguration: @escaping @Sendable () -> URLSessionConfiguration) -> Operation {
    let gpt = preferences?.gpt
    let local = preferences?.executionBinding
    return { connection in
      let urls: [URL]
      switch connection {
      case .gpt:
        urls = [URL(string: gpt == .apiKey ? "https://api.openai.com/v1/models"
                    : "https://chatgpt.com/backend-api/codex/models")!]
      case .local:
        if let local, [.ollama, .lmstudio].contains(local.provider), let endpoint = local.localEndpoint,
           let base = URL(string: endpoint) {
          urls = [base.appendingPathComponent(local.provider == .ollama ? "api/tags" : "api/v1/models")]
        } else {
          urls = [URL(string: "http://127.0.0.1:11434/api/tags")!, URL(string: "http://127.0.0.1:1234/api/v1/models")!]
        }
      }
      let configuration = makeSessionConfiguration()
      configuration.timeoutIntervalForRequest = 1.25
      configuration.timeoutIntervalForResource = 2.5
      configuration.httpShouldSetCookies = false
      configuration.urlCredentialStorage = nil
      let session = URLSession(configuration: configuration, delegate: NoProbeRedirects(), delegateQueue: nil)
      defer { session.invalidateAndCancel() }
      for url in urls {
        if Task.isCancelled { return .unconfirmed }
        var request = URLRequest(url: url); request.httpMethod = "HEAD"
        do {
          let (_, response) = try await session.data(for: request)
          if let http = response as? HTTPURLResponse,
             (200..<300).contains(http.statusCode) || [401, 403, 405].contains(http.statusCode) { return .reachable }
        } catch { if Task.isCancelled { return .unconfirmed } }
      }
      return .failed
    }
  }

  func cancel() { settlePending(as: .cancelled) }

  private func finished(_ connection: Connection, generation: UInt64, outcome: Outcome) {
    guard generation == results.generation else { return }
    workers[connection] = nil
    if completion != nil {
      let state: State
      switch outcome {
      case .reachable: state = .reachable
      case .unconfirmed: state = .unconfirmed
      case .failed: state = .failed
      }
      let result = Result(state: state, measuredAt: Date())
      switch connection {
      case .gpt: results.gpt = result
      case .local: results.local = result
      }
      if workers.isEmpty { complete() }
    } else {
      // Late results release occupied slots, never overwrite the settled UI result.
      cleanupPending = !workers.isEmpty
    }
  }

  private func settlePending(as state: State) {
    guard completion != nil else { return }
    for connection in workers.keys {
      let result = Result(state: state, measuredAt: Date())
      switch connection {
      case .gpt: results.gpt = result
      case .local: results.local = result
      }
    }
    for worker in workers.values { worker.cancel() }
    cleanupPending = !workers.isEmpty
    complete()
  }

  private func complete() {
    timer?.cancel()
    timer = nil
    let callback = completion
    completion = nil
    callback?(results)
  }

  deinit {
    timer?.cancel()
    for worker in workers.values { worker.cancel() }
  }
}

final class NoProbeRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  func urlSession(_ session: URLSession, task: URLSessionTask,
                  willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
                  completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
    completionHandler(nil)
  }
}
