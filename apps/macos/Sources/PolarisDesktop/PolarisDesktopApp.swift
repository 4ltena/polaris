import AppKit
import SwiftUI
import PolarisSettings

@main
struct PolarisDesktopApp: App {
    @NSApplicationDelegateAdaptor(DesktopDelegate.self) private var delegate
    @StateObject private var model: DesktopModel
    private let installerMode: Bool

    init() {
        let mode = Bundle.main.bundleIdentifier?.hasPrefix("com.polaris.installer.") == true
            || CommandLine.arguments.contains("--installer-mode")
        installerMode = mode
        _model = StateObject(wrappedValue: DesktopModel(arguments: Array(CommandLine.arguments.dropFirst())))
    }

    var body: some Scene {
        Window(installerMode ? "Polarisインストーラ" : "polaris", id: "main") {
            Group {
                if installerMode {
                    InstallerRootView(model: model)
                } else {
                    RootView(model: model, openServiceDemo: { delegate.openServiceDemo() })
                // The prepared next screen is kept in the view hierarchy while the
                // splash is visible. Constrain the root to the splash's actual
                // size, otherwise that hidden screen can make AppKit first show a
                // 960/1280-point window and then shrink it on the next run loop.
                .frame(width: model.isStarting ? 668 : nil,
                       height: model.isStarting ? 413 : nil)
                .frame(minWidth: model.isStarting ? 668 : (model.document.isEditing ? 820 : 1120),
                       minHeight: model.isStarting ? 413 : (model.document.isEditing ? 600 : 720))
                .background(StartupWindowSize(isStarting: model.isStarting, isWorkspace: !model.document.isEditing))
                .onAppear {
                    delegate.startupModel = model
                    DesktopTheme.apply(model.document.draft.preferences.theme)
                }
                .onChange(of: model.document.draft.preferences.theme) { theme in
                    DesktopTheme.apply(theme)
                }
                .task { await model.startOnce() }
                }
            }
            .onAppear { delegate.startupModel = model }
        }
        .defaultSize(width: installerMode ? 920 : 668, height: installerMode ? 640 : 413)
        .windowStyle(.hiddenTitleBar)
        // Do not restore a previous wide workspace frame for a fixed-size splash.
        // The phase view relaxes this to a minimum size once the prepared screen
        // is revealed.
        .windowResizability(installerMode || model.isStarting ? .contentSize : .contentMinSize)
        .commands {
            CommandGroup(replacing: .appSettings) {
                Button("設定…") { delegate.openSettings(model) }
                    .keyboardShortcut(",", modifiers: .command)
            }
            CommandGroup(replacing: .newItem) {}
        }

        Window("polaris — 作業画面のデモ", id: "workspace-demo") {
            WorkspaceFixtureView(logo: model.assets["polaris-banner-white"], release: model.release,
                voiceBackend: delegate.workspaceVoice)
        }
        .defaultSize(width: 1280, height: 780)
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentMinSize)
    }
}

@MainActor
enum DesktopTheme {
    /// The root surface and title bar must use the same dynamic AppKit color.
    static var surfaceColor: NSColor { .windowBackgroundColor }

    static func apply(_ theme: ThemePreference) {
        // SwiftUIのpreferredColorSchemeをnilへ戻すと本文の配色が残るため、
        // AppKitの継承元を一箇所で切り替える。nilはOS設定への追従を復元する。
        switch theme {
        case .system: NSApplication.shared.appearance = nil
        case .light: NSApplication.shared.appearance = NSAppearance(named: .aqua)
        case .dark: NSApplication.shared.appearance = NSAppearance(named: .darkAqua)
        }
    }
}

@MainActor
final class DesktopDelegate: NSObject, NSApplicationDelegate {
    let workspaceVoice = WorkspaceNativeSpeechBackend()
    weak var startupModel: DesktopModel?
    private var serviceDemo: ServiceDemoWindowController?
    private var settingsWindow: SettingsWindowController?
    private var terminationPending = false
    private var managedPreferencesStore: ManagedPreferencesStore?

    func openServiceDemo() {
        if serviceDemo == nil { serviceDemo = ServiceDemoWindowController() }
        serviceDemo?.showWindow(nil)
        serviceDemo?.window?.makeKeyAndOrderFront(nil)
    }

