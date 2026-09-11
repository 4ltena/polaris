import Foundation
import Combine
import PolarisSettings

/// One explicitly bound durable service for the lifetime of the application.
@MainActor
final class DesktopWorkspaceOwner: ObservableObject {
    @Published private(set) var service: WorkspaceServiceModel?
    @Published private(set) var binding: WorkspaceBinding?
    @Published private(set) var error: String?
    @Published private(set) var isRecovering = false
    private let helper: () throws -> URL
    private let factory: ((URL, ServiceClient.PersistentStore, String) throws -> WorkspaceServiceModel)?
    private var preparing = false
    private var quitting = false

    init(helper: @escaping () throws -> URL = DesktopWorkspaceOwner.packagedHelper,
         factory: ((URL, ServiceClient.PersistentStore, String) throws -> WorkspaceServiceModel)? = nil) {
        self.helper = helper; self.factory = factory
    }

    nonisolated static func packagedHelper() throws -> URL {
        let url = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/polaris-desktop-service")
        let values = try url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
        guard values.isRegularFile == true, values.isSymbolicLink != true,
              FileManager.default.isExecutableFile(atPath: url.path) else { throw ServiceError.launch }
        return url
    }

    func prepare(document: SettingsDocument, store: SettingsStore) async throws -> SettingsDocument {
        guard !preparing, !quitting else { throw ServiceError.notReady }
        preparing = true; defer { preparing = false }
        let (saved, root) = try store.prepareWorkspace(document)
        guard let selected = saved.workspace, let root else { return saved }
        if let binding {
            guard binding == selected else { throw SettingsError.sourceChanged }
            return saved
        }
        binding = selected
        do {
            let coordinates = try ServiceClient.PersistentStore(root: root, projectID: selected.projectID,
                                                               sessionID: selected.sessionID)
            let model = try makeModel(coordinates, clientID: selected.clientID, settings: store)
            service = model
            await model.start()
            if model.phase != .ready { error = "保存済みの会話を読み込めませんでした。接続状態を確認してください。" }
        } catch {
            self.error = "同梱のサービスを起動できませんでした。保存済みのプロジェクトは保持しています。"
        }
        return saved
    }

    private func makeModel(_ coordinates: ServiceClient.PersistentStore, clientID: String,
                           settings: SettingsStore) throws -> WorkspaceServiceModel {
        if let factory { return try factory(helper(), coordinates, clientID) }
        return try WorkspaceServiceModel(helperURL: helper(), store: coordinates, clientID: clientID,
                                         configurationStore: settings)
    }

