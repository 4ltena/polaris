import Foundation
import XCTest
import PolarisSettings
@testable import PolarisDesktop

private actor NativeComposerTransport: WorkspaceServiceTransport {
    private let sessionID: String
    private var sessionRevision: UInt64 = 10
    private var draftRevision: UInt64 = 4
    private var draftText = ""
    private var runState: String?
    private var rejectNextDraft: String?
    private var loseConfigureACK = false
    private(set) var draftWrites: [String] = []
    private(set) var runStarts = 0
    private(set) var closeCalls = 0

    init(sessionID: String = "session", runState: String? = nil) {
        self.sessionID = sessionID
        self.runState = runState
    }

    func rejectNextDraft(_ code: String) { rejectNextDraft = code }
    func loseNextConfigureACK() { loseConfigureACK = true }

    func start() throws -> ServiceHello {
        try ServiceHello(.object([
            "protocol_version": .number("1"), "engine_epoch": .string("e"),
            "capabilities": .array(["session_read", "history_read", "draft_update", "request_status", "shutdown", "run_start", "session_configure"].map(ServiceValue.string)),
            "limits": .object(["frame_bytes": .string("1048576"), "subscription_events": .string("256"),
                "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"), "text_batch_ms": .string("50")])
        ]))
    }

    private func snapshot() -> ServiceValue {
        .object([
            "snapshot_id": .string("snapshot"), "session_id": .string(sessionID), "summary": .string(""),
            "session_revision": .string(String(sessionRevision)), "content_revision": .string("0"),
            "plan_revision": .string("0"), "policy_revision": .string("2"), "history_start_cursor": .string("first"),
            "position": .object(["engine_epoch": .string("e"), "subscription_id": .string("subscription"), "event_seq": .string("0")]),
            "draft": .object(["draft_revision": .string(String(draftRevision)), "text": .string(draftText), "attachment_ids": .array([])]),
            "configuration": .object(["configuration_revision": .string("3"), "provider": .string("codex"), "model": .string("current"), "effort": .string("medium")]),
            "runs": .array(runState.map { [run($0)] } ?? []), "children": .array([]), "child_attempt_count": .string("0"), "role_bindings": .array([]), "role_catalog": .array([]),
            "tasks": .array([]), "unresolved_approvals": .array([])
        ])
    }

    private func run(_ state: String) -> ServiceValue {
        .object(["run_id": .string("run"), "attempt_id": .string("attempt"), "state": .string(state), "task_ids": .array([])])
    }

    func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
        let payload: ServiceValue
        switch request {
        case .workspaceRead, .attachmentRead, .localModels, .configureRoles, .sourceApplyList, .sourceApplyPage, .sourceApplyResolve: throw ServiceError.notReady
        case .snapshot, .subscribe:
            payload = snapshot()
        case .draft(_, let expectedRevision, let text):
            guard expectedRevision == draftRevision else { throw ServiceError.correlation }
            draftWrites.append(text)
            if let code = rejectNextDraft {
                rejectNextDraft = nil
                return .init(requestID: requestID, method: nil, payload: nil,
                             rejection: .object(["code": .string(code), "message": .string("合成拒否")]))
            }
            draftText = text; draftRevision += 1; sessionRevision += 1
            payload = .object(["session_revision": .string(String(sessionRevision)), "draft_revision": .string(String(draftRevision))])
        case .run(_, let draft, let configuration, let policy):
            guard draft == draftRevision, configuration == 3, policy == 2 else { throw ServiceError.correlation }
            runStarts += 1; runState = "queued"; sessionRevision += 1; draftRevision += 1; draftText = ""
            payload = .object(["session_revision": .string(String(sessionRevision)), "run_id": .string("run"),
                               "attempt_id": .string("attempt"), "state": .string("queued")])
        case .configure(_, let revision, let selection, let historyMode):
            sessionRevision += 1
            if loseConfigureACK { loseConfigureACK = false; throw ServiceError.timeout }
            var configuration: [String: ServiceValue] = [
                "configuration_revision": .string(String(revision + 1)), "provider": .string(selection.provider.rawValue),
                "model": .string(selection.model), "effort": .string(selection.storedEffort)
            ]
            if historyMode != .legacy { configuration["history_mode"] = .string(historyMode.rawValue) }
            payload = .object(["session_revision": .string(String(sessionRevision)), "configuration": .object(configuration)])
        case .shutdown:
            payload = .object(["engine_epoch": .string("e"), "state": .string("ready")])
        case .cancel, .approval:
            throw ServiceError.notReady
        }
        return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
    }

    func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
        let payload: ServiceValue
        switch request {
        case .history:
            payload = .object(["snapshot_id": .string("snapshot"), "session_revision": .string(String(sessionRevision)), "messages": .array([])])
        case .status:
            payload = .object(["status": .string("not_found")])
        }
        return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
    }

    func takeEvents() async throws -> [ServiceValue] { [] }
    func close() async -> ServiceClient.Exit? {
        closeCalls += 1
        return .init(status: 0, forced: false)
    }
    func hasUnreapedChild() async -> Bool { false }
}

