import Foundation
import XCTest
import PolarisSettings
@testable import PolarisDesktop

private actor RoleReconciliationTransport: WorkspaceServiceTransport {
  private var snapshot: [String: ServiceValue]
  private var failNextSubscribe = false
  private(set) var configureRequestIDs: [String] = []

  init(session: String, configurationRevision: UInt64, bindings: [LocalRoleBinding]) throws {
    let root = URL(fileURLWithPath: #filePath)
      .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
      .deletingLastPathComponent().deletingLastPathComponent()
    guard case .object(var fields) = try ServiceCodec.json(Data(contentsOf:
      root.appendingPathComponent("crates/polaris-desktop-protocol/tests/fixtures/snapshot.json")))
    else { throw ServiceError.schema }
    fields["session_id"] = .string(session)
    fields["session_revision"] = .string("10")
    fields["history_start_cursor"] = .string("first")
    fields["position"] = .object([
      "engine_epoch": .string("e"), "subscription_id": .string("sub"), "event_seq": .string("0"),
    ])
    fields["configuration"] = .object([
      "configuration_revision": .string(String(configurationRevision)),
      "provider": .string("codex"), "model": .string("gpt-6-astra"), "effort": .string("medium"),
    ])
    fields["draft"] = .object([
      "draft_revision": .string("4"), "text": .string(""), "attachment_ids": .array([]),
    ])
    fields["role_bindings"] = .array(bindings.map(Self.serviceBinding))
    fields["role_catalog"] = .array([])
    fields["runs"] = .array([])
    fields["children"] = .array([])
    fields["tasks"] = .array([])
    fields["unresolved_approvals"] = .array([])
    snapshot = fields
  }

  private static func serviceBinding(_ binding: LocalRoleBinding) -> ServiceValue {
    .object([
      "role": .string(binding.role),
      "provider": .string(binding.selection.provider.rawValue),
      "endpoint": .string(binding.selection.localEndpoint!),
      "model": .string(binding.selection.model),
      "observed_tool_support": .string(binding.observedToolSupport.rawValue),
    ])
  }

  func start() throws -> ServiceHello {
    try ServiceHello(.object([
      "protocol_version": .number("1"), "engine_epoch": .string("e"),
      "capabilities": .array([
        "session_read", "history_read", "role_configure", "request_status", "shutdown",
      ].map(ServiceValue.string)),
      "limits": .object([
        "frame_bytes": .string("1048576"), "subscription_events": .string("256"),
        "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"),
        "text_batch_ms": .string("50"),
      ]),
    ]))
  }

  func failNextRefresh() { failNextSubscribe = true }

  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    let payload: ServiceValue
    switch request {
    case .snapshot:
      payload = .object(snapshot)
    case .subscribe:
      if failNextSubscribe {
        failNextSubscribe = false
        throw ServiceError.timeout
      }
      payload = .object(snapshot)
    case .configureRoles:
      configureRequestIDs.append(requestID)
      throw ServiceError.notReady
    case .shutdown:
      payload = .object(["engine_epoch": .string("e"), "state": .string("ready")])
    default:
      throw ServiceError.notReady
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }

  func send(requestID: String, request: ServiceReadRequest) throws -> ServiceReply {
    guard case .history = request else { throw ServiceError.notReady }
    return .init(requestID: requestID, method: request.method, payload: .object([
      "snapshot_id": snapshot["snapshot_id"]!, "session_revision": snapshot["session_revision"]!,
      "messages": .array([]),
    ]), rejection: nil)
  }

  func takeEvents() -> [ServiceValue] { [] }
  func close() -> ServiceClient.Exit? { .init(status: 0, forced: false) }
  func hasUnreapedChild() -> Bool { false }
}

@MainActor
final class RoleConfigurationReconciliationTests: XCTestCase {
  private let requested = try! LocalRoleBinding(
    role: "implementation", selection: ExecutionBinding(
      provider: .ollama, model: "requested", effort: nil,
      localEndpoint: "http://127.0.0.1:11434/"), observedToolSupport: .supported)
  private let observed = try! LocalRoleBinding(
    role: "implementation", selection: ExecutionBinding(
      provider: .ollama, model: "observed", effort: nil,
      localEndpoint: "http://127.0.0.1:11434/"), observedToolSupport: .supported)

