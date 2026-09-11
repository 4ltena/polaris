import Foundation
import XCTest
import PolarisSettings
@testable import PolarisDesktop

private actor RecoveryModelTransport: WorkspaceRecoveryTransport {
  var startCount = 0
  var calls: [(String, SourceRecoveryClient.Target?)] = []
  var replies: [SourceRecoveryClient.Status] = []
  var exit: ServiceClient.Exit?
  var unknown: String?
  var failNext = false
  var suspended = false
  var waiter: CheckedContinuation<Void, Never>?
  func start() throws -> ServiceHello { startCount += 1; throw ServiceError.io }
  func send(requestID: String, request: ServiceRequest) throws -> ServiceReply { throw ServiceError.io }
  func send(requestID: String, request: ServiceReadRequest) throws -> ServiceReply { throw ServiceError.io }
  func takeEvents() throws -> [ServiceValue] { throw ServiceError.io }
  func close() -> ServiceClient.Exit? { exit }
  func hasUnreapedChild() async -> Bool { exit == nil }
  func recoveryUnknownRequestID() -> String? { unknown }
  func setUnknown(_ id: String) { unknown = id }
  func finish() { exit = .init(status: 0, forced: false) }
  func enqueue(_ status: SourceRecoveryClient.Status) { replies.append(status) }
  func fail() { failNext = true }
  func suspend() { suspended = true }
  func waiting() -> Bool { waiter != nil }
  func release() { suspended = false; waiter?.resume(); waiter = nil }
  func recoveryHello(requestID: String) async throws -> SourceRecoveryClient.Status { try await reply(requestID, nil) }
  func retryResultSave(requestID: String, target: SourceRecoveryClient.Target) async throws -> SourceRecoveryClient.Status { try await reply(requestID, target) }
  private func reply(_ id: String, _ target: SourceRecoveryClient.Target?) async throws -> SourceRecoveryClient.Status {
    calls.append((id, target))
    if suspended { await withCheckedContinuation { waiter = $0 } }
    if failNext { failNext = false; throw SourceRecoveryClient.Failure.responseTimeout }
    let s = replies.removeFirst()
    return .init(requestID: id, engineEpoch: s.engineEpoch, projectID: s.projectID, sessionID: s.sessionID,
                 state: s.state, result: s.result, retryTarget: s.retryTarget, error: s.error)
  }
}

@MainActor
final class WorkspaceRecoveryTests: XCTestCase {
  private func model(_ t: RecoveryModelTransport, enabled: Bool = true) throws -> (WorkspaceServiceModel, URL) {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-workspace-recovery-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    let store = try ServiceClient.PersistentStore(root: root, projectID: "project", sessionID: "session")
    return (try WorkspaceServiceModel(helperURL: root.appendingPathComponent("not-launched"), store: store,
        clientID: "client", makeTransport: { t }, recoveryMode: enabled ? .resultOnly : .disabled), root)
  }
  private func status(_ state: SourceRecoveryClient.State, operation: String? = nil,
                      next: String? = nil, error: SourceRecoveryClient.RemoteError? = nil,
                      epoch: String = "epoch") -> SourceRecoveryClient.Status {
    .init(requestID: "replaced-by-transport", engineEpoch: epoch, projectID: "project", sessionID: "session", state: state,
      result: operation.map { .init(operationID: $0, resultID: "result-" + $0, savedRevision: 7) },
      retryTarget: next.map { .init(runID: "run", attemptID: "attempt", approvalID: "approval", operationID: $0, payloadHash: String(repeating: "a", count: 64)) }, error: error)
  }

