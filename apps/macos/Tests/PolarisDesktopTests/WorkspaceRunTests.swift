import Foundation
import XCTest
@testable import PolarisDesktop

private actor RunTransport: WorkspaceServiceTransport {
  var enabled = true
  var calls: [(String, ServiceRequest)] = []
  var revision: UInt64 = 10
  var draftRevision: UInt64 = 4
  var text = "合成入力"
  var runState: String?
  var events: [ServiceValue] = []
  var history: [ServiceValue] = []
  var eventWaiter: CheckedContinuation<Void, Never>?
  var holdEvents = false
  func setHistory(_ values: [ServiceValue]) { history = values }
  func suspendEvents() { holdEvents = true }
  func eventsWaiting() -> Bool { eventWaiter != nil }
  func releaseEvents() { holdEvents = false; eventWaiter?.resume(); eventWaiter = nil }

  var status: ServiceValue = .object(["status": .string("not_found")])
  var hold: String?
  var waiter: CheckedContinuation<Void, Never>?
  var staleAfterRun = false
  func returnStaleSnapshotAfterRun() { staleAfterRun = true }
  var failRun = false
  var refusal: String?
  var statusReads = 0
  var pendingApprovals: [ServiceValue] = []
  func reject(_ code: String) { refusal = code }
  func setApprovals(_ values: [ServiceValue]) { pendingApprovals = values }
  func enqueue(_ values: [ServiceValue]) { events += values }
  func setRun(_ state: String?) { runState = state }
  func callCount(_ method: String) -> Int { calls.filter { $0.1.method == method }.count }

  func configure(enabled: Bool) { self.enabled = enabled }
  func suspend(_ method: String) { hold = method }
  func waiting() -> Bool { waiter != nil }
  func release() { hold = nil; waiter?.resume(); waiter = nil }
  func loseRunACK() { failRun = true }
  func setStatus(_ value: ServiceValue) { status = value }
  func start() throws -> ServiceHello {
    let caps = ["session_read", "history_read", "draft_update", "request_status", "shutdown"]
      + (enabled ? ["run_start", "run_cancel", "approval_resolve"] : [])
    return try ServiceHello(.object([
      "protocol_version": .number("1"), "engine_epoch": .string("e"),
      "capabilities": .array(caps.map(ServiceValue.string)),
      "limits": .object(["frame_bytes": .string("1048576"), "subscription_events": .string("256"),
        "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"), "text_batch_ms": .string("50")])]))
  }
  func snapshot() -> ServiceValue {
    .object(["snapshot_id": .string("snap"), "session_id": .string("s"), "summary": .string(""),
      "session_revision": .string(String(revision)), "content_revision": .string("0"), "plan_revision": .string("0"),
      "policy_revision": .string("2"), "history_start_cursor": .string("first"),
      "position": .object(["engine_epoch": .string("e"), "subscription_id": .string("sub"), "event_seq": .string("0")]),
      "draft": .object(["draft_revision": .string(String(draftRevision)), "text": .string(text), "attachment_ids": .array([])]),
      "configuration": .object(["configuration_revision": .string("3"), "provider": .string("synthetic"), "model": .string("test"), "effort": .string("medium")]),
      "runs": .array(runState.map { [run($0)] } ?? []), "children": .array([]), "child_attempt_count": .string("0"), "role_bindings": .array([]), "role_catalog": .array([]),
      "tasks": .array([]), "unresolved_approvals": .array(pendingApprovals)])
  }
  func run(_ state: String) -> ServiceValue {
    .object(["run_id": .string("r"), "attempt_id": .string("a"), "state": .string(state), "task_ids": .array([])])
  }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    calls.append((requestID, request))
    if hold == request.method { await withCheckedContinuation { waiter = $0 } }
    if let refusal {
      self.refusal = nil
      return .init(requestID: requestID, method: nil, payload: nil,
        rejection: .object(["code": .string(refusal), "message": .string("合成拒否")]))
    }
    let payload: ServiceValue
    switch request {
    case .workspaceRead, .attachmentRead, .localModels, .configureRoles, .sourceApplyList, .sourceApplyPage, .sourceApplyResolve: throw ServiceError.notReady
    case .configure: throw ServiceError.notReady
    case .snapshot, .subscribe:
      if staleAfterRun, runState != nil {
        // Simulate a structurally valid pre-ACK snapshot, including the old draft.
        let savedRevision = revision, savedDraft = draftRevision, savedText = text, savedRun = runState
        revision = 10; draftRevision = 4; text = "合成入力"; runState = nil
        payload = snapshot()
        revision = savedRevision; draftRevision = savedDraft; text = savedText; runState = savedRun
      } else { payload = snapshot() }
    case .draft(_, let expected, let value):
      guard expected == draftRevision else { throw ServiceError.correlation }
      draftRevision += 1; revision += 1; text = value
      payload = .object(["session_revision": .string(String(revision)), "draft_revision": .string(String(draftRevision))])
    case .run(_, let draft, let config, let policy):
      guard draft == draftRevision, config == 3, policy == 2 else { throw ServiceError.correlation }
      revision += 1; draftRevision += 1; text = ""; runState = "queued"
      if failRun { throw ServiceError.io }
      payload = .object(["session_revision": .string(String(revision)), "run_id": .string("r"), "attempt_id": .string("a"), "state": .string("queued")])
    case .cancel: payload = .object(["status": .string("cancel_requested")])
    case .shutdown: payload = .object(["engine_epoch": .string("e"), "state": .string("ready")])
    case .approval(_, let id, _, _, _, let decision):
      pendingApprovals.removeAll { $0["approval_id"] == .string(id) }
      payload = .object(["approval_id": .string(id), "state": .string("resolved"), "decision": .string(decision.rawValue)])
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
    let payload: ServiceValue
    switch request {
    case .history(_, let cursor, let limit):
      let start = cursor == "first" ? 0 : Int(cursor)!
      let end = min(history.count, start + limit)
      var fields: [String: ServiceValue] = ["snapshot_id": .string("snap"), "session_revision": .string(String(staleAfterRun && runState != nil ? 10 : revision)), "messages": .array(Array(history[start..<end]))]
      if end < history.count { fields["next_cursor"] = .string(String(end)) }
      payload = .object(fields)
    case .status: statusReads += 1; payload = status
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func takeEvents() async -> [ServiceValue] {
    if holdEvents { await withCheckedContinuation { eventWaiter = $0 } }
    defer { events = [] }; return events
  }
  func close() -> ServiceClient.Exit? { .init(status: 0, forced: false) }
}

@MainActor
final class WorkspaceRunTests: XCTestCase {
  private func model(_ transport: RunTransport) throws -> WorkspaceServiceModel {
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "p", sessionID: "s")
    return try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native", makeTransport: { transport })
  }
  func testAdvertisedRunCapabilityEnablesOnlyRealAdapter() async throws {
    for enabled in [false, true] {
      let transport = RunTransport(); await transport.configure(enabled: enabled)
      let model = try model(transport); await model.start()
      XCTAssertEqual(model.canSend, enabled)
      _ = await model.close()
    }
  }
}

