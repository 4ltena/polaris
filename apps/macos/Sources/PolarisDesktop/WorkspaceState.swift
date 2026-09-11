import SwiftUI
import AppKit
import Combine

// ローカルの選択・下書きだけを所有する。外からのsnapshot更新で選択を奪わない。
@MainActor
final class WorkspaceState: ObservableObject {
    @Published var selectedConversationID: String? {
        didSet { if oldValue != selectedConversationID { voiceController?.cancel() } }
    }
    @Published var drafts: [String: WorkspaceDraft] = [:]
    @Published var expandedProjects: Set<String> = []
    @Published var fileSelections: [String: WorkspaceFileSelection] = [:]
    @Published private(set) var fileFocusRequest = 0
    @Published private(set) var pending: [String: UUID] = [:]
    @Published var sendErrors: [String: String] = [:]
    @Published var layout = WorkspaceLayout()

    @Published var voicePhases: [String: WorkspaceInputPhase] = [:]
    @Published var voiceNotes: [String: String] = [:]
    @Published var voiceDeferred: [String: String] = [:]
    private var voiceController: WorkspaceVoiceController?
    private var voiceTextSink: ((String, String) -> Void)?
    private var terminationObserver: AnyCancellable?
    func enableVoice(backend: any WorkspaceSpeechBackend, appendText: ((String, String) -> Void)? = nil) {
        voiceController?.cancel()
        voiceTextSink = appendText
        voiceController = WorkspaceVoiceController(state: self, backend: backend)
        terminationObserver = NotificationCenter.default.publisher(for: NSApplication.willTerminateNotification)
            .sink { [weak self] _ in self?.cancelVoice() }
    }
    func voice(_ id: String, _ action: WorkspaceVoiceAction) {
        voiceController?.handle(id, action)
    }
    func cancelVoice() { voiceController?.cancel() }
    func appendVoiceText(_ text: String, to id: String) {
        guard pending[id] == nil, drafts[id]?.isComposing != true else { return }
        if let voiceTextSink { voiceTextSink(id, text) }
        else { editDraft(id) { $0.text += ($0.text.isEmpty ? "" : "\n") + text } }
        voiceDeferred[id] = nil
        voiceNotes[id] = "認識文を下書きへ追加しました。送信はしていません。"
    }
    func applyDeferredVoice(_ id: String) {
        guard let text = voiceDeferred[id], voicePhases[id] == nil else { return }
        appendVoiceText(text, to: id)
    }

    init(conversationID: String? = nil) { selectedConversationID = conversationID }
    func endComposition(_ id: String) {
        guard drafts[id]?.isComposing == true else { return }
        editDraft(id) { $0.isComposing = false }
    }
    func editDraft(_ id: String, _ edit: (inout WorkspaceDraft) -> Void) {
        var draft = drafts[id] ?? WorkspaceDraft()
        edit(&draft)
        draft.revision += 1
        drafts[id] = draft
    }
    func openFile(_ id: String, conversationID: String, revisionID: String? = nil) {
        var selection = fileSelections[conversationID] ?? WorkspaceFileSelection()
        if !selection.openIDs.contains(id) { selection.openIDs.append(id) }
        selection.selectedID = id
        if let revisionID {
            selection.versions[id] = .history
            selection.revisions[id] = revisionID
        }
        fileSelections[conversationID] = selection
        fileFocusRequest += 1
    }
    func closeFile(_ id: String, conversationID: String) {
        var selection = fileSelections[conversationID] ?? WorkspaceFileSelection()
        let index = selection.openIDs.firstIndex(of: id) ?? 0
        selection.openIDs.removeAll { $0 == id }
        if selection.selectedID == id {
            selection.selectedID = selection.openIDs.isEmpty ? nil : selection.openIDs[min(index, selection.openIDs.count - 1)]
        }
        fileSelections[conversationID] = selection
    }
    func beginSubmission(conversation: WorkspaceConversation) -> WorkspaceSubmission? {
        let id = conversation.id
        let draft = drafts[id] ?? WorkspaceDraft()
        let model = draft.model ?? conversation.model
        guard pending[id] == nil, voicePhases[id] == nil, voiceDeferred[id] == nil, draft.canSend(model: model, phase: conversation.phase) else { return nil }
        let request = WorkspaceSubmission(requestID: UUID(), conversationID: id, text: draft.text,
            attachments: draft.attachments, model: model, permission: draft.permission, draftRevision: draft.revision)
        pending[id] = request.requestID
        sendErrors[id] = nil
        return request
    }
    func finishSubmission(_ request: WorkspaceSubmission, result: WorkspaceSendResult) {
        guard pending[request.conversationID] == request.requestID else { return }
        pending[request.conversationID] = nil
        switch result {
        case .accepted:
            // 遅いACKで次の入力や別会話を消さない。
            if drafts[request.conversationID, default: WorkspaceDraft()].revision == request.draftRevision {
                editDraft(request.conversationID) { $0.text = ""; $0.attachments = [] }
            }
        case .rejected(let reason): sendErrors[request.conversationID] = reason
        }
    }
}
struct WorkspaceFileSelection: Equatable {
    var openIDs: [String] = []
    var selectedID: String?
    var versions: [String: WorkspaceFileVersion] = [:]
    var revisions: [String: String] = [:]
    var historyDiff: Set<String> = []
}
struct WorkspaceLayout: Equatable {
    var projects: Double = 200
    var chat: Double = 560
    var tree: Double = 180
    var work: Double = 282
    static let minimumWidth: Double = 1120
    static let minimumHeight: Double = 720
    func fitted(width: Double, height: Double) -> Self {
        var result = self
        func bound(_ value: Double, _ low: Double, _ high: Double) -> Double {
            min(max(value.isFinite ? value : low, low), max(low, high))
        }
        // 2本の縦dividerと、右側の本文・treeを確保した残りを会話に使う。
        result.projects = bound(projects, 160, width - 320 - 400 - 14)
        result.chat = bound(chat, 320, width - result.projects - 400 - 14)
        result.tree = bound(tree, 140, width - result.projects - result.chat - 14 - 7 - 240)
        result.work = bound(work, 220, height - 290)
        return result
    }
}
