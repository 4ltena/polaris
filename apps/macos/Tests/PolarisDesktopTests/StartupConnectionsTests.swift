import Foundation
import PolarisSettings
import XCTest

@testable import PolarisDesktop

private actor StartupFixture {
  private var pending:
    [StartupConnections.Connection: CheckedContinuation<StartupConnections.Outcome, Never>] = [:]
  private(set) var calls = 0
  func check(_ connection: StartupConnections.Connection) async -> StartupConnections.Outcome {
    calls += 1
    return await withCheckedContinuation { pending[connection] = $0 }
  }
  func release(_ connection: StartupConnections.Connection, _ outcome: StartupConnections.Outcome) {
    pending.removeValue(forKey: connection)?.resume(returning: outcome)
  }
}

@MainActor
final class StartupConnectionsTests: XCTestCase {
  private func selected() -> Preferences {
    var value = Preferences()
    value.gpt = .chatGPT
    value.local = .linkInstalled
    return value
  }
  private func waitUntil(_ predicate: () async -> Bool) async throws {
    for _ in 0..<200 {
      if await predicate() { return }
      try await Task.sleep(for: .milliseconds(5))
    }
    XCTFail("条件が期限内に成立しませんでした")
  }

  func testDefaultCompletesSynchronouslyWithoutProbing() {
    let checker = StartupConnections()
    var callbacks = 0
    XCTAssertTrue(
      checker.start(savedPreferences: nil) { result in
        callbacks += 1
        XCTAssertEqual(result.gpt.state, .notConfigured)
        XCTAssertEqual(result.local.state, .notConfigured)
      })
    XCTAssertEqual(callbacks, 1)
    checker.start(savedPreferences: selected()) { result in
      callbacks += 1
      XCTAssertEqual(result.gpt.state, .unavailable)
      XCTAssertEqual(result.local.state, .unavailable)
      XCTAssertNil(result.gpt.measuredAt)
    }
    XCTAssertEqual(callbacks, 2)
    XCTAssertFalse(checker.cleanupPending)
  }

  func testUnconfiguredSkipsInjectedOperation() {
    let checker = StartupConnections(operation: { _ in
      XCTFail("未設定の接続確認は禁止")
      return .failed
    })
    var completed = false
    checker.start(savedPreferences: Preferences()) { _ in completed = true }
    XCTAssertTrue(completed)
  }

  func testParallelChecksAndConcurrentStartDoNotAddWorkers() async throws {
    let fixture = StartupFixture()
    let checker = StartupConnections(operation: { await fixture.check($0) })
    var completions = 0
    checker.start(savedPreferences: selected()) { _ in completions += 1 }
    try await waitUntil { await fixture.calls == 2 }
    let generation = checker.results.generation
    for _ in 0..<100 {
      XCTAssertFalse(
        checker.start(savedPreferences: selected()) { result in
          XCTAssertEqual(result.generation, generation)
        })
    }
    let calls = await fixture.calls
    XCTAssertEqual(calls, 2)
    await fixture.release(.gpt, .reachable)
    try await waitUntil { checker.results.gpt.state == .reachable }
    XCTAssertEqual(completions, 0)
    await fixture.release(.local, .unconfirmed)
    try await waitUntil { completions == 1 }
    XCTAssertEqual(checker.results.local.state, .unconfirmed)
    XCTAssertNotNil(checker.results.gpt.measuredAt)
    XCTAssertFalse(checker.cleanupPending)
  }

  func testDeadlineSettlesWithoutWaitingForUncooperativeWorkers() async throws {
    let fixture = StartupFixture()
    let checker = StartupConnections(
      operation: { await fixture.check($0) }, deadline: .milliseconds(80))
    var completions = 0
    let started = ContinuousClock.now
    checker.start(savedPreferences: selected()) { _ in completions += 1 }
    try await waitUntil { await fixture.calls == 2 }
    await fixture.release(.gpt, .reachable)
    try await waitUntil { completions == 1 }
    XCTAssertLessThan(started.duration(to: .now), .seconds(1))
    XCTAssertEqual(checker.results.gpt.state, .reachable)
    XCTAssertEqual(checker.results.local.state, .timedOut)
    XCTAssertTrue(checker.cleanupPending)
    let settled = checker.results
    for _ in 0..<100 {
      XCTAssertFalse(checker.start(savedPreferences: selected()) { XCTAssertEqual($0, settled) })
    }
    await fixture.release(.local, .reachable)
    try await waitUntil { !checker.cleanupPending }
    XCTAssertEqual(checker.results, settled)
    XCTAssertEqual(completions, 1)
  }

