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
enum WorkspaceInputAdmissionState: String, Equatable, Sendable {
    case reserved, classifying, decided, destinationReady = "destination_ready", inputAppended = "input_appended",
         runQueued = "run_queued", queued, running, completed, failed, cancelled, outcomeUnknown = "outcome_unknown"
}
struct WorkspaceInputAdmission: Equatable, Sendable, Identifiable {
    let id: String
    let inputHash: String
    let state: WorkspaceInputAdmissionState
    let sessionRevision: UInt64
    let route: String
    let routeReason: String?
    let destinationSessionID: String?
    let runID: String?
    let attemptID: String?
}
struct WorkspaceConversation: Identifiable, Equatable, Sendable {
    let id: String
    var projectID: String?
    let title: String
    let model: WorkspaceModelConfiguration
    var messages: [WorkspaceMessage]
    var phase: WorkspaceInputPhase = .idle
}
struct WorkspaceQuotaWindow: Identifiable, Equatable, Sendable {
    let id: String
    let title: String
    let usedPercent: Double
    let resetDescription: String

    var boundedUsedPercent: Double { min(max(usedPercent, 0), 100) }
    var remainingPercent: Int { Int((100 - boundedUsedPercent).rounded()) }
}
struct WorkspaceQuotaAccountDisplay: Identifiable, Equatable, Sendable {
    let id: String
    let name: String
    let detail: String
    let windows: [WorkspaceQuotaWindow]
}
struct WorkspaceQuotaDisplay: Equatable, Sendable {
    let status: String
    let accounts: [WorkspaceQuotaAccountDisplay]

    var account: String { accounts.first?.name ?? "" }
    var windows: [WorkspaceQuotaWindow] { accounts.first?.windows ?? [] }

    init(account: String, status: String, windows: [WorkspaceQuotaWindow]) {
        self.status = status
        accounts = [WorkspaceQuotaAccountDisplay(id: account, name: account, detail: "", windows: windows)]
    }

    init(status: String, accounts: [WorkspaceQuotaAccountDisplay]) {
        self.status = status
        self.accounts = accounts
    }
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
    let name: String
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
    /// This is a direct projection of the v2 service payload.  It deliberately
    /// carries explicit states rather than deriving completion from counts.
    let orchestration: WorkspaceOrchestration?
    init(conversationID: String, planRevision: String, status: String, tasks: [WorkspaceTask],
         agents: [WorkspaceAgent], files: [WorkspaceFile], git: WorkspaceGit?,
         orchestration: WorkspaceOrchestration? = nil) {
        self.conversationID = conversationID; self.planRevision = planRevision; self.status = status
        self.tasks = tasks; self.agents = agents; self.files = files; self.git = git
        self.orchestration = orchestration
    }
    var completedCount: Int { tasks.filter { $0.status == .completed }.count }
    var runningCount: Int { agents.filter { $0.status == .running }.count }
    static func empty(_ id: String) -> Self {
        Self(conversationID: id, planRevision: "未作成", status: "作業はまだありません",
             tasks: [], agents: [], files: [], git: nil, orchestration: nil)
    }
}

enum WorkspaceConvergenceState: String, Equatable, Sendable {
    case unavailable, pending, blocked, allowed
}

struct WorkspaceArtifactReference: Equatable, Sendable {
    let id: String
    let contentHash: String
}

struct WorkspaceAcceptanceEvidence: Equatable, Sendable, Identifiable {
    let id: String
    let state: String
    let evidenceCount: UInt64
}

struct WorkspaceEvidenceSummary: Equatable, Sendable {
    let passed: UInt64
    let failed: UInt64
    let stale: UInt64
    let unavailable: UInt64
}

struct WorkspaceConvergence: Equatable, Sendable {
    let state: WorkspaceConvergenceState
    let requiredCriteria: UInt64
    let unresolvedGaps: UInt64
}

struct WorkspaceWorkflowProjection: Equatable, Sendable {
    let phase: String
    let step: String
    let activeFeature: WorkspaceArtifactReference?
    let activePlan: WorkspaceArtifactReference?
    let activeTask: WorkspaceArtifactReference?
    let acceptance: [WorkspaceAcceptanceEvidence]
    let evidence: WorkspaceEvidenceSummary
    let convergence: WorkspaceConvergence
}

