import AppKit
import SwiftUI
import PolarisSettings

enum SettingsActionKind: Hashable {
    case theme, history, workflow, permission, model, localModel, project, answerFinalizer
}

enum SettingsAction {
    case theme(ThemePreference), history(HistoryMode), permission(PermissionPreference?)
    case workflow(String)
    case answerFinalizer(PresentationLanguagePreference, AnswerFinalizerPreference)
    case model, localModel, project

    var kind: SettingsActionKind {
        switch self {
        case .theme: .theme
        case .history: .history
        case .workflow: .workflow
        case .answerFinalizer: .answerFinalizer
        case .permission: .permission
        case .model: .model
        case .localModel: .localModel
        case .project: .project
        }
    }
}

/// 表示値と要求だけを扱う。保存・認証・実行の所有者は既存のhostへ残す。
@MainActor
final class SettingsPresentation: ObservableObject {
    @Published var preferences: Preferences
    @Published var workflowMode: String?
    @Published var category: SettingsCategory = .general
    @Published var query = ""
    @Published var error: String?
    @Published var isApplying = false
    let releaseTitle: String?
    let isPreview: Bool
    @Published var availableActions: Set<SettingsActionKind>
    private let onAction: ((SettingsAction) -> Void)?

    init(preferences: Preferences, workflowMode: String? = nil,
         releaseTitle: String? = nil, isPreview: Bool = false,
         availableActions: Set<SettingsActionKind> = [], onAction: ((SettingsAction) -> Void)? = nil) {
        self.preferences = preferences
        self.workflowMode = workflowMode
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
        !isApplying && onAction != nil && availableActions.contains(kind)
    }
    func perform(_ action: SettingsAction) {
        guard canPerform(action.kind) else { return }
        onAction?(action)
    }

    func setPresentationLanguage(_ language: PresentationLanguagePreference) {
        var finalizer = preferences.answerFinalizer
        if language != .ja { finalizer.translation = TranslationPreference() }
        perform(.answerFinalizer(language, finalizer))
    }

    func setFinalizerMode(_ mode: AnswerFinalizerModePreference) {
        guard mode == .off || mode == .shadow else { return }
        var finalizer = preferences.answerFinalizer
        finalizer.mode = mode
        switch mode {
        case .off:
            finalizer.judgeProfile = nil
            finalizer.externalDisclosure = false
            finalizer.translation = TranslationPreference()
        case .shadow:
            finalizer.judgeProfile = .fulfillmentV1
            if preferences.presentationLanguage != .ja { finalizer.translation = TranslationPreference() }
        case .enforce: return
        }
        perform(.answerFinalizer(preferences.presentationLanguage, finalizer))
    }

    func setExternalDisclosure(_ allowed: Bool) {
        guard preferences.answerFinalizer.mode == .shadow else { return }
        var finalizer = preferences.answerFinalizer
        finalizer.externalDisclosure = allowed
        perform(.answerFinalizer(preferences.presentationLanguage, finalizer))
    }

    func setTranslationRequested(_ requested: Bool) {
        guard preferences.presentationLanguage == .ja,
              preferences.answerFinalizer.mode == .shadow else { return }
        var finalizer = preferences.answerFinalizer
        finalizer.translation.activation = requested ? .requested : .disabled
        finalizer.translation.profile = requested ? .catTranslate3_3b : nil
        perform(.answerFinalizer(preferences.presentationLanguage, finalizer))
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
        // NSHostingView may apply its fitting width when the window is ordered
        // front. Keep the controller-owned size so a resized settings window
        // does not collapse back to its SwiftUI minimum on reopen.
        let frame = window?.frame
        showWindow(nil)
        if let frame { window?.setFrame(frame, display: false) }
        window?.makeKeyAndOrderFront(nil)
    }
}
