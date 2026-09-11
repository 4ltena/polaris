import Foundation
import XCTest
import PolarisSettings
@testable import PolarisDesktop

private actor ConfigurationTransport: WorkspaceServiceTransport {
  let settings: SettingsStore
  var snapshot: [String: ServiceValue]
  var configureIDs: [String] = []
  var statusIDs: [String] = []
  var persistedBeforeSend = false
  var loseACK = false
  var rejection: String?
  var witnessHeld = false
  var witnessWaiter: CheckedContinuation<Void, Never>?
  var closed = false
  var held = false
  var waiter: CheckedContinuation<Void, Never>?
  init(settings: SettingsStore, session: String) throws {
    self.settings = settings
    let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    guard case .object(var fields) = try ServiceCodec.json(Data(contentsOf: root.appendingPathComponent("crates/polaris-desktop-protocol/tests/fixtures/snapshot.json"))) else { throw ServiceError.schema }
    fields["session_id"] = .string(session); fields["session_revision"] = .string("10")
    fields["history_start_cursor"] = .string("first")
    fields["position"] = .object(["engine_epoch": .string("e"), "subscription_id": .string("sub"), "event_seq": .string("0")])
    fields["configuration"] = .object(["configuration_revision": .string("3"), "provider": .string("codex"), "model": .string("old"), "effort": .string("medium")])
    fields["draft"] = .object(["draft_revision": .string("4"), "text": .string(""), "attachment_ids": .array([])])
    fields["runs"] = .array([]); fields["children"] = .array([]); fields["tasks"] = .array([]); fields["unresolved_approvals"] = .array([])
    snapshot = fields
  }
  func start() throws -> ServiceHello {
    closed = false
    return try ServiceHello(.object(["protocol_version": .number("1"), "engine_epoch": .string("e"),
      "capabilities": .array(["session_read", "history_read", "session_configure", "request_status", "shutdown"].map(ServiceValue.string)),
      "limits": .object(["frame_bytes": .string("1048576"), "subscription_events": .string("256"), "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"), "text_batch_ms": .string("50")])]))
  }
  func setLostACK() { loseACK = true }
  func reject(_ code: String) { rejection = code }
  func suspend() { held = true }
  func waiting() -> Bool { waiter != nil }
  func release() { held = false; waiter?.resume(); waiter = nil }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    let payload: ServiceValue
    switch request {
    case .snapshot, .subscribe: payload = .object(snapshot)
    case .configure(_, let revision, let selection, let historyMode):
      configureIDs.append(requestID)
      persistedBeforeSend = try settings.load().pendingConfiguration?.requestID == requestID
      if held { await withCheckedContinuation { waiter = $0 } }
      if let rejection { return .init(requestID: requestID, method: nil, payload: nil, rejection: .object(["code": .string(rejection), "message": .string("合成拒否")])) }
      let nextRevision = (snapshot["session_revision"]?.decimal ?? 0) + 1
      snapshot["session_revision"] = .string(String(nextRevision))
      var configuration: [String: ServiceValue] = ["configuration_revision": .string(String(revision + 1)), "provider": .string(selection.provider.rawValue), "model": .string(selection.model), "effort": .string(selection.storedEffort)]
      if historyMode != .legacy { configuration["history_mode"] = .string(historyMode.rawValue) }
      snapshot["configuration"] = .object(configuration)
      if loseACK { loseACK = false; throw ServiceError.timeout }
      payload = .object(["session_revision": .string(String(nextRevision)), "configuration": snapshot["configuration"]!])
    case .shutdown: payload = .object(["engine_epoch": .string("e"), "state": .string("ready")])
    default: throw ServiceError.notReady
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func send(requestID: String, request: ServiceReadRequest) throws -> ServiceReply {
    let payload: ServiceValue
    switch request {
    case .history: payload = .object(["snapshot_id": snapshot["snapshot_id"]!, "session_revision": snapshot["session_revision"]!, "messages": .array([])])
    case .status(_, _, let id):
      statusIDs.append(id)
      payload = .object(["status": .string("completed"), "session_revision": .string("11"), "result_id": .string("saved-config")])
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func takeEvents() -> [ServiceValue] { [] }
  func close() -> ServiceClient.Exit? { closed = true; return .init(status: 0, forced: false) }
  func holdWitness() { witnessHeld = true }
  func witnessWaiting() -> Bool { witnessWaiter != nil }
  func releaseWitness() { witnessHeld = false; witnessWaiter?.resume(); witnessWaiter = nil }
  func hasUnreapedChild() async -> Bool {
    if witnessHeld { await withCheckedContinuation { witnessWaiter = $0 } }
    return !closed
  }
}

private actor FailedOwnerTransport: WorkspaceServiceTransport {
  func start() async throws -> ServiceHello { throw ServiceError.notReady }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply { throw ServiceError.notReady }
  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply { throw ServiceError.notReady }
  func takeEvents() async throws -> [ServiceValue] { [] }
  func close() async -> ServiceClient.Exit? { .init(status: 0, forced: false) }
  func hasUnreapedChild() async -> Bool { false }
}

private actor OwnedClientTransport: WorkspaceServiceTransport {
  let client: ServiceClient
  init(_ client: ServiceClient) { self.client = client }
  func start() async throws -> ServiceHello { try await client.start() }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply { try await client.send(requestID: requestID, request: request) }
  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply { try await client.send(requestID: requestID, request: request) }
  func takeEvents() async throws -> [ServiceValue] { try await client.takeEvents() }
  func close() async -> ServiceClient.Exit? { await client.close() }
  func hasUnreapedChild() async -> Bool { await client.hasUnreapedChild() }
  func ownedOrderlyExitStatus() async -> Int32? { await client.ownedOrderlyExitStatus() }
}

@MainActor
final class ConfigurationSliceTests: XCTestCase {
  private let cloud = try! ExecutionBinding(provider: .codex, model: "gpt-6-astra", effort: "medium")
  private func fixture() throws -> (WorkspaceServiceModel, ConfigurationTransport, SettingsStore, URL) {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-config-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    let source = root.appendingPathComponent("source")
    try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
    let settings = try SettingsStore(url: root.appendingPathComponent("metadata/settings.json"))
    var document = SettingsDocument(); document.draft.selectFolder(source); document.draft.preferences.executionBinding = cloud; document.finish()
    let (saved, storeRoot) = try settings.prepareWorkspace(document)
    let binding = try XCTUnwrap(saved.workspace)
    let t = try ConfigurationTransport(settings: settings, session: binding.sessionID)
    let store = try ServiceClient.PersistentStore(root: XCTUnwrap(storeRoot), projectID: binding.projectID, sessionID: binding.sessionID)
    let m = try WorkspaceServiceModel(helperURL: root.appendingPathComponent("not-launched"), store: store, clientID: binding.clientID, makeTransport: { t }, configurationStore: settings)
    return (m,t,settings,root)
  }
  func testMigrationAndLocalBindingValidationWithoutInventedModel() throws {
    let data = try JSONEncoder().encode(SettingsDocument())
    let restored = try JSONDecoder().decode(SettingsDocument.self, from: data)
    XCTAssertNil(restored.draft.preferences.executionBinding)
    XCTAssertNil(restored.pendingConfiguration); XCTAssertEqual(restored.draft.preferences.historyMode, .legacy)
    XCTAssertFalse(String(decoding: data, as: UTF8.self).contains("historyMode"))
    XCTAssertEqual(cloud.model, "gpt-6-astra"); XCTAssertEqual(cloud.storedEffort, "medium")
    for endpoint in ["http://example.com/", "http://localhost:11434/", "http://127.0.0.1/a", "http://user@127.0.0.1/", "https://127.0.0.1/?key=x"] {
      XCTAssertThrowsError(try ExecutionBinding(provider: .ollama, model: "observed-model", effort: nil, localEndpoint: endpoint))
    }
    XCTAssertThrowsError(try ExecutionBinding(provider: .ollama, model: "", effort: nil, localEndpoint: "http://127.0.0.1:11434/"))
    XCTAssertThrowsError(try ExecutionBinding(provider: .lmstudio, model: "observed-model", effort: "medium", localEndpoint: "http://127.0.0.1:1234/"))
    let local = try ExecutionBinding(provider: .ollama, model: "observed-model", effort: nil, localEndpoint: "http://127.0.0.1:11434/")
    XCTAssertNil(local.effort); XCTAssertEqual(local.storedEffort, "medium")
    XCTAssertNotEqual(try ExecutionBinding(provider: .codex, model: "é", effort: "medium"), try ExecutionBinding(provider: .codex, model: "e\u{301}", effort: "medium"))
  }
  func testTypedConfigureWireAndBeforeSendPersistence() async throws {
    let (m,t,settings,root) = try fixture(); defer { try? FileManager.default.removeItem(at: root) }
    let wire = try ServiceRequest.configure(session: m.store.sessionID, revision: 3, selection: cloud, historyMode: .legacy).wire(clientID: m.clientID, requestID: "request")
    XCTAssertEqual(wire["method"]?.string, "session.configure")
    XCTAssertEqual(wire["params"]?["expected_configuration_revision"]?.string, "3")
    XCTAssertNil(wire["params"]?["history_mode"])
    await m.start(); await m.configureSelection()
    let persisted = await t.persistedBeforeSend; XCTAssertTrue(persisted)
    XCTAssertEqual(m.savedConfigurationSelection, cloud)
    XCTAssertTrue(m.configurationSelectionCurrent)
    XCTAssertFalse(m.configurationOutcomeUnknown)
    XCTAssertNil(try settings.load().pendingConfiguration)
    _ = await m.close()
  }
  func testStrict10ConfigurationPersistsAcrossIntentAndWire() async throws {
    let (m,_,settings,root) = try fixture(); defer { try? FileManager.default.removeItem(at: root) }
    var document = try settings.load(); document.preferences?.historyMode = .strict10; try settings.save(document)
    let wire = try ServiceRequest.configure(session: m.store.sessionID, revision: 3, selection: cloud, historyMode: .strict10)
      .wire(clientID: m.clientID, requestID: "strict")
    XCTAssertEqual(wire["params"]?["history_mode"], .string("strict10"))
    await m.start(); await m.configureSelection()
    XCTAssertEqual(try settings.load().preferences?.historyMode, .strict10)
    XCTAssertEqual(m.snapshot?["configuration"]?["history_mode"], .string("strict10"))
    XCTAssertNil(try settings.load().pendingConfiguration)
    _ = await m.close()
  }
  func testMemoryStatusRejectsBadBoundsAndAcceptsWireShape() throws {
    let usage: ServiceValue = .object(["input_tokens": .string("1"), "output_tokens": .string("2"), "cached_tokens": .string("0"), "reported_responses": .string("1"), "missing_responses": .string("0"), "failed_requests": .string("0")])
    let mainUsage: ServiceValue = .object(["input_tokens": .string("11"), "output_tokens": .string("12"), "cached_tokens": .string("3"), "reported_responses": .string("4"), "missing_responses": .string("5"), "failed_requests": .string("6")])
    let totalUsage: ServiceValue = .object(["input_tokens": .string("21"), "output_tokens": .string("22"), "cached_tokens": .string("3"), "reported_responses": .string("5"), "missing_responses": .string("5"), "failed_requests": .string("6")])
    let embedding: ServiceValue = .object(["requests": .string("1"), "completed": .string("1"), "failed": .string("0"), "unknown": .string("0"), "input_tokens": .string("9")])
    func memory(_ sources: [ServiceValue], _ reference: String) -> ServiceValue { .object([
      "run_id": .string("run"), "phase": .string("ready"), "detail": .string("prepared"), "recent_raw_turns": .string("10"),
      "retrieval_sources": .array(sources), "reference_tokens": .string(reference), "summary_usage": usage,
      "main_usage": mainUsage, "embedding_usage": embedding, "total_usage": totalUsage,
    ]) }
    let parsed = try WorkspaceMemoryStatus(memory([.string("conversation://project/session/source")], "768"))
    XCTAssertEqual(parsed.retrievalSources.count, 1); XCTAssertEqual(parsed.summaryUsage?.inputTokens, 1)
    XCTAssertEqual(parsed.mainUsage?.inputTokens, 11); XCTAssertEqual(parsed.totalUsage?.inputTokens, 21)
    XCTAssertEqual(parsed.embeddingUsage?.inputTokens, 9)
    guard case .object(var fields) = memory([], "0") else { return XCTFail("memory must be an object") }
    fields.removeValue(forKey: "main_usage")
    XCTAssertNil(try WorkspaceMemoryStatus(.object(fields)).mainUsage)
    XCTAssertThrowsError(try WorkspaceMemoryStatus(memory([.string("conversation://a"), .string("conversation://b"), .string("conversation://c"), .string("conversation://d")], "1")))
    XCTAssertThrowsError(try WorkspaceMemoryStatus(memory([.string("file:///not-a-reference")], "769")))
    let event: ServiceValue = .object(["protocol_version": .number("1"), "kind": .string("event"), "engine_epoch": .string("e"), "subscription_id": .string("sub"), "event_seq": .string("1"), "session_id": .string("s"), "session_revision": .string("2"), "type": .string("memory.updated"), "payload": memory([], "0")])
    XCTAssertNoThrow(try ServiceSchema.validateEvent(event, epoch: "e"))
  }
  func testLostACKRestartUsesOriginalLedgerIDWithoutConfigureResend() async throws {
    let (m,t,settings,root) = try fixture(); defer { try? FileManager.default.removeItem(at: root) }
    await m.start(); await t.setLostACK(); await m.configureSelection()
    let original = try XCTUnwrap(try settings.load().pendingConfiguration)
    XCTAssertTrue(m.configurationOutcomeUnknown)
    let quit = await m.saveAndClose(); XCTAssertFalse(quit)
    _ = await m.close()
    let next = try WorkspaceServiceModel(helperURL: root.appendingPathComponent("not-launched"), store: m.store,
      clientID: m.clientID, makeTransport: { t }, configurationStore: settings)
    await next.start(); await next.configureSelection(); await next.reconcileConfiguration()
    let sent = await t.configureIDs; XCTAssertEqual(sent, [original.requestID])
    let queried = await t.statusIDs; XCTAssertEqual(queried, [original.requestID])
    XCTAssertFalse(next.configurationOutcomeUnknown)
    XCTAssertEqual(next.savedConfigurationSelection, cloud)
    XCTAssertNil(try settings.load().pendingConfiguration)
    _ = await next.close()
  }
  func testLateSelectionAndDraftRemainAfterSuccessfulCAS() async throws {
    let (m,t,settings,root) = try fixture(); defer { try? FileManager.default.removeItem(at: root) }
    await m.start(); await t.suspend()
    var stale = try settings.load()
    let request = Task { await m.configureSelection() }
    for _ in 0..<100 { if await t.waiting() { break }; await Task.yield() }
    let id = try XCTUnwrap(try settings.load().pendingConfiguration?.requestID)
    stale.preferences?.executionBinding = try ExecutionBinding(provider: .openai, model: "gpt-6-astra", effort: "medium")
    try settings.save(stale)
    XCTAssertEqual(try settings.load().pendingConfiguration?.requestID, id)
    m.editDraft("遅い下書き")
    await t.release(); await request.value
    XCTAssertEqual(m.savedConfigurationSelection, cloud)
    XCTAssertFalse(m.configurationSelectionCurrent)
    XCTAssertFalse(m.configurationOutcomeUnknown)
    XCTAssertEqual(m.draftText, "遅い下書き"); XCTAssertTrue(m.isDirty)
    XCTAssertEqual(try settings.load().preferences?.executionBinding?.provider, .openai)
    _ = await m.close()
  }
  func testCASRejectionAndStorageFailureDiffer() async throws {
    for code in ["revision_conflict", "storage_failed"] {
      let (m,t,settings,root) = try fixture(); defer { try? FileManager.default.removeItem(at: root) }
      await m.start(); await t.reject(code); await m.configureSelection()
      XCTAssertEqual(m.configurationOutcomeUnknown, code == "storage_failed")
      XCTAssertEqual(try settings.load().pendingConfiguration != nil, code == "storage_failed")
      XCTAssertNil(m.savedConfigurationSelection)
      _ = await m.close()
    }
  }
  func testOwnerHandoffRetainsModelAndSupportsExplicitRepeatPublication() async throws {
    let (model, transport, settings, root) = try fixture()
    defer { try? FileManager.default.removeItem(at: root) }
    var document = try settings.load()
    document.preferences?.permission = .readOnly
    try settings.save(document)
    let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("not-launched") }, factory: { _,_,_ in model })
    _ = try await owner.prepare(document: settings.load(), store: settings)
    await owner.configureAndStart(store: settings, makeConfiguredTransport: { _ in transport }, verifyAssembly: {})
    XCTAssertTrue(owner.service === model)
    XCTAssertEqual(model.phase, .ready)
    XCTAssertNotNil(try settings.load().bootstrapProof)
    XCTAssertNil(owner.error)
    // An explicit repeat must reconcile the exact earlier receipt, not overwrite it.
    await owner.configureAndStart(store: settings, reconcilePublication: true,
                                  makeConfiguredTransport: { _ in transport }, verifyAssembly: {})
    XCTAssertEqual(model.phase, .ready)
    XCTAssertNil(owner.error)
    _ = await model.saveAndClose()
  }

  func testStrict10OwnerStartupFailureRestoresStorageThenAllowsLegacy() async throws {
    let (service, transport, settings, root) = try fixture(); defer { try? FileManager.default.removeItem(at: root) }
    var document = try settings.load()
    document.preferences?.permission = .readOnly; document.preferences?.historyMode = .strict10
    try settings.save(document)
    let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("not-launched") }, factory: { _,_,_ in service })
    _ = try await owner.prepare(document: settings.load(), store: settings)
    await owner.configureAndStart(store: settings, makeConfiguredTransport: { _ in FailedOwnerTransport() }, verifyAssembly: {})
    XCTAssertEqual(service.phase, .unavailable)
    XCTAssertEqual(try settings.load().preferences?.historyMode, .strict10)
    let legacy = try await owner.selectExecution(cloud, historyMode: .legacy, settings: settings)
    XCTAssertEqual(legacy.preferences?.historyMode, .legacy)
    XCTAssertEqual(service.phase, .ready)
    XCTAssertEqual(try settings.load().preferences?.historyMode, .legacy)
    await owner.configureAndStart(store: settings, makeConfiguredTransport: { _ in transport }, verifyAssembly: {})
    XCTAssertEqual(service.phase, .ready, "\(String(describing: service.failure))")
    XCTAssertNil(owner.error)
    _ = await service.close()
  }

  func testStrict10OwnerStartupExitCodesShowTypedReasonAndAllowLegacyRecovery() async throws {
    let cases: [(Int32, OwnerStartupFailure, String)] = [
      (81, .configurationInvalid, "[embedding] 設定"),
      (84, .manifestRevision, "manifest、SHA-256、revision"),
      (88, .offlineReadiness, "オフライン埋め込みパッケージ"),
    ]
    for (status, expected, message) in cases {
      let (service, transport, settings, root) = try fixture()
      defer { try? FileManager.default.removeItem(at: root) }
      let script = root.appendingPathComponent("startup-exit-\(status).sh")
      try Data("#!/bin/sh\nexit \(status)\n".utf8).write(to: script)
      try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
      var document = try settings.load()
      document.preferences?.permission = .readOnly; document.preferences?.historyMode = .strict10
      try settings.save(document)
      let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("not-launched") }, factory: { _,_,_ in service })
      _ = try await owner.prepare(document: settings.load(), store: settings)
      let clientID = service.clientID
      await owner.configureAndStart(store: settings, makeConfiguredTransport: { launch in
        var configuration = ServiceClient.Configuration()
        configuration.helloTimeout = .milliseconds(300)
        let transport = OwnedClientTransport(try ServiceClient(helperURL: script, clientID: clientID, configuration: configuration,
                                                               persistentStore: launch.store, recoveryMode: .resultOnly))
        return transport
      }, verifyAssembly: {})
      XCTAssertEqual(service.ownerStartupFailure, expected)
      XCTAssertTrue(owner.error?.contains(message) == true)
      XCTAssertTrue(service.canRestoreStorageOnly)
      _ = try await owner.selectExecution(cloud, historyMode: .legacy, settings: settings)
      XCTAssertEqual(service.phase, .ready)
      await owner.configureAndStart(store: settings, makeConfiguredTransport: { _ in transport }, verifyAssembly: {})
      XCTAssertEqual(service.phase, .ready)
      _ = await service.close()
    }
  }

  func testUnknownOrSignalledOwnerStartupDoesNotCreateTypedReasonOrAllowRecovery() async throws {
    for (name, body) in [("unknown", "exit 123"), ("signal", "kill -TERM $$")] {
      let (service, _, settings, root) = try fixture()
      defer { try? FileManager.default.removeItem(at: root) }
      let script = root.appendingPathComponent("startup-\(name).sh")
      try Data("#!/bin/sh\n\(body)\n".utf8).write(to: script)
      try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
      var document = try settings.load()
      document.preferences?.permission = .readOnly; document.preferences?.historyMode = .strict10
      try settings.save(document)
      let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("not-launched") }, factory: { _,_,_ in service })
      _ = try await owner.prepare(document: settings.load(), store: settings)
      let clientID = service.clientID
      await owner.configureAndStart(store: settings, makeConfiguredTransport: { launch in
        var configuration = ServiceClient.Configuration()
        configuration.helloTimeout = .milliseconds(300)
        return OwnedClientTransport(try ServiceClient(helperURL: script, clientID: clientID, configuration: configuration,
                                                      persistentStore: launch.store, recoveryMode: .resultOnly))
      }, verifyAssembly: {})
      XCTAssertNil(service.ownerStartupFailure)
      XCTAssertEqual(owner.error, "設定は保存済みです。実行サービスの接続状態を確認してください。")
      do {
        _ = try await owner.selectExecution(cloud, historyMode: .legacy, settings: settings)
        XCTFail("unknown or signalled owner exit must not permit replacement")
      } catch {
        XCTAssertEqual(error as? ServiceError, .notReady)
      }
    }
  }

  func testFinalWitnessAwaitRevalidatesLateDraftAndOwnerSelection() async throws {
    for editDraft in [true, false] {
      let (model, transport, settings, root) = try fixture()
      defer { try? FileManager.default.removeItem(at: root) }
      var document = try settings.load(); document.preferences?.permission = .readOnly; try settings.save(document)
      let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("not-launched") }, factory: { _,_,_ in model })
      _ = try await owner.prepare(document: settings.load(), store: settings)
      await owner.configureAndStart(store: settings, makeConfiguredTransport: { _ in transport }, verifyAssembly: {})
      XCTAssertEqual(model.phase, .ready)
      let closed = await model.saveAndClose(); XCTAssertTrue(closed)
      let generation = settings.selectionGeneration
      await transport.holdWitness()
      let task = Task { @MainActor in
        do {
          try await model.startConfigured(revalidate: {
            guard settings.selectionGeneration == generation else { throw ServiceError.notReady }
          }, makeTransport: { transport })
          return false
        } catch { return true }
      }
      for _ in 0..<1000 {
        if await transport.witnessWaiting() { break }
        await Task.yield()
      }
      let waiting = await transport.witnessWaiting(); XCTAssertTrue(waiting)
      if editDraft { model.editDraft("late synthetic draft") }
      else {
        var next = try settings.load(); next.preferences?.theme = .dark; try settings.save(next)
      }
      await transport.releaseWitness()
      let rejected = await task.value; XCTAssertTrue(rejected)
      XCTAssertEqual(model.phase, .closed)
      if editDraft { XCTAssertEqual(model.draftText, "late synthetic draft"); XCTAssertTrue(model.isDirty) }
    }
  }

}
