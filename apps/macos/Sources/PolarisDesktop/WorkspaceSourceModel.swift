import Combine
import Foundation

/// Owns only reply freshness and projection; service owns source authority.
@MainActor final class WorkspaceSourceModel: ObservableObject {
    @Published private(set) var files: [WorkspaceFile] = []
    @Published private(set) var git: WorkspaceGit?
    @Published private(set) var unavailableReason: String?
    @Published private(set) var notices: [String] = []
    private var generation = UUID()
    func beginReload() -> UUID { let token = UUID(); generation = token; unavailableReason = nil; return token }
    func apply(_ data: Data, generation token: UUID) {
        guard generation == token else { return }
        do {
            let reply = try WorkspaceSourceReader.decode(data); notices = reply.notices
            guard reply.state == .ready else { files = []; git = nil; unavailableReason = reply.git.reason ?? "ファイルを安全に読み取れませんでした。"; return }
            files = reply.files.map { WorkspaceFile(id: $0.id, path: $0.path, body: $0.body, unstagedDiff: $0.unstagedDiff, stagedDiff: $0.stagedDiff,
                modified: $0.modified, staged: $0.staged, unsaved: $0.unsaved, unavailableReason: $0.unavailableReason) }
            if reply.git.state == "ready", let branch = reply.git.branch, let head = reply.git.head, let summary = reply.git.changeSummary {
                git = WorkspaceGit(branch: branch, head: head, changeSummary: summary, revisions: reply.git.revisions.map {
                    WorkspaceRevision(id: $0.id, parent: $0.parent, title: $0.title, fileID: $0.fileID, body: $0.body, diff: $0.diff) })
            } else { git = nil; unavailableReason = reply.git.reason ?? unavailableReason }
        } catch { files = []; git = nil; notices = []; unavailableReason = "サービスからファイル情報を安全に受け取れませんでした。" }
    }
    func fail(generation token: UUID) {
        guard generation == token else { return }
        files = []; git = nil; notices = []
        unavailableReason = "ファイル情報を安全に読み取れませんでした。"
    }
    func invalidate() { generation = UUID(); files = []; git = nil; unavailableReason = nil; notices = [] }
}
