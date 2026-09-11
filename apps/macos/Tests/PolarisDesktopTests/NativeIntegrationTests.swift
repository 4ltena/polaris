import AppKit
import Foundation
import XCTest
import PolarisSettings
@testable import PolarisDesktop

@MainActor
final class NativeIntegrationTests: XCTestCase {
    private func location() throws -> URL {
        let base = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-native-owner-\(UUID())")
        try FileManager.default.createDirectory(at: base, withIntermediateDirectories: false)
        return base
    }
    private var helper: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("target/debug/polaris-desktop-service")
    }
    func testBlankWorkspaceDoesNotResolveOrLaunchHelper() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let store = try SettingsStore(url: base.appendingPathComponent("settings.json"))
        var document = SettingsDocument(); document.finish()
        let owner = DesktopWorkspaceOwner(helper: { XCTFail("未選択でhelper解決を呼びました"); throw ServiceError.launch })
        let prepared = try await owner.prepare(document: document, store: store)
        XCTAssertEqual(prepared, document); XCTAssertNil(owner.service); XCTAssertNil(owner.binding)
        let quit = await owner.quit(); XCTAssertTrue(quit)
    }
    func testStableIDsPersistBeforeOneHelperAndQuitSavesBeforeReaping() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let store = try SettingsStore(url: base.appendingPathComponent("metadata/settings.json"))
        var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
        let explicitHelper = helper
        var resolutions = 0
        let owner = DesktopWorkspaceOwner(helper: {
            resolutions += 1
            XCTAssertNotNil(try store.load().workspace)
            return explicitHelper
        })
        let prepared = try await owner.prepare(document: document, store: store)
        let savedBinding = try XCTUnwrap(prepared.workspace)
        _ = try ServiceClient.PersistentStore(root: URL(fileURLWithPath: store.url.path + ".workspace"), projectID: savedBinding.projectID, sessionID: savedBinding.sessionID)
        let service = try XCTUnwrap(owner.service)
        XCTAssertEqual(service.phase, .ready)
        XCTAssertTrue(service.messages.isEmpty); XCTAssertEqual(service.draftText, "")
        _ = try await owner.prepare(document: prepared, store: store)
        XCTAssertTrue(owner.service === service); XCTAssertEqual(resolutions, 1)
        service.editDraft("終了前に保存する実際のテスト入力")
        let quit = await owner.quit(); XCTAssertTrue(quit)
        XCTAssertTrue(service.shutdownReady); XCTAssertEqual(service.lastExit?.status, 0)
        let next = DesktopWorkspaceOwner(helper: { explicitHelper })
        let restored = try await next.prepare(document: store.load(), store: store)
        XCTAssertEqual(restored.workspace, prepared.workspace)
        XCTAssertEqual(next.service?.draftText, "終了前に保存する実際のテスト入力")
        let closed = await next.quit(); XCTAssertTrue(closed)
    }
    func testMainBlankPreparesWorkspaceSizeAndStaleDrawCannotReveal() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let url = base.appendingPathComponent("settings.json")
        var document = SettingsDocument(); document.finish()
        do { let store = try SettingsStore(url: url); try store.save(document) }
        let model = DesktopModel(arguments: ["--settings-path", url.path])
        await model.startOnce()
        XCTAssertNil(model.workspace.service); XCTAssertEqual(model.preparedSize, NSSize(width: 1280, height: 780))
        let current = try XCTUnwrap(model.preparationID)
        model.screenPrepared(UUID(), succeeded: true); XCTAssertTrue(model.isStarting)
        model.screenPrepared(current, succeeded: true); XCTAssertFalse(model.isStarting)
        await model.startOnce(); XCTAssertEqual(model.preparationID, current)
    }
    func testUnavailablePackagedHelperKeepsPersistedBindingWithoutFallback() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let store = try SettingsStore(url: base.appendingPathComponent("metadata/settings.json"))
        var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
        let owner = DesktopWorkspaceOwner(helper: { throw ServiceError.launch })
        let saved = try await owner.prepare(document: document, store: store)
        XCTAssertEqual(try store.load().workspace, saved.workspace)
        XCTAssertNotNil(owner.binding); XCTAssertNotNil(owner.error); XCTAssertNil(owner.service)
        let closed = await owner.quit(); XCTAssertTrue(closed)
    }
}