  func testKnownStorageFailureAndUnknownRetryKeepExactTargetAndLateDraft() async throws {
    let t = RecoveryModelTransport(); let (m, root) = try model(t)
    defer { try? FileManager.default.removeItem(at: root) }
    await m.start(); m.editDraft("保持する本文")
    await t.enqueue(status(.reportPending, next: "op")); await m.checkRecoveryStatus()
    await t.enqueue(status(.reportPending, next: "op", error: .storageFailed)); await m.retryRecoveryResult()
    XCTAssertFalse(m.recoveryOutcomeUnknown)
    XCTAssertTrue(m.recoveryFailure)
    let original = m.recoveryRequestID
    await t.fail(); await m.reconcileRecoveryRequest()
    XCTAssertTrue(m.recoveryOutcomeUnknown)
    m.editDraft("遅い編集も保持")
    await t.enqueue(status(.readyToExit, operation: "op")); await m.reconcileRecoveryRequest()
    XCTAssertFalse(m.recoveryOutcomeUnknown)
    XCTAssertEqual(m.recoveryRequestID, original)
    let calls = await t.calls
    XCTAssertEqual(calls.count, 4)
    XCTAssertEqual(Data(calls[1].0.utf8), Data(calls[2].0.utf8))
    XCTAssertEqual(Data(calls[1].0.utf8), Data(calls[3].0.utf8))
    XCTAssertEqual(calls[1].1, calls[3].1)
    XCTAssertEqual(m.draftText, "遅い編集も保持")
    XCTAssertTrue(m.isDirty)
    let beforeExit = await m.saveAndClose(); XCTAssertFalse(beforeExit)
    await t.finish()
    let afterExit = await m.saveAndClose(); XCTAssertFalse(afterExit)
    XCTAssertEqual(m.phase, .unavailable)
  }

  func testInitialUnknownHelloAdoptedWithoutNewIDAndDefaultOff() async throws {
    let t = RecoveryModelTransport(); await t.setUnknown("original-start-hello")
    let (m, root) = try model(t); defer { try? FileManager.default.removeItem(at: root) }
    await m.start()
    XCTAssertEqual(m.recoveryRequestID, "original-start-hello")
    XCTAssertTrue(m.recoveryOutcomeUnknown)
    await m.checkRecoveryStatus()
    let noNewCall = await t.calls.count; XCTAssertEqual(noNewCall, 0)
    await t.enqueue(status(.working)); await m.reconcileRecoveryRequest()
    let calls = await t.calls; XCTAssertEqual(calls.map(\.0), ["original-start-hello"])
    let other = RecoveryModelTransport(); let (disabled, root2) = try model(other, enabled: false)
    defer { try? FileManager.default.removeItem(at: root2) }
    await disabled.start(); await disabled.checkRecoveryStatus()
    XCTAssertFalse(disabled.recoveryAvailable)
    let count = await other.calls.count; XCTAssertEqual(count, 0)
  }

  func testExitBeforeProofCannotQuitOrReplaceChild() async throws {
    let t = RecoveryModelTransport(); let (m, root) = try model(t)
    defer { try? FileManager.default.removeItem(at: root) }
    await m.start()
    await t.enqueue(status(.reportPending, next: "op")); await m.checkRecoveryStatus()
    await m.reconnect()
    var count = await t.startCount; XCTAssertEqual(count, 1)
    await t.enqueue(status(.readyToExit)); await m.checkRecoveryStatus()
    await t.finish(); await m.observeRecoveryExit()
    let closed = await m.saveAndClose(); XCTAssertFalse(closed)
    await m.reconnect(); count = await t.startCount; XCTAssertEqual(count, 1)
    XCTAssertEqual(m.phase, .unavailable)
    XCTAssertTrue(m.savedRecoveryResults.isEmpty)
  }

  func testAllObservedProofsAndActualExitRequiredForCleanQuit() async throws {
    let t = RecoveryModelTransport(); let (m, root) = try model(t)
    defer { try? FileManager.default.removeItem(at: root) }
    await m.start()
    await t.enqueue(status(.reportPending, next: "first")); await m.checkRecoveryStatus()
    await t.enqueue(status(.reportPending, operation: "first", next: "second")); await m.retryRecoveryResult()
    XCTAssertEqual(m.savedRecoveryResults.count, 1)
    await t.enqueue(status(.readyToExit, operation: "second")); await m.retryRecoveryResult()
    let notExited = await m.saveAndClose(); XCTAssertFalse(notExited)
    await t.finish()
    let exited = await m.saveAndClose(); XCTAssertTrue(exited)
    XCTAssertEqual(m.phase, .closed)
    XCTAssertEqual(m.savedRecoveryResults.count, 2)
  }