extension WorkspaceRunTests {
  private func wait(_ transport: RunTransport) async throws {
    let deadline = ContinuousClock.now + .seconds(3)
    while !(await transport.waiting()), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    let waiting = await transport.waiting(); XCTAssertTrue(waiting)
  }
  private func event(_ type: String, _ payload: ServiceValue, sequence: UInt64, epoch: String = "e") -> ServiceValue {
    .object(["protocol_version": .number("1"), "kind": .string("event"), "engine_epoch": .string(epoch),
      "subscription_id": .string("sub"), "session_id": .string("s"), "event_seq": .string(String(sequence)),
      "session_revision": .string("10"), "type": .string(type), "payload": payload])
  }
  private func delta(_ text: String, offset: UInt64 = 0, saved: Bool = false) -> ServiceValue {
    .object(["message_id": .string("m"), "byte_offset": .string(String(offset)), "text": .string(text),
             "durability": .string(saved ? "saved" : "tentative")])
  }
  private func approval(expires: String = "1893456000000", policy: String = "2") -> ServiceValue {
    .object(["approval_id": .string("approval"), "run_id": .string("r"), "attempt_id": .string("a"),
      "operation_id": .string("op"), "operation": .string("edit"), "scope": .string("synthetic-copy"),
      "payload_hash": .string("synthetic-hash"), "policy_revision": .string(policy), "expires_at_unix_ms": .string(expires),
      "state": .string("pending"), "display": .object(["title": .string("合成承認"), "description": .string("合成操作のみ"),
        "choices": .array([.string("allow"), .string("deny")])])])
  }
  func testSaveBeforeRunPinsRevisionsAndRetainsLateEdit() async throws {
    let t = RunTransport(), m = try model(t)
    await m.start(); m.editDraft("送る本文")
    await t.suspend("draft.update")
    let start = Task { await m.startRun() }
    try await wait(t); m.editDraft("待機中の次の本文")
    await t.release(); await start.value
    XCTAssertEqual(m.phase, .ready); XCTAssertFalse(m.startOutcomeUnknown)
    XCTAssertEqual(m.draftText, "待機中の次の本文"); XCTAssertTrue(m.isDirty)
    XCTAssertFalse(m.draftConflict); XCTAssertEqual(m.savedDraftRevision, 6)
    XCTAssertEqual(m.runs.first?.state, "queued"); XCTAssertFalse(m.canSend)
    let calls = await t.calls
    XCTAssertEqual(calls.filter { $0.1.method == "draft.update" }.count, 1)
    guard case .run(_, let draft, let config, let policy) = calls.first(where: { $0.1.method == "run.start" })?.1 else {
      return XCTFail("開始要求なし")
    }
    XCTAssertEqual(draft, 5); XCTAssertEqual(config, 3); XCTAssertEqual(policy, 2)
    _ = await m.close()
  }
  func testRejectedSaveNeverStartsOldDraft() async throws {
    let t = RunTransport(), m = try model(t)
    await m.start(); m.editDraft("保存拒否の本文"); await t.reject("permission_denied")
    await m.startRun()
    let count = await t.callCount("run.start"); XCTAssertEqual(count, 0)
    XCTAssertTrue(m.isDirty); XCTAssertEqual(m.draftText, "保存拒否の本文")
    _ = await m.close()
  }
  func testUnknownStartRequiresExplicitOriginalStatusWithoutResend() async throws {
    let t = RunTransport(), m = try model(t)
    await m.start(); await t.loseRunACK(); await m.startRun()
    let original = try XCTUnwrap(m.lastStartRequestID)
    XCTAssertTrue(m.startOutcomeUnknown); XCTAssertEqual(m.phase, .unavailable)
    m.editDraft("切断後の次の本文")
    let quit = await m.saveAndClose(); XCTAssertFalse(quit)
    await m.reconnect()
    var reads = await t.statusReads; XCTAssertEqual(reads, 0)
    XCTAssertTrue(m.startOutcomeUnknown); XCTAssertEqual(m.lastStartRequestID, original)
    await m.reconcileStart() // NotFound cannot make another start safe.
    XCTAssertTrue(m.startOutcomeUnknown); await m.startRun()
    var count = await t.callCount("run.start"); XCTAssertEqual(count, 1)
    await t.setStatus(.object(["status": .string("accepted"), "session_revision": .string("11"),
      "run": .object(["run_id": .string("r"), "attempt_id": .string("a")])]))
    await m.reconcileStart()
    XCTAssertFalse(m.startOutcomeUnknown); XCTAssertEqual(m.draftText, "切断後の次の本文")
    XCTAssertTrue(m.isDirty); XCTAssertFalse(m.draftConflict)
    reads = await t.statusReads; XCTAssertEqual(reads, 2)
    count = await t.callCount("run.start"); XCTAssertEqual(count, 1)
    XCTAssertEqual(m.lastStartRequestID, original)
    _ = await m.close()
  }
  func testLateStartACKAfterCloseCannotPublishRun() async throws {
    let t = RunTransport(), m = try model(t)
    await m.start(); await t.suspend("run.start")
    let sending = Task { await m.startRun() }; try await wait(t)
    _ = await m.close(); await t.release(); await sending.value
    XCTAssertTrue(m.runs.isEmpty); XCTAssertTrue(m.startOutcomeUnknown)
  }
  func testStreamOverlapSavedReplacementAndStaleEpoch() async throws {
    let t = RunTransport(), m = try model(t); await m.start()
    await t.enqueue([event("message.delta", delta("日本"), sequence: 1),
      event("message.delta", delta("日本"), sequence: 2),
      event("message.delta", delta("語", offset: 6), sequence: 3)])
    await m.poll()
    XCTAssertEqual(m.streamMessages.first?.text, "日本語")
    await t.enqueue([event("message.delta", delta("保存本文", saved: true), sequence: 4)])
    await m.poll(); XCTAssertEqual(m.streamMessages.first?.text, "保存本文")
    XCTAssertEqual(m.streamMessages.first?.saved, true)
    await t.enqueue([event("message.delta", delta("古い"), sequence: 5, epoch: "old")])
    await m.poll(); XCTAssertEqual(m.phase, .unavailable)
    XCTAssertEqual(m.streamMessages.first?.text, "保存本文")
    _ = await m.close()
  }
  func testCancelACKDoesNotInventTerminalState() async throws {
    let t = RunTransport(); await t.setRun("running")
    let m = try model(t); await m.start()
    let run = try XCTUnwrap(m.runs.first)
    await m.cancelRun(run); await m.cancelRun(run)
    XCTAssertEqual(m.runs.first?.state, "running"); XCTAssertTrue(m.cancelRequested.contains(Data("r".utf8)))
    let count = await t.callCount("run.cancel"); XCTAssertEqual(count, 1)
    _ = await m.close()
  }
  func testApprovalGatesAndExactTypedWire() async throws {
    for (enabled, expires, policy, allowed) in [(true,"1893456000000","2",true),
      (false,"1893456000000","2",false),(true,"0","2",false),(true,"1893456000000","3",false)] {
      let t = RunTransport(); await t.configure(enabled: enabled); await t.setRun("awaiting_approval")
      await t.setApprovals([approval(expires: expires, policy: policy)])
      let m = try model(t); await m.start(); let a = try XCTUnwrap(m.approvals.first)
      XCTAssertEqual(m.canResolve(a), allowed)
      await m.resolveApproval(a, decision: .deny)
      let calls = await t.calls.filter { $0.1.method == "approval.resolve" }
      XCTAssertEqual(calls.count, allowed ? 1 : 0)
      if let call = calls.first {
        let wire = try call.1.wire(clientID: "native", requestID: call.0)
        XCTAssertEqual(wire["params"]?["approval_id"], .string("approval"))
        XCTAssertEqual(wire["params"]?["attempt_id"], .string("a"))
        XCTAssertEqual(wire["params"]?["policy_revision"], .string("2"))
        XCTAssertEqual(wire["params"]?["decision"], .string("deny"))
        XCTAssertTrue(m.approvals.isEmpty)
      }
      _ = await m.close()
    }
  }
}