extension NativeIntegrationTests {
    func testStoreSymlinkAndReplacementBeforeLaunchAreRejected() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let root = base.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let alias = base.appendingPathComponent("alias")
        try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: root)
        XCTAssertThrowsError(try ServiceClient.PersistentStore(root: alias, projectID: "p", sessionID: "s"))
        let pinned = try ServiceClient.PersistentStore(root: root, projectID: "p", sessionID: "s")
        try FileManager.default.moveItem(at: root, to: base.appendingPathComponent("original"))
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let client = try ServiceClient(helperURL: URL(fileURLWithPath: "/missing-explicit-helper"), persistentStore: pinned)
        do { _ = try await client.start(); XCTFail("差替え後のrootで起動しました") }
        catch { XCTAssertEqual(error as? SettingsError, .unavailableLocation) }
    }
    func testWindowCloseRequestsTerminationWithoutHidingConnectedWindow() async throws {
        let phase = StartupWindowSize.PhaseView()
        phase.isStarting = false; phase.isWorkspace = true
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1280, height: 780),
                              styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = phase
        var requested = 0
        phase.requestTermination = { requested += 1 }
        XCTAssertFalse(phase.windowShouldClose(window))
        XCTAssertEqual(requested, 1)
        XCTAssertTrue(window.contentView === phase)
        window.close()
    }
    func testQuitDoesNotDiscardFailedSettingsSave() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let url = base.appendingPathComponent("settings.json")
        let model = DesktopModel(arguments: ["--settings-path", url.path])
        await model.start()
        model.screenPrepared(try XCTUnwrap(model.preparationID), succeeded: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        model.change { $0.preferences.theme = .dark }
        XCTAssertNotNil(model.saveError)
        let blocked = await model.prepareToQuit()
        XCTAssertFalse(blocked); XCTAssertEqual(model.document.draft.preferences.theme, .dark)
        try FileManager.default.removeItem(at: url)
        let retried = await model.prepareToQuit()
        XCTAssertTrue(retried); XCTAssertNil(model.saveError)
    }
}

private actor ShutdownGateTransport: WorkspaceServiceTransport {
    let client: ServiceClient
    var waiting: CheckedContinuation<Void, Never>?
    var held = false
    init(client: ServiceClient) { self.client = client }
    func isWaiting() -> Bool { waiting != nil }
    func release() { waiting?.resume(); waiting = nil }
    func start() async throws -> ServiceHello { try await client.start() }
    func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
        if case .shutdown = request, !held {
            held = true
            await withCheckedContinuation { waiting = $0 }
        }
        return try await client.send(requestID: requestID, request: request)
    }
    func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
        try await client.send(requestID: requestID, request: request)
    }
    func takeEvents() async throws -> [ServiceValue] { try await client.takeEvents() }
    func close() async -> ServiceClient.Exit? { await client.close() }
}

extension NativeIntegrationTests {
    func testSettingsSaveFailureDuringShutdownCancelsTermination() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let url = base.appendingPathComponent("metadata/settings.json")
        var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
        do { let store = try SettingsStore(url: url); try store.save(document) }
        let explicitHelper = helper
        var gate: ShutdownGateTransport?
        let owner = DesktopWorkspaceOwner(helper: { explicitHelper }, factory: { helper, coordinates, clientID in
            let transport = ShutdownGateTransport(client: try ServiceClient(helperURL: helper, clientID: clientID, persistentStore: coordinates))
            gate = transport
            return try WorkspaceServiceModel(helperURL: helper, store: coordinates, clientID: clientID, makeTransport: { transport })
        })
        let model = DesktopModel(arguments: ["--settings-path", url.path], workspace: owner)
        await model.start()
        XCTAssertEqual(owner.service?.phase, .ready)
        let transport = try XCTUnwrap(gate)
        let quit = Task { await model.prepareToQuit() }
        let deadline = ContinuousClock.now + .seconds(3)
        while !(await transport.isWaiting()), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
        let waiting = await transport.isWaiting(); XCTAssertTrue(waiting)
        try FileManager.default.moveItem(at: url, to: url.appendingPathExtension("preserved"))
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        model.change { $0.preferences.theme = .dark }
        XCTAssertNotNil(model.saveError)
        await transport.release()
        let allowed = await quit.value
        XCTAssertFalse(allowed)
        XCTAssertEqual(model.document.draft.preferences.theme, .dark)
        XCTAssertNotNil(model.saveError)
        try FileManager.default.removeItem(at: url)
        model.retrySave()
        XCTAssertNil(model.saveError)
        let saved = try JSONDecoder().decode(SettingsDocument.self, from: Data(contentsOf: url))
        XCTAssertEqual(saved.draft.preferences.theme, .dark)
    }
}

