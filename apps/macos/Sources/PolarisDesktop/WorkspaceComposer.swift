import AppKit
import SwiftUI
import PolarisSettings

struct WorkspaceConversationView: View {
    let conversation: WorkspaceConversation
    let snapshot: WorkspaceSnapshot
    @ObservedObject var state: WorkspaceState
    let onSend: @MainActor (WorkspaceSubmission) async -> WorkspaceSendResult
    let onVoice: ((String, WorkspaceVoiceAction) -> Void)?
    private var draft: WorkspaceDraft { state.drafts[conversation.id] ?? WorkspaceDraft() }
    private var model: WorkspaceModelConfiguration { draft.model ?? conversation.model }
    private var phase: WorkspaceInputPhase { state.pending[conversation.id] == nil ? (state.voicePhases[conversation.id] ?? conversation.phase) : .sending }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 5) {
                Text(conversation.title).font(.headline)
                Text("\(model.title) · \(model.destination) · \(model.effort)")
                    .font(.caption).foregroundStyle(.secondary)
            }.padding(18)
            Divider()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 24) {
                    if conversation.messages.isEmpty {
                        Text("この会話で進めたいことを入力してください。").foregroundStyle(.secondary)
                    }
                    ForEach(conversation.messages) { message in
                        VStack(alignment: .leading, spacing: 8) {
                            Text(message.author).font(.caption).foregroundStyle(.secondary)
                            Text(message.text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                }.padding(18)
            }
            VStack(alignment: .leading, spacing: 8) {
                WorkspaceComposerInput(
                    text: Binding(get: { draft.text }, set: { text in state.editDraft(conversation.id) { $0.text = text } }),
                    isComposing: Binding(get: { draft.isComposing }, set: { marked in
                        if draft.isComposing != marked { state.editDraft(conversation.id) { $0.isComposing = marked } }
                    }), attachments: draft.attachments, models: snapshot.models, selectedModel: model,
                    selectedPermission: draft.permission, phase: phase, showsSend: draft.showsSend,
                    canSend: draft.canSend(model: model, phase: phase) && state.voiceDeferred[conversation.id] == nil,
                    attachmentPreview: { WorkspaceComposerAttachmentPreview(attachment: $0, detail: $0.url.path) },
                    onSelectAttachments: selectAttachments,
                    onRemoveAttachment: { attachment in state.editDraft(conversation.id) { $0.attachments.removeAll { $0.id == attachment.id } } },
                    onSelectModel: { option in state.editDraft(conversation.id) { $0.model = option } },
                    onSelectPermission: { permission in state.editDraft(conversation.id) { $0.permission = permission } },
                    onSend: send,
                    onVoice: onVoice.map { callback in { callback(conversation.id, $0) } },
                    voiceNote: state.voiceNotes[conversation.id], deferredVoice: state.voiceDeferred[conversation.id],
                    onApplyDeferredVoice: { state.applyDeferredVoice(conversation.id) },
                    onDiscardDeferredVoice: { state.voiceDeferred[conversation.id] = nil },
                    attachmentNotice: snapshot.isDemo ? "添付は名前と場所のみを確認します。内容の読取・アップロードは行いません。" : nil)
                if let error = state.sendErrors[conversation.id] { note(error) }
            }
            .padding(12).background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 18))
                .overlay(RoundedRectangle(cornerRadius: 18).stroke(.separator, lineWidth: 1))
                .padding(12)
        }
        .onDisappear {
            // 入力Viewの破棄後に変換中フラグだけが会話へ残ることを防ぐ。
            onVoice?(conversation.id, .cancel)
            state.endComposition(conversation.id)
        }
    }
    private func note(_ text: String) -> some View { Text(text).font(.caption).foregroundStyle(.secondary) }
    private func send() {
        guard let request = state.beginSubmission(conversation: conversation) else { return }
        Task { @MainActor in state.finishSubmission(request, result: await onSend(request)) }
    }
    private func selectAttachments() {
        let id = conversation.id // picker中の会話切替で添付先を変えない。
        let panel = NSOpenPanel()
        panel.title = "添付ファイルを選択"
        panel.canChooseFiles = true; panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true; panel.canCreateDirectories = false
        panel.begin { response in
            guard response == .OK else { return }
            state.editDraft(id) { draft in
                for url in panel.urls where !draft.attachments.contains(where: { $0.url == url }) {
                    draft.attachments.append(WorkspaceAttachment(url: url))
                }
            }
        }
    }
}

struct WorkspaceTextInput: NSViewRepresentable {
    @Binding var text: String
    let onComposition: (Bool) -> Void
    let onSend: () -> Void
    func makeCoordinator() -> Coordinator { Coordinator(self) }
    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSScrollView()
        let input = WorkspaceInputTextView()
        input.isRichText = false
        input.isAutomaticQuoteSubstitutionEnabled = false
        input.isAutomaticDashSubstitutionEnabled = false
        input.font = .preferredFont(forTextStyle: .body)
        input.textColor = .textColor
        input.drawsBackground = false
        input.isVerticallyResizable = true
        input.isHorizontallyResizable = false
        input.autoresizingMask = [.width]
        input.textContainer?.widthTracksTextView = true
        input.textContainerInset = NSSize(width: 3, height: 6)
        input.delegate = context.coordinator
        input.string = text
        input.onSend = onSend
        input.onComposition = onComposition
        input.setAccessibilityLabel("指示入力")
        scroll.documentView = input
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        return scroll
    }
    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let input = scroll.documentView as? WorkspaceInputTextView else { return }
        input.onSend = onSend; input.onComposition = onComposition
        if input.string != text && !input.hasMarkedText() {
            input.string = text
        }
    }
    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: WorkspaceTextInput
        init(_ parent: WorkspaceTextInput) { self.parent = parent }
        func textDidChange(_ notification: Notification) {
            guard let input = notification.object as? NSTextView else { return }
            parent.text = input.string
            parent.onComposition(input.hasMarkedText())
        }
    }
}

final class WorkspaceInputTextView: NSTextView {
    var onSend: (() -> Void)?
    var onComposition: ((Bool) -> Void)?
    static func shouldSend(keyCode: UInt16, modifiers: NSEvent.ModifierFlags, marked: Bool) -> Bool {
        (keyCode == 36 || keyCode == 76) && !marked
            && modifiers.intersection([.shift, .option, .control, .command]).isEmpty
    }
    override func keyDown(with event: NSEvent) {
        // 変換確定前の状態を判定する。super後のhasMarkedTextでは確定Enterを誤送信する。
        if Self.shouldSend(keyCode: event.keyCode, modifiers: event.modifierFlags, marked: hasMarkedText()) {
            onSend?()
        } else { super.keyDown(with: event) }
        onComposition?(hasMarkedText())
    }
    override func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        super.setMarkedText(string, selectedRange: selectedRange, replacementRange: replacementRange)
        onComposition?(hasMarkedText())
    }
    override func unmarkText() { super.unmarkText(); onComposition?(hasMarkedText()) }
}