  func testErrorReadyAndEpochMismatchNeverGrantQuit() async throws {
    let t = RecoveryModelTransport(); let (m, root) = try model(t)
    defer { try? FileManager.default.removeItem(at: root) }
    await m.start(); await t.enqueue(status(.working)); await m.checkRecoveryStatus()
    await t.enqueue(status(.readyToExit, epoch: "wrong")); await m.checkRecoveryStatus()
    XCTAssertTrue(m.recoveryOutcomeUnknown)
    XCTAssertEqual(m.recoveryStatus?.state, .working)
    await t.enqueue(status(.readyToExit, error: .storageFailed)); await m.reconcileRecoveryRequest()
    XCTAssertFalse(m.recoveryOutcomeUnknown)
    await t.finish()
    let closed = await m.saveAndClose(); XCTAssertFalse(closed)
  }

  func testLateRecoveryResponseAfterCloseCannotChangeStatusOrDraft() async throws {
    let t = RecoveryModelTransport(); let (m, root) = try model(t)
    defer { try? FileManager.default.removeItem(at: root) }
    await m.start(); m.editDraft("未保存")
    await t.suspend(); await t.enqueue(status(.readyToExit))
    let request = Task { await m.checkRecoveryStatus() }
    for _ in 0..<100 {
      if await t.waiting() { break }
      await Task.yield()
    }
    _ = await m.close()
    await t.release(); await request.value
    XCTAssertNil(m.recoveryStatus)
    XCTAssertEqual(m.draftText, "未保存")
    XCTAssertTrue(m.isDirty)
  }
  func testOwnerRecoveryRetainsBindingModelAndDraftAcrossConcurrentQuit() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-recovery-owner-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: root) }
    let source = root.appendingPathComponent("source")
    try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
    let settings = try SettingsStore(url: root.appendingPathComponent("metadata/settings.json"))
    var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
    let t = RecoveryModelTransport()
    var factories = 0
    let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("not-launched") }, factory: { helper, coordinates, clientID in
      factories += 1
      return try WorkspaceServiceModel(helperURL: helper, store: coordinates, clientID: clientID,
        makeTransport: { t }, recoveryMode: .resultOnly)
    })
    let prepared = try await owner.prepare(document: document, store: settings)
    let binding = try XCTUnwrap(prepared.workspace)
    let m = try XCTUnwrap(owner.service)
    m.editDraft("回復中も保持")
    await t.enqueue(.init(requestID: "unused", engineEpoch: "epoch", projectID: binding.projectID,
      sessionID: binding.sessionID, state: .reportPending, result: nil,
      retryTarget: .init(runID: "run", attemptID: "attempt", approvalID: "approval", operationID: "op", payloadHash: String(repeating: "a", count: 64)), error: nil))
    await t.suspend()
    let check = Task { await owner.checkRecoveryStatus() }
    for _ in 0..<100 { if await t.waiting() { break }; await Task.yield() }
    XCTAssertTrue(owner.isRecovering)
    let quit = await owner.quit(); XCTAssertFalse(quit)
    await owner.reconnect(store: settings)
    await t.release(); await check.value
    await owner.reconnect(store: settings)
    XCTAssertEqual(factories, 1)
    XCTAssertTrue(owner.service === m)
    XCTAssertEqual(owner.binding, binding)
    XCTAssertEqual(m.draftText, "回復中も保持")
    XCTAssertTrue(m.isDirty)
    XCTAssertTrue(m.recoveryAwaitingProof)
  }

}
