import AppKit
import SwiftUI
import PolarisSettings

enum InstallerPage: Int, CaseIterable, Identifiable {
    case welcome, destination, models, workspace, review, install
    var id: Int { rawValue }
    var title: String {
        switch self {
        case .welcome: "ようこそ"
        case .destination: "インストール先"
        case .models: "モデル"
        case .workspace: "作業環境"
        case .review: "確認"
        case .install: "インストール"
        }
    }
    var symbol: String {
        switch self {
        case .welcome: "sparkles"
        case .destination: "internaldrive"
        case .models: "cpu"
        case .workspace: "folder"
        case .review: "checklist"
        case .install: "arrow.down.circle"
        }
    }
}

enum InstallerRunState: Equatable {
    case idle
    case running(detail: String, progress: Double)
    case completed(URL)
    case failed(String)

    var isRunning: Bool {
        if case .running = self { return true }
        return false
    }
}

@MainActor
final class InstallerCoordinator: ObservableObject {
    @Published var page: InstallerPage = .welcome
    @Published var destination = URL(fileURLWithPath: "/Applications", isDirectory: true)
    @Published private(set) var preflight: InstallerPreflight?
    @Published private(set) var runState: InstallerRunState = .idle
    @Published var replacementPending = false
    @Published var dependencyConsentPending = false
    @Published var launchAfterInstall = true
    @Published var installCATTranslate = false
    @Published var installRust = false
    private var replacementApproved = false
    private var pendingDependencySelection: DependencySelection?

    private struct DependencySelection: Equatable {
        let destination: URL
        let catTranslate: Bool
        let rust: Bool
        let ggufSource: String
    }

