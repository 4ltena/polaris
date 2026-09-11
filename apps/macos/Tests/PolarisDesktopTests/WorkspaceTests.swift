import AppKit
import XCTest
@testable import PolarisDesktop

final class WorkspaceTests: XCTestCase {
    @MainActor
    func testCompositionAndModifiedEnterDoNotSend() {
        XCTAssertTrue(WorkspaceInputTextView.shouldSend(keyCode: 36, modifiers: [], marked: false))
        XCTAssertTrue(WorkspaceInputTextView.shouldSend(keyCode: 76, modifiers: [], marked: false))
        XCTAssertFalse(WorkspaceInputTextView.shouldSend(keyCode: 36, modifiers: [], marked: true))
        for modifier: NSEvent.ModifierFlags in [.shift, .control, .option, .command] {
            XCTAssertFalse(WorkspaceInputTextView.shouldSend(keyCode: 36, modifiers: modifier, marked: false))
        }
        XCTAssertFalse(WorkspaceInputTextView.shouldSend(keyCode: 0, modifiers: [], marked: false))
    }

    @MainActor
    func testActualTextViewMarkedReturnDoesNotCallSend() {
        _ = NSApplication.shared
        let input = WorkspaceInputTextView()
        var sent = 0
        input.onSend = { sent += 1 }
        input.setMarkedText("よやく", selectedRange: NSRange(location: 3, length: 0),
                            replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertTrue(input.hasMarkedText())
        let enter = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
            windowNumber: 0, context: nil, characters: "\r", charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36)!
        input.keyDown(with: enter)
        XCTAssertEqual(sent, 0)
        input.unmarkText()
        input.keyDown(with: enter)
        XCTAssertEqual(sent, 1)
    }

    func testEmptyWhitespaceAttachmentAndBusySendConditions() {
        var draft = WorkspaceDraft()
        XCTAssertFalse(draft.showsSend)
        XCTAssertFalse(draft.canSend(model: WorkspaceFixture.cloud, phase: .idle))
        draft.text = " \n　"
        XCTAssertTrue(draft.showsSend)
        XCTAssertFalse(draft.canSend(model: WorkspaceFixture.cloud, phase: .idle))
        draft.text = ""
        draft.attachments = [WorkspaceAttachment(url: URL(fileURLWithPath: "/demo/a.txt"))]
        XCTAssertTrue(draft.showsSend)
        XCTAssertTrue(draft.canSend(model: WorkspaceFixture.cloud, phase: .idle))
        XCTAssertFalse(draft.canSend(model: WorkspaceFixture.local, phase: .idle))
        draft.text = "予約"
        for phase in [WorkspaceInputPhase.recording, .transcribing, .sending] {
            XCTAssertFalse(draft.canSend(model: WorkspaceFixture.cloud, phase: phase))
        }
        draft.isComposing = true
        XCTAssertFalse(draft.canSend(model: WorkspaceFixture.cloud, phase: .idle))
    }

    @MainActor
    func testSubmissionIsImmutableAndLateAckPreservesNextDraft() throws {
        let state = WorkspaceState(conversationID: "demo-chat")
        let conversation = WorkspaceFixture.snapshot().conversations[0]
        state.editDraft(conversation.id) { $0.text = "最初の指示"; $0.model = WorkspaceFixture.cloud }
        let request = try XCTUnwrap(state.beginSubmission(conversation: conversation))
        XCTAssertNil(state.beginSubmission(conversation: conversation))
        state.selectedConversationID = "demo-chat-b"
        state.editDraft(conversation.id) { $0.text = "次の指示"; $0.model = WorkspaceFixture.local }
        state.finishSubmission(request, result: .accepted)
        XCTAssertEqual(request.text, "最初の指示")
        XCTAssertEqual(request.model, WorkspaceFixture.cloud)
        XCTAssertEqual(state.drafts[conversation.id]?.text, "次の指示")
        XCTAssertEqual(state.selectedConversationID, "demo-chat-b")
        XCTAssertNil(state.pending[conversation.id])
    }