private actor LostDraftACKTransport: WorkspaceServiceTransport {
    let client: ServiceClient
    init(client: ServiceClient) { self.client = client }
    func start() async throws -> ServiceHello { try await client.start() }
    func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
        let reply = try await client.send(requestID: requestID, request: request)
        if case .draft = request { throw ServiceError.io } // Durable write succeeded; its ACK is lost.
        return reply
    }
    func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
        try await client.send(requestID: requestID, request: request)
    }
    func takeEvents() async throws -> [ServiceValue] { try await client.takeEvents() }
    func close() async -> ServiceClient.Exit? { await client.close() }
}

extension NativeIntegrationTests {
    func testOwnerReconnectReconcilesRealDurableWriteAfterLostACK() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let store = try SettingsStore(url: base.appendingPathComponent("metadata/settings.json"))
        var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
        let explicitHelper = helper
        let owner = DesktopWorkspaceOwner(helper: { explicitHelper }, factory: { helper, coordinates, clientID in
            try WorkspaceServiceModel(helperURL: helper, store: coordinates, clientID: clientID, makeTransport: {
                LostDraftACKTransport(client: try ServiceClient(helperURL: helper, clientID: clientID, persistentStore: coordinates))
            })
        })
        let saved = try await owner.prepare(document: document, store: store)
        let model = try XCTUnwrap(owner.service)
        let oldEpoch = model.snapshot?["position"]?["engine_epoch"]
        model.editDraft("実保存後にACKを失うテスト本文")
        await model.saveDraft()
        XCTAssertEqual(model.phase, .unavailable); XCTAssertTrue(model.draftOutcomeUnknown)
        let originalID = model.lastDraftRequestID
        await owner.reconnect(store: store)
        XCTAssertTrue(owner.service === model); XCTAssertEqual(model.phase, .ready, "接続失敗：\(String(describing: model.failure))")
        XCTAssertEqual(model.lastDraftRequestID, originalID)
        XCTAssertFalse(model.draftOutcomeUnknown); XCTAssertFalse(model.isDirty)
        XCTAssertEqual(model.draftText, "実保存後にACKを失うテスト本文")
        XCTAssertNotEqual(oldEpoch, model.snapshot?["position"]?["engine_epoch"])
        XCTAssertEqual(try store.load().workspace, saved.workspace)
        XCTAssertNotNil(model.lastExit)
        let closed = await owner.quit(); XCTAssertTrue(closed)
    }

    func testOwnerReconnectRejectsChangedSourceWithoutReplacingModel() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let store = try SettingsStore(url: base.appendingPathComponent("metadata/settings.json"))
        var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
        let explicitHelper = helper
        let owner = DesktopWorkspaceOwner(helper: { explicitHelper })
        let saved = try await owner.prepare(document: document, store: store)
        let model = try XCTUnwrap(owner.service)
        model.editDraft("復旧前の本文")
        _ = await model.close()
        try FileManager.default.moveItem(at: source, to: source.appendingPathExtension("original"))
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        await owner.reconnect(store: store)
        XCTAssertNotNil(owner.error); XCTAssertTrue(owner.service === model)
        XCTAssertEqual(model.phase, .unavailable); XCTAssertEqual(model.draftText, "復旧前の本文")
        XCTAssertEqual(try store.load().workspace, saved.workspace)
    }
}

