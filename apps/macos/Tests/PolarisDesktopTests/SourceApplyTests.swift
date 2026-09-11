import Foundation
import XCTest
@testable import PolarisDesktop

private actor SourceApplyTransport: WorkspaceServiceTransport {
  var calls: [ServiceRequest] = []
  var stalePage = false
  var loseResolve = false
  var expired = false
  var pendingResultLifecycle = false
  var resultPublished = false
  var pendingResultListsBeforePublication: Int?
  var pendingResultListReads = 0
  var suspendNextResolvedSnapshot = false
  var suspendedSnapshot: CheckedContinuation<Void, Never>?
  var snapshotSuspensionWaiter: CheckedContinuation<Void, Never>?
  var snapshotCancellationObserved = false
  var suspendNextSourceApplyPage = false
  var suspendedSourceApplyPage: CheckedContinuation<Void, Never>?
  var sourceApplyPageSuspensionWaiter: CheckedContinuation<Void, Never>?
  var events: [ServiceValue] = []
  let snapshot: ServiceValue
  var resolved = false
  var postResolveSnapshots = 0
  var historyRevision: UInt64 = 10
  init(snapshot: ServiceValue) { self.snapshot = snapshot }
  func setStalePage() { stalePage = true }
  func loseNextResolve() { loseResolve = true }
  func setExpired() { expired = true }
  func enablePendingResultLifecycle() { pendingResultLifecycle = true }
  func publishSavedResultAfterPendingLists(_ count: Int) {
    pendingResultLifecycle = true
    pendingResultListsBeforePublication = count
  }
  func suspendNextResultReconciliationSnapshot() { suspendNextResolvedSnapshot = true }
  func waitForSuspendedSnapshot() async {
    if suspendedSnapshot != nil { return }
    await withCheckedContinuation { snapshotSuspensionWaiter = $0 }
  }
  func releaseSuspendedSnapshot() {
    suspendedSnapshot?.resume()
    suspendedSnapshot = nil
  }
  func suspendNextSourceApplyPageRequest() { suspendNextSourceApplyPage = true }
  func waitForSuspendedSourceApplyPage() async {
    if suspendedSourceApplyPage != nil { return }
    await withCheckedContinuation { sourceApplyPageSuspensionWaiter = $0 }
  }
  func releaseSuspendedSourceApplyPage() {
    suspendedSourceApplyPage?.resume()
    suspendedSourceApplyPage = nil
  }
  func publishSavedResultEvent() {
    resultPublished = true
    events.append(.object(["protocol_version": .number("1"), "kind": .string("event"),
      "engine_epoch": .string("epoch"), "subscription_id": .string("sub"), "session_id": .string("session"),
      "event_seq": .string("1"), "session_revision": .string("12"), "type": .string("run.state"),
      "payload": .object(["run_id": .string("run"), "attempt_id": .string("attempt"),
        "state": .string("succeeded"), "task_ids": .array([])])]))
  }
  func enqueue(_ event: ServiceValue) { events.append(event) }
  func start() throws -> ServiceHello {
    try ServiceHello(.object([
      "protocol_version": .number("1"), "engine_epoch": .string("epoch"),
      "capabilities": .array(["session_read", "history_read", "source_apply_read", "source_apply_resolve", "shutdown"].map(ServiceValue.string)),
      "limits": .object(["frame_bytes": .string("1048576"), "subscription_events": .string("256"),
        "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"), "text_batch_ms": .string("50")]),
    ]))
  }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    calls.append(request)
    let payload: ServiceValue
    switch request {
    case .snapshot, .subscribe:
      if resolved && suspendNextResolvedSnapshot {
        suspendNextResolvedSnapshot = false
        snapshotSuspensionWaiter?.resume()
        snapshotSuspensionWaiter = nil
        await withCheckedContinuation { suspendedSnapshot = $0 }
        if Task.isCancelled {
          snapshotCancellationObserved = true
          throw CancellationError()
        }
      }
      if resultPublished {
        historyRevision = 12
      } else if resolved {
        postResolveSnapshots += 1
        historyRevision = postResolveSnapshots == 1 ? 10 : 11
      } else { historyRevision = 10 }
      payload = revisedSnapshot(historyRevision)
    case .sourceApplyList(_, let revision, let offset, let limit):
      guard offset == 0, limit == 32 else { throw ServiceError.correlation }
      if resolved && revision == 10 {
        return .init(requestID: requestID, method: nil, payload: nil,
          rejection: .object(["code": .string("revision_conflict"), "message": .string("遅延した一覧")]))
      }
      if pendingResultLifecycle && resolved && !resultPublished {
        pendingResultListReads += 1
        if let threshold = pendingResultListsBeforePublication, pendingResultListReads >= threshold {
          // Result persistence advances the durable snapshot. A stale list
          // must be reconciled by reading again, never by repeating resolve.
          resultPublished = true
          return .init(requestID: requestID, method: nil, payload: nil,
            rejection: .object(["code": .string("revision_conflict"), "message": .string("結果保存後の一覧")]))
        }
      }
      guard revision == (resultPublished ? 12 : resolved ? 11 : 10) else { throw ServiceError.correlation }
      payload = .object(["session_revision": .string(String(revision)), "items": .array([candidate()])])
    case .sourceApplyPage(_, let approval, let hash, let revision, let offset, let limit):
      let expectedRevision: UInt64 = resultPublished ? 12 : resolved ? 11 : 10
      guard approval == "approval", hash == "payload", revision == expectedRevision, offset == 0, limit == 32 else { throw ServiceError.correlation }
      if suspendNextSourceApplyPage {
        suspendNextSourceApplyPage = false
        sourceApplyPageSuspensionWaiter?.resume()
        sourceApplyPageSuspensionWaiter = nil
        await withCheckedContinuation { suspendedSourceApplyPage = $0 }
      }
      payload = .object(["approval_id": .string("approval"), "payload_hash": .string("payload"),
        "session_revision": .string(stalePage ? String(expectedRevision - 1) : String(expectedRevision)), "entries": .array([.object(["relative_path": .string("src/main.rs"),
          "before": .object(["hash": .string("before"), "mode": .number("420")]),
          "after": .object(["hash": .string("after"), "mode": .number("420")])])])])
    case .sourceApplyResolve(_, let run, let attempt, let approval, let hash, let revision, let policy, let decision):
      guard run == "run", attempt == "attempt", approval == "approval", hash == "payload", revision == 10, policy == 2 else { throw ServiceError.correlation }
      if loseResolve { loseResolve = false; throw ServiceError.io }
      resolved = true
      payload = .object(["approval_id": .string("approval"), "state": .string("resolved"), "decision": .string(decision.rawValue)])
    case .shutdown: payload = .object(["engine_epoch": .string("epoch"), "state": .string("ready")])
    default: throw ServiceError.notReady
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
    let payload: ServiceValue
    switch request {
    case .history: payload = .object(["snapshot_id": .string("snapshot"), "session_revision": .string(String(historyRevision)), "messages": .array([])])
    case .status: payload = .object(["status": .string("not_found")])
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func takeEvents() -> [ServiceValue] { defer { events = [] }; return events }
  func close() async -> ServiceClient.Exit? { .init(status: 0, forced: false) }
  private func candidate() -> ServiceValue {
    if pendingResultLifecycle && resolved && !resultPublished {
      return .object(["run_id": .string("run"), "attempt_id": .string("attempt"), "approval_id": .string("approval"),
        "operation_id": .string("operation"), "policy_revision": .string("2"), "expires_at_unix_ms": .string(expired ? "1" : "1893456000000"),
        "payload_hash": .string("payload"), "source_path": .string("/safe/source"), "recovery_parent_path": .string("/safe/recovery"),
        "entry_count": .string("1"), "invalidated": .bool(false), "intent_committed": .bool(true), "result_saved": .bool(false)])
    }
    return .object(["run_id": .string("run"), "attempt_id": .string("attempt"), "approval_id": .string("approval"),
      "operation_id": .string("operation"), "policy_revision": .string("2"), "expires_at_unix_ms": .string(expired ? "1" : "1893456000000"),
      "payload_hash": .string("payload"), "source_path": .string("/safe/source"), "recovery_parent_path": .string("/safe/recovery"),
      "entry_count": .string("1"), "invalidated": .bool(false), "intent_committed": .bool(false), "result_saved": .bool(true),
      "result": .object(["result_id": .string("result"), "status": .string("partial"), "failure_kind": .string("conflict"),
        "installed_count": .string("1"), "deleted_count": .string("0"), "restored_count": .string("1")])])
  }
  private func revisedSnapshot(_ revision: UInt64) -> ServiceValue {
    guard case .object(var fields) = snapshot else { return snapshot }
    fields["session_revision"] = .string(String(revision))
    return .object(fields)
  }
}

@MainActor
final class SourceApplyTests: XCTestCase {
  private var repository: URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
      .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
  }
  private func snapshot(runState: String = "succeeded") throws -> ServiceValue {
    let url = repository.appendingPathComponent("crates/polaris-desktop-protocol/tests/fixtures/snapshot.json")
    guard case .object(var value) = try ServiceCodec.json(Data(contentsOf: url)) else { throw ServiceError.schema }
    value["session_id"] = .string("session"); value["snapshot_id"] = .string("snapshot")
    value["session_revision"] = .string("10"); value["policy_revision"] = .string("2")
    value["history_start_cursor"] = .string("first")
    value["position"] = .object(["engine_epoch": .string("epoch"), "subscription_id": .string("sub"), "event_seq": .string("0")])
    value["runs"] = .array([.object(["run_id": .string("run"), "attempt_id": .string("attempt"), "state": .string(runState), "task_ids": .array([])])])
    return .object(value)
  }
  func testSavedCandidateUsesDedicatedBoundedProtocolAndSeparatesResult() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "project", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native", makeTransport: { transport })
    await model.start()
    await model.loadSourceApplyCandidates()
    let candidate = try XCTUnwrap(model.sourceApplyCandidates.first)
    XCTAssertEqual(candidate.result?.status, "partial")
    XCTAssertEqual(candidate.result?.installedCount, 1)
    XCTAssertFalse(model.canResolveSourceApply(candidate, decision: .allow))
    XCTAssertTrue(model.canResolveSourceApply(candidate, decision: .deny))
    await model.loadSourceApplyPage(candidate)
    XCTAssertEqual(model.sourceApplyPages[candidate.key]?.entries.first?.relativePath, "src/main.rs")
    XCTAssertTrue(model.canResolveSourceApply(candidate, decision: .allow))
    await model.resolveSourceApply(candidate, decision: .deny)
    let calls = await transport.calls
    XCTAssertTrue(calls.contains { if case .sourceApplyList(_, 10, 0, 32) = $0 { return true }; return false })
    XCTAssertTrue(calls.contains { if case .sourceApplyPage(_, "approval", "payload", 10, 0, 32) = $0 { return true }; return false })
    XCTAssertTrue(calls.contains { if case .sourceApplyResolve(_, "run", "attempt", "approval", "payload", 10, 2, .deny) = $0 { return true }; return false })
    XCTAssertEqual(calls.filter { $0.method == "source_apply.resolve" }.count, 1)
    XCTAssertEqual(calls.filter { $0.method == "source_apply.list" }.count, 2)
    XCTAssertEqual(model.phase, .ready)
    XCTAssertNil(model.failure)
    _ = await model.close()
  }

  func testSourceApplyWireRejectsUnboundedPageAndOmitsPathsFromResolve() throws {
    XCTAssertThrowsError(try ServiceRequest.sourceApplyList(session: "session", revision: 1, offset: 0, limit: 33)
      .wire(clientID: "client", requestID: "request"))
    let wire = try ServiceRequest.sourceApplyResolve(session: "session", run: "run", attempt: "attempt",
      approval: "approval", hash: "hash", revision: 3, policy: 4, decision: .allow).wire(clientID: "client", requestID: "request")
    XCTAssertEqual(wire["method"], .string("source_apply.resolve"))
    XCTAssertNil(wire["params"]?["source_path"])
    XCTAssertNil(wire["params"]?["entries"])
    XCTAssertEqual(wire["params"]?["expected_session_revision"], .string("3"))
    func page(mode: ServiceValue) -> ServiceValue {
      .object(["approval_id": .string("approval"), "payload_hash": .string("hash"), "session_revision": .string("3"),
        "entries": .array([.object(["relative_path": .string("file"),
          "after": .object(["hash": .string("hash"), "mode": mode])])])])
    }
    XCTAssertNoThrow(try ServiceSchema.sourceApplyPage.check(page(mode: .number("420"))))
    XCTAssertThrowsError(try ServiceSchema.sourceApplyPage.check(page(mode: .string("420"))))
    XCTAssertThrowsError(try ServiceSchema.sourceApplyPage.check(page(mode: .number("4294967296"))))
  }

  func testStalePageAndLostResolveStayUnconfirmedWithoutRetry() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "project", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native", makeTransport: { transport })
    await model.start(); await model.loadSourceApplyCandidates()
    let candidate = try XCTUnwrap(model.sourceApplyCandidates.first)
    await transport.setStalePage(); await model.loadSourceApplyPage(candidate)
    XCTAssertNil(model.sourceApplyPages[candidate.key])
    XCTAssertNotNil(model.sourceApplyNotice)
    _ = await model.close()
    let retryTransport = SourceApplyTransport(snapshot: try snapshot())
    let retryStore = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "project-retry", sessionID: "session")
    let retryModel = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: retryStore,
      clientID: "native-retry", makeTransport: { retryTransport })
    await retryModel.start(); await retryModel.loadSourceApplyCandidates()
    let retryCandidate = try XCTUnwrap(retryModel.sourceApplyCandidates.first)
    await retryModel.loadSourceApplyPage(retryCandidate)
    await retryTransport.loseNextResolve(); await retryModel.resolveSourceApply(retryCandidate, decision: .allow)
    XCTAssertTrue(retryModel.sourceApplyOutcomeUnknown.contains(retryCandidate.key))
    XCTAssertFalse(retryModel.canResolveSourceApply(retryCandidate))
    let calls = await retryTransport.calls
    XCTAssertEqual(calls.filter { $0.method == "source_apply.resolve" }.count, 1)
    _ = await retryModel.close()
  }

  func testExpiredCandidateCannotResolveAndMissingResultIsNotSuccess() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    await transport.setExpired()
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "project", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native", makeTransport: { transport })
    await model.start(); await model.loadSourceApplyCandidates()
    let expired = try XCTUnwrap(model.sourceApplyCandidates.first)
    XCTAssertTrue(expired.isExpired(at: Date(timeIntervalSince1970: 2)))
    XCTAssertFalse(expired.shouldShowExpiry(at: Date(timeIntervalSince1970: 2)), "保存済み結果は期限表示で上書きしない")
    XCTAssertFalse(model.canResolveSourceApply(expired))
    let missing = try SourceApplyCandidate(.object(["run_id": .string("run"), "attempt_id": .string("attempt"),
      "approval_id": .string("missing"), "operation_id": .string("operation"), "policy_revision": .string("2"),
      "expires_at_unix_ms": .string("1893456000000"), "payload_hash": .string("hash"), "source_path": .string("/source"),
      "recovery_parent_path": .string("/recovery"), "entry_count": .string("0"), "invalidated": .bool(false),
      "intent_committed": .bool(true), "result_saved": .bool(true)]))
    XCTAssertTrue(missing.resultSaved); XCTAssertNil(missing.result)
    _ = await model.close()
  }

  func testNonterminalRunCannotAuthorizeEvenAfterFullReview() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot(runState: "awaiting_approval"))
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "project-open", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native-open", makeTransport: { transport })
    await model.start(); await model.loadSourceApplyCandidates()
    let candidate = try XCTUnwrap(model.sourceApplyCandidates.first)
    await model.loadSourceApplyPage(candidate)
    XCTAssertFalse(model.canResolveSourceApply(candidate, decision: .allow))
    XCTAssertFalse(model.canResolveSourceApply(candidate, decision: .deny))
    await model.resolveSourceApply(candidate, decision: .allow)
    let calls = await transport.calls
    XCTAssertEqual(calls.filter { $0.method == "source_apply.resolve" }.count, 0)
    _ = await model.close()
  }

  func testResultWithoutSavedRecordIsRejected() throws {
    let malformed: ServiceValue = .object(["run_id": .string("run"), "attempt_id": .string("attempt"),
      "approval_id": .string("approval"), "operation_id": .string("operation"), "policy_revision": .string("2"),
      "expires_at_unix_ms": .string("1893456000000"), "payload_hash": .string("hash"), "source_path": .string("/source"),
      "recovery_parent_path": .string("/recovery"), "entry_count": .string("0"), "invalidated": .bool(false),
      "intent_committed": .bool(false), "result_saved": .bool(false),
      "result": .object(["result_id": .string("result"), "status": .string("applied"),
        "installed_count": .string("1"), "deleted_count": .string("0"), "restored_count": .string("0")])])
    XCTAssertThrowsError(try SourceApplyCandidate(malformed))
  }

  func testPendingResultReconcilesAfterIntentWithoutResolveReplay() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    await transport.enablePendingResultLifecycle()
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
      projectID: "project-result", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native-result", makeTransport: { transport },
      sourceApplyResultReconciliationDelays: Array(repeating: .milliseconds(10), count: 6))
    await model.start(); await model.loadSourceApplyCandidates()
    let candidate = try XCTUnwrap(model.sourceApplyCandidates.first)
    await model.loadSourceApplyPage(candidate)
    XCTAssertTrue(model.canResolveSourceApply(candidate, decision: .allow))
    await model.resolveSourceApply(candidate, decision: .allow)
    XCTAssertTrue(try XCTUnwrap(model.sourceApplyCandidates.first).intentCommitted)
    XCTAssertNil(model.sourceApplyCandidates.first?.result)

    // The list has already returned the saved intent. The later ordinary event
    // represents a persisted result revision; reconciliation must only reread.
    await transport.publishSavedResultEvent()
    await model.poll()
    for _ in 0..<30 where model.sourceApplyCandidates.first?.result == nil {
      try await Task.sleep(for: .milliseconds(100))
    }
    XCTAssertEqual(model.sourceApplyCandidates.first?.result?.status, "partial")
    XCTAssertNil(model.failure)
    let calls = await transport.calls
    XCTAssertEqual(calls.filter { $0.method == "source_apply.resolve" }.count, 1)
    XCTAssertGreaterThanOrEqual(calls.filter { $0.method == "source_apply.list" }.count, 3)
    _ = await model.close()
  }

  func testPendingResultContinuesPastThreeReadsAndStopsOnClose() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    await transport.publishSavedResultAfterPendingLists(5)
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
      projectID: "project-delayed-result", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native-delayed-result", makeTransport: { transport },
      sourceApplyResultReconciliationDelays: Array(repeating: .milliseconds(10), count: 6))
    await model.start(); await model.loadSourceApplyCandidates()
    let candidate = try XCTUnwrap(model.sourceApplyCandidates.first)
    await model.loadSourceApplyPage(candidate); await model.resolveSourceApply(candidate, decision: .allow)
    for _ in 0..<30 where model.sourceApplyCandidates.first?.result == nil {
      try await Task.sleep(for: .milliseconds(20))
    }
    XCTAssertEqual(model.sourceApplyCandidates.first?.result?.status, "partial")
    let pendingResultListReads = await transport.pendingResultListReads
    let delayedCalls = await transport.calls
    XCTAssertGreaterThanOrEqual(pendingResultListReads, 5)
    XCTAssertEqual(delayedCalls.filter { $0.method == "source_apply.resolve" }.count, 1)
    _ = await model.close()

    // A fresh pending instance proves close cancels delayed background reads.
    let stoppingTransport = SourceApplyTransport(snapshot: try snapshot())
    await stoppingTransport.enablePendingResultLifecycle()
    let stoppingStore = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
      projectID: "project-stop-result", sessionID: "session")
    let stoppingModel = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: stoppingStore,
      clientID: "native-stop-result", makeTransport: { stoppingTransport },
      sourceApplyResultReconciliationDelays: [.seconds(1)])
    await stoppingModel.start(); await stoppingModel.loadSourceApplyCandidates()
    let stoppingCandidate = try XCTUnwrap(stoppingModel.sourceApplyCandidates.first)
    await stoppingModel.loadSourceApplyPage(stoppingCandidate)
    await stoppingModel.resolveSourceApply(stoppingCandidate, decision: .allow)
    let listsBeforeClose = (await stoppingTransport.calls).filter { $0.method == "source_apply.list" }.count
    _ = await stoppingModel.close()
    try await Task.sleep(for: .milliseconds(50))
    let listsAfterClose = (await stoppingTransport.calls).filter { $0.method == "source_apply.list" }.count
    XCTAssertEqual(listsAfterClose, listsBeforeClose)

    let unresolvedTransport = SourceApplyTransport(snapshot: try snapshot())
    await unresolvedTransport.enablePendingResultLifecycle()
    let unresolvedStore = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
      projectID: "project-unconfirmed-result", sessionID: "session")
    let unresolvedModel = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: unresolvedStore,
      clientID: "native-unconfirmed-result", makeTransport: { unresolvedTransport },
      sourceApplyResultReconciliationDelays: [.milliseconds(10)])
    await unresolvedModel.start(); await unresolvedModel.loadSourceApplyCandidates()
    let unresolvedCandidate = try XCTUnwrap(unresolvedModel.sourceApplyCandidates.first)
    await unresolvedModel.loadSourceApplyPage(unresolvedCandidate)
    await unresolvedModel.resolveSourceApply(unresolvedCandidate, decision: .allow)
    try await Task.sleep(for: .milliseconds(50))
    XCTAssertTrue(unresolvedModel.sourceApplyResultTimedOut)
    let unresolvedCalls = await unresolvedTransport.calls
    XCTAssertEqual(unresolvedCalls.filter { $0.method == "source_apply.resolve" }.count, 1)
    let listsAtDeadline = unresolvedCalls.filter { $0.method == "source_apply.list" }.count
    await unresolvedModel.poll(); await unresolvedModel.poll(); await unresolvedModel.poll()
    let listsAfterEmptyPolls = (await unresolvedTransport.calls).filter { $0.method == "source_apply.list" }.count
    XCTAssertTrue(unresolvedModel.sourceApplyResultTimedOut)
    XCTAssertEqual(listsAfterEmptyPolls, listsAtDeadline)
    _ = await unresolvedModel.close()

    let busyTransport = SourceApplyTransport(snapshot: try snapshot())
    await busyTransport.enablePendingResultLifecycle()
    let busyStore = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
      projectID: "project-busy-deadline", sessionID: "session")
    let busyModel = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: busyStore,
      clientID: "native-busy-deadline", makeTransport: { busyTransport },
      sourceApplyResultReconciliationDelays: [.milliseconds(100)])
    await busyModel.start(); await busyModel.loadSourceApplyCandidates()
    let busyCandidate = try XCTUnwrap(busyModel.sourceApplyCandidates.first)
    await busyModel.loadSourceApplyPage(busyCandidate); await busyModel.resolveSourceApply(busyCandidate, decision: .allow)
    let committedCandidate = try XCTUnwrap(busyModel.sourceApplyCandidates.first)
    await busyTransport.setStalePage(); await busyTransport.suspendNextSourceApplyPageRequest()
    let foregroundPage = Task { @MainActor in await busyModel.loadSourceApplyPage(committedCandidate) }
    await busyTransport.waitForSuspendedSourceApplyPage()
    try await Task.sleep(for: .milliseconds(150))
    XCTAssertTrue(busyModel.sourceApplyResultTimedOut)
    await busyTransport.releaseSuspendedSourceApplyPage()
    _ = await foregroundPage.value
    XCTAssertNotNil(busyModel.failure)
    XCTAssertEqual(busyModel.sourceApplyNotice, "変更内容を現在の候補と照合できませんでした。再送せず、状態を更新してください。")
    XCTAssertTrue(busyModel.sourceApplyResultTimedOut)
    _ = await busyModel.close()
  }

  func testCloseDrainsInFlightResultSnapshotBeforeGracefulShutdown() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    await transport.enablePendingResultLifecycle()
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
      projectID: "project-drain-result", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native-drain-result", makeTransport: { transport },
      sourceApplyResultReconciliationDelays: [.seconds(1)])
    await model.start(); await model.loadSourceApplyCandidates()
    let candidate = try XCTUnwrap(model.sourceApplyCandidates.first)
    await model.loadSourceApplyPage(candidate)
    await model.resolveSourceApply(candidate, decision: .allow)
    await transport.suspendNextResultReconciliationSnapshot()
    await transport.waitForSuspendedSnapshot()

    let closing = Task { @MainActor in await model.close() }
    let concurrentClosing = Task { @MainActor in await model.close() }
    try await Task.sleep(for: .milliseconds(10))
    await transport.releaseSuspendedSnapshot()
    let didClose = await closing.value
    let didConcurrentlyClose = await concurrentClosing.value
    let calls = await transport.calls
    let snapshotCancellationObserved = await transport.snapshotCancellationObserved
    XCTAssertTrue(didClose)
    XCTAssertTrue(didConcurrentlyClose)
    XCTAssertEqual(calls.filter { $0.method == "source_apply.resolve" }.count, 1)
    XCTAssertEqual(calls.filter { $0.method == "shutdown.request" }.count, 1)
    XCTAssertFalse(snapshotCancellationObserved)
  }

  func testExpiryEventAloneDoesNotCreateGenericServiceFailure() async throws {
    let transport = SourceApplyTransport(snapshot: try snapshot())
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "project-event", sessionID: "session")
    let model = try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
      clientID: "native-event", makeTransport: { transport })
    await model.start()
    await transport.enqueue(.object(["protocol_version": .number("1"), "kind": .string("event"),
      "engine_epoch": .string("epoch"), "subscription_id": .string("sub"), "session_id": .string("session"),
      "event_seq": .string("1"), "session_revision": .string("11"), "type": .string("approval.expired"),
      "payload": .object(["approval_id": .string("approval"), "reason": .string("expired")])]))
    await model.poll()
    XCTAssertEqual(model.phase, .ready)
    XCTAssertNil(model.failure)
    _ = await model.close()
  }
}