    @MainActor
    func testRejectedSubmissionPreservesAttachmentsAndRetryGetsNewID() throws {
        let state = WorkspaceState()
        let conversation = WorkspaceFixture.snapshot().conversations[0]
        state.editDraft(conversation.id) {
            $0.text = "添付を確認"
            $0.attachments = [WorkspaceAttachment(url: URL(fileURLWithPath: "/demo/a.txt"))]
        }
        let first = try XCTUnwrap(state.beginSubmission(conversation: conversation))
        state.finishSubmission(first, result: .rejected("接続がありません"))
        XCTAssertEqual(state.drafts[conversation.id]?.attachments.count, 1)
        XCTAssertEqual(state.sendErrors[conversation.id], "接続がありません")
        let second = try XCTUnwrap(state.beginSubmission(conversation: conversation))
        XCTAssertNotEqual(first.requestID, second.requestID)
        state.finishSubmission(first, result: .accepted)
        XCTAssertEqual(state.pending[conversation.id], second.requestID)
        state.finishSubmission(second, result: .accepted)
        XCTAssertEqual(state.drafts[conversation.id]?.text, "")
        XCTAssertEqual(state.drafts[conversation.id]?.attachments.count, 0)
    }

    @MainActor
    func testConversationDraftsAndModelsStaySeparate() {
        let state = WorkspaceState()
        state.editDraft("a") { $0.text = "A"; $0.model = WorkspaceFixture.cloud }
        state.editDraft("b") { $0.text = "B"; $0.model = WorkspaceFixture.local }
        state.selectedConversationID = "b"
        state.selectedConversationID = "a"
        XCTAssertEqual(state.drafts["a"]?.text, "A")
        XCTAssertEqual(state.drafts["a"]?.model, WorkspaceFixture.cloud)
        XCTAssertEqual(state.drafts["b"]?.model, WorkspaceFixture.local)
    }

    @MainActor
    func testLeavingComposerClearsOnlyItsCompositionFlag() {
        let state = WorkspaceState()
        state.editDraft("a") { $0.text = "変換中"; $0.isComposing = true }
        state.editDraft("b") { $0.text = "別の入力"; $0.isComposing = true }
        state.endComposition("a")
        XCTAssertEqual(state.drafts["a"]?.text, "変換中")
        XCTAssertEqual(state.drafts["a"]?.isComposing, false)
        XCTAssertEqual(state.drafts["b"]?.isComposing, true)
    }

    @MainActor
    func testPinKeepsSectionAndUnregisterKeepsConversationsAndDrafts() {
        let demo = WorkspaceDemoStore()
        demo.perform(.pin(projectID: "demo-project", pinned: false))
        XCTAssertTrue(demo.snapshot.projects(in: "demo-section").contains { $0.id == "demo-project" })
        XCTAssertTrue(demo.snapshot.projects(in: "projects").contains { $0.id == "demo-project" })
        demo.perform(.pin(projectID: "demo-project", pinned: true))
        XCTAssertFalse(demo.snapshot.projects(in: "demo-section").contains { $0.id == "demo-project" })
        XCTAssertEqual(demo.snapshot.projects.first?.sectionID, "demo-section")
        demo.state.editDraft("demo-chat") { $0.text = "保持する" }
        demo.perform(.removeProject("demo-project"))
        XCTAssertEqual(demo.snapshot.conversations.count, 2)
        XCTAssertNil(demo.snapshot.conversations[0].projectID)
        XCTAssertEqual(demo.state.drafts["demo-chat"]?.text, "保持する")
    }

