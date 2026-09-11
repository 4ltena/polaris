import AppKit
import SwiftUI
import PolarisSettings

@main
struct PolarisDesktopApp: App {
    @NSApplicationDelegateAdaptor(DesktopDelegate.self) private var delegate
    @StateObject private var model = DesktopModel()

    var body: some Scene {
        Window("polaris", id: "welcome") {
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
        .defaultSize(width: 668, height: 413)
        .windowStyle(.hiddenTitleBar)
        // Do not restore a previous wide workspace frame for a fixed-size splash.
        // The phase view relaxes this to a minimum size once the prepared screen
        // is revealed.
        .windowResizability(model.isStarting ? .contentSize : .contentMinSize)
        .commands { CommandGroup(replacing: .newItem) {} }

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
    private var terminationPending = false

    func openServiceDemo() {
        if serviceDemo == nil { serviceDemo = ServiceDemoWindowController() }
        serviceDemo?.showWindow(nil)
        serviceDemo?.window?.makeKeyAndOrderFront(nil)
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
