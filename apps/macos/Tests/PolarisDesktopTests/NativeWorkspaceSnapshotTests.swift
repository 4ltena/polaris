import Foundation
import XCTest
import PolarisSettings
@testable import PolarisDesktop

final class NativeWorkspaceSnapshotTests: XCTestCase {
    func testSavedSelectionDoesNotRevertToPreviousServiceModel() throws {
        let selection = try ExecutionBinding(provider: .codex, model: "gpt-6-astra", effort: "medium")
        let previous: ServiceValue = .object(["model": .string("verification-runtime"), "provider": .string("lmstudio")])
        let displayed = NativeWorkspaceSnapshot.selectedModel(selection, fallback: previous)
        XCTAssertEqual(displayed.title, "gpt-6-astra")
        XCTAssertEqual(displayed.id, "codex|gpt-6-astra")
        XCTAssertEqual(NativeWorkspaceSnapshot.selectedModel(nil, fallback: previous).title, "verification-runtime")
    }

    private func binding() throws -> WorkspaceBinding {
        let id = UUID().uuidString
        let bytes = try JSONSerialization.data(withJSONObject: ["projectID": id, "sessionID": id,
            "clientID": id, "sourcePath": "/fixture", "sourceDevice": "1", "sourceInode": "2"])
        return try JSONDecoder().decode(WorkspaceBinding.self, from: bytes)
    }

    func testMissingSnapshotDoesNotInventCompletedWork() throws {
        let snapshot = NativeWorkspaceSnapshot.make(binding: try binding(), payload: nil)
        XCTAssertEqual(snapshot.conversations.count, 1)
        XCTAssertTrue(snapshot.activities[0].tasks.isEmpty)
        XCTAssertTrue(snapshot.activities[0].agents.isEmpty)
        XCTAssertNil(snapshot.activities[0].git)
    }

    func testBlockerTakesPrecedenceAndChildDoesNotInheritCurrentModel() throws {
        let payload: ServiceValue = .object([
            "configuration": .object(["model": .string("new-main"), "provider": .string("codex"), "effort": .string("medium")]),
            "tasks": .array([.object(["task_id": .string("task"), "title": .string("検証"),
                "state": .string("completed"), "acceptance": .array([]),
                "blockers": .array([.object(["kind": .string("outcome_unknown"), "detail": .string("未保存")])])])]),
            "children": .array([.object(["attempt_id": .string("attempt"), "run_id": .string("run"),
                "state": .string("succeeded"), "task_ids": .array([.string("task")])])])
        ])
        let snapshot = NativeWorkspaceSnapshot.make(binding: try binding(), payload: payload)
        XCTAssertEqual(snapshot.activities[0].tasks[0].status, .unknown)
        XCTAssertEqual(snapshot.activities[0].completedCount, 0)
        XCTAssertEqual(snapshot.activities[0].agents[0].model.id, "unreported")
        XCTAssertEqual(snapshot.activities[0].agents[0].attempt, 0)
    }

    @MainActor
    func testNativeVoiceUsesAuthoritativeDraftAndLeavesDisplayDraftEmpty() {
        let state = WorkspaceState(conversationID: "native")
        var text = "編集中"
        state.enableVoice(backend: SilentSpeech(), appendText: { id, addition in
            XCTAssertEqual(id, "native")
            text += "\n" + addition
        })
        state.appendVoiceText("認識文", to: "native")
        XCTAssertEqual(text, "編集中\n認識文")
        XCTAssertNil(state.drafts["native"])
        state.editDraft("native") { $0.isComposing = true }
        state.appendVoiceText("変換中", to: "native")
        XCTAssertEqual(text, "編集中\n認識文")
    }
}

@MainActor private final class SilentSpeech: WorkspaceSpeechBackend {
    func start(_ receive: @escaping @MainActor (WorkspaceSpeechEvent) -> Void) {}
    func stop() {}
    func cancel() {}
}
