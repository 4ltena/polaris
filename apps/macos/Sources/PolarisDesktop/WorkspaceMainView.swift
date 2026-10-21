import AppKit
import SwiftUI
import PolarisSettings

/// Actual entry uses only persisted selection and service replies. The empty
/// display contract carries no model, conversation, file or git fixture values.
struct WorkspaceMainView: View {
    @ObservedObject var model: DesktopModel
    @ObservedObject var owner: DesktopWorkspaceOwner
    let openDemo: () -> Void
    let openServiceDemo: () -> Void
    @StateObject private var state = WorkspaceState()
    @State private var showsMcp = false
    private let empty = WorkspaceSnapshot(isDemo: false, projects: [], sections: [], conversations: [], models: [], activities: [], quota: nil)

    var body: some View {
        if let service = owner.service {
            ConnectedWorkspaceView(model: model, owner: owner, service: service,
                                   state: state, sidebar: AnyView(sidebar))
        } else {
            WorkspaceView(snapshot: empty, state: state, logo: model.assets["polaris-banner-white"],
                          release: model.release, onAction: { _ in }, onSend: { _ in .rejected("推論は未接続です") },
                          nativeSidebar: AnyView(sidebar), nativeConversation: AnyView(conversation),
                          nativeActivity: AnyView(activity))
        }
    }
    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("プロジェクト").font(.headline)
            if let binding = owner.binding {
                Label(URL(fileURLWithPath: binding.sourcePath).lastPathComponent, systemImage: "folder")
                Text(binding.sourcePath).font(.caption).textSelection(.enabled)
                Button("フォルダを開く") { NSWorkspace.shared.open(URL(fileURLWithPath: binding.sourcePath)) }
                Label("保存済みの会話", systemImage: "bubble.left")
                if let service = owner.service, service.canManageMcp {
                    Button("MCPサーバーを管理") { showsMcp = true }
                        .sheet(isPresented: $showsMcp) { McpSettingsView(model: McpSettingsModel(service: service)) }
                }
            } else {
                Text("プロジェクトは未選択です").foregroundStyle(.secondary)
                Button("設定でフォルダを選択") { model.reopen() }
            }
            Spacer()
            Button("設定を編集") { model.reopen() }
            Menu("検証用デモ") {
                Button("作業画面のデモを開く", action: openDemo)
                Button("ローカル通信を検証する", action: openServiceDemo)
            }
            WorkspaceQuotaWidget(quota: nil)
        }.padding(14).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
    @ViewBuilder private var conversation: some View {
        if let service = owner.service {
            NativeConversationView(model: model, service: service, owner: owner,
                                   reconnect: { await model.reconnectWorkspace() },
                                   startExecution: { await model.startConfiguredExecution() })
        } else {
            VStack(spacing: 16) {
                Text(owner.error ?? "フォルダを明示的に選択すると、保存済みの会話を開きます。")
                    .foregroundStyle(.secondary)
                if owner.binding == nil { Button("設定を開く") { model.reopen() } }
                else {
                    Button("同じ保存先へ再接続") { Task { await model.reconnectWorkspace() } }
                        .disabled(owner.isRecovering)
                }
            }.padding().frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
    private var activity: some View {
        HomeBrowseView(model: owner.home)
    }
}

private struct ConnectedWorkspaceView: View {
    @ObservedObject var model: DesktopModel
    @ObservedObject var owner: DesktopWorkspaceOwner
    @ObservedObject var service: WorkspaceServiceModel
    @ObservedObject var state: WorkspaceState
    let sidebar: AnyView
    @StateObject private var source = WorkspaceSourceModel()
    @State private var sourceLoading = false
    @State private var showsHome = false
    private var snapshot: WorkspaceSnapshot {
        NativeWorkspaceSnapshot.make(binding: owner.binding, payload: service.snapshot,
                                     files: source.files, git: source.git, orchestration: service.orchestration)
    }
    private var selectedPath: String? {
        guard let id = owner.binding?.sessionID else { return nil }
        return state.fileSelections[id]?.selectedID
    }
    private var sourceUnavailableReason: String? {
        service.workspaceReadAvailable ? source.unavailableReason
            : "保存した設定を確認して接続すると、ファイルとGit情報を表示します。"
    }
    var body: some View {
        WorkspaceView(snapshot: snapshot, state: state, logo: model.assets["polaris-banner-white"],
            release: model.release, onAction: { _ in }, onSend: { _ in .rejected("会話の入力欄から送信してください") },
            nativeSidebar: sidebar,
            nativeConversation: AnyView(NativeConversationView(model: model, service: service, owner: owner,
                reconnect: { await model.reconnectWorkspace() }, startExecution: { await model.startConfiguredExecution() })),
            nativeActivity: AnyView(VStack(spacing: 0) {
                HStack {
                    Picker("表示する場所", selection: $showsHome) {
                        Text("プロジェクト").tag(false)
                        Text("ホーム").tag(true)
                    }.pickerStyle(.segmented).labelsHidden().frame(width: 180)
                    if !showsHome {
                        Text(sourceUnavailableReason ?? source.notices.first ?? "選択したプロジェクトのファイル")
                            .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        Spacer()
                        Button("ファイル・Gitを更新") { Task { await reloadSource() } }
                            .disabled(service.phase != .ready || service.isBusy || !service.workspaceReadAvailable)
                    } else { Spacer() }
                }.padding(.horizontal, 12).padding(.top, 6)
                if showsHome {
                    HomeBrowseView(model: owner.home)
                } else {
                    WorkspaceActivityView(activity: snapshot.activity(for: owner.binding?.sessionID ?? ""), state: state)
                }
            }), sourceUnavailableReason: sourceUnavailableReason)
        .task {
            state.selectedConversationID = owner.binding?.sessionID
            await reloadSource()
        }
        .onChange(of: selectedPath) { _ in Task { await reloadSource() } }
        .onChange(of: service.workspaceReadAvailable) { _ in Task { await reloadSource() } }
    }
    private func reloadSource() async {
        guard service.phase == .ready, service.workspaceReadAvailable, !service.isBusy, !sourceLoading else { return }
        sourceLoading = true
        defer { sourceLoading = false }
        let generation = source.beginReload()
        do {
            let payload = try await service.readWorkspace(selectedPath: selectedPath)
            source.apply(try WorkspaceSourceReader.jsonPayload(payload), generation: generation)
        } catch { source.fail(generation: generation) }
    }
}

private struct NativeConversationView: View {
    @ObservedObject var model: DesktopModel
    @ObservedObject var service: WorkspaceServiceModel
    @ObservedObject var owner: DesktopWorkspaceOwner
    let reconnect: @MainActor () async -> Void
    let startExecution: @MainActor () async -> Void
    @StateObject private var voice = WorkspaceState()
    @State private var isComposing = false
    @State private var executionDetailsExpanded = false
    @State private var importing = false
    @State private var inputNotice: String?
    @State private var inventories: [LocalInventory] = []
    @State private var localSettingsOpen = false
    @State private var localRefreshing = false
    @State private var roleDraft: [LocalRoleBinding] = []
    @State private var roleRevision: UInt64 = 0
    @State private var clarificationText: [String: String] = [:]
    private var modelOptions: [WorkspaceModelConfiguration] {
        let cloud = [
            WorkspaceModelConfiguration(id: "codex|gpt-6-astra", title: "gpt-6-astra", destination: "codex", effort: "medium", unavailableReason: nil, supportsAttachments: true),
            WorkspaceModelConfiguration(id: "codex|gpt-6.1-sol", title: "gpt-6.1-sol", destination: "codex", effort: "medium", unavailableReason: nil, supportsAttachments: true),
            WorkspaceModelConfiguration(id: "codex|gpt-6-luna", title: "gpt-6-luna", destination: "codex", effort: "medium", unavailableReason: nil, supportsAttachments: true)
        ]
        let local = inventories.flatMap { inventory in
            inventory.models.map { observed in
                WorkspaceModelConfiguration(id: inventory.provider.rawValue + "|" + inventory.endpoint + "|" + observed.id,
                    title: observed.id, destination: inventory.provider.rawValue + " · " + inventory.endpoint,
                    effort: "ランタイム設定", unavailableReason: observed.executionLocation == "remote" || observed.completion == .unsupported ? "主会話に対応していません" : nil,
                    supportsAttachments: true)
            }
        }
        let localFallback = local.isEmpty
            ? [WorkspaceModelConfiguration(id: "local|unavailable", title: "ローカルモデル", destination: "未検出", effort: "ランタイム設定",
                unavailableReason: "ローカルモデルを検出してください", supportsAttachments: true)]
            : []
        return [currentModel] + (cloud + local + localFallback).filter { $0.id != currentModel.id && $0.title != currentModel.title }
    }
    private var inputPhase: WorkspaceInputPhase {
        if service.isBusy || service.startOutcomeUnknown { return .sending }
        return voice.voicePhases[service.activeSessionID] ?? .idle
    }
    private var currentModel: WorkspaceModelConfiguration { NativeWorkspaceSnapshot.selectedModel(model.document.preferences?.executionBinding, fallback: service.snapshot?["configuration"]) }
    private func recoveryTitle(_ state: SourceRecoveryClient.State) -> String {
        switch state {
        case .working: return "処理中"
        case .reportPending: return "結果の保存待ち"
        case .recoveryRequired: return "復旧確認が必要"
        case .readyToExit: return "終了準備完了（実終了は未確認）"
        }
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("会話").font(.headline)
            if !service.approvals.isEmpty || service.sourceApplyCandidates.contains(where: { !$0.invalidated && $0.decision == nil && !$0.intentCommitted && !$0.isExpired() }) {
                Button("確認が必要な操作があります・表示する") { executionDetailsExpanded = true }
            }
            conversationHistory
            if let error = owner.error { Text(error).font(.caption).foregroundStyle(.red) }
            if service.phase == .ready, !service.executionAvailable || service.executionNeedsRefresh {
                Button("保存した設定を確認して接続") { Task { await startExecution() } }
                    .disabled(service.isBusy || owner.isRecovering)
            }
            if service.canReconnect {
                Button("同じ保存先へ再接続") { Task { await reconnect() } }.disabled(owner.isRecovering)
            }
            if service.recoveryAvailable {
                if let status = service.recoveryStatus {
                    Text("結果保存：" + recoveryTitle(status.state)).font(.caption)
                    if let error = status.error { Text("応答：" + error.rawValue).font(.caption2) }
                }
                if service.recoveryAwaitingProof { Text("結果の保存証明が未確認です。終了・置換は保留しています。").font(.caption) }
                if service.recoveryOutcomeUnknown {
                    Text("応答が未確認です。元の要求だけを照合できます。").font(.caption)
                } else if service.recoveryFailure {
                    Text("結果保存は完了していません。同じ要求で再試行できます。").font(.caption)
                }
                if let id = service.recoveryRequestID { Text("要求 ID：" + id).font(.caption2).textSelection(.enabled) }
                ForEach(service.savedRecoveryResults.map { (key: Data($0.operationID.utf8), result: $0) }, id: \.key) { result in
                    Text(recoveryResultTitle(result.result))
                        .font(.caption2).textSelection(.enabled)
                }
                HStack {
                    Button("結果保存の状態を確認") { Task { await owner.checkRecoveryStatus() } }
                        .disabled(!service.canCheckRecovery || service.canReconcileRecovery)
                    if service.canReconcileRecovery {
                        Button(service.recoveryOutcomeUnknown ? "元の回復要求を照合" : "同じ回復要求で再試行") {
                            Task { await owner.reconcileRecoveryRequest() }
                        }
                    } else {
                        Button("保持された結果を保存") { Task { await owner.retryRecoveryResult() } }
                            .disabled(!service.canRetryRecovery)
                    }
                    Button("サービスの終了を確認") { Task { await owner.observeRecoveryExit() } }
                        .disabled(service.isBusy)
                }.disabled(owner.isRecovering)
                if service.recoveryHasExited { Text("プロセス終了を確認済み。保存証明と下書きは別に確認します。").font(.caption) }
            }
            if service.draftOutcomeUnknown {
                Text("下書きの保存成否が不明です。本文を保持しています。再送は行いません。")
                    .font(.caption).foregroundStyle(.red)
                if let requestID = service.lastDraftRequestID {
                    Text("要求 ID：\(requestID)").font(.caption2).textSelection(.enabled)
                }
                Button("元の保存要求を照合") { Task { await service.reconcileDraft() } }
                    .disabled(service.phase != .ready || service.isBusy)
                if let status = service.requestStatus {
                    Text(statusTitle(status)).font(.caption).foregroundStyle(.secondary)
                }
            } else if service.draftConflict {
                Text("外部の保存と競合しています。本文を確認してください。").font(.caption)
                HStack {
                    Button("保存状態を再読込") { Task { await service.refresh() } }
                    Button("保存済み本文を採用") { service.useObservedDraft() }
                }.disabled(service.isBusy)
            } else if service.failure != nil {
                Text(serviceFailureTitle)
                    .font(.caption).foregroundStyle(.red)
                if service.phase == .ready {
                    Button("実行状態を再読込") { Task { await service.refresh() } }.disabled(service.isBusy)
                }
            }
            if !service.attachmentIDs.isEmpty {
                Text("保存済み添付 \(service.attachmentIDs.count) 件を次の入力へ含めます。").font(.caption).foregroundStyle(.secondary)
            }
            HStack {
                Button("ローカルモデル・役割設定") {
                    roleDraft = service.observedRoleBindings
                    roleRevision = service.snapshot?["configuration"]?["configuration_revision"]?.decimal ?? 0
                    localSettingsOpen = true
                }.disabled(service.phase != .ready)
                if service.pendingRoleConfiguration != nil {
                    Button("役割設定の保存を照合") { Task { await service.reconcileRoleConfiguration() } }.disabled(service.isBusy)
                    Button("現在の役割設定を採用") { Task { await service.adoptObservedRoleConfiguration() } }.disabled(service.isBusy)
                }
            }
            if let notice = service.roleConfigurationNotice { Text(notice).font(.caption).foregroundStyle(.secondary) }
            if let error = model.saveError { Text(error).font(.caption).foregroundStyle(.red) }
            if service.runs.contains(where: { !$0.isTerminal }) {
                HStack(spacing: 8) {
                    PolarisThinkingIndicator()
                    Text("Thinking").font(.callout.weight(.medium)).foregroundStyle(.secondary)
                    Text("orchestrator").font(.caption).foregroundStyle(.tertiary)
                    Text(currentModel.title).font(.caption).foregroundStyle(.secondary)
                    Text(service.isJudging ? "effort judging" : currentModel.effort)
                        .font(.caption.monospaced().weight(.medium))
                        .foregroundStyle(service.isJudging ? Color.primary : .secondary)
                }
                .padding(.vertical, 2)
                .accessibilityElement(children: .combine)
                .accessibilityLabel(service.isJudging
                    ? "Thinking、orchestrator、\(currentModel.title)、effort judging"
                    : "Thinking、orchestrator、\(currentModel.title)、effort \(currentModel.effort)")
            }
            if let admission = service.inputAdmission {
                Text(inputAdmissionTitle(admission))
                    .font(.caption).foregroundStyle(admission.state == .outcomeUnknown ? .red : .secondary)
                Text(inputAdmissionDetails(admission))
                    .font(.caption2).foregroundStyle(.secondary).textSelection(.enabled)
                    .accessibilityLabel("入力の経路。\(inputAdmissionDetails(admission))")
                if admission.state != .completed && admission.state != .failed && admission.state != .cancelled {
                    Button("この入力を取り消す") { Task { await service.cancelPendingInput() } }
                        .disabled(service.isBusy || service.orchestration == nil)
                        .accessibilityLabel("現在の入力を取り消す")
                }
                if service.inputAdmissionOutcomeUnknown {
                    Text("入力の受付成否は未確認です。本文を保持し、同じ内容は再送しません。")
                        .font(.caption).foregroundStyle(.red)
                    Button("入力受付の状態を確認") { Task { await service.refreshInputAdmissionStatus() } }
                        .disabled(service.isBusy || service.phase != .ready)
                }
            }
            if let manager = service.orchestration?.sessionManager, manager.sessions.count > 1 {
                sessionStatus(manager)
            }
            composer
            HStack {
                Text(service.isDirty ? "未保存" : (service.savedDraftRevision == nil ? "未読込" : "保存済み")).font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("下書きを保存") { Task { await service.saveDraft() } }.disabled(!service.canSaveDraft || importing)
            }
        }.padding(16)
        .onAppear {
            voice.selectedConversationID = service.activeSessionID
            voice.enableVoice(backend: WorkspaceNativeSpeechBackend(), appendText: { id, text in
                guard id == service.activeSessionID else { return }
                service.editDraft(service.draftText + (service.draftText.isEmpty ? "" : "\n") + text)
            })
        }
        .onChange(of: service.activeSessionID) { sessionID in
            isComposing = false
            voice.selectedConversationID = sessionID
        }

        .onDisappear { voice.cancelVoice(); isComposing = false }
        .popover(isPresented: $localSettingsOpen) {
            ScrollView {
                LocalModelSettingsView(inventories: inventories, roles: service.observedRoleCatalog, bindings: roleDraft,
                    isRefreshing: localRefreshing, isSaving: service.isBusy || service.pendingRoleConfiguration != nil,
                    refresh: { provider, endpoint in
                        // One explicit click queries both defaults in a single owned sequence.
                        if provider == .ollama { Task { await refreshLocalModels() } }
                    }, selectMain: { inventory, observed in
                        if let selection = try? ExecutionBinding(provider: inventory.provider, model: observed.id, effort: nil, localEndpoint: inventory.endpoint) {
                            localSettingsOpen = false
                            Task { await model.selectWorkspaceExecution(selection) }
                        }
                    }, assign: { role, inventory, observed in
                        guard let selection = try? ExecutionBinding(provider: inventory.provider, model: observed.id, effort: nil, localEndpoint: inventory.endpoint),
                              let binding = try? LocalRoleBinding(role: role, selection: selection,
                                observedToolSupport: ObservedToolSupport(rawValue: observed.tools.rawValue) ?? .unknown) else { return }
                        roleDraft.removeAll { $0.role == role }; roleDraft.append(binding)
                    }, clear: { role in roleDraft.removeAll { $0.role == role } },
                    save: { Task {
                        await service.saveRoleBindings(roleDraft, revision: roleRevision)
                        if service.pendingRoleConfiguration == nil, service.observedRoleBindings == roleDraft.sorted(by: { $0.role < $1.role }) {
                            roleRevision = service.snapshot?["configuration"]?["configuration_revision"]?.decimal ?? roleRevision
                        }
                    } })
                if service.observedRoleCatalog.isEmpty { Text("実行接続後に役割一覧を表示します。").font(.caption).foregroundStyle(.secondary) }
                Text("検出後は入力欄のモデルメニューから主会話のモデルも選択できます。").font(.caption)
                Button("閉じる") { localSettingsOpen = false }
            }.padding(18).frame(width: 430, height: 430)
        }
    }
    private var conversationHistory: some View {
        ScrollViewReader { proxy in
            ScrollView(.vertical, showsIndicators: true) {
                conversationHistoryContent
            }
            .onAppear { _ = Task<Void, Never> { await service.catchUpHistory() } }
            .onChange(of: service.snapshot?["session_revision"]?.decimal) { _ in
                _ = Task<Void, Never> { await service.catchUpHistory() }
            }
            .onChange(of: executionDetailsExpanded) { expanded in
                if expanded { proxy.scrollTo("execution-details", anchor: .top) }
            }
            .onChange(of: service.messages.last?.id) { _ in
                proxy.scrollTo("latest-message", anchor: .bottom)
            }
            .onChange(of: service.streamMessages.last?.text) { _ in
                proxy.scrollTo("latest-message", anchor: .bottom)
            }
            .onChange(of: service.runs.last(where: { !$0.isTerminal })?.id) { _ in
                proxy.scrollTo("latest-message", anchor: .bottom)
            }
        }
    }
    private var conversationHistoryContent: some View {
                VStack(alignment: .leading, spacing: 16) {
                    if service.nextHistoryCursor != nil {
                        Text("会話履歴を読み込んでいます。最新の本文はまだ表示されていません。")
                            .font(.caption).foregroundStyle(.secondary)
                        Button("履歴の読込みを続ける") { Task { await service.catchUpHistory() } }
                            .disabled(service.isBusy || service.phase != .ready)
                    }
                    DisclosureGroup("実行状況・変更の確認", isExpanded: $executionDetailsExpanded) {
                        conversationExecutionDetails
                    }.id("execution-details")
                    ForEach(service.messages.filter { history in !service.streamMessages.contains { $0.id.utf8.elementsEqual(history.id.utf8) } }, id: \.key) { message in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(role(message.role)).font(.caption).foregroundStyle(.secondary)
                            Text(message.text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                                .foregroundStyle(message.role == .user || message.role == .assistant ? Color.primary : .secondary)
                            if service.isContextJudging && message.role == .user
                                && message.id == service.messages.last(where: { $0.role == .user })?.id {
                                Text("context judging")
                                    .font(.caption.monospaced().weight(.medium))
                                    .foregroundStyle(.secondary)
                                    .accessibilityLabel("現在の文脈との関連を判定中")
                            }
                        }
                    }
                    ForEach(service.retainedHistory.filter { history in !service.streamMessages.contains { $0.id.utf8.elementsEqual(history.id.utf8) } }, id: \.key) { message in
                        VStack(alignment: .leading, spacing: 6) {
                            Text("前回確認した履歴（更新後は未確認）").font(.caption).foregroundStyle(.secondary)
                            Text(message.text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                                .foregroundStyle(message.role == .user || message.role == .assistant ? Color.primary : .secondary)
                        }
                    }
                    ForEach(service.streamMessages, id: \.key) { message in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(service.streamHistoryConflict ? "受信本文（履歴との照合不一致）" : (message.saved ? "受信本文（保存済み）" : "受信中（未保存）")).font(.caption).foregroundStyle(.secondary)
                            Text(message.text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    Color.clear.frame(height: 1).id("latest-message")

                }
    }
    /// Saved text shown as plain selectable text; tool data is never run or linked.
    @ViewBuilder private func runDetailsView(_ run: WorkspaceRun) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            if let details = service.runDetails {
                if details.historyGap { Text("この実行の記録には欠落があります。").font(.caption).foregroundStyle(.orange) }
                if !run.isTerminal { Text("実行中です。表示は保存済みの部分だけです。").font(.caption).foregroundStyle(.secondary) }
                if service.runDetailsOutdated(run) {
                    Text("実行状態が更新されています。読み込み直すと最新の保存内容を表示します。").font(.caption).foregroundStyle(.secondary)
                }
                if details.entries.count <= 1 && run.state == "cancelled" {
                    Text("保存された実行ログはありません。").font(.caption).foregroundStyle(.secondary)
                }
                ForEach(details.entries) { entry in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(entry.title + (entry.complete ? "" : "（続きあり）")).font(.caption2).foregroundStyle(.secondary)
                        Text(entry.text).font(entry.isPrimary ? .body : .caption.monospaced())
                            .foregroundStyle(entry.isPrimary ? Color.primary : .secondary)
                            .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                if details.nextCursor != nil {
                    Button("続きを読み込む") { Task { await service.continueRunDetails() } }
                }
            } else if service.runDetailsNotice == nil {
                Text("実行内容を読み込んでいます。").font(.caption).foregroundStyle(.secondary)
            }
            if let notice = service.runDetailsNotice {
                Text(notice).font(.caption).foregroundStyle(.secondary)
            }
            if service.runDetails != nil || service.runDetailsNotice != nil {
                Button("読み込み直す") { Task { await service.openRunDetails(run) } }
                    .disabled(!service.canReadRunDetails)
            }
        }
        .padding(.leading, 12)
    }

    private var conversationExecutionDetails: some View {
        VStack(alignment: .leading, spacing: 12) {
                    ForEach(service.runs, id: \.key) { run in
                        HStack {
                            Text("実行：" + run.title).font(.caption).foregroundStyle(.secondary)
                            if service.cancelRequested.contains(run.key) { Text("取消要求済み・終了待ち").font(.caption).foregroundStyle(.secondary) }
                            Spacer()
                            if service.expandedRunKey == run.key {
                                Button("内容を閉じる") { service.closeRunDetails() }
                            } else if service.canReadRunDetails {
                                Button("内容を表示") { Task { await service.openRunDetails(run) } }
                            }
                            if !run.isTerminal {
                                Button("実行を取り消す") { Task { await service.cancelRun(run) } }
                                    .disabled(!service.canCancel(run))
                            }
                        }
                        if service.expandedRunKey == run.key {
                            runDetailsView(run)
                        }
                    }
                    if !service.decisionStatus.isEmpty {
                        DisclosureGroup("型付き判断の状態（snapshot取得時）") {
                            VStack(alignment: .leading, spacing: 4) {
                                ForEach(Array(service.decisionStatus.enumerated()), id: \.offset) { _, line in
                                    Text(line).font(.caption).textSelection(.enabled)
                                }
                            }.frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    if let memory = service.memoryStatus {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("記憶の準備：\(memory.phaseTitle) · 実ターン \(memory.recentRawTurns) · 出典 \(memory.retrievalSources.count)件 · 参照 \(memory.referenceTokens) tokens").font(.caption)
                            Text(memory.detail).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                            if let omission = memory.relatedOmissionDetail {
                                Text(omission).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                            }
                            ForEach(memory.retrievalSources, id: \.self) { source in
                                Text(source).font(.caption2).foregroundStyle(.secondary).textSelection(.enabled)
                            }
                            if let usage = memory.summaryUsage { Text("要約：\(usage.inputTokens)/\(usage.outputTokens) tokens・欠測 \(usage.missingResponses)・失敗 \(usage.failedRequests)").font(.caption2) }
                            else { Text("要約：未取得").font(.caption2).foregroundStyle(.secondary) }
                            if let usage = memory.mainUsage { Text("主会話：\(usage.inputTokens)/\(usage.outputTokens) tokens・キャッシュ \(usage.cachedTokens)・欠測 \(usage.missingResponses)・失敗 \(usage.failedRequests)").font(.caption2) }
                            else { Text("主会話：未取得").font(.caption2).foregroundStyle(.secondary) }
                            if let usage = memory.embeddingUsage { Text("ローカル埋め込み：要求 \(usage.requests)・完了 \(usage.completed)・失敗 \(usage.failed)・不明 \(usage.unknown)・入力 \(usage.inputTokens)").font(.caption2) }
                            else { Text("ローカル埋め込み：未取得").font(.caption2).foregroundStyle(.secondary) }
                            if let usage = memory.totalUsage { Text("主会話＋要約：\(usage.inputTokens)/\(usage.outputTokens) tokens・キャッシュ \(usage.cachedTokens)・欠測 \(usage.missingResponses)・失敗 \(usage.failedRequests)").font(.caption2) }
                            else { Text("合計：未取得").font(.caption2).foregroundStyle(.secondary) }
                        }
                    }
                    if service.startOutcomeUnknown {
                        Text("開始要求の成否が不明です。再送せず、元の要求を照合してください。")
                            .font(.caption).foregroundStyle(.red)
                        if let id = service.lastStartRequestID { Text("要求 ID：" + id).font(.caption2).textSelection(.enabled) }
                        Button("元の開始要求を照合") { Task { await service.reconcileStart() } }
                            .disabled(service.phase != .ready || service.isBusy)
                    }
                    if let orchestration = service.orchestration {
                        controlPlaneStatus(orchestration)
                        clarificationCards(orchestration.clarification)
                    }
                    ForEach(service.approvals, id: \.key) { approval in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(approval.title).font(.headline)
                            Text(approval.detail).textSelection(.enabled)
                            Text(approval.scope).font(.caption).textSelection(.enabled)
                            HStack {
                                if approval.choices.contains(.allow) {
                                    Button("許可") { Task { await service.resolveApproval(approval, decision: .allow) } }
                                }
                                if approval.choices.contains(.deny) {
                                    Button("拒否") { Task { await service.resolveApproval(approval, decision: .deny) } }
                                }
                            }.disabled(!service.canResolve(approval))
                            if service.pendingApprovalAnswers.contains(approval.key) {
                                Text("回答の確認待ちです。再送は行いません。").font(.caption)
                            }
                        }
                    }
                    SourceApplyReviewView(service: service)
        }
    }
    @ViewBuilder private func sessionStatus(_ manager: WorkspaceSessionManagerProjection) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(sidechatTitle(manager.sidechat)).font(.caption).foregroundStyle(.secondary)
            if manager.sessions.count > 1 {
                Picker("表示する会話", selection: Binding(get: { service.activeSessionID }, set: { id in
                    guard id != service.activeSessionID else { return }
                    Task { await service.selectManagedSession(id) }
                })) {
                    ForEach(manager.sessions) { session in
                        Text(sessionDisplayName(session)).tag(session.id)
                    }
                }
                .pickerStyle(.menu)
                .disabled(!service.canSelectManagedSession)
                .accessibilityLabel("表示する会話")
                Text(service.canSelectManagedSession ? "会話を選ぶと、保存済みの会話を表示します。" : "この接続では会話切替を利用できません。")
                    .font(.caption2).foregroundStyle(.secondary)
            }
        }
    }
    @ViewBuilder private func clarificationCards(_ clarification: WorkspaceClarification) -> some View {
        if clarification.state == .pending {
            VStack(alignment: .leading, spacing: 8) {
                Text("確認したいことがあります").font(.caption.weight(.medium))
                ForEach(clarification.questions.filter { !$0.answered }) { question in
                    VStack(alignment: .leading, spacing: 5) {
                        Text(question.text).font(.caption)
                        ForEach(question.options) { option in
                            Button(option.label) { submitClarification(question, answer: option.label) }
                                .disabled(service.isBusy)
                                .accessibilityLabel("\(question.text)。\(option.label) を選ぶ")
                        }
                        if question.freeText.allowed {
                            TextField(question.freeText.required ? "回答を入力（必須）" : "補足を入力（任意）",
                                      text: Binding(get: { clarificationText[question.id, default: ""] }, set: { clarificationText[question.id] = String($0.utf8.prefix(Int(question.freeText.maxBytes))) }))
                                .accessibilityLabel("確認事項 \(question.id)：\(question.text)")
                            Button("回答を送る") { submitClarification(question, answer: clarificationText[question.id, default: ""]) }
                                .disabled(service.isBusy || (question.freeText.required && clarificationText[question.id, default: ""].isEmpty))
                        }
                    }.padding(8).background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
                }
            }
        }
    }
    @ViewBuilder private func controlPlaneStatus(_ orchestration: WorkspaceOrchestration) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            if !orchestration.subagents.isEmpty {
                Text("協力中の作業").font(.caption.weight(.medium))
                ForEach(orchestration.subagents.filter { [.queued, .running, .waiting, .cancelling, .outcomeUnknown].contains($0.state) }) { child in
                    VStack(alignment: .leading, spacing: 3) {
                        Text(subagentDisplayTitle(child))
                            .font(.caption2).foregroundStyle(.secondary)
                            .accessibilityLabel(subagentAccessibilityLabel(child))
                            .accessibilityHint(child.state == .outcomeUnknown ? "子作業の実行結果は未確認です。" : "子作業の現在の保存済み状態です。")
                        if child.state == .outcomeUnknown {
                            Text("子作業の結果は未確認です。更新しても確定できない場合があります。")
                                .font(.caption2).foregroundStyle(.orange)
                        }
                        if let result = service.subagentCancelResult,
                           result.logicalChildID == child.logicalChildID, result.childAttemptID == child.id {
                            if result.runtimeEffect == .unavailable {
                                Text("停止要求は記録されましたが、実行環境へは届いていません。")
                                    .font(.caption2).foregroundStyle(.orange)
                            } else if result.runtimeEffect == .outcomeUnknown {
                                Text("停止要求の実行結果を確認できません。更新してください。")
                                    .font(.caption2).foregroundStyle(.orange)
                            }
                        }
                        if let root = activeRoot(for: child) {
                            HStack(spacing: 8) {
                                Button("更新") {
                                    let cursor = service.subagentWaitResult?.rootRunID == root.id
                                      && service.subagentWaitResult?.rootAttemptID == root.attemptID
                                      ? service.subagentWaitResult?.cursor : nil
                                    Task { await service.waitSubagents(root: root, afterCursor: cursor, controlRevision: child.controlRevision) }
                                }
                                .disabled(!service.canWaitSubagents)
                                .accessibilityLabel("\(child.displayName) の状態を更新")
                                .accessibilityHint("保存済みの子作業状態を読み込みます。")
                                if [.queued, .running, .waiting].contains(child.state) {
                                    Button("停止") { Task { await service.cancelSubagent(root: root, child: child) } }
                                        .disabled(!service.canCancelSubagents)
                                        .accessibilityLabel("\(child.displayName) を停止")
                                        .accessibilityHint("子作業への停止要求を送ります。")
                                }
                            }.font(.caption2)
                        }
                    }
                }
            }
            if !orchestration.mcpLeases.isEmpty {
                Text("連携中の外部ツール").font(.caption.weight(.medium))
                ForEach(orchestration.mcpLeases) { lease in
                    Text(lease.serverID + " · " + leaseTitle(lease.state))
                        .font(.caption2).foregroundStyle(.secondary)
                        .accessibilityLabel("外部ツール \(lease.serverID)、\(leaseTitle(lease.state))")
                }
            }
            if let runner = orchestration.commonRunner {
                Text("共通試験：" + runnerTitle(runner))
                    .font(.caption2).foregroundStyle(.secondary)
                    .accessibilityLabel("共通試験、\(runnerTitle(runner))")
            }
        }
    }
    private func submitClarification(_ question: WorkspaceClarificationQuestion, answer: String) {
        guard !answer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        // M25 accepts a normal next user input. The card retains its question
        // identity locally for rendering; only the user's answer enters history.
        service.editDraft(answer)
        Task { await service.submitInput() }
    }
    private var attachmentSelection: (() -> Void)? {
        guard service.executionAvailable && !importing else { return nil }
        return { selectAttachments() }
    }
    private var serviceFailureTitle: String {
        switch service.failure {
        case .transport(.timeout): return "サービスの応答が期限内に届きませんでした。本文を保持しています。"
        case .transport(.schema), .transport(.correlation), .stale:
            return "サービスの応答と保存状態を照合できませんでした。本文を保持しています。"
        default: return "サービスとの処理を完了できませんでした。本文を保持しています。"
        }
    }
    private func selectModel(_ option: WorkspaceModelConfiguration) {
                    if option.id == currentModel.id, let selection = model.document.preferences?.executionBinding {
                        Task { await model.selectWorkspaceExecution(selection) }
                        return
                    }
                    if option.id.hasPrefix("codex|") {
                        let modelID = String(option.id.dropFirst("codex|".count))
                        if let selection = try? ExecutionBinding(provider: .codex, model: modelID, effort: option.effort) {
                            Task { await model.selectWorkspaceExecution(selection) }
                        }
                        return
                    }
                    for inventory in inventories {
                        let prefix = inventory.provider.rawValue + "|" + inventory.endpoint + "|"
                        if let observed = inventory.models.first(where: { prefix + $0.id == option.id }),
                           let selection = try? ExecutionBinding(provider: inventory.provider, model: observed.id, effort: nil, localEndpoint: inventory.endpoint) {
                            Task { await model.selectWorkspaceExecution(selection) }
                        }
                    }
    }
    private var composer: some View {
            WorkspaceComposerInput(
                text: Binding(get: { service.draftText }, set: { service.editDraft($0) }),
                isComposing: Binding(get: { isComposing }, set: {
                    isComposing = $0; voice.editDraft(service.activeSessionID) { $0.isComposing = isComposing }
                }), attachments: service.importedAttachments.map(\.attachment), models: modelOptions,
                selectedModel: currentModel, selectedPermission: model.document.preferences?.permission,
                phase: inputPhase, showsSend: !service.draftText.isEmpty || !service.importedAttachments.isEmpty,
                canSend: (service.inputAdmissionAvailable ? service.canSubmitInput : service.canSend)
                    && !isComposing && !importing && inputPhase == .idle && voice.voiceDeferred[service.activeSessionID] == nil,
                attachmentPreview: { attachment in
                    service.reviewImportedAttachment(attachment.id)
                    return WorkspaceComposerAttachmentPreview(attachment: attachment,
                        detail: "送信時にこの本文を指示へ追加します。保存時にも本文へ展開します。",
                        text: service.importedAttachments.first { $0.id == attachment.id }?.text)
                }, onSelectAttachments: attachmentSelection,
                onRemoveAttachment: { service.removeImportedAttachment($0.id) },
                onSelectModel: selectModel, onSelectPermission: { permission in
                    if let selection = model.document.preferences?.executionBinding {
                        Task { await model.selectWorkspaceExecution(selection, permission: permission) }
                    }
                }, onSend: { Task {
                    if service.inputAdmissionAvailable { await service.submitInput() }
                    else { await service.startRun() }
                } },
                onVoice: { voice.voice(service.activeSessionID, $0) },
                voiceNote: voice.voiceNotes[service.activeSessionID], deferredVoice: voice.voiceDeferred[service.activeSessionID],
                onApplyDeferredVoice: { voice.applyDeferredVoice(service.activeSessionID) },
                onDiscardDeferredVoice: { voice.voiceDeferred[service.activeSessionID] = nil },
                attachmentNotice: inputNotice ?? (service.importedAttachments.isEmpty ? nil : "添付名を押して内容を確認してください。UTF-8テキスト4件・各64KiB・合計128KiBまで。画像・バイナリは非対応です。"))
                .padding(12).background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 18))
                .overlay(RoundedRectangle(cornerRadius: 18).stroke(.separator, lineWidth: 1))
                .disabled(service.phase == .connecting || service.phase == .closing || service.phase == .closed)
    }
    private func refreshLocalModels() async {
        guard !localRefreshing else { return }
        localRefreshing = true; defer { localRefreshing = false }
        for (provider, endpoint) in [(ExecutionBinding.Provider.ollama, "http://127.0.0.1:11434/"), (.lmstudio, "http://127.0.0.1:1234/")] {
            do {
                let result = try await service.localInventory(provider: provider, endpoint: endpoint)
                inventories.removeAll { $0.provider == provider && $0.endpoint == endpoint }
                inventories.append(result)
            } catch { inputNotice = "モデル一覧を取得できませんでした。接続状態を確認して再検出してください。" }
        }
    }
    private func selectAttachments() {
        let panel = NSOpenPanel()
        panel.title = "添付するUTF-8テキストを選択"
        panel.canChooseFiles = true; panel.canChooseDirectories = false; panel.allowsMultipleSelection = true
        panel.begin { response in
            guard response == .OK else { return }
            Task { @MainActor in
                guard !importing else { return }
                importing = true; defer { importing = false }
                inputNotice = nil
                for url in panel.urls.prefix(4) {
                    do {
                        let value = try await service.readAttachment(path: url.path)
                        guard value["state"]?.string == "ready", let text = value["text"]?.string else {
                            inputNotice = value["reason"]?.string ?? "添付を安全に読み取れませんでした。"; break
                        }
                        try service.addImportedAttachment(WorkspaceAttachment(url: url), text: text)
                    } catch { inputNotice = "添付を取り込めませんでした。接続状態と上限を確認してください。"; break }
                }
                if panel.urls.count > 4 { inputNotice = "添付は4件までです。残りのファイルは取り込んでいません。" }
            }
        }
    }
    private func recoveryResultTitle(_ result: SourceRecoveryClient.SavedResult) -> String {
        "結果の保存証明：\(result.resultID)（revision \(result.savedRevision)）"
    }
    private func inputAdmissionTitle(_ admission: WorkspaceInputAdmission) -> String {
        switch admission.state {
        case .reserved: "入力を受付予約しています。"
        case .classifying: "入力の経路を判定しています。"
        case .decided: "入力先を決定しています。"
        case .destinationReady: "入力先の会話を準備しています。"
        case .inputAppended: "入力を保存しています。"
        case .runQueued: "入力の実行を準備しています。"
        case .queued: "入力は実行待ちです。"
        case .running: "入力を実行しています。"
        case .completed: "入力の実行は完了として記録されています。"
        case .failed: "入力の実行は失敗として記録されています。"
        case .cancelled: "入力は取り消されました。"
        case .outcomeUnknown: "入力の受付状態を確認できません。"
        }
    }
    private func inputAdmissionDetails(_ admission: WorkspaceInputAdmission) -> String {
        var values = ["経路：\(routeTitle(admission.route))"]
        if let reason = admission.routeReason { values.append("理由：\(routeReasonTitle(reason))") }
        if let destination = admission.destinationSessionID { values.append("入力先：\(destination)") }
        if let run = admission.runID {
            values.append("実行：\(run)" + (admission.attemptID.map { "（試行 \($0)）" } ?? ""))
        }
        return values.joined(separator: " · ")
    }
    private func routeTitle(_ route: String) -> String {
        switch route { case "current": "この会話"; case "new": "別の会話"; default: "自動選択" }
    }
    private func routeReasonTitle(_ reason: String) -> String {
        switch reason {
        case "same_context": return "現在の話題に続くため"
        case "distinct_context": return "別の話題のため"
        case "ambiguous": return "判断が分かれるため"
        case "classifier_unavailable", "classifier_invalid": return "自動判定を使えないため"
        case "explicit_current": return "この会話が指定されたため"
        case "explicit_new": return "別の会話が指定されたため"
        case "approval_reply": return "確認への回答のため"
        case "context_changed": return "作業状況が変わったため"
        case "crash_recovery": return "復旧中のため"
        case "cancelled": return "取り消されたため"
        default: return reason
        }
    }
    private func sidechatTitle(_ state: String) -> String {
        switch state {
        case "active": "Sidechat は有効です。上の会話選択から切り替えられます。"
        case "full": "Sidechat の上限に達しています。上の会話選択で既存の会話へ切り替えられます。"
        default: "Sidechat の状態を確認できません。"
        }
    }
    private func activeRoot(for child: WorkspaceSubagentProjection) -> WorkspaceRun? {
        service.runs.first {
            $0.id.utf8.elementsEqual(child.rootRunID.utf8)
              && $0.attemptID.utf8.elementsEqual(child.rootAttemptID.utf8)
              && !$0.isTerminal
        }
    }
    private func sessionDisplayName(_ session: WorkspaceManagedSession) -> String {
        let kind = session.kind == .current ? "現在の会話" : "Sidechat"
        return kind + " · " + sessionStateTitle(session.state)
    }
    private func sessionStateTitle(_ state: WorkspaceManagedSessionState) -> String {
        switch state { case .idle: "待機中"; case .queued: "実行待ち"; case .running: "実行中"; case .closed: "終了" }
    }
    private func subagentTitle(_ state: WorkspaceSubagentState) -> String {
        switch state { case .queued: "待機中"; case .running: "実行中"; case .waiting: "待機中"; case .cancelling: "停止中"; case .succeeded: "完了"; case .failed: "失敗"; case .cancelled: "取消"; case .outcomeUnknown: "状態不明" }
    }
    private func subagentAccessibilityLabel(_ child: WorkspaceSubagentProjection) -> String {
        "協力中の作業、" + child.displayName + "、" + child.role + "、" + subagentTitle(child.state) + "、" + (child.activity ?? "状況は未取得")
    }
    private func subagentDisplayTitle(_ child: WorkspaceSubagentProjection) -> String {
        child.displayName + " · " + child.role + " · " + subagentTitle(child.state) + (child.activity.map { " · " + $0 } ?? "")
    }
    private func leaseTitle(_ state: WorkspaceMcpLeaseState) -> String {
        switch state { case .reserved: "準備中"; case .active: "利用中"; case .releasing: "終了中"; case .released: "終了"; case .expired: "期限切れ"; case .outcomeUnknown: "状態不明" }
    }
    private func runnerTitle(_ runner: WorkspaceCommonRunnerProjection) -> String {
        let state: String
        switch runner.state { case .unavailable: state = "未提供"; case .idle: state = "待機中"; case .preparing: state = "準備中"; case .running: state = "実行中"; case .completed: state = "完了"; case .failed: state = "失敗"; case .cleanupPending: state = "片付け待ち"; case .outcomeUnknown: state = "状態不明" }
        return state + " · 完了 \(runner.completedCases) · 失敗 \(runner.failedCases)"
    }
    private func role(_ value: ServiceHistoryMessage.Role) -> String {
        switch value { case .user: "ユーザー"; case .assistant: "アシスタント"; case .tool: "ツール"; case .system: "システム" }
    }
    private func statusTitle(_ status: ServiceRequestStatus) -> String {
        switch status {
        case .notFound: "台帳に見つかりません。保存成否は未確定のままです。"
        case .accepted: "受付済みですが、保存完了は未確認です。"
        case .outcomeUnknown: "台帳でも成否不明です。再送せず本文を保持しています。"
        case .completed: "台帳は完了していますが、保存済み本文との照合が必要です。"
        }
    }
}

private struct PolarisThinkingIndicator: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Group {
            if reduceMotion {
                mark(activeSpoke: 0)
            } else {
                TimelineView(.periodic(from: .now, by: 0.16)) { context in
                    let step = Int(context.date.timeIntervalSinceReferenceDate / 0.16) % 6
                    mark(activeSpoke: step)
                }
            }
        }
        .frame(width: 20, height: 20)
        .accessibilityHidden(true)
    }

    private func mark(activeSpoke: Int) -> some View {
        ZStack {
            ForEach(0..<6, id: \.self) { spoke in
                Capsule()
                    .fill(spoke == activeSpoke ? Color.white : Color.gray.opacity(0.48))
                    .frame(width: 2.5, height: 7)
                    .offset(y: -5.6)
                    .rotationEffect(.degrees(Double(spoke) * 60))
                    .shadow(color: spoke == activeSpoke ? .black.opacity(0.28) : .clear, radius: 0.6)
            }
        }
    }
}