    private func dependencySelection(model: DesktopModel) -> DependencySelection {
        DependencySelection(destination: destination.standardizedFileURL,
                            catTranslate: installCATTranslate, rust: installRust,
                            ggufSource: model.ggufSourceURL.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private let payload: InstallerPayloadManager
    private var task: Task<Void, Never>?

    init(payload: InstallerPayloadManager = InstallerPayloadManager()) {
        self.payload = payload
    }

    func checkDestination() {
        let destination = destination
        Task {
            do { preflight = try await payload.preflight(destination: destination, additionalBytes: 0) }
            catch { preflight = nil }
        }
    }

    func chooseDestination() {
        let panel = NSOpenPanel()
        panel.title = "Polarisのインストール先"
        panel.prompt = "このフォルダを選択"
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.canCreateDirectories = true
        panel.directoryURL = destination
        panel.begin { [weak self] response in
            guard response == .OK, let url = panel.url else { return }
            self?.destination = url
            self?.checkDestination()
        }
    }

    func begin(model: DesktopModel, replaceExisting: Bool = false,
               installDependencies: Bool? = nil) {
        guard task == nil else { return }
        let destination = destination
        task = Task { [weak self, weak model] in
            guard let self, let model else { return }
            defer { task = nil }
            do {
                runState = .running(detail: "インストール先を確認しています", progress: 0.04)
                let check = try await payload.preflight(destination: destination, additionalBytes: 0)
                preflight = check
                if check.replacesExistingApplication,
                   !NSRunningApplication.runningApplications(withBundleIdentifier: "com.polaris.desktop.local").isEmpty {
                    throw InstallFailure.message("起動中のPolarisを終了してから置き換えてください。")
                }
                if check.replacesExistingApplication && !replaceExisting {
                    runState = .idle
                    replacementPending = true
                    return
                }

                if installDependencies == nil {
                    replacementApproved = replaceExisting
                    pendingDependencySelection = dependencySelection(model: model)
                    runState = .idle
                    dependencyConsentPending = true
                    return
                }

                if installDependencies == true {
                    _ = try await payload.preflight(destination: destination, additionalBytes: 4_100_000_000)
                }

                if installDependencies == true {
                    model.startPresetInstallation()
                    while model.presetInstallationStatus.isBusy {
                        try Task.checkCancellation()
                        let progress: Double
                        let detail: String
                        switch model.presetInstallationStatus {
                        case .installing(let completed, let total):
                            progress = 0.08 + 0.62 * Double(completed) / Double(max(total, 1))
                            detail = "必要な資源を導入中（\(completed) / \(total)）"
                        default:
                            progress = 0.08; detail = "必要な資源を準備しています"
                        }
                        runState = .running(detail: detail, progress: progress)
                        try await Task.sleep(for: .milliseconds(80))
                    }
                    guard model.presetInstallationReady else {
                        if case .failed(let message) = model.presetInstallationStatus { throw InstallFailure.message(message) }
                        throw InstallFailure.message("必要な資源を導入できませんでした。")
                    }
                }

                if installCATTranslate && installDependencies == true {
                    runState = .running(detail: "CAT-Translate 3.3Bを導入しています", progress: 0.72)
                    guard try await model.installOptionalDependencyAndWait(.catTranslate3B) else {
                        if case .failed(let message) = model.dependencyStatus(.catTranslate3B) {
                            throw InstallFailure.message("CAT-Translate 3.3B：\(message)")
                        }
                        throw InstallFailure.message("CAT-Translate 3.3Bを導入できませんでした。")
                    }
                }

                if installRust && installDependencies == true {
                    runState = .running(detail: "開発用Rustを導入しています", progress: 0.75)
                    guard try await model.installOptionalDependencyAndWait(.rust) else {
                        if case .failed(let message) = model.dependencyStatus(.rust) {
                            throw InstallFailure.message("Rust：\(message)")
                        }
                        throw InstallFailure.message("開発用Rustを導入できませんでした。")
                    }
                }

                if installDependencies == true && !model.ggufSourceURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                    runState = .running(detail: "指定した端末内モデルを導入しています", progress: 0.78)
                    guard await model.installQueuedGGUFIfNeeded() else {
                        if case .failed(let message) = model.ggufStatus { throw InstallFailure.message(message) }
                        throw InstallFailure.message("指定した端末内モデルを導入できませんでした。")
                    }
                }

                runState = .running(detail: "設定を安全に保存しています", progress: 0.88)
                guard model.commitInstallerConfiguration() else {
                    throw InstallFailure.message(model.jevCredentialError ?? model.saveError ?? "設定を保存できませんでした。")
                }

                runState = .running(detail: "Polaris.appを配置しています", progress: 0.95)
                let installed = try await payload.install(destination: destination, replaceExisting: replaceExisting)
                model.releaseInstallerSettingsLock()
                runState = .completed(installed)
                if launchAfterInstall {
                    _ = try? await NSWorkspace.shared.openApplication(at: installed, configuration: .init())
                }
            } catch is CancellationError {
                model.cancelPresetInstallation(); model.cancelGGUFInstall()
                runState = .failed("インストールを取り消しました。再試行できます。")
            } catch {
                runState = .failed((error as? LocalizedError)?.errorDescription ?? "インストールを完了できませんでした。")
            }
        }
    }

    func cancel(model: DesktopModel) {
        task?.cancel(); model.cancelPresetInstallation(); model.cancelGGUFInstall()
    }

    func confirmDependencies(model: DesktopModel, install: Bool) {
        guard let pendingDependencySelection else { return }
        self.pendingDependencySelection = nil
        let replaceExisting = replacementApproved
        replacementApproved = false
        guard pendingDependencySelection == dependencySelection(model: model) else {
            begin(model: model)
            return
        }
        begin(model: model, replaceExisting: replaceExisting, installDependencies: install)
    }

    func dependencySummary(model: DesktopModel) -> String {
        var names = ["llama.cpp", "埋め込み実行環境", "multilingual-e5-small/base"]
        if installCATTranslate { names.append("CAT-Translate 3.3B") }
        if installRust { names.append("Rust 1.96.0") }
        let ggufSource = model.ggufSourceURL.trimmingCharacters(in: .whitespacesAndNewlines)
        if !ggufSource.isEmpty { names.append("指定したGGUF（\(ggufSource)）") }
        return names.joined(separator: "、") + "を検証して ~/.polaris へ導入します。いいえを選ぶとPolaris本体だけを配置します。"
    }

    private enum InstallFailure: Error, LocalizedError {
        case message(String)
        var errorDescription: String? { if case .message(let value) = self { value } else { nil } }
    }
}

struct InstallerRootView: View {
    @ObservedObject var model: DesktopModel
    @StateObject private var installer = InstallerCoordinator()

