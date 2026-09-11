import AppKit
import SwiftUI
import PolarisSettings

@MainActor
final class DesktopModel: ObservableObject {
    @Published var document = SettingsDocument()
    @Published var stage: StartupStage = .configuration
    @Published var release: ReleaseConfiguration?
    @Published var isStarting = true
    @Published var startupError: String?
    @Published var saveError: String?
    @Published var assets: [String: NSImage] = [:]
    @Published private(set) var preparationID: UUID?
    @Published private(set) var connectionSummary = ""
    private(set) var preparationSeconds: TimeInterval?
    private var preparationStarted: TimeInterval = 0
    private var screenIsPrepared = false
    private var connectionsArePrepared = false
    private let startupConnections: StartupConnections
    let workspace: DesktopWorkspaceOwner
    private let arguments: [String]
    private var store: SettingsStore?
    private var pendingDocument: SettingsDocument?

    init(arguments: [String] = Array(CommandLine.arguments.dropFirst()),
         startupConnections: StartupConnections = StartupConnections(usesProductionProbes: true),
         workspace: DesktopWorkspaceOwner? = nil) {
        self.workspace = workspace ?? DesktopWorkspaceOwner()
        self.arguments = arguments
        self.startupConnections = startupConnections
        // 最初の描画前に同梱素材を用意し、設定I/O中・読取失敗時にも採用画面を表示する。
        bootstrap()
    }

    private func bootstrap() {
        startupError = nil
        do {
            stage = .interface
            for (name, ext) in [("polaris-banner-white", "svg"), ("astra-a-stars-layer", "png"),
                                ("astra-a-orbits-layer", "svg")] {
                guard let url = Bundle.module.url(forResource: name, withExtension: ext),
                      let image = NSImage(contentsOf: url), image.isValid else { throw StartupFailure.resource }
                assets[name] = image
            }
            stage = .configuration
            guard let releaseURL = Bundle.module.url(forResource: "release", withExtension: "json") else {
                throw StartupFailure.resource
            }
            release = try ReleaseConfiguration.load(from: releaseURL)
        } catch {
            startupError = "起動用の素材またはリリース情報を読み込めませんでした。"
        }
    }

    private var transitionInFlight = false
    private var startInFlight = false
    private var initialStartRequested = false
    private var initialStartupTask: Task<Void, Never>?
    func startOnce() async {
        if let initialStartupTask { await initialStartupTask.value; return }
        guard !initialStartRequested else { return }
        initialStartRequested = true
        // SwiftUI may cancel the view's .task as the root/window changes. The
        // application owns startup; replacement views await this same task.
        let task = Task { await self.start() }
        initialStartupTask = task
        await task.value
        initialStartupTask = nil
    }
    func start() async {
        guard !startInFlight else { return }
        startInFlight = true
        defer { startInFlight = false }
        isStarting = true
        preparationID = nil
        startupConnections.cancel()
        screenIsPrepared = false
        connectionsArePrepared = false
        preparationSeconds = nil
        preparationStarted = ProcessInfo.processInfo.systemUptime
        store = nil
        bootstrap()
        guard startupError == nil else { return }
        do {
            stage = .settings
            let location = try SettingsLocation.resolve(arguments: arguments)
            let (settingsStore, loaded) = try await Task.detached {
                let store = try SettingsStore(url: location)
                return (store, try store.load())
            }.value
            store = settingsStore
            document = loaded
            if !loaded.isEditing { document = try await workspace.prepare(document: loaded, store: settingsStore) }
            DesktopTheme.apply(loaded.draft.preferences.theme)
            stage = .interface
            let id = UUID()
            preparationID = id
            connectionSummary = "接続状態を確認しています"
            let accepted = startupConnections.start(savedPreferences: loaded.preferences) { [weak self] result in
                guard let self, self.preparationID == id, self.startupError == nil else { return }
                self.connectionSummary = "GPT：\(Self.connectionTitle(result.gpt.state)) · ローカル：\(Self.connectionTitle(result.local.state))"
                self.connectionsArePrepared = true
                self.finishPreparationIfReady()
            }
            if !accepted {
                connectionSummary = "前回の確認処理の終了待ちです。今回の接続状態は未確認です。"
                connectionsArePrepared = true
                finishPreparationIfReady()
            }
        } catch {
            startupError = (error as? SettingsError)?.errorDescription ?? SettingsError.readFailed.errorDescription
        }
    }

    func screenPrepared(_ id: UUID, succeeded: Bool) {
        guard preparationID == id, isStarting, startupError == nil else { return }
        guard succeeded else {
            preparationID = nil
            startupError = "次の画面を描画できませんでした。再試行してください。"
            startupConnections.cancel()
            return
        }
        screenIsPrepared = true
        finishPreparationIfReady()
    }

    var preparedSize: NSSize { document.isEditing ? NSSize(width: 960, height: 680) : NSSize(width: 1280, height: 780) }