    func openSettings(_ model: DesktopModel) {
        if settingsWindow == nil {
            let presentation = SettingsPresentation(
                preferences: model.document.preferences ?? model.document.draft.preferences,
                workflowMode: Self.workflowMode(model.workspace.service?.workflowStatus),
                releaseTitle: model.release?.title,
                onAction: { [weak self, weak model] action in
                    guard let self, let model else { return }
                    self.applySettings(action, to: model)
                })
            settingsWindow = SettingsWindowController(presentation: presentation)
        }
        refreshSettings(model)
        settingsWindow?.present()
    }

    private func applySettings(_ action: SettingsAction, to model: DesktopModel) {
        guard let presentation = settingsWindow?.presentation, !presentation.isApplying else { return }
        presentation.error = nil
        switch action {
        case .history(let mode):
            guard let selection = model.document.preferences?.executionBinding,
                  model.workspace.service != nil else {
                presentation.error = "作業中の会話を開いてから変更してください。"
                return
            }
            presentation.isApplying = true
            Task { [weak self, weak model] in
                guard let self, let model else { return }
                await model.selectWorkspaceExecution(selection, historyMode: mode)
                let saved = model.document.preferences?.historyMode
                self.refreshSettings(model, error: saved == mode ? nil
                    : (model.saveError ?? "会話履歴の設定を保存できませんでした。"))
            }
        case .workflow(let requested):
            guard let service = model.workspace.service,
                  let workflow = service.workflowStatus else {
                presentation.error = "作業中の会話を開いてから変更してください。"
                return
            }
            presentation.isApplying = true
            Task { [weak self, weak model, weak service] in
                guard let self, let model, let service else { return }
                await service.configureWorkflow(
                    enabled: requested != "disabled",
                    mode: requested == "adaptive" ? "adaptive" : "manual",
                    initialPhase: workflow.phase)
                self.refreshSettings(model, error: Self.workflowMode(service.workflowStatus) == requested
                    ? nil : "作業内容に合わせた情報の使い方を保存できませんでした。")
            }
        case .answerFinalizer(let language, let finalizer):
            let requested: ManagedPreferences
            do { requested = try ManagedPreferences(language: language, finalizer: finalizer) }
            catch {
                presentation.error = "回答の表示設定の組合せを確認してください。"
                return
            }
            do {
                let store = try managedPreferences()
                let previous = try store.load()
                try store.save(requested)
                guard model.updateAnswerFinalizerPreferences(language: language, finalizer: finalizer) else {
                    // The visible document is still authoritative for this UI. Restore
                    // the private service file if its companion write did not commit.
                    try store.save(previous)
                    presentation.error = model.saveError ?? "回答の表示設定を保存できませんでした。"
                    return
                }
                refreshSettings(model)
            } catch {
                presentation.error = "回答の表示設定を保存できませんでした。"
            }
        default:
            presentation.error = "この設定は現在の画面から変更できません。"
        }
    }

    private func refreshSettings(_ model: DesktopModel, error: String? = nil) {
        guard let presentation = settingsWindow?.presentation else { return }
        presentation.preferences = model.document.preferences ?? model.document.draft.preferences
        presentation.workflowMode = Self.workflowMode(model.workspace.service?.workflowStatus)
        let serviceActions: Set<SettingsActionKind> = model.workspace.service == nil ? [] : [.history, .workflow]
        presentation.availableActions = serviceActions.union([.answerFinalizer])
        presentation.isApplying = false
        presentation.error = error
    }

    private func managedPreferences() throws -> ManagedPreferencesStore {
        if let managedPreferencesStore { return managedPreferencesStore }
        let store = try ManagedPreferencesStore()
        managedPreferencesStore = store
        return store
    }

    private static func workflowMode(
        _ status: (enabled: Bool, mode: String, phase: String, revision: UInt64)?
    ) -> String? {
        guard let status else { return nil }
        return status.enabled ? status.mode : "disabled"
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        workspaceVoice.cancel()
        startupModel?.cancelStartupChecks()
        let demo = serviceDemo?.model
        let desktop = startupModel
        guard !terminationPending else { return .terminateLater }
        terminationPending = true
        Task {
            let saved = await desktop?.prepareToQuit() ?? true
            var settled = saved
            if saved, let demo, demo.needsClose { settled = await demo.close() }
            terminationPending = false
            sender.reply(toApplicationShouldTerminate: settled)
        }
        return .terminateLater
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}