    var body: some View {
        HStack(spacing: 0) {
            InstallerSidebar(current: installer.page)
            VStack(spacing: 0) {
                Group {
                    if let error = model.startupError {
                        VStack(spacing: 14) {
                            Image(systemName: "exclamationmark.triangle").font(.system(size: 40))
                            Text("インストーラを準備できませんでした").font(.title2.weight(.semibold))
                            Text(error).foregroundStyle(.secondary)
                        }.frame(maxWidth: .infinity, maxHeight: .infinity)
                    } else if model.isStarting {
                        VStack(spacing: 14) {
                            ProgressView()
                            Text("インストーラを準備しています").foregroundStyle(.secondary)
                        }.frame(maxWidth: .infinity, maxHeight: .infinity)
                    } else {
                        installerContent
                    }
                }
                Divider()
                footer.frame(height: 70).padding(.horizontal, 30)
            }
            .background(Color(nsColor: .windowBackgroundColor))
        }
        .frame(width: 920, height: 640)
        .task {
            await model.startOnce()
            installer.checkDestination()
        }
        .alert("既存のPolarisを置き換えますか？", isPresented: $installer.replacementPending) {
            Button("キャンセル", role: .cancel) {}
            Button("置き換える", role: .destructive) { installer.begin(model: model, replaceExisting: true) }
        } message: {
            Text("同じインストール先にあるPolaris.appを退避してから置き換えます。失敗した場合は元へ戻します。")
        }
        .confirmationDialog("Polaris以外の資源を取得しますか？", isPresented: $installer.dependencyConsentPending) {
            Button("はい、取得する") { installer.confirmDependencies(model: model, install: true) }
            Button("いいえ、Polarisのみ") { installer.confirmDependencies(model: model, install: false) }
            Button("キャンセル", role: .cancel) {}
        } message: {
            Text(installer.dependencySummary(model: model))
        }
    }

    @ViewBuilder private var installerContent: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                switch installer.page {
                case .welcome: welcome
                case .destination: destination
                case .models: models
                case .workspace: workspace
                case .review: review
                case .install: installation
                }
            }
            .padding(.horizontal, 42).padding(.vertical, 36)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var welcome: some View {
        VStack(alignment: .leading, spacing: 22) {
            InstallerMark().frame(width: 72, height: 72)
            Text("PolarisをこのMacに\nインストールします")
                .font(.system(size: 34, weight: .semibold, design: .rounded)).lineSpacing(3)
            Text("アプリ本体、固定した実行環境、モデル接続、作業場所と権限を一度に準備します。すべての内容はインストール前に確認できます。")
                .foregroundStyle(.secondary).lineSpacing(5)
            HStack(spacing: 26) {
                Label("macOS 13以降", systemImage: "laptopcomputer")
                Label("依存資源の容量は選択後に確認", systemImage: "internaldrive")
                Label("依存資源の取得にはネット接続が必要", systemImage: "network")
            }.font(.caption).foregroundStyle(.secondary).padding(.top, 16)
        }
    }

