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
                "display_name": .string("Vega"),
                "state": .string("succeeded"), "task_ids": .array([.string("task")])])])
        ])
        let snapshot = NativeWorkspaceSnapshot.make(binding: try binding(), payload: payload)
        XCTAssertEqual(snapshot.activities[0].tasks[0].status, .unknown)
        XCTAssertEqual(snapshot.activities[0].completedCount, 0)
        XCTAssertEqual(snapshot.activities[0].agents[0].model.id, "unreported")
        XCTAssertEqual(snapshot.activities[0].agents[0].attempt, 0)
        XCTAssertEqual(snapshot.activities[0].agents[0].name, "Vega")
    }

    func testV2OrchestrationProjectsExplicitControlPlaneState() throws {
        let payload: ServiceValue = .object([
            "configuration": .object(["model": .string("astra"), "provider": .string("codex"), "effort": .string("medium")]),
            "tasks": .array([]), "children": .array([]),
            "orchestration": .object([
                "workflow": .object([
                    "phase": .string("verify"), "step": .string("converging"),
                    "acceptance": .array([.object(["criterion_id": .string("criterion"), "state": .string("passed"), "evidence_count": .string("1")])]),
                    "evidence": .object(["passed": .string("1"), "failed": .string("0"), "stale": .string("0"), "unavailable": .string("0")]),
                    "convergence": .object(["state": .string("allowed"), "required_criteria": .string("1"), "unresolved_gaps": .string("0")]),
                ]),
                "session_manager": .object([
                    "manager_revision": .string("4"), "session_count": .string("2"), "session_total": .string("2"), "session_omitted": .bool(false),
                    "sessions": .array([.object(["session_id": .string("main"), "kind": .string("current"), "state": .string("running")])]),
                    "sidechat": .string("active"), "queued_admissions": .string("0"), "active_runs": .string("1"),
                ]),
                "subagents": .array([.object(["root_run_id": .string("run"), "root_attempt_id": .string("attempt-root"), "control_revision": .string("1"), "logical_child_id": .string("child"), "attempt_id": .string("attempt"), "display_name": .string("Sirius"), "role": .string("reviewer"), "state": .string("running")])]),
                "subagent_total": .string("1"), "subagent_omitted": .bool(false), "mcp_leases": .array([]), "mcp_lease_total": .string("0"), "mcp_lease_omitted": .bool(false),
                "common_runner": .object(["state": .string("idle"), "completed_cases": .string("0"), "failed_cases": .string("0"), "cleanup_pending": .bool(false)]),
                "clarification": .object(["state": .string("none"), "questions": .array([]), "unanswered_count": .string("0")]),
            ]),
        ])
        let snapshot = NativeWorkspaceSnapshot.make(binding: try binding(), payload: payload)
        let projected = try XCTUnwrap(snapshot.activities.first?.orchestration)
        XCTAssertEqual(projected.workflow.phase, "verify")
        XCTAssertEqual(projected.workflow.convergence.state, .allowed)
        XCTAssertEqual(projected.sessionManager.sidechat, "active")
        XCTAssertEqual(projected.subagents.first?.displayName, "Sirius")
        XCTAssertEqual(snapshot.activities.first?.agents.first?.status, .running)
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