private actor StartupGateTransport: WorkspaceServiceTransport {
    let client: ServiceClient
    var waiting: CheckedContinuation<Void, Never>?
    init(client: ServiceClient) { self.client = client }
    func isWaiting() -> Bool { waiting != nil }
    func release() { waiting?.resume(); waiting = nil }
    func start() async throws -> ServiceHello {
        await withCheckedContinuation { waiting = $0 }
        return try await client.start()
    }
    func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
        try await client.send(requestID: requestID, request: request)
    }
    func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
        try await client.send(requestID: requestID, request: request)
    }
    func takeEvents() async throws -> [ServiceValue] { try await client.takeEvents() }
    func close() async -> ServiceClient.Exit? { await client.close() }
}

extension NativeIntegrationTests {
    func testStartupViewTaskCancellationDoesNotCancelOwnedPreparation() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let url = base.appendingPathComponent("metadata/settings.json")
        var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
        do { let store = try SettingsStore(url: url); try store.save(document) }
        let explicitHelper = helper
        var gate: StartupGateTransport?
        var factories = 0
        let owner = DesktopWorkspaceOwner(helper: { explicitHelper }, factory: { helper, coordinates, clientID in
            factories += 1
            let transport = StartupGateTransport(client: try ServiceClient(helperURL: helper, clientID: clientID, persistentStore: coordinates))
            gate = transport
            return try WorkspaceServiceModel(helperURL: helper, store: coordinates, clientID: clientID, makeTransport: { transport })
        })
        let model = DesktopModel(arguments: ["--settings-path", url.path], workspace: owner)
        let viewTask = Task { await model.startOnce() }
        let deadline = ContinuousClock.now + .seconds(3)
        while gate == nil, ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
        let transport = try XCTUnwrap(gate)
        while !(await transport.isWaiting()), ContinuousClock.now < deadline { try await Task.sleep(for: .milliseconds(5)) }
        let waiting = await transport.isWaiting(); XCTAssertTrue(waiting)
        XCTAssertFalse(model.document.isEditing) // Persisted root transition already occurred.
        let duringStartup = await model.prepareToQuit()
        XCTAssertFalse(duringStartup)
        viewTask.cancel() // SwiftUI removes/replaces the view that owns .task.
        await transport.release()
        await viewTask.value
        await model.startOnce() // A replacement view must share the same startup.
        XCTAssertEqual(factories, 1)
        XCTAssertEqual(owner.service?.phase, .ready)
        XCTAssertNil(owner.service?.failure)
        let generation = try XCTUnwrap(model.preparationID)
        model.screenPrepared(UUID(), succeeded: true); XCTAssertTrue(model.isStarting)
        model.screenPrepared(generation, succeeded: true); XCTAssertFalse(model.isStarting)
        let closed = await model.prepareToQuit()
        XCTAssertTrue(closed)
        XCTAssertTrue(owner.service?.shutdownReady == true)
        XCTAssertEqual(owner.service?.lastExit?.status, 0)
    }
}

extension NativeIntegrationTests {
    func testNeverStartedCancelledTransportCanRetryWithoutFabricatedExit() async throws {
        let base = try location(); defer { try? FileManager.default.removeItem(at: base) }
        let root = base.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        let coordinates = try ServiceClient.PersistentStore(root: root, projectID: UUID().uuidString, sessionID: UUID().uuidString)
        let model = try WorkspaceServiceModel(helperURL: helper, store: coordinates, clientID: UUID().uuidString)
        let caller = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            await model.start()
        }
        await caller.value
        XCTAssertEqual(model.phase, .unavailable)
        XCTAssertEqual(model.failure, .transport(.cancelled))
        XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: root.path).isEmpty)
        XCTAssertNil(model.lastExit)
        model.editDraft("起動前取消の後に入力した本文")
        await model.reconnect()
        XCTAssertEqual(model.phase, .ready, "接続失敗：\(String(describing: model.failure))")
        XCTAssertEqual(model.draftText, "起動前取消の後に入力した本文")
        XCTAssertTrue(model.isDirty); XCTAssertTrue(model.canSaveDraft)
        XCTAssertNil(model.lastExit) // No process exit is invented for the first attempt.
        let closed = await model.saveAndClose()
        XCTAssertTrue(closed)
    }
}