extension WorkspaceRunTests {
  func testOldApprovalAfterReconnectCannotAnswerEvenSamePayload() async throws {
    let t = RunTransport(); await t.setRun("awaiting_approval"); await t.setApprovals([approval()])
    let m = try model(t); await m.start(); let old = try XCTUnwrap(m.approvals.first)
    _ = await m.close(); await m.reconnect()
    XCTAssertFalse(m.canResolve(old))
    await m.resolveApproval(old, decision: .allow)
    let count = await t.callCount("approval.resolve"); XCTAssertEqual(count, 0)
    let current = try XCTUnwrap(m.approvals.first); XCTAssertTrue(m.canResolve(current))
    _ = await m.close()
  }
  func testUnknownLedgerOrNewerStatusCannotUnlockStart() async throws {
    for status: ServiceValue in [
      .object(["status": .string("outcome_unknown"), "session_revision": .string("11"), "run_id": .string("r"), "attempt_id": .string("a")]),
      .object(["status": .string("completed"), "session_revision": .string("99"), "result_id": .string("result")])
    ] {
      let t = RunTransport(), m = try model(t); await m.start(); await t.loseRunACK(); await m.startRun()
      await m.reconnect(); await t.setStatus(status); await m.reconcileStart()
      XCTAssertTrue(m.startOutcomeUnknown); XCTAssertFalse(m.canSend)
      let count = await t.callCount("run.start"); XCTAssertEqual(count, 1)
      _ = await m.close()
    }
  }
  func testGapAndConflictingDeltaNeverAppend() async throws {
    for payload in [delta("gap", offset: 99), delta("X", offset: 0)] {
      let t = RunTransport(), m = try model(t); await m.start()
      await t.enqueue([event("message.delta", delta("abc"), sequence: 1)])
      await m.poll()
      await t.enqueue([event("message.delta", payload, sequence: 2)])
      await m.poll()
      XCTAssertEqual(m.phase, .unavailable); XCTAssertEqual(m.streamMessages.first?.text, "abc")
      _ = await m.close()
    }
  }
  func testSourceCapabilitiesAreAcceptedWithoutNativeMutationAPI() throws {
    let value: ServiceValue = .object([
      "protocol_version": .number("1"), "engine_epoch": .string("e"),
      "capabilities": .array([.string("source_apply_read"), .string("source_apply_resolve")]),
      "limits": .object(["frame_bytes": .string("1048576"), "subscription_events": .string("256"),
        "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"), "text_batch_ms": .string("50")])])
    XCTAssertEqual(try ServiceHello(value).capabilities, ["source_apply_read", "source_apply_resolve"])
  }
}

