import AppKit
import SwiftUI
import PolarisSettings

enum PresetInstallationStatus: Equatable {
    case waiting
    case installing(completed: Int, total: Int)
    case installed
    case failed(String)

    var title: String {
        switch self {
        case .waiting: "セットアップ開始待ち"
        case .installing(let completed, let total): "必要な資源を導入中（\(completed) / \(total)）"
        case .installed: "必要な資源を導入済み"
        case .failed: "必要な資源の導入が止まりました"
        }
    }

    var isBusy: Bool {
        if case .installing = self { return true }
        return false
    }
}

@MainActor
final class DesktopModel: ObservableObject {
    @Published var document = SettingsDocument()
    @Published var stage: StartupStage = .configuration
    @Published var release: ReleaseConfiguration?
    @Published var isStarting = true
    @Published var startupError: String?
    @Published var saveError: String?
    @Published var assets: [String: NSImage] = [:]
    @Published var jevAPIKey = ""
    @Published var localEndpointDraft = "http://127.0.0.1:11434/"
    @Published var ggufSourceURL = ""
    @Published private(set) var hasStoredJevAPIKey = false
    @Published private(set) var jevCredentialError: String?
    @Published private(set) var localProbeStatus: LocalProbeStatus = .idle
    @Published private(set) var localInventory: LocalInventory?
    @Published private(set) var ggufPlan: ManagedGGUFPlan?
    @Published private(set) var ggufModels: [ManagedGGUFModel] = []
    @Published private(set) var ggufStatus: ManagedGGUFStatus = .idle
    @Published private(set) var dependencyStatuses = Dictionary(
        uniqueKeysWithValues: ManagedDependencyID.allCases.map { ($0, ManagedDependencyStatus.checking) })
    @Published private(set) var presetInstallationStatus: PresetInstallationStatus = .waiting
    @Published private(set) var preparationID: UUID?
    @Published private(set) var connectionSummary = ""
    private(set) var preparationSeconds: TimeInterval?
    private var preparationStarted: TimeInterval = 0
    private var splashDelayTask: Task<Void, Never>?
    private var screenIsPrepared = false
    private var connectionsArePrepared = false
    private let startupConnections: StartupConnections
    private let jevCredentials: JevCredentialAccess
    private let localProbe: LocalModelProbeAccess
    private let dependencyInstaller: any ManagedDependencyInstalling
    private var localProbeTask: Task<Void, Never>?
    private var dependencyTasks: [ManagedDependencyID: Task<Void, Never>] = [:]
    private var presetInstallationTask: Task<Void, Never>?
    private var ggufTask: Task<Void, Never>?
    let workspace: DesktopWorkspaceOwner
    private let arguments: [String]
    let isInstallerMode: Bool
    private var store: SettingsStore?
    private var pendingDocument: SettingsDocument?

    init(arguments: [String] = Array(CommandLine.arguments.dropFirst()),
         startupConnections: StartupConnections = StartupConnections(usesProductionProbes: true),
         jevCredentials: JevCredentialAccess = .keychain,
         localProbe: LocalModelProbeAccess = .live,
         dependencyInstaller: any ManagedDependencyInstalling = ManagedDependencyInstaller(),
         workspace: DesktopWorkspaceOwner? = nil) {
        self.jevCredentials = jevCredentials
        self.localProbe = localProbe
        self.dependencyInstaller = dependencyInstaller
        self.workspace = workspace ?? DesktopWorkspaceOwner(decisionCredential: jevCredentials.load)
        self.arguments = arguments
        self.isInstallerMode = arguments.contains("--installer-mode")
            || Bundle.main.bundleIdentifier?.hasPrefix("com.polaris.installer.") == true
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
        splashDelayTask?.cancel()
        splashDelayTask = nil
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
            let location = try SettingsLocation.resolve(arguments: arguments.filter { $0 != "--installer-mode" })
            let (settingsStore, loaded) = try await Task.detached {
                let store = try SettingsStore(url: location)
                return (store, try store.load())
            }.value
            store = settingsStore
            var prepared = loaded
            if isInstallerMode && !prepared.isEditing { prepared.reopen() }
            document = prepared
            localEndpointDraft = prepared.draft.preferences.localConnection?.endpoint
                ?? prepared.draft.preferences.localConnection?.runtime.defaultEndpoint
                ?? LocalRuntimePreference.ollama.defaultEndpoint!
            Task { await refreshDependencyStatuses() }
            if prepared.draft.preferences.jevEnabled || prepared.preferences?.jevEnabled == true {
                refreshJevCredentialState()
            } else {
                hasStoredJevAPIKey = false
                jevCredentialError = nil
            }
            DesktopTheme.apply(prepared.draft.preferences.theme)
            if isInstallerMode {
                stage = .ready
                connectionSummary = "インストール設定を準備しました"
                isStarting = false
                return
            }
            if !prepared.isEditing { document = try await workspace.prepare(document: prepared, store: settingsStore) }
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
            splashDelayTask?.cancel()
            splashDelayTask = nil
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
        if isInstallerMode {
            localProbeTask?.cancel(); presetInstallationTask?.cancel(); ggufTask?.cancel()
            return true
        }
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
        guard isStarting, startupError == nil, let id = preparationID else { return }
        guard screenIsPrepared, connectionsArePrepared else {
            stage = screenIsPrepared ? .connections : .interface
            return
        }
        let elapsed = ProcessInfo.processInfo.systemUptime - preparationStarted
        // Count preparation toward the minimum display time; never block the UI.
        if elapsed < 1 {
            guard splashDelayTask == nil else { return }
            splashDelayTask = Task { [weak self] in
                do { try await Task.sleep(for: .seconds(1 - elapsed)) }
                catch { return }
                guard let self, self.preparationID == id else { return }
                self.splashDelayTask = nil
                self.finishPreparationIfReady()
            }
            return
        }
        preparationSeconds = elapsed
        stage = .ready
        isStarting = false
    }