enum WorkspaceManagedSessionState: String, Equatable, Sendable { case idle, queued, running, closed }
enum WorkspaceManagedSessionKind: String, Equatable, Sendable { case current, sidechat }
struct WorkspaceManagedSession: Equatable, Sendable, Identifiable {
    let id: String
    let kind: WorkspaceManagedSessionKind
    let state: WorkspaceManagedSessionState
    let sourceSessionID: String?
    let sessionRevision: UInt64?
    let goalRevision: UInt64?
}
struct WorkspaceSessionManagerProjection: Equatable, Sendable {
    let revision: UInt64
    let sessions: [WorkspaceManagedSession]
    let sessionTotal: UInt64
    let sessionOmitted: Bool
    let sidechat: String
    let queuedAdmissions: UInt64
    let activeRuns: UInt64
}
enum WorkspaceSubagentState: String, Equatable, Sendable {
    case queued, running, waiting, cancelling, succeeded, failed, cancelled, outcomeUnknown = "outcome_unknown"
}
struct WorkspaceSubagentProjection: Equatable, Sendable, Identifiable {
    let id: String
    let rootRunID: String
    let rootAttemptID: String
    let controlRevision: UInt64
    let logicalChildID: String
    let displayName: String
    let role: String
    let state: WorkspaceSubagentState
    let activity: String?
}
enum WorkspaceSubagentWaitState: String, Equatable, Sendable { case unchanged, updated }
struct WorkspaceSubagentWaitResult: Equatable, Sendable {
    let rootRunID: String
    let rootAttemptID: String
    let revision: UInt64
    let cursor: UInt64
    let state: WorkspaceSubagentWaitState
    let childTotal: UInt64
    let childOmitted: Bool
    let children: [WorkspaceSubagentProjection]
}
enum WorkspaceSubagentCancelDisposition: String, Equatable, Sendable { case requested, alreadyRequested = "already_requested", alreadyTerminal = "already_terminal" }
enum WorkspaceSubagentRuntimeEffect: String, Equatable, Sendable { case unavailable, requested, outcomeUnknown = "outcome_unknown" }
struct WorkspaceSubagentCancelResult: Equatable, Sendable {
    let rootRunID: String
    let rootAttemptID: String
    let logicalChildID: String
    let childAttemptID: String
    let revision: UInt64
    let cursor: UInt64
    let disposition: WorkspaceSubagentCancelDisposition
    let state: WorkspaceSubagentState
    let runtimeEffect: WorkspaceSubagentRuntimeEffect
}
enum WorkspaceMcpLeaseState: String, Equatable, Sendable {
    case reserved, active, releasing, released, expired, outcomeUnknown = "outcome_unknown"
}
struct WorkspaceMcpLeaseProjection: Equatable, Sendable, Identifiable {
    let id: String
    let serverID: String
    let holderRunID: String
    let holderAttemptID: String
    let state: WorkspaceMcpLeaseState
    let expiresAtUnixMilliseconds: UInt64
}
enum WorkspaceCommonRunnerState: String, Equatable, Sendable {
    case unavailable, idle, preparing, running, completed, failed, cleanupPending = "cleanup_pending", outcomeUnknown = "outcome_unknown"
}
enum WorkspaceClarificationState: String, Equatable, Sendable { case unavailable, none, pending, resolved }
struct WorkspaceClarificationOption: Equatable, Sendable, Identifiable { let id: String; let label: String }
struct WorkspaceClarificationFreeText: Equatable, Sendable { let allowed: Bool; let required: Bool; let maxBytes: UInt64 }
struct WorkspaceClarificationQuestion: Equatable, Sendable, Identifiable {
    let id: String; let text: String; let options: [WorkspaceClarificationOption]
    let freeText: WorkspaceClarificationFreeText; let answered: Bool
}
struct WorkspaceClarification: Equatable, Sendable {
    let state: WorkspaceClarificationState; let questions: [WorkspaceClarificationQuestion]; let unansweredCount: UInt64
}
struct WorkspaceCommonRunnerProjection: Equatable, Sendable {
    let state: WorkspaceCommonRunnerState
    let caseID: String?
    let completedCases: UInt64
    let failedCases: UInt64
    let cleanupPending: Bool
}
struct WorkspaceOrchestration: Equatable, Sendable {
    let workflow: WorkspaceWorkflowProjection
    let sessionManager: WorkspaceSessionManagerProjection
    let subagents: [WorkspaceSubagentProjection]
    let subagentTotal: UInt64
    let subagentOmitted: Bool
    let mcpLeases: [WorkspaceMcpLeaseProjection]
    let mcpLeaseTotal: UInt64
    let mcpLeaseOmitted: Bool
  let commonRunner: WorkspaceCommonRunnerProjection?
  let clarification: WorkspaceClarification
}
struct WorkspaceSnapshot: Equatable, Sendable {
    let isDemo: Bool
    var projects: [WorkspaceProject]
    var sections: [WorkspaceSection]
    var conversations: [WorkspaceConversation]
    let models: [WorkspaceModelConfiguration]
    var activities: [WorkspaceActivity]
    let quota: WorkspaceQuotaDisplay?

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