extension WorkspaceRunTests {
  func testAmbiguousRunRejectionKeepsOriginalIDAndNoNewStart() async throws {
    for code in ["storage_failed", "recovery_required"] {
      let t = RunTransport(), m = try model(t); await m.start(); await t.reject(code)
      await m.startRun(); let id = m.lastStartRequestID
      XCTAssertEqual(m.phase, .unavailable); XCTAssertTrue(m.startOutcomeUnknown)
      await m.reconnect(); await m.startRun()
      XCTAssertEqual(m.lastStartRequestID, id)
      let count = await t.callCount("run.start"); XCTAssertEqual(count, 1)
      _ = await m.close()
    }
  }
  func testUnknownApprovalAnswerIsNotResentAfterReconnect() async throws {
    let t = RunTransport(); await t.setRun("awaiting_approval"); await t.setApprovals([approval()])
    let m = try model(t); await m.start(); let a = try XCTUnwrap(m.approvals.first)
    await t.reject("storage_failed"); await m.resolveApproval(a, decision: .allow)
    XCTAssertEqual(m.phase, .unavailable)
    await m.reconnect(); let restored = try XCTUnwrap(m.approvals.first)
    XCTAssertFalse(m.canResolve(restored)); await m.resolveApproval(restored, decision: .allow)
    let count = await t.callCount("approval.resolve"); XCTAssertEqual(count, 1)
    _ = await m.close()
  }
  func testDistinctUTF8RunIDsAreNotMergedByCanonicalEquality() async throws {
    let t = RunTransport(), m = try model(t); await m.start()
    let ids = ["\u{00e9}", "e\u{0301}"]
    let values = ids.enumerated().map { index, id in
      event("run.state", .object(["run_id": .string(id), "attempt_id": .string("a"),
        "state": .string("queued"), "task_ids": .array([])]), sequence: UInt64(index + 1))
    }
    await t.enqueue(values); await m.poll()
    XCTAssertEqual(m.runs.count, 2); XCTAssertNotEqual(m.runs[0].key, m.runs[1].key)
    _ = await m.close()
  }
}


