import AppKit
import Combine
import Foundation
import SwiftUI
import XCTest

@testable import PolarisDesktop

private actor ServiceDemoFixture: ServiceDemoTransport {
  let hello: ServiceHello
  let snapshot: ServiceValue
  var holdHello: Bool
  var holdClose: Bool
  let exit: ServiceClient.Exit?
  var startContinuation: CheckedContinuation<ServiceHello, Error>?
  var closeContinuation: CheckedContinuation<ServiceClient.Exit?, Never>?
  var requests: [ServiceRequest] = []
  var events: [ServiceValue] = []
  var rejectNextRequest = false
  var failRunRequest = false
  var holdRun = false
  var runContinuation: CheckedContinuation<Void, Never>?
  var failNextRequest = false
  var closed = false
  var closeCount = 0
  var startCount = 0

  init(
    snapshot: ServiceValue, holdHello: Bool = false, holdClose: Bool = false,
    exit: ServiceClient.Exit? = .init(status: 0, forced: false)
  ) throws {
    self.snapshot = snapshot
    self.holdHello = holdHello
    self.holdClose = holdClose
    self.exit = exit
    hello = try ServiceHello(
      ServiceCodec.json(
        Data(
          #"{"protocol_version":1,"engine_epoch":"e","capabilities":["session_read","draft_update","run_start","run_cancel","shutdown"],"limits":{"frame_bytes":"1048576","subscription_events":"256","subscription_bytes":"4194304","text_batch_bytes":"32768","text_batch_ms":"50"}}"#
            .utf8)))
  }
  func start() async throws -> ServiceHello {
    startCount += 1
    if holdHello { return try await withCheckedThrowingContinuation { startContinuation = $0 } }
    return hello
  }
  func releaseHello() {
    startContinuation?.resume(returning: hello)
    startContinuation = nil
  }
  func releaseClose() {
    closeContinuation?.resume(returning: exit)
    closeContinuation = nil
  }
  func rejectRequest() { rejectNextRequest = true }
  func failRequest() { failNextRequest = true }
  func failRun() { failRunRequest = true }
  func pauseRun() { holdRun = true }
  func releaseRun() {
    runContinuation?.resume()
    runContinuation = nil
  }
  func enqueue(_ event: ServiceValue) { events.append(event) }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    requests.append(request)
    if failNextRequest {
      failNextRequest = false
      throw ServiceError.timeout
    }
    if rejectNextRequest {
      rejectNextRequest = false
      return ServiceReply(
        requestID: requestID, method: nil, payload: nil,
        rejection: .object(["code": .string("revision_conflict"), "message": .string("fixture")]))
    }
    let payload: ServiceValue
    switch request {
        case .workspaceRead, .attachmentRead, .localModels, .configureRoles, .sourceApplyList, .sourceApplyPage, .sourceApplyResolve: throw ServiceError.notReady
    case .configure: throw ServiceError.notReady
    case .approval: throw ServiceError.notReady
    case .snapshot, .subscribe: payload = snapshot
    case .draft:
      payload = .object(["draft_revision": .string("1"), "session_revision": .string("1")])
    case .run:
      if failRunRequest { throw ServiceError.timeout }
      if holdRun { await withCheckedContinuation { runContinuation = $0 } }
      payload = .object([
        "run_id": .string("r"), "attempt_id": .string("a"), "state": .string("queued"),
        "session_revision": .string("2"),
      ])
    case .cancel: payload = .object(["status": .string("cancel_requested")])
    case .shutdown: payload = .object(["engine_epoch": .string("e"), "state": .string("ready")])
    }
    return ServiceReply(
      requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func takeEvents() async throws -> [ServiceValue] {
    let result = events
    events.removeAll()
    return result
  }
  func close() async -> ServiceClient.Exit? {
    closeCount += 1
    closed = true
    if holdClose { return await withCheckedContinuation { closeContinuation = $0 } }
    return exit
  }
}

@MainActor
final class ServiceDemoTests: XCTestCase {
  private var repository: URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
      .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
  }
  private var helper: URL { repository.appendingPathComponent("target/debug/polaris-fake-service") }
  private func fixture(
    holdHello: Bool = false, holdClose: Bool = false,
    exit: ServiceClient.Exit? = .init(status: 0, forced: false)
  ) throws -> ServiceDemoFixture {
    let url = repository.appendingPathComponent(
      "crates/polaris-desktop-protocol/tests/fixtures/snapshot.json")
    guard case .object(var fields) = try ServiceCodec.json(Data(contentsOf: url)) else {
      throw ServiceError.schema
    }
    fields["session_id"] = .string("fake-session")
    fields["runs"] = .array([])
    return try ServiceDemoFixture(
      snapshot: .object(fields), holdHello: holdHello, holdClose: holdClose, exit: exit)
  }
  private func model(_ transport: ServiceDemoFixture) -> ServiceDemoModel {
    let model = ServiceDemoModel(makeClient: { _ in transport })
    model.selectHelper(helper)
    return model
  }
  private func waitUntil(_ condition: @MainActor () async -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(3)
    while !(await condition()) && ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    let met = await condition()
    XCTAssertTrue(met)
  }

  func testNativeViewFitsBeforeLaunch() throws {
    _ = NSApplication.shared
    let model = ServiceDemoModel()
    let view = NSHostingView(rootView: ServiceDemoView(model: model))
    view.frame = NSRect(x: 0, y: 0, width: 800, height: 780)
    view.layoutSubtreeIfNeeded()
    XCTAssertLessThanOrEqual(view.fittingSize.width, 800)
    XCTAssertLessThanOrEqual(view.fittingSize.height, 780)
    XCTAssertNil(model.helperURL)
    XCTAssertFalse(model.needsClose)
  }

  func testRejectedDraftRequiresNewSnapshotWithoutStartingRun() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    XCTAssertTrue(model.canRun)
    await transport.rejectRequest()
    await model.startRun()
    XCTAssertNil(model.snapshot)
    XCTAssertNil(model.run)
    XCTAssertFalse(model.canRun)
    XCTAssertEqual(model.phase, .connected)
    let requests = await transport.requests
    XCTAssertEqual(requests.map(\.method), ["session.subscribe", "draft.update"])
    let closed = await model.close()
    XCTAssertTrue(closed)
  }

  func testExplicitSelectionDoesNotLaunchAndSnapshotGatesRun() async throws {
    let transport = try fixture()
    let model = ServiceDemoModel(makeClient: { _ in transport })
    XCTAssertNil(model.helperURL)
    XCTAssertFalse(model.canStart)
    model.selectHelper(nil)
    model.selectHelper(URL(fileURLWithPath: "/usr/bin/true"))
    XCTAssertNil(model.helperURL)
    model.selectHelper(helper)
    let starts = await transport.startCount
    XCTAssertEqual(starts, 0)
    XCTAssertTrue(model.canStart)
    await model.start()
    XCTAssertEqual(model.phase, .connected)
    XCTAssertFalse(model.canRun)
    await model.fetchSnapshot()
    XCTAssertTrue(model.canRun)
    model.draftText = String(repeating: "あ", count: 6_000)
    XCTAssertFalse(model.canRun)
    let closed = await model.close()
    XCTAssertTrue(closed)
  }

  func testCancelAckDoesNotBecomeTerminalAndTextStaysBounded() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await model.startRun()
    XCTAssertEqual(model.run?.state, "queued")
    await model.cancelRun()
    XCTAssertTrue(model.cancelAccepted)
    XCTAssertFalse(model.run?.isTerminal ?? true)
    XCTAssertFalse(model.canCancel)
    await transport.enqueue(
      .object([
        "session_id": .string("fake-session"), "event_seq": .string("1"),
        "type": .string("message.delta"),
        "payload": .object([
          "message_id": .string("bounded"), "byte_offset": .string("0"),
          "durability": .string("tentative"),
          "text": .string(String(repeating: "🙂", count: 10_000)),
        ]),
      ]))
    await transport.enqueue(
      .object([
        "session_id": .string("fake-session"), "event_seq": .string("2"),
        "type": .string("run.state"),
        "payload": .object([
          "run_id": .string("r"), "attempt_id": .string("a"), "state": .string("cancelled"),
        ]),
      ]))
    try await waitUntil { model.run?.state == "cancelled" }
    XCTAssertFalse(model.responseText.isEmpty)
    XCTAssertLessThanOrEqual(model.responseText.utf8.count, ServiceDemoModel.maxDisplayBytes)
    XCTAssertFalse(model.responseText.contains("�"))
    let closed = await model.close()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.run?.state, "cancelled")
  }

  func testConcurrentCloseAwaitsReapingAndLateHelloCannotReconnect() async throws {
    let transport = try fixture(holdHello: true, holdClose: true)
    let model = model(transport)
    let start = Task { await model.start() }
    try await waitUntil { await transport.startContinuation != nil }
    let close = Task { await model.close() }
    try await waitUntil { await transport.closeContinuation != nil }
    let secondClose = Task { await model.close() }
    await transport.releaseHello()
    await start.value
    XCTAssertEqual(model.phase, .closing)
    XCTAssertNil(model.hello)
    XCTAssertFalse(model.canStart)
    await transport.releaseClose()
    let first = await close.value
    let second = await secondClose.value
    XCTAssertTrue(first)
    XCTAssertTrue(second)
    let calls = await transport.closeCount
    XCTAssertEqual(calls, 1)
    XCTAssertEqual(model.phase, .stopped)
  }

  func testShutdownReadyWithoutObservedExitNeverEnablesStart() async throws {
    let transport = try fixture(exit: nil)
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await model.startRun()
    let closed = await model.close()
    XCTAssertTrue(model.shutdownReady)
    XCTAssertFalse(closed)
    XCTAssertEqual(model.phase, .unknown)
    XCTAssertEqual(model.run?.state, "outcome_unknown")
    XCTAssertFalse(model.canStart)
    XCTAssertFalse(model.canSelectHelper)
    XCTAssertTrue(model.needsClose)
  }

  func testTransportFailureReapsWithoutRetryAndKeepsUnknown() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await transport.failRequest()
    await model.fetchSnapshot()
    XCTAssertEqual(model.phase, .unknown)
    XCTAssertNotNil(model.lastExit)
    XCTAssertFalse(model.canRun)
    let calls = await transport.requests.count
    XCTAssertEqual(calls, 1)
    let closed = await model.close()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.phase, .stopped)
  }

  func testLostRunACKRemainsUnknownAfterChildReaping() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await transport.failRun()
    await model.startRun()
    XCTAssertTrue(model.runStartPending)
    XCTAssertEqual(model.runLabel, "開始要求の結果未確認")
    XCTAssertEqual(model.phase, .unknown)
    XCTAssertNotNil(model.lastExit)
    XCTAssertFalse(model.canRun)
    let requests = await transport.requests
    XCTAssertEqual(requests.map(\.method), ["session.subscribe", "draft.update", "run.start"])
    let closed = await model.close()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.runLabel, "開始要求の結果未確認")
  }

  func testTerminalEventBeforeRunACKIsNotDowngraded() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await transport.pauseRun()
    let starting = Task { await model.startRun() }
    try await waitUntil { await transport.runContinuation != nil }
    await transport.enqueue(
      .object([
        "session_id": .string("fake-session"), "event_seq": .string("1"),
        "type": .string("run.state"),
        "payload": .object([
          "run_id": .string("r"), "attempt_id": .string("a"), "state": .string("succeeded"),
        ]),
      ]))
    try await waitUntil { model.run?.state == "succeeded" }
    await transport.releaseRun()
    await starting.value
    XCTAssertFalse(model.runStartPending)
    XCTAssertEqual(model.run?.state, "succeeded")
    let closed = await model.close()
    XCTAssertTrue(closed)
  }

  func testRepeatedSnapshotUsesOneSubscription() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    for _ in 0..<10 { await model.fetchSnapshot() }
    let requests = await transport.requests
    XCTAssertEqual(
      requests.map(\.method), ["session.subscribe"] + Array(repeating: "session.snapshot", count: 9)
    )
    let closed = await model.close()
    XCTAssertTrue(closed)
  }

  func testCancelledCloseCallerStillAwaitsChildReaping() async throws {
    let transport = try fixture(holdClose: true)
    let model = model(transport)
    await model.start()
    let closing = Task { await model.close() }
    try await waitUntil { await transport.closeContinuation != nil }
    closing.cancel()
    XCTAssertEqual(model.phase, .closing)
    XCTAssertNil(model.lastExit)
    XCTAssertFalse(model.canStart)
    await transport.releaseClose()
    let closed = await closing.value
    XCTAssertTrue(closed)
    XCTAssertNotNil(model.lastExit)
    XCTAssertEqual(model.phase, .stopped)
  }

  private func delta(
    _ id: String, _ offset: UInt64, _ text: String, _ durability: String, sequence: UInt64
  ) -> ServiceValue {
    .object([
      "session_id": .string("fake-session"), "event_seq": .string(String(sequence)),
      "type": .string("message.delta"),
      "payload": .object([
        "message_id": .string(id), "byte_offset": .string(String(offset)),
        "text": .string(text), "durability": .string(durability),
      ]),
    ])
  }

  func testSavedFullMessageReplacesTentativeWithoutDoubleDisplay() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await model.startRun()
    await transport.enqueue(delta("message-2", 0, "こんにちは。", "tentative", sequence: 1))
    await transport.enqueue(
      delta("message-2", UInt64("こんにちは。".utf8.count), "これはfakeの応答です。", "tentative", sequence: 2))
    await transport.enqueue(delta("message-2", 0, "こんにちは。これはfakeの応答です。", "saved", sequence: 3))
    await transport.enqueue(delta("message-2", 0, "こんにちは。これはfakeの応答です。", "saved", sequence: 4))
    await transport.enqueue(
      .object([
        "session_id": .string("fake-session"), "event_seq": .string("5"),
        "type": .string("run.state"),
        "payload": .object([
          "run_id": .string("r"), "attempt_id": .string("a"), "state": .string("succeeded"),
        ]),
      ]))
    try await waitUntil { model.run?.state == "succeeded" }
    XCTAssertEqual(model.responseText, "こんにちは。これはfakeの応答です。")
    let closed = await model.close()
    XCTAssertTrue(closed)
  }

  private func finishEvent(sequence: UInt64) -> ServiceValue {
    .object([
      "session_id": .string("fake-session"), "event_seq": .string(String(sequence)),
      "type": .string("run.state"),
      "payload": .object([
        "run_id": .string("r"), "attempt_id": .string("a"), "state": .string("succeeded"),
      ]),
    ])
  }

  func testUTF8OffsetsReplayOverlapAndGapDoNotAppendTwice() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await model.startRun()
    await transport.enqueue(delta("m", 0, "日本", "tentative", sequence: 1))
    await transport.enqueue(delta("m", 0, "日本", "tentative", sequence: 2))
    await transport.enqueue(delta("m", 3, "本語🙂", "tentative", sequence: 3))
    await transport.enqueue(delta("m", 100, "隙間", "tentative", sequence: 4))
    await transport.enqueue(delta("m", 0, "相違", "tentative", sequence: 5))
    await transport.enqueue(delta("other", 0, "別メッセージ", "saved", sequence: 6))
    await transport.enqueue(delta("m", UInt64.max, "overflow", "tentative", sequence: 7))
    await transport.enqueue(finishEvent(sequence: 8))
    try await waitUntil { model.run?.state == "succeeded" }
    XCTAssertEqual(model.responseText, "日本語🙂")
    _ = await model.close()
  }

  func testShorterSavedAnswerReplacesTentativeAndRejectsLateTentative() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await model.startRun()
    await transport.enqueue(delta("m", 0, "未確定の長い回答", "tentative", sequence: 1))
    await transport.enqueue(delta("m", 0, "確定", "saved", sequence: 2))
    await transport.enqueue(delta("m", UInt64("確定".utf8.count), "遅着", "tentative", sequence: 3))
    await transport.enqueue(finishEvent(sequence: 4))
    try await waitUntil { model.run?.state == "succeeded" }
    XCTAssertEqual(model.responseText, "確定")
    _ = await model.close()
  }

  func testKnownOldMessageCannotBindBeforeNewRunFirstDelta() async throws {
    let transport = try fixture()
    let model = model(transport)
    await model.start()
    await model.fetchSnapshot()
    await model.startRun()
    await transport.enqueue(delta("old", 0, "旧回答", "saved", sequence: 1))
    await transport.enqueue(finishEvent(sequence: 2))
    try await waitUntil { model.run?.state == "succeeded" }
    await model.fetchSnapshot()
    await model.startRun()
    await transport.enqueue(delta("old", 0, "旧回答", "saved", sequence: 3))
    await transport.enqueue(delta("new", 0, "新", "tentative", sequence: 4))
    await transport.enqueue(delta("old", 0, "旧回答", "tentative", sequence: 5))
    await transport.enqueue(delta("new", 0, "新回答", "saved", sequence: 6))
    // Fixture reuses run IDs; wait on the new saved text before asserting old exclusion.
    try await waitUntil { model.responseText.contains("新回答") }
    XCTAssertEqual(model.responseText, "新回答")
    _ = await model.close()
  }

  func testClosePublishesReleasedOwnershipForUIControlsAndRestart() async throws {
    let transport = try fixture()
    let model = model(transport)
    var ownership: [Bool] = []
    let observation = model.$needsClose.dropFirst().sink { ownership.append($0) }
    await model.start()
    XCTAssertTrue(model.needsClose)
    XCTAssertFalse(model.canSelectHelper)
    let closed = await model.close()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.lastExit?.status, 0)
    XCTAssertEqual(ownership, [true, false])
    XCTAssertTrue(model.canSelectHelper)
    XCTAssertTrue(model.canStart)
    XCTAssertFalse(model.needsClose)
    await model.start()
    XCTAssertEqual(model.phase, .connected)
    XCTAssertTrue(model.needsClose)
    _ = await model.close()
    withExtendedLifetime(observation) {}
  }

  // Each helper launch owns a fresh PrototypeRoot; this does not test recovery of one store.
  func testRealFakeSavedResponseIsDisplayedOnceAndRestartUsesFreshRoot() async throws {
    let model = ServiceDemoModel()
    model.selectHelper(helper)
    await model.start()
    XCTAssertEqual(model.phase, .connected, model.message)
    await model.fetchSnapshot()
    let initial = try XCTUnwrap(model.snapshot)
    XCTAssertEqual(initial["runs"], .array([]))
    XCTAssertTrue(model.canRun, model.message)
    await model.startRun()
    try await waitUntil { model.run?.state == "succeeded" }
    XCTAssertEqual(model.responseText, "こんにちは。これはfakeの応答です。")
    await model.fetchSnapshot()
    XCTAssertNotEqual(model.snapshot?["runs"], initial["runs"])
    let closed = await model.close()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.lastExit?.status, 0)
    XCTAssertFalse(model.needsClose)
    XCTAssertTrue(model.canStart)
    XCTAssertTrue(model.canSelectHelper)
    await model.start()
    XCTAssertEqual(model.phase, .connected, model.message)
    XCTAssertEqual(model.responseText, "")
    XCTAssertNil(model.run)
    await model.fetchSnapshot()
    let restarted = try XCTUnwrap(model.snapshot)
    XCTAssertTrue(model.canRun, model.message)
    // Volatile snapshot/subscription IDs and engine epoch are deliberately excluded.
    for key in ["draft", "configuration", "runs", "children", "session_revision",
                "content_revision", "plan_revision", "policy_revision"] {
      XCTAssertNotNil(initial[key], key)
      XCTAssertEqual(restarted[key], initial[key], key)
    }
    XCTAssertNil(model.run)
    XCTAssertEqual(model.responseText, "")
    let closedAgain = await model.close()
    XCTAssertTrue(closedAgain)
    XCTAssertEqual(model.phase, .stopped)
    XCTAssertEqual(model.lastExit?.status, 0)
    XCTAssertEqual(model.lastExit?.forced, false)
  }

  func testRealFakeHelperThroughDemoModelAndAwaitedClose() async throws {
    let model = ServiceDemoModel()
    model.selectHelper(helper)
    XCTAssertEqual(model.phase, .idle)
    await model.start()
    XCTAssertEqual(model.phase, .connected, model.message)
    await model.fetchSnapshot()
    XCTAssertTrue(model.canRun, model.message)
    await model.startRun()
    XCTAssertNotNil(model.run, model.message)
    await model.cancelRun()
    try await waitUntil { model.run?.isTerminal == true }
    let closed = await model.close()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.phase, .stopped)
    XCTAssertEqual(model.lastExit?.status, 0)
    XCTAssertEqual(model.lastExit?.forced, false)
  }
}
