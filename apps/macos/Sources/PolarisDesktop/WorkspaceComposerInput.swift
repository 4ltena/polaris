import Foundation
import SwiftUI
import PolarisSettings

/// 呼出側が表示してよい添付本文だけを渡す。ここではURLからの読取や取込をしない。
struct WorkspaceComposerAttachmentPreview: Identifiable, Equatable {
    let id: UUID
    let name: String
    let detail: String
    let text: String?

    init(attachment: WorkspaceAttachment, detail: String, text: String? = nil) {
        id = attachment.id
        name = attachment.name
        self.detail = detail
        self.text = text
    }
}

/// 再利用する入力面。可変値の正本は所有者にあり、このViewはBinding表示と明示操作だけを担う。
struct WorkspaceComposerInput: View {
    @Binding private var text: String
    @Binding private var isComposing: Bool
    let attachments: [WorkspaceAttachment]
    let models: [WorkspaceModelConfiguration]
    let selectedModel: WorkspaceModelConfiguration
    let selectedPermission: PermissionPreference?
    let phase: WorkspaceInputPhase
    let showsSend: Bool
    let canSend: Bool
    let attachmentPreview: (WorkspaceAttachment) -> WorkspaceComposerAttachmentPreview
    let onSelectAttachments: (() -> Void)?
    let onRemoveAttachment: (WorkspaceAttachment) -> Void
    let onSelectModel: (WorkspaceModelConfiguration) -> Void
    let onSelectPermission: (PermissionPreference) -> Void
    let onSend: () -> Void
    let onVoice: ((WorkspaceVoiceAction) -> Void)?
    let voiceNote: String?
    let deferredVoice: String?
    let onApplyDeferredVoice: (() -> Void)?
    let onDiscardDeferredVoice: (() -> Void)?
    let attachmentNotice: String?
    @State private var inspectedAttachment: WorkspaceComposerAttachmentPreview?