extension WorkspaceRunTests {
  func testPreACKSnapshotCannotReenableAnotherStart() async throws {
    let t = RunTransport(), m = try model(t); await m.start()
    await t.returnStaleSnapshotAfterRun(); await m.startRun()
    XCTAssertEqual(m.phase, .unavailable)
    XCTAssertFalse(m.canSend)
    await m.startRun()
    let count = await t.callCount("run.start"); XCTAssertEqual(count, 1)
    _ = await m.close()
  }
}

extension WorkspaceRunTests {
  func testSavedFinalChunksAppendAndOffsetZeroReplayDoesNotTruncate() throws {
    var message = WorkspaceStreamMessage(id: "final")
    try message.apply(delta("仮の応答"))
    let first = String(repeating: "a", count: 32_768)
    let second = String(repeating: "日", count: 1_024)
    try message.apply(delta(first, saved: true))
    try message.apply(delta(second, offset: 32_768, saved: true))
    XCTAssertEqual(message.text, first + second)
    XCTAssertTrue(message.saved)
    try message.apply(delta(first, saved: true))
    XCTAssertEqual(message.text, first + second)
    try message.apply(delta(second, offset: 32_768, saved: true))
    XCTAssertEqual(message.text, first + second)
    let before = message
    XCTAssertThrowsError(try message.apply(delta("X", offset: 32_768, saved: true)))
    XCTAssertEqual(message, before)
    XCTAssertThrowsError(try message.apply(delta("追記", offset: UInt64(message.text.utf8.count))))
    XCTAssertEqual(message, before)
    XCTAssertThrowsError(try message.apply(delta("隙間", offset: UInt64(message.text.utf8.count + 1), saved: true)))
    XCTAssertEqual(message, before)
  }
}