    private var destination: some View {
        VStack(alignment: .leading, spacing: 26) {
            pageHeader("インストール先", "Polaris.appを配置するフォルダを選びます。モデルと実行環境は利用者ごとの ~/.polaris へ保存します。")
            VStack(alignment: .leading, spacing: 10) {
                Text("アプリケーションの保存先").fontWeight(.medium)
                HStack {
                    Image(systemName: "folder.fill").foregroundStyle(.tint)
                    Text(installer.destination.path).lineLimit(1).truncationMode(.middle)
                    Spacer()
                    Button("選択…") { installer.chooseDestination() }
                }.padding(14).background(.quaternary.opacity(0.45), in: RoundedRectangle(cornerRadius: 10))
                if let preflight = installer.preflight {
                    Text(preflight.replacesExistingApplication
                         ? "既存のPolaris.appは確認後に安全に置き換えます。"
                         : "この場所へインストールできます。")
                        .font(.caption).foregroundStyle(.secondary)
                    Text("Polaris本体：\(capacity(preflight.payloadBytes))　利用可能：\(capacity(preflight.availableBytes))")
                        .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                } else {
                    Label("この場所へ書き込めないか、空き容量が不足しています。", systemImage: "exclamationmark.triangle")
                        .font(.caption).foregroundStyle(.orange)
                }
            }
            VStack(alignment: .leading, spacing: 8) {
                Text("個人用資源").fontWeight(.medium)
                Text("~/.polaris  ·  選択した依存資源、設定とモデル")
                    .font(.callout).foregroundStyle(.secondary)
                Text("システムのRust・Python・PATHは変更しません。")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private var models: some View {
        VStack(alignment: .leading, spacing: 20) {
            pageHeader("モデルと内部判定", "クラウド接続、内部判定、端末内モデルを設定します。接続はあとからPolarisの設定でも変更できます。")
            installerSection("GPT接続") {
                VStack(alignment: .leading, spacing: 10) {
                    Picker("接続方法", selection: preference(\.gpt)) {
                        ForEach([GPTPreference.chatGPT, .apiKey, .later], id: \.self) { Text($0.title).tag($0) }
                    }.pickerStyle(.segmented)
                    if model.document.draft.preferences.gpt != .later {
                        TextField("モデル", text: Binding(
                            get: { model.document.draft.preferences.executionBinding?.model ?? "gpt-6-astra" },
                            set: { name in model.change { state in
                                state.preferences.executionBinding = try? ExecutionBinding(
                                    provider: state.preferences.gpt == .apiKey ? .openai : .codex,
                                    model: name, effort: "medium")
                            }}))
                            .textFieldStyle(.roundedBorder)
                    }
                }
            }
            installerSection("内部判定") {
                VStack(alignment: .leading, spacing: 10) {
                    Toggle("Jevを内部判定に使う", isOn: Binding(
                        get: { model.document.draft.preferences.jevEnabled },
                        set: { model.setJevEnabled($0) }))
                    Text(model.document.draft.preferences.jevEnabled
                         ? "型付き判定をAPIへ委任し、判定精度を高めます。API利用量が発生します。"
                         : "端末内のe5を使います。Jevと比べて判定精度が少し下がります。")
                        .font(.caption).foregroundStyle(.secondary)
                    if model.document.draft.preferences.jevEnabled {
                        SecureField("Jev APIキー", text: $model.jevAPIKey)
                            .textFieldStyle(.roundedBorder)
                        if let error = model.jevCredentialError { Text(error).font(.caption).foregroundStyle(.red) }
                    }
                }
            }
            installerSection("Clef判断器の導入（任意）") {
                DecisionInstallPanel()
            }
            installerSection("端末内モデル") {
                VStack(alignment: .leading, spacing: 10) {
                    Picker("利用方法", selection: Binding(
                        get: { model.document.draft.preferences.local }, set: { model.setLocalPreference($0) })) {
                        ForEach([LocalPreference.linkInstalled, .later], id: \.self) { Text($0.title).tag($0) }
                    }.pickerStyle(.segmented)
                    if model.document.draft.preferences.local == .linkInstalled { localModelFields }
                }
            }
            installerSection("日本語翻訳モデル（任意）") {
                Toggle("CAT-Translate 3.3Bを導入する", isOn: $installer.installCATTranslate)
                Text(installer.installCATTranslate
                     ? "取得サイズは約6.7 GBです。展開・検証を含め、導入時には\(capacity(ManagedDependencyInstaller.catTranslateRequiredSpaceBytes))以上の空き容量が必要です。固定した版を ~/.polaris/models/cat-translate-3.3b/\(ManagedDependencyInstaller.catTranslateRevision) へ保存し、導入後も自動では有効化しません。"
                     : "既定では導入しません。必要になった場合だけ、このインストールで追加できます。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            installerSection("開発用ツール（任意）") {
                Toggle("Rust 1.96.0を導入する", isOn: $installer.installRust)
                Text("配布済みPolarisの実行には不要です。ソースからのビルドに使う場合だけ ~/.polaris/toolchains に導入します。システムのRustは変更しません。")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder private var localModelFields: some View {
        Picker("実行環境", selection: Binding(
            get: { model.document.draft.preferences.localConnection?.runtime ?? .ollama },
            set: { model.setLocalRuntime($0) })) {
            ForEach(LocalRuntimePreference.allCases, id: \.self) { Text($0.title).tag($0) }
        }.pickerStyle(.segmented)
        let runtime = model.document.draft.preferences.localConnection?.runtime ?? .ollama
        if runtime == .ollama || runtime == .lmstudio {
            TextField("ループバックendpoint", text: Binding(
                get: { model.localEndpointDraft }, set: { model.setLocalEndpointDraft($0) }))
                .textFieldStyle(.roundedBorder)
            HStack {
                Button("接続を確認") { model.probeSelectedLocalRuntime() }
                Text(model.localProbeStatus.title).font(.caption).foregroundStyle(.secondary)
            }
            if let inventory = model.localInventory, inventory.availability == .available {
                ForEach(inventory.models) { observed in
                    Button {
                        model.selectLocalModel(observed)
                    } label: {
                        Label(observed.id, systemImage: model.document.draft.preferences.localConnection?.model == observed.id
                              ? "checkmark.circle.fill" : "circle")
                    }.buttonStyle(.plain)
                }
            }
        } else {
            TextField("Hugging Faceまたは公開HTTPSの.ggufリンク（任意）", text: $model.ggufSourceURL)
                .textFieldStyle(.roundedBorder)
            Text("llama.cppは自動取得します。リンクを指定した場合はモデルも検証して導入します。")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var workspace: some View {
        VStack(alignment: .leading, spacing: 24) {
            pageHeader("作業場所と外観", "最初に開くフォルダ、自動許可の希望、画面のテーマを設定します。OSの権限は変更しません。")
            installerSection("プロジェクト") {
                VStack(alignment: .leading, spacing: 10) {
                    Text(model.document.draft.preferences.projectPath ?? "未選択（ホームを表示）")
                        .foregroundStyle(.secondary).textSelection(.enabled)
                    HStack {
                        Button("フォルダを選択…") { model.selectFolder() }
                        if model.document.draft.preferences.projectPath != nil {
                            Button("選択を解除") { model.change { $0.preferences.projectPath = nil } }
                        }
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
            installerSection("自動許可の希望") {
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 8) {
                    permissionChoice(nil, title: "未選択")
                    ForEach(PermissionPreference.allCases, id: \.self) { permission in
                        permissionChoice(permission, title: permission.title)
                    }
                }
                .accessibilityElement(children: .contain)
            }
            installerSection("テーマ") {
                Picker("テーマ", selection: preference(\.theme)) {
                    ForEach(ThemePreference.allCases, id: \.self) { Text($0.title).tag($0) }
                }.pickerStyle(.segmented)
            }
        }
    }

    private var review: some View {
        VStack(alignment: .leading, spacing: 24) {
            pageHeader("インストールの確認", "次の内容でPolarisと必要な資源を導入します。戻ると各項目を変更できます。")
            Grid(alignment: .leading, horizontalSpacing: 34, verticalSpacing: 15) {
                summary("インストール先", installer.destination.path)
                summary("GPT接続", model.document.draft.preferences.gpt.title)
                summary("内部判定", model.document.draft.preferences.jevEnabled ? "Jev API" : "端末内のe5")
                summary("端末内モデル", localSummary)
                summary("翻訳モデル", installer.installCATTranslate
                        ? "CAT-Translate 3.3Bを導入（必要空き：\(capacity(ManagedDependencyInstaller.catTranslateRequiredSpaceBytes))以上）"
                        : "導入しない")
                summary("プロジェクト", model.document.draft.preferences.projectPath ?? "後で選択")
                summary("自動許可", model.document.draft.preferences.permission?.title ?? "未選択")
                summary("テーマ", model.document.draft.preferences.theme.title)
                summary("取得候補", "llama.cpp・multilingual-e5 small/base・埋め込み実行環境")
            }
            Toggle("完了後にPolarisを開く", isOn: $installer.launchAfterInstall).padding(.top, 10)
        }
    }

    private var installation: some View {
        VStack(alignment: .leading, spacing: 22) {
            switch installer.runState {
            case .idle:
                pageHeader("Polarisをインストール", "インストール先と設定を確認しました。開始すると固定した資源を検証しながら導入します。")
                Text("既存のPolarisがある場合は、置き換える前に確認します。")
                    .font(.caption).foregroundStyle(.secondary)
            case .running(let detail, let progress):
                pageHeader("インストール中", "ウィンドウを閉じずにお待ちください。取得済みの正しい資源は再利用します。")
                ProgressView(value: progress)
                Text(detail).font(.callout.monospacedDigit()).foregroundStyle(.secondary)
            case .completed(let url):
                Image(systemName: "checkmark.circle.fill").font(.system(size: 54)).foregroundStyle(.green)
                pageHeader("インストールが完了しました", "Polaris本体、設定、実行環境を準備しました。")
                Text(url.path).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            case .failed(let message):
                Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 45)).foregroundStyle(.orange)
                pageHeader("インストールを完了できませんでした", message)
                Text("原因を修正したあと、この画面から再試行できます。取得済みの検証済み資源は再利用します。")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private var footer: some View {
        HStack {
            Text("\(installer.page.rawValue + 1) / 6").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            Spacer()
            if installer.page != .welcome && !installer.runState.isRunning {
                Button("戻る") { installer.page = InstallerPage(rawValue: installer.page.rawValue - 1)! }
                    .disabled(installer.page == .install && isCompleted)
            }
            if installer.page != .install {
                Button(installer.page == .review ? "インストールへ" : "続ける") {
                    installer.page = InstallerPage(rawValue: installer.page.rawValue + 1)!
                }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                    .disabled(model.isStarting || (installer.page == .destination && installer.preflight == nil)
                              || (installer.page == .models && missingJevKey))
            } else {
                switch installer.runState {
                case .idle, .failed:
                    Button("インストール") { installer.begin(model: model) }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                case .running:
                    Button("取り消す") { installer.cancel(model: model) }
                case .completed:
                    Button("閉じる") { NSApplication.shared.terminate(nil) }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                }
            }
        }
    }

    private var missingJevKey: Bool {
        model.document.draft.preferences.jevEnabled
            && model.jevAPIKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !model.hasStoredJevAPIKey
    }
    private var isCompleted: Bool { if case .completed = installer.runState { true } else { false } }
    private var localSummary: String {
        let preferences = model.document.draft.preferences
        guard preferences.local == .linkInstalled else { return "使用しない" }
        return preferences.localConnection?.runtime.title ?? "Ollama"
    }
    private func preference<T>(_ keyPath: WritableKeyPath<Preferences, T>) -> Binding<T> {
        Binding(get: { model.document.draft.preferences[keyPath: keyPath] },
                set: { value in model.change { $0.preferences[keyPath: keyPath] = value } })
    }
    private func pageHeader(_ title: String, _ description: String) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.system(size: 30, weight: .semibold, design: .rounded))
            Text(description).foregroundStyle(.secondary).lineSpacing(5)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
    private func installerSection<Content: View>(
        _ title: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.headline)
            content()
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.quaternary.opacity(0.32), in: RoundedRectangle(cornerRadius: 12))
        .accessibilityElement(children: .contain)
    }
    private func permissionChoice(_ permission: PermissionPreference?, title: String) -> some View {
        let selected = model.document.draft.preferences.permission == permission
        return Button {
            model.change { $0.preferences.permission = permission }
        } label: {
            HStack(spacing: 8) {
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                Text(title).font(.caption).multilineTextAlignment(.leading)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 10).padding(.vertical, 8)
            .frame(maxWidth: .infinity, minHeight: 38, alignment: .leading)
            .background(selected ? Color.accentColor.opacity(0.12) : Color.primary.opacity(0.035),
                        in: RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(title)
        .accessibilityValue(selected ? "選択中" : "未選択")
    }
    private func summary(_ label: String, _ value: String) -> some View {
        GridRow { Text(label).foregroundStyle(.secondary); Text(value).fontWeight(.medium) }
    }
    private func capacity(_ bytes: Int64?) -> String {
        guard let bytes else { return "取得できません" }
        return ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }
}

private struct InstallerSidebar: View {
    let current: InstallerPage
    var body: some View {
        ZStack {
            LinearGradient(colors: [Color(red: 0.035, green: 0.075, blue: 0.12),
                                    Color(red: 0.055, green: 0.15, blue: 0.23)],
                           startPoint: .top, endPoint: .bottom)
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 12) {
                    InstallerMark().frame(width: 42, height: 42)
                    Text("polaris").font(.system(size: 22, weight: .semibold, design: .rounded))
                }.padding(.bottom, 30)
                ForEach(InstallerPage.allCases) { page in
                    HStack(spacing: 12) {
                        ZStack {
                            Circle().fill(page == current ? Color(red: 0.42, green: 0.72, blue: 0.94)
                                          : Color.white.opacity(page.rawValue < current.rawValue ? 0.18 : 0.08))
                                .frame(width: 30, height: 30)
                            Image(systemName: page.rawValue < current.rawValue ? "checkmark" : page.symbol)
                                .font(.system(size: 12, weight: .semibold))
                                .foregroundStyle(page == current ? Color(red: 0.025, green: 0.09, blue: 0.14)
                                                 : .white.opacity(page.rawValue < current.rawValue ? 0.9 : 0.62))
                        }
                        Text(page.title).font(.system(size: 13, weight: page == current ? .semibold : .regular))
                            .foregroundStyle(.white.opacity(page == current ? 1 : 0.62))
                        Spacer()
                    }.frame(height: 48)
                    if page != .install {
                        Rectangle().fill(Color.white.opacity(0.12)).frame(width: 1, height: 11).padding(.leading, 14.5)
                    }
                }
                Spacer()
                Text("POLARIS INSTALLER").font(.system(size: 10, weight: .medium, design: .rounded))
                    .tracking(1.1).foregroundStyle(.white.opacity(0.42))
            }.padding(28)
        }.frame(width: 242).fixedSize(horizontal: true, vertical: false)
    }
}

private struct InstallerMark: View {
    var body: some View {
        ZStack {
            ForEach(0..<6, id: \.self) { index in
                Capsule().fill(Color.white.opacity(0.94)).frame(width: 6, height: 24)
                    .offset(y: -19).rotationEffect(.degrees(Double(index) * 60))
            }
            Circle().fill(Color(red: 0.42, green: 0.72, blue: 0.94)).frame(width: 7, height: 7)
        }.accessibilityLabel("Polaris")
    }
}

struct SetupRequiredView: View {
    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "shippingbox").font(.system(size: 46)).foregroundStyle(.secondary)
            Text("Polarisのインストールが必要です").font(.title2.weight(.semibold))
            Text("初回設定はPolarisインストーラへ統合されました。\nインストーラを実行してからPolarisを開いてください。")
                .multilineTextAlignment(.center).foregroundStyle(.secondary)
        }.frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