@MainActor
final class NativeComposerServiceTests: XCTestCase {
    private func model(_ transport: NativeComposerTransport, root: URL = FileManager.default.temporaryDirectory) throws -> WorkspaceServiceModel {
        let store = try ServiceClient.PersistentStore(root: root, projectID: "project", sessionID: "session")
        return try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
                                         clientID: "native", makeTransport: { transport })
    }

    private func reviewedAttachment(_ model: WorkspaceServiceModel, name: String = "reviewed.txt", text: String = "添付本文") throws -> WorkspaceAttachment {
        let attachment = WorkspaceAttachment(url: URL(fileURLWithPath: "/imported/\(name)"))
        try model.addImportedAttachment(attachment, text: text)
        model.reviewImportedAttachment(attachment.id)
        return attachment
    }

    func testImportedTextCannotStartUntilExplicitReview() async throws {
        let transport = NativeComposerTransport()
        let model = try model(transport)
        await model.start()
        let attachment = WorkspaceAttachment(url: URL(fileURLWithPath: "/imported/pending.txt"))
        try model.addImportedAttachment(attachment, text: "確認前の本文")

        XCTAssertFalse(model.canSend)
        await model.startRun()
        let startsBeforeReview = await transport.runStarts
        XCTAssertEqual(startsBeforeReview, 0)
        XCTAssertEqual(model.importedAttachments.count, 1)

        model.reviewImportedAttachment(attachment.id)
        XCTAssertTrue(model.canSend)
        await model.startRun()
        let startsAfterReview = await transport.runStarts
        XCTAssertEqual(startsAfterReview, 1)
        XCTAssertTrue(model.importedAttachments.isEmpty)
        _ = await model.close()
    }

    func testAttachmentMaterializesOnceAcrossRejectedSaveAndRetry() async throws {
        let transport = NativeComposerTransport()
        let model = try model(transport)
        await model.start()
        _ = try reviewedAttachment(model, text: "重複してはいけない本文")
        await transport.rejectNextDraft("permission_denied")

        await model.saveDraft()
        XCTAssertEqual(model.importedAttachments.count, 0)
        XCTAssertTrue(model.draftText.contains("重複してはいけない本文"))
        XCTAssertTrue(model.isDirty)

        await model.saveDraft()
        let writes = await transport.draftWrites
        XCTAssertEqual(writes.count, 2)
        XCTAssertEqual(writes[0], writes[1])
        XCTAssertEqual(writes[1].components(separatedBy: "重複してはいけない本文").count, 2)
        _ = await model.close()
    }

    func testSaveAndClosePersistsReviewedAttachmentBytesBeforeExit() async throws {
        let transport = NativeComposerTransport()
        let model = try model(transport)
        await model.start()
        _ = try reviewedAttachment(model, name: "utf8.txt", text: "日本語\n🙂")

        let closed = await model.saveAndClose()
        XCTAssertTrue(closed)
        XCTAssertEqual(model.phase, .closed)
        let writes = await transport.draftWrites
        XCTAssertEqual(writes.count, 1)
        XCTAssertEqual(writes[0], "\n\n--- 添付: utf8.txt ---\n日本語\n🙂\n--- 添付終端 ---")
    }

    func testUnreviewedImportCannotMutateDraftOrSaveUntilReviewed() async throws {
        let transport = NativeComposerTransport()
        let model = try model(transport)
        await model.start()
        model.editDraft("既存の下書き")
        let attachment = WorkspaceAttachment(url: URL(fileURLWithPath: "/imported/unreviewed.txt"))
        try model.addImportedAttachment(attachment, text: "未確認の本文")

        await model.saveDraft()
        let closedBeforeReview = await model.saveAndClose()
        XCTAssertFalse(closedBeforeReview)
        XCTAssertEqual(model.draftText, "既存の下書き")
        XCTAssertEqual(model.importedAttachments.map(\.id), [attachment.id])
        XCTAssertTrue(model.isDirty)
        let writesBeforeReview = await transport.draftWrites
        let closesBeforeReview = await transport.closeCalls
        XCTAssertTrue(writesBeforeReview.isEmpty)
        XCTAssertEqual(closesBeforeReview, 0)

        model.reviewImportedAttachment(attachment.id)
        let closedAfterReview = await model.saveAndClose()
        XCTAssertTrue(closedAfterReview)
        let writes = await transport.draftWrites
        XCTAssertEqual(writes, ["既存の下書き\n\n--- 添付: unreviewed.txt ---\n未確認の本文\n--- 添付終端 ---"])
        let closesAfterReview = await transport.closeCalls
        XCTAssertEqual(closesAfterReview, 1)
    }

    func testDuplicateStandardizedAttachmentURLIsRejectedWithoutReplacingFirstImport() async throws {
        let transport = NativeComposerTransport()
        let model = try model(transport)
        await model.start()
        let first = WorkspaceAttachment(url: URL(fileURLWithPath: "/imported/dir/../same.txt"))
        let duplicate = WorkspaceAttachment(url: URL(fileURLWithPath: "/imported/same.txt"))
        try model.addImportedAttachment(first, text: "最初に確認した本文")

        XCTAssertThrowsError(try model.addImportedAttachment(duplicate, text: "置換してはいけない本文")) {
            XCTAssertEqual($0 as? ServiceError, .capacity)
        }
        XCTAssertEqual(model.importedAttachments.count, 1)
        XCTAssertEqual(model.importedAttachments.first?.attachment.id, first.id)
        XCTAssertEqual(model.importedAttachments.first?.text, "最初に確認した本文")
        _ = await model.close()
    }

    private func ownerFixture(runState: String? = nil) async throws -> (DesktopWorkspaceOwner, WorkspaceServiceModel, NativeComposerTransport, SettingsStore, URL) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-native-composer-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        let source = root.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let settings = try SettingsStore(url: root.appendingPathComponent("metadata/settings.json"))
        let old = try ExecutionBinding(provider: .codex, model: "current", effort: "medium")
        var document = SettingsDocument()
        document.draft.selectFolder(source)
        document.draft.preferences.executionBinding = old
        document.draft.preferences.permission = .readOnly
        document.finish()
        let (saved, storeRoot) = try settings.prepareWorkspace(document)
        let binding = try XCTUnwrap(saved.workspace)
        let transport = NativeComposerTransport(sessionID: binding.sessionID, runState: runState)
        let service = try WorkspaceServiceModel(helperURL: root.appendingPathComponent("helper"),
                                                store: ServiceClient.PersistentStore(root: try XCTUnwrap(storeRoot), projectID: binding.projectID, sessionID: binding.sessionID),
                                                clientID: binding.clientID, makeTransport: { transport }, configurationStore: settings)
        let owner = DesktopWorkspaceOwner(helper: { root.appendingPathComponent("helper") }, factory: { _, _, _ in service })
        _ = try await owner.prepare(document: try settings.load(), store: settings)
        return (owner, service, transport, settings, root)
    }

    func testModelSelectionRefusesPendingUnknownConfigurationAndRetainsDraft() async throws {
        let (owner, service, transport, settings, root) = try await ownerFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        service.editDraft("保持する下書き")
        await transport.loseNextConfigureACK()
        await service.configureSelection()
        XCTAssertTrue(service.configurationOutcomeUnknown)
        XCTAssertNotNil(service.pendingConfiguration)
        let next = try ExecutionBinding(provider: .openai, model: "gpt-6-astra", effort: "medium")

        do { _ = try await owner.selectExecution(next, settings: settings); XCTFail("未確定設定での切替を拒否する") }
        catch { XCTAssertEqual(error as? ServiceError, .notReady) }
        XCTAssertEqual(service.draftText, "保持する下書き")
        XCTAssertEqual(try settings.load().preferences?.executionBinding?.model, "current")
        _ = await service.close()
    }

    func testModelSelectionRefusesActiveRunAndRetainsDraft() async throws {
        let (owner, service, _, settings, root) = try await ownerFixture(runState: "running")
        defer { try? FileManager.default.removeItem(at: root) }
        service.editDraft("実行中も保持する下書き")
        let next = try ExecutionBinding(provider: .openai, model: "gpt-6-astra", effort: "medium")

        do { _ = try await owner.selectExecution(next, settings: settings); XCTFail("実行中の切替を拒否する") }
        catch { XCTAssertEqual(error as? ServiceError, .notReady) }
        XCTAssertEqual(service.draftText, "実行中も保持する下書き")
        XCTAssertEqual(try settings.load().preferences?.executionBinding?.model, "current")
        _ = await service.close()
    }
}