    func change(_ mutation: (inout WelcomeState) -> Void) {
        mutation(&document.draft)
        document.isEditing = true
        persist(document)
    }
    func setJevEnabled(_ enabled: Bool) {
        change { $0.preferences.jevEnabled = enabled }
        if enabled { refreshJevCredentialState() }
        else {
            jevAPIKey = ""
            jevCredentialError = nil
        }
    }
    func setLocalPreference(_ preference: LocalPreference) {
        localProbeTask?.cancel()
        localInventory = nil
        localProbeStatus = .idle
        change { state in
            state.preferences.local = preference
            if preference == .later { state.preferences.localConnection = nil }
            else if state.preferences.localConnection == nil {
                state.preferences.localConnection = try? LocalConnectionPreference(runtime: .ollama)
            }
        }
    }
    func setLocalRuntime(_ runtime: LocalRuntimePreference) {
        localProbeTask?.cancel()
        localInventory = nil
        localProbeStatus = .idle
        localEndpointDraft = runtime.defaultEndpoint ?? ""
        change { state in
            state.preferences.local = .linkInstalled
            state.preferences.localConnection = try? LocalConnectionPreference(runtime: runtime)
        }
    }
    func setLocalEndpointDraft(_ endpoint: String) {
        localProbeTask?.cancel()
        localInventory = nil
        localProbeStatus = .idle
        localEndpointDraft = endpoint
    }
    func probeSelectedLocalRuntime() {
        localProbeTask?.cancel()
        guard let connection = document.draft.preferences.localConnection,
              connection.runtime != .llamacpp else { return }
        let endpoint = localEndpointDraft
        do { try ExecutionBinding.validateLocalEndpoint(endpoint) }
        catch { localProbeStatus = .failed; return }
        change { state in
            state.preferences.localConnection = try? LocalConnectionPreference(
                runtime: connection.runtime, endpoint: endpoint, model: nil, verified: false)
        }
        localProbeStatus = .checking
        localInventory = nil
        localProbeTask = Task { [weak self] in
            guard let self else { return }
            do {
                let inventory = try await localProbe.probe(connection.runtime, endpoint)
                try Task.checkCancellation()
                localInventory = inventory
                switch inventory.availability {
                case .available: localProbeStatus = .available(inventory.models.count)
                case .unreachable: localProbeStatus = .unreachable
                case .invalidResponse: localProbeStatus = .invalidResponse
                }
            } catch is CancellationError {
                localProbeStatus = .idle
            } catch {
                localProbeStatus = .failed
            }
        }
    }
    func selectLocalModel(_ model: LocalModelObservation) {
        guard let current = document.draft.preferences.localConnection,
              current.runtime != .llamacpp else { return }
        change { state in
            state.preferences.localConnection = try? LocalConnectionPreference(
                runtime: current.runtime, endpoint: current.endpoint, model: model.id, verified: true)
        }
    }
    func dependencyStatus(_ id: ManagedDependencyID) -> ManagedDependencyStatus {
        dependencyStatuses[id] ?? .checking
    }
    func refreshDependencyStatuses() async {
        do {
            dependencyStatuses = try await dependencyInstaller.statuses()
        } catch {
            let message = (error as? LocalizedError)?.errorDescription ?? "依存関係を確認できませんでした。"
            dependencyStatuses = Dictionary(uniqueKeysWithValues: ManagedDependencyID.allCases.map {
                ($0, ManagedDependencyStatus.failed(message))
            })
        }
        if ManagedDependencyID.preset.allSatisfy({ id in
            if case .installed = dependencyStatuses[id] { return true }
            return false
        }) {
            do {
                try await dependencyInstaller.activatePreset()
                presetInstallationStatus = .installed
            } catch {
                let message = (error as? LocalizedError)?.errorDescription
                    ?? "導入済み資源をPolarisへ接続できませんでした。"
                presetInstallationStatus = .failed(message)
            }
        }
        refreshInstalledGGUFModels()
    }
    var presetInstallationReady: Bool { presetInstallationStatus == .installed }
    func installPresetAndWait() async -> Bool {
        startPresetInstallation()
        while presetInstallationStatus.isBusy {
            do { try await Task.sleep(for: .milliseconds(50)) }
            catch { return false }
        }
        return presetInstallationReady
    }
    func startPresetInstallation() {
        guard presetInstallationTask == nil, !presetInstallationReady else { return }
        let dependencies = ManagedDependencyID.preset
        presetInstallationStatus = .installing(completed: 0, total: dependencies.count)
        presetInstallationTask = Task { [weak self] in
            guard let self else { return }
            defer { presetInstallationTask = nil }
            for (index, id) in dependencies.enumerated() {
                do {
                    let current = try await dependencyInstaller.status(id)
                    if case .installed = current {
                        dependencyStatuses[id] = current
                    } else {
                        dependencyStatuses[id] = .downloading
                        let version = try await dependencyInstaller.install(id)
                        dependencyStatuses[id] = .installed(version: version)
                    }
                    presetInstallationStatus = .installing(
                        completed: index + 1, total: dependencies.count)
                } catch is CancellationError {
                    presetInstallationStatus = .failed("導入を取り消しました。再試行できます。")
                    return
                } catch {
                    let message = (error as? LocalizedError)?.errorDescription ?? "導入できませんでした。"
                    dependencyStatuses[id] = .failed(message)
                    presetInstallationStatus = .failed("\(id.title)：\(message)")
                    return
                }
            }
            do {
                try await dependencyInstaller.activatePreset()
            } catch {
                let message = (error as? LocalizedError)?.errorDescription
                    ?? "導入済み資源をPolarisへ接続できませんでした。"
                presetInstallationStatus = .failed(message)
                return
            }
            presetInstallationStatus = .installed
            refreshInstalledGGUFModels()
        }
    }
    func installDependency(_ id: ManagedDependencyID) {
        guard dependencyTasks[id] == nil else { return }
        dependencyStatuses[id] = .downloading
        dependencyTasks[id] = Task { [weak self] in
            guard let self else { return }
            defer { dependencyTasks[id] = nil }
            do {
                let version = try await dependencyInstaller.install(id)
                dependencyStatuses[id] = .installed(version: version)
                if id == .llamaCpp { refreshInstalledGGUFModels() }
            } catch is CancellationError {
                dependencyStatuses[id] = .notInstalled
            } catch {
                let message = (error as? LocalizedError)?.errorDescription ?? "導入できませんでした。"
                dependencyStatuses[id] = .failed(message)
            }
        }
    }