    @MainActor
    func testAssignSectionUnpinsWithoutDuplicatingProject() {
        let demo = WorkspaceDemoStore()
        demo.perform(.createSection(projectID: "demo-project", name: "調査"))
        let id = demo.snapshot.sections.last!.id
        XCTAssertEqual(demo.snapshot.projects.count, 2)
        XCTAssertEqual(demo.snapshot.projects(in: id).map(\.id), ["demo-project"])
        XCTAssertTrue(demo.snapshot.projects(in: "projects").contains { $0.id == "demo-project" })
        XCTAssertFalse(demo.snapshot.projects.first!.pinned)
    }

    func testZeroAndManyTasksKeepExactCountsAndNonCompletionStates() {
        XCTAssertEqual(WorkspaceFixture.activity(id: "a", count: 0).completedCount, 0)
        let many = WorkspaceFixture.activity(id: "a", count: 200)
        XCTAssertEqual(many.tasks.count, 200)
        XCTAssertEqual(Set(many.tasks.map(\.id)).count, 200)
        XCTAssertEqual(many.completedCount, 46)
        XCTAssertEqual(many.runningCount, 1)
        XCTAssertEqual(many.agents.count, 2)
        for status in [WorkspaceTaskStatus.cancelled, .failed, .unknown] {
            XCTAssertTrue(many.tasks.contains { $0.status == status })
        }
    }

    @MainActor
    func testFileSelectionVersionAndTabsAreConversationScoped() {
        let state = WorkspaceState()
        state.openFile("schema", conversationID: "a", revisionID: "commit1")
        state.openFile("design", conversationID: "a")
        state.openFile("schema", conversationID: "b")
        XCTAssertEqual(state.fileSelections["a"]?.versions["schema"], .history)
        XCTAssertNil(state.fileSelections["b"]?.versions["schema"])
        state.closeFile("design", conversationID: "a")
        XCTAssertEqual(state.fileSelections["a"]?.selectedID, "schema")
        XCTAssertEqual(state.fileSelections["a"]?.revisions["schema"], "commit1")
        let focusRequest = state.fileFocusRequest
        state.openFile("schema", conversationID: "a")
        XCTAssertEqual(state.fileFocusRequest, focusRequest + 1)
        state.closeFile("schema", conversationID: "a")
        XCTAssertNil(state.fileSelections["a"]?.selectedID)
        XCTAssertEqual(state.fileSelections["b"]?.selectedID, "schema")
    }

    func testLayoutBoundsRecoverNonfiniteAndOffscreenSizes() {
        var saved = WorkspaceLayout()
        saved.projects = .infinity; saved.chat = 9999; saved.tree = -999; saved.work = 9999
        let fitted = saved.fitted(width: 1120, height: 656)
        XCTAssertGreaterThanOrEqual(fitted.projects, 160)
        XCTAssertGreaterThanOrEqual(fitted.chat, 320)
        XCTAssertGreaterThanOrEqual(fitted.tree, 140)
        XCTAssertGreaterThanOrEqual(1120 - fitted.projects - fitted.chat - fitted.tree - 21, 240)
        XCTAssertLessThanOrEqual(fitted.work, 366)
    }

    func testDefaultLayoutPrioritizesChatAndAllowsSidebarResize() {
        let initial = WorkspaceLayout().fitted(width: 1280, height: 716)
        let right = 1280 - initial.projects - initial.chat - 14
        XCTAssertGreaterThan(initial.chat, right)
        var resized = WorkspaceLayout()
        resized.projects = 280
        let fitted = resized.fitted(width: 1280, height: 716)
        XCTAssertEqual(fitted.projects, 280)
        XCTAssertGreaterThanOrEqual(1280 - fitted.projects - fitted.chat - fitted.tree - 21, 240)
    }

    func testTreePreservesPathsAndFileIDs() {
        let files = WorkspaceFixture.activity(id: "a", count: 0).files
        let tree = WorkspaceTreeNode.make(files)
        XCTAssertEqual(tree[0].name, "docs")
        XCTAssertEqual(tree[0].children?.first?.file?.id, "design")
        XCTAssertEqual(tree[1].file?.id, "schema")
    }
}
