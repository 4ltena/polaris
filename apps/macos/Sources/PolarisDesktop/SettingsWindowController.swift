import AppKit
import SwiftUI
import PolarisSettings

enum SettingsActionKind: Hashable {
    case theme, history, permission, model, localModel, project, setup
}

enum SettingsAction {
    case theme(ThemePreference), history(HistoryMode), permission(PermissionPreference?)
    case model, localModel, project, setup

    var kind: SettingsActionKind {
        switch self {
        case .theme: .theme
        case .history: .history
        case .permission: .permission
        case .model: .model
        case .localModel: .localModel
        case .project: .project
        case .setup: .setup
        }
    }
}

/// 表示値と要求だけを扱う。保存・認証・実行の所有者は既存のhostへ残す。
@MainActor
final class SettingsPresentation: ObservableObject {
    @Published var preferences: Preferences
    @Published var category: SettingsCategory = .general
    @Published var query = ""
    @Published var error: String?
    let releaseTitle: String?
    let isPreview: Bool
    let availableActions: Set<SettingsActionKind>
    private let onAction: ((SettingsAction) -> Void)?

    init(preferences: Preferences, releaseTitle: String? = nil, isPreview: Bool = false,
         availableActions: Set<SettingsActionKind> = [], onAction: ((SettingsAction) -> Void)? = nil) {
        self.preferences = preferences
        self.releaseTitle = releaseTitle
        self.isPreview = isPreview
        self.availableActions = availableActions
        self.onAction = onAction
    }

    var isSearching: Bool { !query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    var visibleItems: [SettingsItem] {
        SettingsCatalog.matching(query, category: isSearching ? nil : category)
    }
    func canPerform(_ kind: SettingsActionKind) -> Bool {
        onAction != nil && availableActions.contains(kind)
    }
    func perform(_ action: SettingsAction) {
        guard canPerform(action.kind) else { return }
        onAction?(action)
    }
    func select(_ category: SettingsCategory) {
        self.category = category
        query = ""
    }
}

@MainActor
final class SettingsWindowController: NSWindowController {
    let presentation: SettingsPresentation

    init(presentation: SettingsPresentation) {
        self.presentation = presentation
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1040, height: 740),
            styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        super.init(window: window)
        window.isReleasedWhenClosed = false
        window.title = presentation.isPreview ? "設定 — polaris（UIプレビュー）" : "設定 — polaris"
        window.contentMinSize = NSSize(width: 820, height: 560)
        window.backgroundColor = .windowBackgroundColor
        window.titlebarAppearsTransparent = true
        window.contentView = NSHostingView(rootView: SettingsOverviewView(presentation: presentation))
        window.center()
    }

    required init?(coder: NSCoder) { nil }

    func present() {
        showWindow(nil)
        window?.makeKeyAndOrderFront(nil)
    }
}