    /// Installs an explicitly selected optional resource as part of the installer flow.
    /// Unlike the preset, this never changes the managed-resource activation file.
    func installOptionalDependencyAndWait(_ id: ManagedDependencyID) async throws -> Bool {
        guard !ManagedDependencyID.preset.contains(id), presetInstallationReady else { return false }
        do {
            let current = try await dependencyInstaller.status(id)
            if case .installed = current {
                dependencyStatuses[id] = current
                return true
            }
            dependencyStatuses[id] = .downloading
            let version = try await dependencyInstaller.install(id)
            dependencyStatuses[id] = .installed(version: version)
            return true
        } catch is CancellationError {
            dependencyStatuses[id] = .notInstalled
            throw CancellationError()
        } catch ManagedDependencyError.cancelled {
            dependencyStatuses[id] = .notInstalled
            throw CancellationError()
        } catch {
            let message = (error as? LocalizedError)?.errorDescription ?? "導入できませんでした。"
            dependencyStatuses[id] = .failed(message)
            return false
        }
    }
    func cancelDependency(_ id: ManagedDependencyID) {
        dependencyTasks[id]?.cancel()
    }
    func cancelPresetInstallation() { presetInstallationTask?.cancel() }
    func inspectGGUFLink() {
        ggufTask?.cancel()
        ggufPlan = nil
        ggufStatus = .inspecting
        let source = ggufSourceURL
        ggufTask = Task { [weak self] in
            guard let self else { return }
            defer { ggufTask = nil }
            do {
                ggufPlan = try await dependencyInstaller.inspectGGUF(source)
                ggufStatus = .ready
            } catch is CancellationError {
                ggufStatus = .idle
            } catch {
                let message = (error as? LocalizedError)?.errorDescription ?? "リンクを確認できませんでした。"
                ggufStatus = .failed(message)
            }
        }
    }
    func installInspectedGGUF() {
        guard let plan = ggufPlan, ggufTask == nil else { return }
        ggufStatus = .downloading
        ggufTask = Task { [weak self] in
            guard let self else { return }
            defer { ggufTask = nil }
            do {
                let installed = try await dependencyInstaller.installGGUF(plan)
                ggufModels = try await dependencyInstaller.installedGGUFModels()
                ggufStatus = .installed
                selectGGUFModel(installed)
            } catch is CancellationError {
                ggufStatus = .ready
            } catch {
                let message = (error as? LocalizedError)?.errorDescription ?? "モデルを導入できませんでした。"
                ggufStatus = .failed(message)
            }
        }
    }
    func cancelGGUFInstall() {
        ggufTask?.cancel()
    }
    func installQueuedGGUFIfNeeded() async -> Bool {
        guard !ggufSourceURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return true }
        inspectGGUFLink()
        while ggufStatus == .inspecting {
            do { try await Task.sleep(for: .milliseconds(50)) }
            catch { return false }
        }
        guard ggufStatus == .ready else { return false }
        installInspectedGGUF()
        while ggufStatus == .downloading {
            do { try await Task.sleep(for: .milliseconds(50)) }
            catch { return false }
        }
        return ggufStatus == .installed
    }
    func selectGGUFModel(_ installed: ManagedGGUFModel) {
        change { state in
            state.preferences.local = .linkInstalled
            state.preferences.localConnection = try? LocalConnectionPreference(
                runtime: .llamacpp, endpoint: nil, model: installed.filename, verified: true)
        }
    }
    private func refreshInstalledGGUFModels() {
        Task { [weak self] in
            guard let self else { return }
            do { ggufModels = try await dependencyInstaller.installedGGUFModels() }
            catch { ggufModels = [] }
        }
    }
    func finish() {
        guard presetInstallationReady else {
            startPresetInstallation()
            return
        }
        guard saveJevCredentialIfNeeded() else { return }
        var candidate = document
        candidate.finish()
        persist(candidate)
    }
    func commitInstallerConfiguration() -> Bool {
        guard isInstallerMode, presetInstallationReady, saveJevCredentialIfNeeded(), let store else { return false }
        var candidate = document
        candidate.finish()
        do {
            try store.save(candidate)
            document = candidate
            pendingDocument = nil
            saveError = nil
            return true
        } catch {
            pendingDocument = candidate
            saveError = (error as? SettingsError)?.errorDescription ?? SettingsError.saveFailed.errorDescription
            return false
        }
    }
    func releaseInstallerSettingsLock() {
        guard isInstallerMode else { return }
        store = nil
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
    private func refreshJevCredentialState() {
        do {
            hasStoredJevAPIKey = try jevCredentials.load() != nil
            jevCredentialError = nil
        } catch {
            hasStoredJevAPIKey = false
            jevCredentialError = "保存済みのJev APIキーを確認できませんでした。新しいキーを入力してください。"
        }
    }
    private func saveJevCredentialIfNeeded() -> Bool {
        guard document.draft.preferences.jevEnabled else { return true }
        let key = jevAPIKey.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty || hasStoredJevAPIKey else {
            jevCredentialError = JevCredentialError.invalidKey.errorDescription
            return false
        }
        guard !key.isEmpty else { return true }
        do {
            try jevCredentials.save(key)
            jevAPIKey = ""
            hasStoredJevAPIKey = true
            jevCredentialError = nil
            return true
        } catch {
            jevCredentialError = (error as? LocalizedError)?.errorDescription
                ?? "Jev APIキーを保存できませんでした。"
            return false
        }
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

    /// Keeps the existing desktop document in sync with the private service
    /// preferences. The caller writes the private file first and restores it if
    /// this document save is rejected.
    func updateAnswerFinalizerPreferences(language: PresentationLanguagePreference,
                                          finalizer: AnswerFinalizerPreference) -> Bool {
        var candidate = document
        candidate.draft.preferences.presentationLanguage = language
        candidate.draft.preferences.answerFinalizer = finalizer
        if var saved = candidate.preferences {
            saved.presentationLanguage = language
            saved.answerFinalizer = finalizer
            candidate.preferences = saved
        }
        persist(candidate)
        let current = document.preferences ?? document.draft.preferences
        return current.presentationLanguage == language && current.answerFinalizer == finalizer && saveError == nil
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