  func testCancelFreezesResultsUntilBothSlotsReclaimedThenAllowsNewGeneration() async throws {
    let fixture = StartupFixture()
    let checker = StartupConnections(operation: { await fixture.check($0) })
    var completions = 0
    checker.start(savedPreferences: selected()) { _ in completions += 1 }
    try await waitUntil { await fixture.calls == 2 }
    checker.cancel()
    checker.cancel()
    XCTAssertEqual(completions, 1)
    XCTAssertEqual(checker.results.gpt.state, .cancelled)
    XCTAssertEqual(checker.results.local.state, .cancelled)
    let settled = checker.results
    await fixture.release(.gpt, .failed)
    try await Task.sleep(for: .milliseconds(10))
    XCTAssertTrue(checker.cleanupPending)
    XCTAssertFalse(checker.start(savedPreferences: selected()) { _ in })
    await fixture.release(.local, .reachable)
    try await waitUntil { !checker.cleanupPending }
    XCTAssertEqual(checker.results, settled)
    XCTAssertTrue(checker.start(savedPreferences: selected()) { _ in completions += 1 })
    XCTAssertEqual(checker.results.generation, settled.generation + 1)
    try await waitUntil { await fixture.calls == 4 }
    await fixture.release(.gpt, .failed)
    await fixture.release(.local, .reachable)
    try await waitUntil { completions == 2 }
    XCTAssertEqual(checker.results.gpt.state, .failed)
    XCTAssertEqual(checker.results.local.state, .reachable)
  }
}

private final class ProbeRequests: @unchecked Sendable {
  private let lock = NSLock()
  private var values: [URLRequest] = []
  func append(_ request: URLRequest) { lock.lock(); defer { lock.unlock() }; values.append(request) }
  func reset() { lock.lock(); defer { lock.unlock() }; values = [] }
  func snapshot() -> [URLRequest] { lock.lock(); defer { lock.unlock() }; return values }
}

private final class ProbeProtocol: URLProtocol, @unchecked Sendable {
  static let requests = ProbeRequests()
  override class func canInit(with request: URLRequest) -> Bool { true }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    Self.requests.append(request)
    let url = request.url!
    let status = url.path == "/v1/models" ? 401 : 405
    let response = HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1", headerFields: [:])!
    client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
    client?.urlProtocolDidFinishLoading(self)
  }
  override func stopLoading() {}
}

extension StartupConnectionsTests {
  func testProductionProbesSendCredentialFreeHeadAndAccept401And405() async throws {
    for selection in [GPTPreference.chatGPT, .apiKey] {
      ProbeProtocol.requests.reset()
      let checker = StartupConnections(usesProductionProbes: true, makeSessionConfiguration: {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [ProbeProtocol.self]
        return configuration
      })
      var preferences = Preferences(); preferences.gpt = selection; preferences.local = .later
      var completed = false
      XCTAssertTrue(checker.start(savedPreferences: preferences) { _ in completed = true })
      try await waitUntil { completed }
      XCTAssertEqual(checker.results.gpt.state, .reachable)
      let requests = ProbeProtocol.requests.snapshot()
      XCTAssertEqual(requests.count, 1)
      let request = try XCTUnwrap(requests.first)
      XCTAssertEqual(request.httpMethod, "HEAD")
      XCTAssertNil(request.value(forHTTPHeaderField: "Authorization"))
      XCTAssertNil(request.value(forHTTPHeaderField: "Cookie"))
      XCTAssertNil(request.httpBody)
      XCTAssertEqual(request.url?.host, selection == .apiKey ? "api.openai.com" : "chatgpt.com")
    }
  }

  func testProbeRedirectDoesNotForwardRequest() async {
    let session = URLSession(configuration: .ephemeral)
    defer { session.invalidateAndCancel() }
    let original = URL(string: "https://chatgpt.com/backend-api/codex/models")!
    let redirect = URLRequest(url: URL(string: "https://unexpected.invalid")!)
    let accepted: URLRequest? = await withCheckedContinuation { continuation in
      NoProbeRedirects().urlSession(session, task: session.dataTask(with: original),
        willPerformHTTPRedirection: HTTPURLResponse(url: original, statusCode: 302, httpVersion: nil, headerFields: [:])!,
        newRequest: redirect) { continuation.resume(returning: $0) }
    }
    XCTAssertNil(accepted)
  }
}