extension WorkspaceRunTests {
  private func historyMessage(_ id: String, _ text: String) -> ServiceValue {
    .object(["message_id": .string(id), "role": .string("assistant"), "text": .string(text),
      "saved_byte_offset": .string(String(text.utf8.count))])
  }
  func testTerminalRefreshRetainsFinalBeyondFirstHistoryPage() async throws {
    let t = RunTransport(); await t.setRun("running")
    let older = (0..<4).map { historyMessage("old-\($0)", "履歴\($0)") }
    await t.setHistory(older)
    let m = try model(t); await m.start()
    let final = "最新の保存済み本文"
    await t.setHistory(older + [historyMessage("m", final)])
    await t.setRun("succeeded")
    await t.enqueue([event("message.delta", delta(final, saved: true), sequence: 1),
      event("run.state", await t.run("succeeded"), sequence: 2)])
    await m.poll()
    XCTAssertEqual(m.phase, .ready)
    XCTAssertEqual(m.messages.count, 4)
    XCTAssertEqual(m.streamMessages.first?.text, final)
    XCTAssertEqual(m.streamMessages.first?.saved, true)
    XCTAssertNotNil(m.nextHistoryCursor)
    _ = await m.close()
  }
  func testTerminalDuringCancelACKIsReconciledByEmptyPoll() async throws {
    let t = RunTransport(); await t.setRun("running")
    let m = try model(t); await m.start(); let run = try XCTUnwrap(m.runs.first)
    await t.suspendEvents()
    let polling = Task { await m.poll() }
    let deadline = ContinuousClock.now + .seconds(3)
    while !(await t.eventsWaiting()), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    let held = await t.eventsWaiting(); XCTAssertTrue(held)
    await t.suspend("run.cancel")
    let cancelling = Task { await m.cancelRun(run) }; try await wait(t)
    await t.setHistory([historyMessage("final", "終了時の本文")]); await t.setRun("cancelled")
    await t.enqueue([event("run.state", await t.run("cancelled"), sequence: 1)])
    await t.releaseEvents(); await polling.value
    let before = await t.callCount("session.subscribe"); XCTAssertEqual(before, 1)
    await t.release(); await cancelling.value
    await m.poll() // No further event is needed to honor the pending refresh.
    let after = await t.callCount("session.subscribe"); XCTAssertEqual(after, 2)
    XCTAssertEqual(m.messages.first?.text, "終了時の本文")
    await m.poll()
    let stable = await t.callCount("session.subscribe"); XCTAssertEqual(stable, 2)
    _ = await m.close()
  }
}