    /// Explicit owner transition; no startup or unknown-result automatic retry.
    func configureAndStart(store settings: SettingsStore, reconcilePublication: Bool = false,
                           makeConfiguredTransport: (@Sendable (OwnerServiceLaunch) throws -> any WorkspaceServiceTransport)? = nil,
                           verifyAssembly: () throws -> Void = {
                               try OwnerServiceLaunch.verifyPackagedExecutionHelper(bundle: Bundle.main.bundleURL)
                           }) async {
        guard !preparing, !quitting, !isRecovering, let binding, let service else { return }
        preparing = true; defer { preparing = false }
        var attemptedHistoryMode: HistoryMode?
        do {
            let selected = try settings.load()
            guard selected.workspace == binding, let preferences = selected.preferences,
                  let selection = preferences.executionBinding, let permission = preferences.permission else {
                throw ServiceError.notReady
            }
            let historyMode = preferences.historyMode
            attemptedHistoryMode = historyMode
            let generation = settings.selectionGeneration
            func validateSelection() throws {
                let current = try settings.load()
                guard settings.selectionGeneration == generation, current.workspace == binding,
                      current.preferences == preferences, current.pendingConfiguration == nil else { throw ServiceError.notReady }
            }
            if service.pendingConfiguration != nil || service.pendingRoleConfiguration != nil { throw ServiceError.notReady }
            if service.savedConfigurationSelection != selection
                || (service.snapshot?["configuration"]?["history_mode"]?.string ?? HistoryMode.legacy.rawValue) != historyMode.rawValue {
                await service.configureSelection()
            }
            guard service.savedConfigurationSelection == selection, !service.configurationOutcomeUnknown,
                  service.pendingConfiguration == nil,
                  let revision = service.snapshot?["configuration"]?["configuration_revision"]?.decimal,
                  let policy = service.snapshot?["policy_revision"]?.decimal else { throw ServiceError.notReady }
            try validateSelection()
            try verifyAssembly()
            guard await service.saveAndClose() else { throw ServiceError.notReady }
            try validateSelection()
            let publisher = try BootstrapPublisher(storeURL: service.store.root)
            func validatePublication() throws {
                try validateSelection()
                guard service.snapshot?["configuration"]?["configuration_revision"]?.decimal == revision,
                      (service.snapshot?["configuration"]?["history_mode"]?.string ?? HistoryMode.legacy.rawValue) == historyMode.rawValue,
                      service.snapshot?["policy_revision"]?.decimal == policy else { throw ServiceError.notReady }
            }
            if let previous = try settings.load().bootstrapProof {
                guard reconcilePublication else { throw ServiceError.notReady }
                do {
                    guard case .published = try publisher.reconcile(expected: previous, revalidate: validatePublication) else {
                        throw ServiceError.notReady
                    }
                    try publisher.retire(expected: previous, revalidate: validatePublication)
                } catch BootstrapReconciliationError.absent {
                    // Explicit reconciliation proved no fixed publication exists;
                    // pre-rename or post-retirement failure is safe to retry.
                }
                try settings.recordBootstrapProof(nil)
            }
            let publication = try publisher.publish(binding: binding, selection: selection, permission: permission,
                policyRevision: policy, configurationRevision: revision, historyMode: historyMode, revalidate: validatePublication,
                recordProof: { try settings.recordBootstrapProof($0) })
            guard case .published(let proof) = publication else { throw ServiceError.notReady }
            try validatePublication(); try verifyAssembly()
            let launch = try OwnerServiceLaunch(store: service.store, binding: binding, proof: proof,
                                               permission: permission, policyRevision: policy)
            let executable = try helper(); let clientID = binding.clientID
            try await service.startConfigured(revalidate: validatePublication) {
                if let makeConfiguredTransport { return try makeConfiguredTransport(launch) }
                // The configured owner validates catalogs and initializes the
                // tokenizer before Hello (about six seconds in a cold debug app).
                // Keep ordinary requests and storage-only startup at five seconds.
                var configuration = ServiceClient.Configuration()
                configuration.helloTimeout = .seconds(30)
                return try ServiceClient(helperURL: executable, clientID: clientID, configuration: configuration,
                                  persistentStore: launch.store,
                                  recoveryMode: .resultOnly, ownerLaunch: launch)
            }
            error = service.phase == .ready ? nil : (service.ownerStartupFailure?.japaneseMessage ?? "設定は保存済みです。実行サービスの接続状態を確認してください。")
        } catch {
            self.error = service.ownerStartupFailure?.japaneseMessage ?? (attemptedHistoryMode == .strict10
                ? "strict10 の起動準備を確認できませんでした。~/.polaris/config.toml の [embedding] runtime・model_path・revision と multilingual-e5-small の検証状態を確認するか、履歴を従来の履歴へ戻してください。"
                : "設定の保存結果と本文を保持しています。引継ぎの確認を完了できませんでした。")
        }
    }

    func quit() async -> Bool {
        guard !preparing, !quitting else { return false }
        quitting = true; defer { quitting = false }
        guard let service else { return true }
        let closed = await service.saveAndClose()
        if !closed { error = "終了を取り消しました。下書きの保存状態を確認してください。" }
        return closed
    }