    init(text: Binding<String>, isComposing: Binding<Bool>, attachments: [WorkspaceAttachment],
         models: [WorkspaceModelConfiguration], selectedModel: WorkspaceModelConfiguration,
         selectedPermission: PermissionPreference?, phase: WorkspaceInputPhase, showsSend: Bool,
         canSend: Bool, attachmentPreview: @escaping (WorkspaceAttachment) -> WorkspaceComposerAttachmentPreview,
         onSelectAttachments: (() -> Void)?, onRemoveAttachment: @escaping (WorkspaceAttachment) -> Void,
         onSelectModel: @escaping (WorkspaceModelConfiguration) -> Void,
         onSelectPermission: @escaping (PermissionPreference) -> Void, onSend: @escaping () -> Void,
         onVoice: ((WorkspaceVoiceAction) -> Void)?, voiceNote: String?, deferredVoice: String?,
         onApplyDeferredVoice: (() -> Void)?, onDiscardDeferredVoice: (() -> Void)?, attachmentNotice: String?) {
        _text = text
        _isComposing = isComposing
        self.attachments = attachments
        self.models = models
        self.selectedModel = selectedModel
        self.selectedPermission = selectedPermission
        self.phase = phase
        self.showsSend = showsSend
        self.canSend = canSend
        self.attachmentPreview = attachmentPreview
        self.onSelectAttachments = onSelectAttachments
        self.onRemoveAttachment = onRemoveAttachment
        self.onSelectModel = onSelectModel
        self.onSelectPermission = onSelectPermission
        self.onSend = onSend
        self.onVoice = onVoice
        self.voiceNote = voiceNote
        self.deferredVoice = deferredVoice
        self.onApplyDeferredVoice = onApplyDeferredVoice
        self.onDiscardDeferredVoice = onDiscardDeferredVoice
        self.attachmentNotice = attachmentNotice
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !attachments.isEmpty {
                ScrollView(.horizontal) {
                    HStack {
                        ForEach(attachments) { attachment in
                            HStack(spacing: 4) {
                                Button(attachment.name) { inspectedAttachment = attachmentPreview(attachment) }
                                Button { onRemoveAttachment(attachment) } label: { Image(systemName: "xmark") }
                                    .accessibilityLabel("\(attachment.name)を添付から外す")
                            }.buttonStyle(.borderless).padding(6)
                                .background(.quaternary, in: RoundedRectangle(cornerRadius: 5))
                        }
                    }
                }.frame(maxHeight: 35)
            }
            WorkspaceTextInput(text: $text, onComposition: { isComposing = $0 }, onSend: sendIfAllowed)
                .frame(minHeight: 75, maxHeight: 130)
                .accessibilityLabel("指示を入力。Enterで送信、Shift Enterで改行")
            HStack(spacing: 6) {
                Menu {
                    Button("ファイルを添付…") { onSelectAttachments?() }.disabled(onSelectAttachments == nil)
                } label: { Image(systemName: "plus") }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).frame(width: 24)
                    .accessibilityLabel("添付を追加")
                    .disabled(onSelectAttachments == nil)
                Menu {
                    Text("次の送信に対する希望。実権限はエンジンが判定します。")
                    ForEach(PermissionPreference.allCases, id: \.self) { permission in
                        Button(permission.title) { onSelectPermission(permission) }
                    }
                } label: { Image(systemName: "lock.shield") }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).frame(width: 24)
                    .accessibilityLabel("権限：\(selectedPermission?.title ?? "プロジェクト設定")")
                    .help(selectedPermission?.title ?? "プロジェクトの権限設定を使用")
                Menu {
                    ForEach(models) { option in
                        Button("\(option.title) · \(option.destination)\(option.unavailableReason.map { " · \($0)" } ?? "")") {
                            onSelectModel(option)
                        }.disabled(option.unavailableReason != nil)
                    }
                } label: { Text(selectedModel.title).lineLimit(1) }
                    .menuStyle(.borderlessButton).help("次の要求：\(selectedModel.id) · \(selectedModel.destination) · \(selectedModel.effort)")
                Spacer(minLength: 0)
                Button { onVoice?(phase == .recording ? .stop : .start) } label: {
                    Image(systemName: phase == .recording ? "stop.circle" : "mic")
                }
                .buttonStyle(.borderless).frame(width: 26, height: 28)
                .disabled(onVoice == nil || phase == .transcribing || phase == .sending || deferredVoice != nil)
                .accessibilityLabel(phase == .recording ? "録音を停止" : "音声入力")
                .help(onVoice == nil ? "音声入力は未接続です" : "録音の停止では送信しません")
                if phase == .recording || phase == .transcribing {
                    Button("取消") { onVoice?(.cancel) }.disabled(onVoice == nil)
                }
                if showsSend {
                    Button(action: sendIfAllowed) { Image(systemName: "arrow.up.circle.fill").font(.title2) }
                        .buttonStyle(.borderless).accessibilityLabel("送信").disabled(!canSend)
                }
            }
            if let reason = selectedModel.unavailableReason { note(reason) }
            if !attachments.isEmpty && !selectedModel.supportsAttachments { note("このモデルは添付に対応していません。") }
            if phase != .idle { note(phase.title) }
            if let voiceNote { note(voiceNote) }
            if deferredVoice != nil {
                HStack {
                    Button("認識文を追加") { onApplyDeferredVoice?() }
                        .disabled(onApplyDeferredVoice == nil || isComposing || phase != .idle)
                    Button("認識文を破棄") { onDiscardDeferredVoice?() }.disabled(onDiscardDeferredVoice == nil)
                }
            }
            if let attachmentNotice { note(attachmentNotice) }
        }
        .sheet(item: $inspectedAttachment) { attachment in
            VStack(alignment: .leading, spacing: 16) {
                Text(attachment.name).font(.headline)
                Text(attachment.detail).textSelection(.enabled)
                if let text = attachment.text { Text(text).textSelection(.enabled) }
                Button("閉じる") { inspectedAttachment = nil }.keyboardShortcut(.cancelAction)
            }.padding(24).frame(width: 430)
        }
    }

    private func note(_ text: String) -> some View {
        Text(text).font(.caption).foregroundStyle(.secondary)
    }

    private func sendIfAllowed() {
        guard canSend else { return }
        onSend()
    }
}
