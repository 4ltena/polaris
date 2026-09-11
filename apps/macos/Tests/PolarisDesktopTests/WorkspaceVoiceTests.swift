import XCTest
@testable import PolarisDesktop

@MainActor
private final class VoiceFake: WorkspaceSpeechBackend {
    var callback: (@MainActor (WorkspaceSpeechEvent) -> Void)?
    var starts = 0
    var cancels = 0
    var stops = 0
    var immediate: WorkspaceSpeechEvent?
    func start(_ receive: @escaping @MainActor (WorkspaceSpeechEvent) -> Void) {
        starts += 1; callback = receive
        if let immediate { receive(immediate) }
    }
    func stop() { stops += 1 }
    func cancel() { cancels += 1 }
    func emit(_ event: WorkspaceSpeechEvent) { callback?(event) }
}

@MainActor
final class WorkspaceVoiceTests: XCTestCase {
    func testFinalAppendsToLatestDraftOnlyOnceAndNeverSends() {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake(); state.enableVoice(backend: fake)
        state.editDraft("c") { $0.text = "元" }
        state.voice("c", .start)
        fake.emit(.recording)
        state.editDraft("c") { $0.text = "編集中の文" }
        fake.emit(.final("認識文")); fake.emit(.final("重複"))
        XCTAssertEqual(state.drafts["c"]?.text, "編集中の文\n認識文")
        XCTAssertTrue(state.pending.isEmpty)
        XCTAssertNil(state.voicePhases["c"])
    }
    func testIMEKeepsDraftAndRequiresExplicitAppend() {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake(); state.enableVoice(backend: fake)
        state.editDraft("c") { $0.text = "変換"; $0.isComposing = true }
        state.voice("c", .start); fake.emit(.final("音声"))
        state.applyDeferredVoice("c")
        XCTAssertEqual(state.drafts["c"]?.text, "変換")
        XCTAssertEqual(state.voiceDeferred["c"], "音声")
        state.endComposition("c"); state.applyDeferredVoice("c"); state.applyDeferredVoice("c")
        XCTAssertEqual(state.drafts["c"]?.text, "変換\n音声")
    }
    func testConversationSwitchCancelsAndOldPermissionOrResultCannotAppend() {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake(); state.enableVoice(backend: fake)
        state.voice("c", .start)
        let old = fake.callback
        state.selectedConversationID = "other"
        old?(.recording); old?(.final("遅延"))
        XCTAssertTrue(state.voicePhases.isEmpty)
        XCTAssertTrue(state.drafts.isEmpty)
        XCTAssertGreaterThan(fake.cancels, 0)
    }
    func testSynchronousDenialPreservesDraftAndClearsBusy() {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake(); fake.immediate = .failure("権限拒否")
        state.enableVoice(backend: fake)
        state.editDraft("c") { $0.text = "保持" }
        state.voice("c", .start)
        XCTAssertEqual(state.voiceNotes["c"], "権限拒否")
        XCTAssertEqual(state.drafts["c"]?.text, "保持")
        XCTAssertNil(state.voicePhases["c"])
    }
    func testStopDoesNotAppendAndCancellationDiscardsFinal() {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake(); state.enableVoice(backend: fake)
        state.voice("c", .start); fake.emit(.recording)
        state.voice("c", .stop)
        XCTAssertEqual(fake.stops, 1)
        XCTAssertEqual(state.voicePhases["c"], .transcribing)
        XCTAssertTrue(state.drafts.isEmpty)
        state.cancelVoice(); fake.emit(.final("破棄"))
        XCTAssertTrue(state.drafts.isEmpty)
    }
    func testRecordingAndSendingMutuallyExcludeEachOther() {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake(); state.enableVoice(backend: fake)
        let conversation = WorkspaceConversation(id: "c", projectID: nil, title: "test", model: WorkspaceFixture.cloud, messages: [])
        state.editDraft("c") { $0.text = "送信文" }
        state.voice("c", .start)
        XCTAssertNil(state.beginSubmission(conversation: conversation))
        state.cancelVoice()
        XCTAssertNotNil(state.beginSubmission(conversation: conversation))
        state.voice("c", .start)
        XCTAssertEqual(fake.starts, 1)
    }
    func testFinalDeadlineReleasesControllerAndIgnoresLateResult() async throws {
        let state = WorkspaceState(conversationID: "c")
        let fake = VoiceFake()
        let controller = WorkspaceVoiceController(state: state, backend: fake, finalLimit: .milliseconds(10))
        controller.handle("c", .start); fake.emit(.recording); controller.handle("c", .stop)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertNil(state.voicePhases["c"])
        XCTAssertTrue(state.voiceNotes["c"]?.contains("時間切れ") == true)
        fake.emit(.final("遅延"))
        XCTAssertTrue(state.drafts.isEmpty)
    }
}