    func prepareToQuit() async -> Bool {
        if pendingDocument != nil { retrySave() }
        guard pendingDocument == nil, initialStartupTask == nil, !startInFlight, !transitionInFlight else { return false }
        let closed = await workspace.quit()
        // Settings remain editable while shutdown drains. A save failure or a
        // new preparation during that await must cancel application termination.
        return closed && pendingDocument == nil && initialStartupTask == nil && !startInFlight && !transitionInFlight
    }

    func cancelStartupChecks() { startupConnections.cancel() }

    func reconnectWorkspace() async {
        guard pendingDocument == nil, !startInFlight, !transitionInFlight, let store else { return }
        await workspace.reconnect(store: store)
    }

    func startConfiguredExecution() async {
        guard pendingDocument == nil, !startInFlight, !transitionInFlight, let store else { return }
        await workspace.configureAndStart(store: store, reconcilePublication: true)
    }

    func selectWorkspaceExecution(_ selection: ExecutionBinding, permission: PermissionPreference? = nil,
                                  historyMode: HistoryMode? = nil) async {
        guard pendingDocument == nil, !startInFlight, !transitionInFlight, let store else { return }
        transitionInFlight = true
        defer { transitionInFlight = false }
        do {
            document = try await workspace.selectExecution(selection, permission: permission, historyMode: historyMode, settings: store)
            await workspace.configureAndStart(store: store, reconcilePublication: true)
            document = try store.load()
            saveError = nil
        } catch {
            // The saved selection may have changed before a failed owner start; display that fact.
            if let saved = try? store.load() { document = saved }
            saveError = historyMode == .strict10 || document.preferences?.historyMode == .strict10 ? "strict10 の保存済み設定と起動結果が一致しません。履歴欄の現在値を確認し、必要なら従来の履歴へ戻してください。" : "モデルの切替を完了できませんでした。下書きと保存状態を確認してください。"
        }
    }

    private static func connectionTitle(_ state: StartupConnections.State) -> String {
        switch state {
        case .notConfigured: "未設定"
        case .unavailable: "接続機能の準備中"
        case .checking: "前回の確認処理の終了待ち"
        case .reachable: "到達を確認（推論は未確認）"
        case .unconfirmed: "未確認"
        case .failed: "確認できませんでした"
        case .timedOut: "確認が時間切れになりました"
        case .cancelled: "確認を取り消しました"
        }
    }

    private func finishPreparationIfReady() {
        guard isStarting, startupError == nil, preparationID != nil else { return }
        guard screenIsPrepared, connectionsArePrepared else {
            stage = screenIsPrepared ? .connections : .interface
            return
        }
        preparationSeconds = ProcessInfo.processInfo.systemUptime - preparationStarted
        stage = .ready
        isStarting = false
    }

    func change(_ mutation: (inout WelcomeState) -> Void) {
        mutation(&document.draft)
        document.isEditing = true
        persist(document)
    }
    func finish() {
        var candidate = document
        candidate.finish()
        persist(candidate)
    }
    func reopen() {
        var candidate = document
        candidate.reopen()
        persist(candidate)
    }
    func retrySave() {
        guard let pendingDocument else { return }
        persist(pendingDocument)
    }
    private func persist(_ candidate: SettingsDocument) {
        guard let store else { return }
        do {
            if document.workspace != nil,
               candidate.preferences?.projectPath != document.preferences?.projectPath {
                throw SettingsError.sourceChanged
            }
            try store.save(candidate)
            let wasEditing = document.isEditing
            document = candidate
            if wasEditing && !candidate.isEditing {
                isStarting = true; preparationID = nil; screenIsPrepared = false
                preparationStarted = ProcessInfo.processInfo.systemUptime
                transitionInFlight = true
                Task {
                    defer { self.transitionInFlight = false }
                    do { self.document = try await workspace.prepare(document: candidate, store: store) }
                    catch {
                        self.startupError = (error as? SettingsError)?.errorDescription ?? SettingsError.readFailed.errorDescription
                        return
                    }
                    self.preparationID = UUID()
                    self.finishPreparationIfReady()
                }
            } else if wasEditing != candidate.isEditing {
                preparationID = UUID()
            }
            pendingDocument = nil
            saveError = nil
        } catch {
            pendingDocument = candidate
            saveError = (error as? SettingsError)?.errorDescription ?? SettingsError.saveFailed.errorDescription
        }
    }
    func selectFolder() {
        let panel = NSOpenPanel()
        panel.title = "プロジェクトのフォルダを選択"
        panel.prompt = "このフォルダを選択"
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.canCreateDirectories = false
        panel.begin { [weak self] response in
            guard response == .OK, let url = panel.url else { return }
            self?.change { $0.selectFolder(url) }
        }
    }
    enum StartupFailure: Error { case resource }
}
