import AppKit
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
    @State private var inputExpanded = false
    @State private var inputOverflows = false
    @State private var modelMenuOpen = false
    private let compactInputHeight: CGFloat = 50

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
            WorkspaceTextInput(text: $text, onComposition: { isComposing = $0 }, onSend: sendIfAllowed,
                               overflowHeight: compactInputHeight, onOverflowChanged: { inputOverflows = $0 })
                .frame(height: inputExpanded ? 180 : compactInputHeight)
                .padding(.trailing, 28)
                .accessibilityLabel("指示を入力。Enterで送信、Shift Enterで改行")
                .overlay(alignment: .topTrailing) {
                    if inputOverflows || inputExpanded {
                        Button { inputExpanded.toggle() } label: {
                            Image(systemName: inputExpanded ? "arrow.down.right.and.arrow.up.left" : "arrow.up.left.and.arrow.down.right")
                                .frame(width: 28, height: 28)
                        }.buttonStyle(.borderless)
                            .accessibilityLabel(inputExpanded ? "入力欄を縮小" : "入力欄を拡大")
                            .help(isComposing ? "文字の変換を確定すると、入力欄の高さを変更できます" : (inputExpanded ? "入力欄を縮小" : "入力欄を拡大"))
                            .disabled(isComposing)
                    }
                }
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
                        Button { onSelectPermission(permission) } label: {
                            Label(permission.title, systemImage: permissionSymbol(permission))
                        }
                    }
                } label: { Image(systemName: permissionSymbol(selectedPermission)) }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).frame(width: 24)
                    .accessibilityLabel("権限：\(selectedPermission?.title ?? "プロジェクト設定")")
                    .help(selectedPermission?.title ?? "プロジェクトの権限設定を使用")
                Spacer(minLength: 0)
                WorkspaceSelectionMenu(
                    title: modelButtonTitle,
                    heading: "Orchestratorモデル",
                    selectedID: selectedModel.id,
                    choices: cloudModels.map { option in
                        WorkspaceSelectionChoice(id: option.id, title: modelDisplayName(option),
                            detail: cloudModelDetail(option), symbol: modelSymbol(option),
                            enabled: option.unavailableReason == nil)
                    },
                    secondaryTitle: "ローカルモデル",
                    secondaryDetail: localModels.isEmpty ? "認識済みモデルはありません" : "認識済み \(localModels.count)件",
                    secondarySymbol: "desktopcomputer",
                    secondaryChoices: localModels.map { option in
                        WorkspaceSelectionChoice(id: option.id, title: option.title,
                            detail: option.destination, symbol: "cpu",
                            enabled: option.unavailableReason == nil)
                    },
                    isOpen: $modelMenuOpen,
                    opensAbove: true,
                    width: 286,
                    onSelect: { id in
                        guard let option = models.first(where: { $0.id == id }) else { return }
                        onSelectModel(option)
                    })
                .buttonStyle(.borderless)
                .accessibilityLabel("Orchestratorモデル：\(modelButtonTitle)")
                .help("次の要求：\(selectedModel.id) · \(selectedModel.destination) · \(selectedModel.effort)")
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
        .onChange(of: text.isEmpty && !isComposing) { shouldCollapse in
            if shouldCollapse { inputExpanded = false }
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

    private var cloudModels: [WorkspaceModelConfiguration] {
        models.filter { !isLocalModel($0) }
    }

    private var localModels: [WorkspaceModelConfiguration] {
        models.filter { isLocalModel($0) && $0.id != "local|unavailable" }
    }

    private func cloudModelDetail(_ option: WorkspaceModelConfiguration) -> String {
        if modelDisplayName(option) == "Astra" { return "\(option.title) · recommend" }
        return [option.title, option.effort, option.unavailableReason]
            .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
    }

    private var modelButtonTitle: String {
        isLocalModel(selectedModel) ? selectedModel.title : modelDisplayName(selectedModel)
    }

    private func isLocalModel(_ option: WorkspaceModelConfiguration) -> Bool {
        let value = (option.id + " " + option.destination).lowercased()
        return value.contains("ollama|") || value.contains("lmstudio|") || value.contains("llamacpp|")
            || value.contains("llama_cpp|")
            || value.hasPrefix("local|") || value.contains(" ollama") || value.contains(" lmstudio")
            || value.contains(" llamacpp")
    }

    private func modelDisplayName(_ option: WorkspaceModelConfiguration) -> String {
        let value = (option.id + " " + option.title).lowercased()
        if value.contains("gpt-6-astra") { return "Astra" }
        if value.contains("gpt-6.1-sol") || value.contains("gpt-5.6-sol") { return "Sol" }
        if value.contains("gpt-5.6-terra") { return "Terra" }
        if value.contains("gpt-6-luna") || value.contains("gpt-5.6-luna") { return "Luna" }
        if value.contains("ollama|") || value.contains("lmstudio|") || value.contains("llamacpp|") || value.contains("local") {
            return "ローカルモデル"
        }
        return option.title
    }

    private func modelSymbol(_ option: WorkspaceModelConfiguration) -> String {
        switch modelDisplayName(option) {
        case "Astra": "sparkles"
        case "Sol": "sun.max"
        case "Terra": "globe.americas"
        case "Luna": "moon"
        case "ローカルモデル": "desktopcomputer"
        default: "cpu"
        }
    }

    private func permissionSymbol(_ permission: PermissionPreference?) -> String {
        switch permission {
        case .readOnly: "eye"
        case .readCreate: "square.and.pencil"
        case .readCreateBuild: "hammer"
        case .readCreateBuildExternal: "globe"
        case nil: "lock.shield"
        }
    }

    private func sendIfAllowed() {
        guard canSend else { return }
        onSend()
    }
}

