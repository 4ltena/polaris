import Foundation
import PolarisSettings

// M4 adapterが値を供給する表示契約。認証情報・実行器・可変providerを持たない。
struct WorkspaceModelConfiguration: Equatable, Sendable, Identifiable {
    let id: String
    let title: String
    let destination: String
    let effort: String
    let unavailableReason: String?
    let supportsAttachments: Bool
}

struct WorkspaceProject: Identifiable, Equatable, Sendable {
    let id: String
    var name: String
    var source: String
    var permission: PermissionPreference?
    var pinned: Bool
    var sectionID: String?
}
struct WorkspaceSection: Identifiable, Equatable, Sendable {
    let id: String
    let name: String
}
struct WorkspaceMessage: Identifiable, Equatable, Sendable {
    let id: String
    let author: String
    let text: String
}
enum WorkspaceInputPhase: String, Sendable {
    case idle, recording, transcribing, sending
    var title: String {
        switch self {
        case .idle: "入力できます"
        case .recording: "録音中"
        case .transcribing: "文字起こし中"
        case .sending: "送信中"
        }
    }
}
struct WorkspaceConversation: Identifiable, Equatable, Sendable {
    let id: String
    var projectID: String?
    let title: String
    let model: WorkspaceModelConfiguration
    var messages: [WorkspaceMessage]
    var phase: WorkspaceInputPhase = .idle
}
enum WorkspaceTaskStatus: String, CaseIterable, Sendable {
    case pending, implementing, review, retry, completed, cancelled, failed, unknown
    var title: String {
        switch self {
        case .pending: "未着手"
        case .implementing: "実装中"
        case .review: "レビュー待ち"
        case .retry: "再試行"
        case .completed: "完了"
        case .cancelled: "取消"
        case .failed: "失敗"
        case .unknown: "状態不明"
        }
    }
}
struct WorkspaceTask: Identifiable, Equatable, Sendable {
    let id: String
    let title: String
    let status: WorkspaceTaskStatus
    let detail: String
    let fileID: String?
}
enum WorkspaceAgentStatus: String, Sendable {
    case queued, loading, running, cancelling, succeeded, failed, cancelled, unknown
    var title: String {
        switch self {
        case .queued: "実行枠を待機"
        case .loading: "モデルをロード中"
        case .running: "実行中"
        case .cancelling: "停止要求中"
        case .succeeded: "完了"
        case .failed: "失敗"
        case .cancelled: "取消"
        case .unknown: "結果不明"
        }
    }
}
struct WorkspaceAgent: Identifiable, Equatable, Sendable {
    let id: String // 受理済みattempt ID。再試行は別ID。
    let title: String
    let summary: String
    let status: WorkspaceAgentStatus
    let model: WorkspaceModelConfiguration
    let taskID: String
    let attempt: Int
}
enum WorkspaceFileVersion: String, CaseIterable, Sendable {
    case current, unstaged, staged, history
    var title: String {
        switch self {
        case .current: "本文・現在"
        case .unstaged: "未ステージ · worktree ↔ index"
        case .staged: "ステージ済み · index ↔ HEAD"
        case .history: "履歴・本文"
        }
    }
}
struct WorkspaceFile: Identifiable, Equatable, Sendable {
    let id: String
    let path: String
    let body: String
    let unstagedDiff: String?
    let stagedDiff: String?
    let modified: Bool
    let staged: Bool
    let unsaved: Bool
    let unavailableReason: String?
}
struct WorkspaceRevision: Identifiable, Equatable, Sendable {
    let id: String
    let parent: String
    let title: String
    let fileID: String
    let body: String
    let diff: String
}
struct WorkspaceGit: Equatable, Sendable {
    let branch: String
    let head: String
    let changeSummary: String
    let revisions: [WorkspaceRevision]
}
struct WorkspaceActivity: Equatable, Sendable {
    let conversationID: String
    let planRevision: String
    let status: String
    let tasks: [WorkspaceTask]
    let agents: [WorkspaceAgent]
    let files: [WorkspaceFile]
    let git: WorkspaceGit?
    var completedCount: Int { tasks.filter { $0.status == .completed }.count }
    var runningCount: Int { agents.filter { $0.status == .running }.count }
    static func empty(_ id: String) -> Self {
        Self(conversationID: id, planRevision: "未作成", status: "作業はまだありません",
             tasks: [], agents: [], files: [], git: nil)
    }
}
struct WorkspaceSnapshot: Equatable, Sendable {
    let isDemo: Bool
    var projects: [WorkspaceProject]
    var sections: [WorkspaceSection]
    var conversations: [WorkspaceConversation]
    let models: [WorkspaceModelConfiguration]
    var activities: [WorkspaceActivity]

    func projects(in section: String) -> [WorkspaceProject] {
        switch section {
        case "pinned": projects.filter(\.pinned)
        case "projects": projects.filter { !$0.pinned }
        default: projects.filter { !$0.pinned && $0.sectionID == section }
        }
    }
    func activity(for conversation: String) -> WorkspaceActivity {
        activities.first { $0.conversationID == conversation } ?? .empty(conversation)
    }
}

struct WorkspaceAttachment: Identifiable, Equatable, Sendable {
    let id: UUID
    let url: URL
    var name: String { url.lastPathComponent }
    init(url: URL) { self.id = UUID(); self.url = url }
}
struct WorkspaceDraft: Equatable, Sendable {
    var text = ""
    var attachments: [WorkspaceAttachment] = []
    var model: WorkspaceModelConfiguration?
    var permission: PermissionPreference?
    var isComposing = false
    var revision = 0
    var showsSend: Bool { !text.isEmpty || !attachments.isEmpty }
    func canSend(model: WorkspaceModelConfiguration, phase: WorkspaceInputPhase) -> Bool {
        phase == .idle && !isComposing && model.unavailableReason == nil
        && (!text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty)
        && (attachments.isEmpty || model.supportsAttachments)
    }
}
struct WorkspaceSubmission: Equatable, Sendable {
    let requestID: UUID
    let conversationID: String
    let text: String
    let attachments: [WorkspaceAttachment]
    let model: WorkspaceModelConfiguration
    let permission: PermissionPreference?
    let draftRevision: Int
}
enum WorkspaceSendResult: Sendable { case accepted, rejected(String) }
enum WorkspaceVoiceAction: Sendable { case start, stop, cancel }
enum WorkspaceAction: Sendable {
    case newConversation(projectID: String?)
    case pin(projectID: String, pinned: Bool)
    case assignSection(projectID: String, sectionID: String?)
    case createSection(projectID: String, name: String)
    case saveProject(WorkspaceProject)
    case removeProject(String)
    case openFolder(String)
    case selectConversation(String)
}