  private func fixture(configurationRevision: UInt64, observedBindings: [LocalRoleBinding],
                       pending: Bool = true) throws
    -> (WorkspaceServiceModel, RoleReconciliationTransport, SettingsStore, URL)
  {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-role-reconcile-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    let source = root.appendingPathComponent("source")
    try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
    let settings = try SettingsStore(url: root.appendingPathComponent("metadata/settings.json"))
    var document = SettingsDocument()
    document.draft.selectFolder(source)
    document.draft.preferences.executionBinding = try ExecutionBinding(
      provider: .codex, model: "gpt-6-astra", effort: "medium")
    document.finish()
    let (saved, storeRoot) = try settings.prepareWorkspace(document)
    let workspace = try XCTUnwrap(saved.workspace)
    if pending {
      try settings.recordRoleConfigurationIntent(RoleConfigurationIntent(
        requestID: "unknown-role-save", clientID: workspace.clientID,
        projectID: workspace.projectID, sessionID: workspace.sessionID,
        expectedRevision: 3, bindings: [requested]))
    }
    let transport = try RoleReconciliationTransport(
      session: workspace.sessionID, configurationRevision: configurationRevision,
      bindings: observedBindings)
    let store = try ServiceClient.PersistentStore(
      root: XCTUnwrap(storeRoot), projectID: workspace.projectID, sessionID: workspace.sessionID)
    let model = try WorkspaceServiceModel(
      helperURL: root.appendingPathComponent("not-launched"), store: store,
      clientID: workspace.clientID, makeTransport: { transport }, configurationStore: settings)
    return (model, transport, settings, root)
  }

  func testRestartReconcilesConfirmedUnknownSaveWithoutReplay() async throws {
    let (model, transport, settings, root) = try fixture(
      configurationRevision: 4, observedBindings: [requested])
    defer { try? FileManager.default.removeItem(at: root) }
    await model.start()
    await model.reconcileRoleConfiguration()
    XCTAssertNil(model.pendingRoleConfiguration)
    XCTAssertNil(try settings.load().pendingRoleConfiguration)
    XCTAssertEqual(model.roleConfigurationNotice, "役割設定を保存しました。")
    let requests = await transport.configureRequestIDs
    XCTAssertTrue(requests.isEmpty)
    _ = await model.close()
  }

  func testConflictRequiresExplicitAdoptionAndNeverReplaysUnknownSave() async throws {
    let (model, transport, settings, root) = try fixture(
      configurationRevision: 5, observedBindings: [observed])
    defer { try? FileManager.default.removeItem(at: root) }
    await model.start()
    await model.reconcileRoleConfiguration()
    XCTAssertNotNil(model.pendingRoleConfiguration)
    XCTAssertNotNil(try settings.load().pendingRoleConfiguration)

    await model.adoptObservedRoleConfiguration()
    XCTAssertNil(model.pendingRoleConfiguration)
    XCTAssertNil(try settings.load().pendingRoleConfiguration)
    XCTAssertTrue(model.executionNeedsRefresh)
    XCTAssertEqual(model.roleConfigurationNotice,
      "現在の保存済み役割設定を採用しました。元の保存結果は確認できません。")
    let requests = await transport.configureRequestIDs
    XCTAssertTrue(requests.isEmpty)
    _ = await model.close()
  }

  func testAdoptionRetainsUnknownIntentWhenRefreshFails() async throws {
    let (model, transport, settings, root) = try fixture(
      configurationRevision: 5, observedBindings: [observed])
    defer { try? FileManager.default.removeItem(at: root) }
    await model.start()
    await transport.failNextRefresh()
    await model.adoptObservedRoleConfiguration()
    XCTAssertNotNil(model.pendingRoleConfiguration)
    XCTAssertNotNil(try settings.load().pendingRoleConfiguration)
    let requests = await transport.configureRequestIDs
    XCTAssertTrue(requests.isEmpty)
    _ = await model.close()
  }

  func testStaleEditorRevisionDoesNotSendRoleConfiguration() async throws {
    let (model, transport, _, root) = try fixture(
      configurationRevision: 5, observedBindings: [observed], pending: false)
    defer { try? FileManager.default.removeItem(at: root) }
    await model.start()
    await model.saveRoleBindings([requested], revision: 4)
    XCTAssertEqual(model.roleConfigurationNotice,
      "役割設定が更新されています。最新の設定を確認してから保存してください。")
    let requests = await transport.configureRequestIDs
    XCTAssertTrue(requests.isEmpty)
    _ = await model.close()
  }
}