struct WorkspaceSelectionChoice: Identifiable, Equatable {
    let id: String
    let title: String
    let detail: String
    let symbol: String
    var enabled = true
}

struct WorkspaceSelectionMenu: View {
    let title: String
    let heading: String
    let selectedID: String
    let choices: [WorkspaceSelectionChoice]
    let secondaryTitle: String?
    let secondaryDetail: String
    let secondarySymbol: String
    let secondaryChoices: [WorkspaceSelectionChoice]
    @Binding var isOpen: Bool
    let opensAbove: Bool
    let width: CGFloat
    let onSelect: (String) -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @StateObject private var clickMonitor = WorkspaceSelectionMenuClickMonitor()
    @State private var secondaryOpen = false

    var body: some View {
        Button { setOpen(!isOpen) } label: {
            HStack(spacing: 5) {
                Text(title).lineLimit(1)
                Image(systemName: opensAbove ? "chevron.up" : "chevron.down")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityLabel("\(heading)：\(title)")
        .accessibilityValue(isOpen ? "選択肢を表示中" : "閉じています")
        .background(WorkspaceSelectionMenuHitRegion(id: "button", monitor: clickMonitor))
        .overlay(alignment: opensAbove ? .bottomTrailing : .topTrailing) {
            if isOpen {
                panels
                    .offset(y: opensAbove ? -34 : 34)
                    .transition(.opacity)
            }
        }
        .zIndex(isOpen ? 100 : 0)
        .onExitCommand { setOpen(false) }
        .onChange(of: isOpen) { open in
            if open {
                clickMonitor.start { setOpen(false) }
            } else {
                clickMonitor.stop()
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in
            if isOpen { setOpen(false) }
        }
        .onDisappear { clickMonitor.stop() }
    }

    private var panels: some View {
        primaryPanel
            .overlay(alignment: opensAbove ? .bottomLeading : .topLeading) {
                if secondaryOpen {
                    secondaryPanel
                        .offset(x: width + 8)
                        .transition(.opacity)
                }
            }
    }

    private var primaryPanel: some View {
        primaryPanelContent
            .background(WorkspaceSelectionMenuHitRegion(id: "primary", monitor: clickMonitor))
    }

    private var primaryPanelContent: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(heading)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 10).padding(.top, 7).padding(.bottom, 2)
            ForEach(choices) { choice in
                Button {
                    onSelect(choice.id)
                    setOpen(false)
                } label: {
                    HStack(alignment: .top, spacing: 9) {
                        Image(systemName: choice.symbol)
                            .frame(width: 17)
                            .foregroundStyle(selectedID == choice.id ? Color.accentColor : .secondary)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(choice.title).font(.callout.weight(.medium)).foregroundStyle(.primary)
                            Text(choice.detail).font(.caption2).foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer(minLength: 8)
                        if selectedID == choice.id {
                            Image(systemName: "checkmark")
                                .font(.caption.weight(.semibold)).foregroundStyle(Color.accentColor)
                        }
                    }
                    .padding(.horizontal, 10).padding(.vertical, 7)
                    .contentShape(Rectangle())
                }
                .buttonStyle(WorkspaceSelectionRowStyle(selected: selectedID == choice.id))
                .disabled(!choice.enabled)
            }
            if let secondaryTitle {
                Button {
                    if reduceMotion {
                        secondaryOpen.toggle()
                    } else {
                        withAnimation(.easeOut(duration: 0.08)) { secondaryOpen.toggle() }
                    }
                } label: {
                    HStack(alignment: .top, spacing: 9) {
                        Image(systemName: secondarySymbol)
                            .frame(width: 17)
                            .foregroundStyle(secondarySelection ? Color.accentColor : .secondary)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(secondaryTitle).font(.callout.weight(.medium)).foregroundStyle(.primary)
                            Text(secondaryDetail).font(.caption2).foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 8)
                        Image(systemName: secondaryOpen ? "chevron.left" : "chevron.right")
                            .font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
                    }
                    .padding(.horizontal, 10).padding(.vertical, 7)
                    .contentShape(Rectangle())
                }
                .buttonStyle(WorkspaceSelectionRowStyle(selected: secondarySelection))
            }
        }
        .padding(.bottom, 5)
        .frame(width: width)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
        .shadow(color: .black.opacity(0.28), radius: 18, y: 7)
    }

    private var secondaryPanel: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(secondaryTitle ?? "ローカルモデル")
                .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                .padding(.horizontal, 10).padding(.top, 7).padding(.bottom, 2)
            if secondaryChoices.isEmpty {
                Text("認識済みモデルはありません")
                    .font(.caption).foregroundStyle(.secondary)
                    .padding(.horizontal, 10).padding(.vertical, 9)
            } else {
                ForEach(secondaryChoices) { choice in
                    Button {
                        onSelect(choice.id)
                        setOpen(false)
                    } label: {
                        HStack(alignment: .top, spacing: 9) {
                            Image(systemName: choice.symbol)
                                .frame(width: 17)
                                .foregroundStyle(selectedID == choice.id ? Color.accentColor : .secondary)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(choice.title).font(.callout.weight(.medium)).foregroundStyle(.primary)
                                Text(choice.detail).font(.caption2).foregroundStyle(.secondary)
                                    .lineLimit(2).truncationMode(.middle)
                            }
                            Spacer(minLength: 8)
                            if selectedID == choice.id {
                                Image(systemName: "checkmark")
                                    .font(.caption.weight(.semibold)).foregroundStyle(Color.accentColor)
                            }
                        }
                        .padding(.horizontal, 10).padding(.vertical, 7)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(WorkspaceSelectionRowStyle(selected: selectedID == choice.id))
                    .disabled(!choice.enabled)
                }
            }
        }
        .padding(.bottom, 5)
        .frame(width: width)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
        .shadow(color: .black.opacity(0.28), radius: 18, y: 7)
        .background(WorkspaceSelectionMenuHitRegion(id: "secondary", monitor: clickMonitor))
        .transition(.opacity)
    }

    private var secondarySelection: Bool {
        secondaryChoices.contains { $0.id == selectedID }
    }

    private func setOpen(_ open: Bool) {
        if reduceMotion {
            isOpen = open
        } else {
            withAnimation(.easeOut(duration: 0.10)) { isOpen = open }
        }
        if !open { secondaryOpen = false }
    }
}