extension WorkspaceRunTests {
  func testSavedStreamHandsOffOnlyToExactHistoryAndLoadedPagesRemainVisible() async throws {
    let t = RunTransport(); await t.setRun("running")
    let older = (0..<4).map { historyMessage("old-\($0)", "履歴\($0)") }
    await t.setHistory(older)
    let m = try model(t); await m.start()
    await t.setHistory(older + [historyMessage("m", "最終本文")]); await t.setRun("succeeded")
    await t.enqueue([event("message.delta", delta("最終本文", saved: true), sequence: 1),
      event("run.state", await t.run("succeeded"), sequence: 2)])
    await m.poll(); await m.loadNextHistoryPage()
    XCTAssertEqual(m.messages.last?.text, "最終本文"); XCTAssertTrue(m.streamMessages.isEmpty)
    await m.refresh() // New first-page snapshot invalidates old cursor, not displayed final text.
    XCTAssertEqual(m.messages.count, 4)
    XCTAssertEqual(m.retainedHistory.map(\.text), ["最終本文"])
    XCTAssertNotNil(m.nextHistoryCursor)
    await m.loadNextHistoryPage()
    XCTAssertTrue(m.retainedHistory.isEmpty); XCTAssertTrue(m.streamMessages.isEmpty)
    XCTAssertEqual(m.messages.filter { $0.id == "m" }.count, 1)
    _ = await m.close()
  }
  func testHistoryMismatchPreservesSavedStreamAndMarksConflict() async throws {
    let t = RunTransport(); await t.setRun("running")
    let m = try model(t); await m.start()
    await t.enqueue([event("message.delta", delta("受信した本文", saved: true), sequence: 1)])
    await m.poll(); await t.setHistory([historyMessage("m", "違う本文")])
    await m.refresh()
    XCTAssertEqual(m.phase, .unavailable); XCTAssertTrue(m.streamHistoryConflict)
    XCTAssertEqual(m.streamMessages.first?.text, "受信した本文")
    XCTAssertTrue(m.messages.isEmpty)
    _ = await m.close()
  }
  func testTerminalRefreshDoesNotPromoteTentativeTextToSaved() async throws {
    let t = RunTransport(); await t.setRun("running")
    let m = try model(t); await m.start(); await t.setRun("interrupted")
    await t.enqueue([event("message.delta", delta("未保存の途中本文"), sequence: 1),
      event("run.state", await t.run("interrupted"), sequence: 2)])
    await m.poll()
    XCTAssertEqual(m.streamMessages.first?.text, "未保存の途中本文")
    XCTAssertEqual(m.streamMessages.first?.saved, false)
    _ = await m.close()
  }
  func testObsoleteSuspendedPollCannotRefreshReplacementGeneration() async throws {
    let t = RunTransport(); await t.setRun("running")
    let m = try model(t); await m.start(); await t.suspendEvents()
    let polling = Task { await m.poll() }
    let deadline = ContinuousClock.now + .seconds(3)
    while !(await t.eventsWaiting()), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
    let held = await t.eventsWaiting(); XCTAssertTrue(held)
    await m.poll() // Must return without a second suspended takeEvents.
    _ = await m.close(); await m.reconnect()
    await t.enqueue([event("run.state", await t.run("cancelled"), sequence: 1)])
    await t.releaseEvents(); await polling.value
    await m.poll()
    XCTAssertEqual(m.runs.first?.state, "running")
    let reads = await t.callCount("session.subscribe"); XCTAssertEqual(reads, 2)
    _ = await m.close()
  }
}


extension WorkspaceRunTests {
  func testHistoryAndRetainedRowsUseByteDistinctIdentity() async throws {
    let composed = "\u{00e9}", decomposed = "e\u{0301}"
    XCTAssertEqual(composed, decomposed, "String identity canonically folds these distinct protocol IDs")
    let t = RunTransport()
    let first = (0..<4).map { historyMessage("old-\($0)", "履歴\($0)") }
    await t.setHistory(first + [historyMessage(composed, "一つ目"), historyMessage(decomposed, "二つ目")])
    let m = try model(t); await m.start(); await m.loadNextHistoryPage()
    XCTAssertEqual(Set(m.messages.map(\.key)).count, 6)
    XCTAssertNotEqual(m.messages[4].key, m.messages[5].key)
    await m.refresh()
    XCTAssertEqual(m.retainedHistory.count, 2)
    XCTAssertEqual(Set(m.retainedHistory.map(\.key)).count, 2)
    XCTAssertEqual(m.retainedHistory.map(\.text), ["一つ目", "二つ目"])
    _ = await m.close()
  }
}