    func reconnect(store: SettingsStore) async {
        guard !preparing, !quitting, !isRecovering, let binding else { return }
        isRecovering = true; preparing = true
        defer { isRecovering = false; preparing = false }
        do {
            let saved = try store.load()
            guard saved.workspace == binding else { throw SettingsError.sourceChanged }
            let (_, root) = try store.prepareWorkspace(saved)
            guard let root else { throw SettingsError.sourceChanged }
            if let service {
                guard service.store.root.path == root.path else { throw SettingsError.sourceChanged }
                await service.reconnect()
            } else {
                let coordinates = try ServiceClient.PersistentStore(root: root, projectID: binding.projectID,
                                                                   sessionID: binding.sessionID)
                let model = try makeModel(coordinates, clientID: binding.clientID, settings: store)
                service = model
                await model.start()
            }
            error = service?.phase == .ready ? nil : "再接続を完了できませんでした。本文と元の要求 ID を保持しています。"
        } catch {
            self.error = (error as? SettingsError)?.errorDescription ?? "同じ保存先への再接続を完了できませんでした。"
        }
    }
    func checkRecoveryStatus() async { await recover { await $0.checkRecoveryStatus() } }
    /// Explicit model change drains the existing owner before changing its saved selection.
    func selectExecution(_ selection: ExecutionBinding, permission: PermissionPreference? = nil, historyMode: HistoryMode? = nil,
                         settings: SettingsStore) async throws -> SettingsDocument {
        guard !preparing, !quitting, !isRecovering, let binding, let service,
              !service.isBusy, !service.runs.contains(where: \.blocksStart),
              !service.configurationOutcomeUnknown, service.pendingConfiguration == nil, service.pendingRoleConfiguration == nil else { throw ServiceError.notReady }
        preparing = true; defer { preparing = false }
        if service.phase != .ready {
            guard await service.restoreStorageOnlyAfterOwnerFailure() else { throw ServiceError.notReady }
        }
        var before = try settings.load()
        guard before.workspace == binding, before.preferences != nil else { throw ServiceError.notReady }
        let nextHistoryMode = historyMode ?? before.preferences!.historyMode
        guard nextHistoryMode != .strict10 || selection.provider == .codex || selection.provider == .openai else {
            throw SettingsError.invalidDocument
        }
        guard await service.saveAndClose() else { throw ServiceError.notReady }
        guard try settings.load() == before else { throw SettingsError.sourceChanged }
        // A failed configured owner can leave an exact bootstrap receipt behind.
        // Retire it only after the owner has exited and before changing the
        // storage-side configuration, so the replacement can publish its own
        // mode and configuration revision.
        if let proof = before.bootstrapProof {
            let publisher = try BootstrapPublisher(storeURL: service.store.root)
            func validatePriorPublication() throws {
                guard try settings.load() == before,
                      service.savedConfigurationSelection == before.preferences?.executionBinding,
                      (service.snapshot?["configuration"]?["history_mode"]?.string ?? HistoryMode.legacy.rawValue) == before.preferences?.historyMode.rawValue else {
                    throw ServiceError.notReady
                }
            }
            do {
                guard case .published = try publisher.reconcile(expected: proof, revalidate: validatePriorPublication) else {
                    throw ServiceError.notReady
                }
                try publisher.retire(expected: proof, revalidate: validatePriorPublication)
            } catch BootstrapReconciliationError.absent {
                // Exact retained-object verification proved no launchable file remains.
            }
            try settings.recordBootstrapProof(nil)
            before = try settings.load()
        }
        var next = before
        next.preferences?.executionBinding = selection
        next.preferences?.historyMode = nextHistoryMode
        next.draft.preferences.executionBinding = selection
        next.draft.preferences.historyMode = nextHistoryMode
        if let permission {
            next.preferences?.permission = permission
            next.draft.preferences.permission = permission
        }
        try settings.save(next)
        try await service.startStorageOnlyAfterClose(revalidate: {
            guard try settings.load() == next else { throw SettingsError.sourceChanged }
        })
        error = nil
        return next
    }
    func reconcileRecoveryRequest() async { await recover { await $0.reconcileRecoveryRequest() } }
    func retryRecoveryResult() async { await recover { await $0.retryRecoveryResult() } }
    func observeRecoveryExit() async { await recover { await $0.observeRecoveryExit() } }

    private func recover(_ action: (WorkspaceServiceModel) async -> Void) async {
        guard !preparing, !quitting, !isRecovering, let service else { return }
        isRecovering = true
        defer { isRecovering = false }
        await action(service)
        error = service.recoveryFailure ? "結果保存の確認を完了できませんでした。元の要求と本文を保持しています。" : nil
    }

}