@MainActor
private final class WorkspaceSelectionMenuClickMonitor: ObservableObject {
    private struct Region {
        let windowNumber: Int
        let frame: NSRect
    }

    private var regions: [String: Region] = [:]
    private var eventMonitor: Any?
    private var onOutsideClick: (() -> Void)?

    func start(onOutsideClick: @escaping () -> Void) {
        stop()
        self.onOutsideClick = onOutsideClick
        eventMonitor = NSEvent.addLocalMonitorForEvents(
            matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown]
        ) { [weak self] event in
            guard let self else { return event }
            let inside = self.regions.values.contains { region in
                region.windowNumber == event.windowNumber && region.frame.contains(event.locationInWindow)
            }
            if !inside {
                let action = self.onOutsideClick
                DispatchQueue.main.async { action?() }
            }
            return event
        }
    }

    func stop() {
        if let eventMonitor { NSEvent.removeMonitor(eventMonitor) }
        eventMonitor = nil
        onOutsideClick = nil
    }

    func update(id: String, windowNumber: Int, frame: NSRect) {
        regions[id] = Region(windowNumber: windowNumber, frame: frame)
    }

    func remove(id: String) {
        regions.removeValue(forKey: id)
    }
}

private struct WorkspaceSelectionMenuHitRegion: NSViewRepresentable {
    let id: String
    let monitor: WorkspaceSelectionMenuClickMonitor

    func makeNSView(context: Context) -> RegionView {
        RegionView(id: id, monitor: monitor)
    }

    func updateNSView(_ nsView: RegionView, context: Context) {
        nsView.publishFrame()
    }

    static func dismantleNSView(_ nsView: RegionView, coordinator: ()) {
        nsView.monitor.remove(id: nsView.id)
    }

    final class RegionView: NSView {
        let id: String
        let monitor: WorkspaceSelectionMenuClickMonitor

        init(id: String, monitor: WorkspaceSelectionMenuClickMonitor) {
            self.id = id
            self.monitor = monitor
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { nil }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            publishFrame()
        }

        override func layout() {
            super.layout()
            publishFrame()
        }

        func publishFrame() {
            guard let window else {
                monitor.remove(id: id)
                return
            }
            monitor.update(id: id, windowNumber: window.windowNumber, frame: convert(bounds, to: nil))
        }
    }
}

private struct WorkspaceSelectionRowStyle: ButtonStyle {
    let selected: Bool
    @State private var hovered = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                selected || configuration.isPressed || hovered
                    ? Color.primary.opacity(selected ? 0.075 : 0.055)
                    : .clear,
                in: RoundedRectangle(cornerRadius: 7)
            )
            .padding(.horizontal, 5)
            .onHover { hovered = $0 }
    }
}
