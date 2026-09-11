import AppKit
import SwiftUI
import PolarisSettings

// 親は明示したデモ入口でだけこのViewを使う。WorkspaceView自体はfixtureを生成しない。
struct WorkspaceFixtureView: View {
    let logo: NSImage?
    let release: ReleaseConfiguration?
    var voiceBackend: (any WorkspaceSpeechBackend)? = nil
    @StateObject private var demo = WorkspaceDemoStore()
    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("デモの操作条件").font(.caption)
                Button("作業0件") { demo.setTaskCount(0) }
                Button("採用例") { demo.setTaskCount(12) }
                Button("作業200件") { demo.setTaskCount(200) }
                Spacer()
                Text(demo.notice).font(.caption).foregroundStyle(.secondary).lineLimit(2)
            }.padding(8)
            Divider()
            WorkspaceView(snapshot: demo.snapshot, state: demo.state, logo: logo, release: release,
                onAction: demo.perform, onSend: { demo.send($0) },
                onVoice: voiceBackend == nil ? nil : { demo.state.voice($0, $1) })
        }
        .onAppear { if let voiceBackend { demo.state.enableVoice(backend: voiceBackend) } }
        .onDisappear { demo.state.cancelVoice() }
    }
}

@MainActor
final class WorkspaceDemoStore: ObservableObject {
    @Published var snapshot = WorkspaceFixture.snapshot()
    @Published var notice = "画面内だけの表示例です。"
    let state = WorkspaceState(conversationID: "demo-chat")
    init() {
        state.expandedProjects = ["demo-project"]
        state.openFile("schema", conversationID: "demo-chat")
    }
    func send(_ request: WorkspaceSubmission) -> WorkspaceSendResult {
        guard let index = snapshot.conversations.firstIndex(where: { $0.id == request.conversationID }) else {
            return .rejected("送信先の会話がありません。下書きは保持しています。")
        }
        snapshot.conversations[index].messages.append(WorkspaceMessage(id: request.requestID.uuidString,
            author: "あなた · デモ", text: request.text.isEmpty ? "添付の表示例（\(request.attachments.count)件）" : request.text))
        notice = "デモ内に入力を表示しました。実際の送信はしていません。"
        return .accepted
    }
    func setTaskCount(_ count: Int) {
        guard let id = state.selectedConversationID else { return }
        let fixture = WorkspaceFixture.activity(id: id, count: count)
        snapshot.activities.removeAll { $0.conversationID == id }
        snapshot.activities.append(fixture)
    }
    func perform(_ action: WorkspaceAction) {
        switch action {
        case .selectConversation: break
        case .newConversation(let projectID):
            let id = UUID().uuidString
            snapshot.conversations.append(WorkspaceConversation(id: id, projectID: projectID,
                title: "新しい会話 · デモ", model: WorkspaceFixture.cloud, messages: []))
            state.selectedConversationID = id
            if let projectID { state.expandedProjects.insert(projectID) }
        case .pin(let id, let pinned):
            updateProject(id) { $0.pinned = pinned }
        case .assignSection(let id, let sectionID):
            updateProject(id) { $0.sectionID = sectionID; $0.pinned = false }
        case .createSection(let id, let name):
            let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { return }
            let sectionID = snapshot.sections.first { $0.name == trimmed }?.id ?? UUID().uuidString
            if !snapshot.sections.contains(where: { $0.id == sectionID }) {
                snapshot.sections.append(WorkspaceSection(id: sectionID, name: trimmed))
            }
            updateProject(id) { $0.sectionID = sectionID; $0.pinned = false }
        case .saveProject(let project):
            if let index = snapshot.projects.firstIndex(where: { $0.id == project.id }) { snapshot.projects[index] = project }
            else { snapshot.projects.append(project) }
        case .removeProject(let id):
            snapshot.projects.removeAll { $0.id == id }
            for index in snapshot.conversations.indices where snapshot.conversations[index].projectID == id {
                snapshot.conversations[index].projectID = nil
            }
            notice = "デモの登録を外しました。会話は未分類に保持しています。"
        case .openFolder:
            notice = "フォルダを開く操作はデモでは未接続です。"
        }
    }
    private func updateProject(_ id: String, _ edit: (inout WorkspaceProject) -> Void) {
        guard let index = snapshot.projects.firstIndex(where: { $0.id == id }) else { return }
        edit(&snapshot.projects[index])
    }
}

enum WorkspaceFixture {
    static let cloud = WorkspaceModelConfiguration(id: "gpt-6-astra", title: "GPT-6 Astra",
        destination: "クラウド・表示例", effort: "medium", unavailableReason: nil, supportsAttachments: true)
    static let local = WorkspaceModelConfiguration(id: "demo-local", title: "ローカルモデル例",
        destination: "ローカル・未検出の表示例", effort: "既定", unavailableReason: nil, supportsAttachments: false)
    static func snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot(isDemo: true,
            projects: [WorkspaceProject(id: "demo-project", name: "機材貸出", source: "/デモ/機材貸出",
                permission: .readOnly, pinned: true, sectionID: "demo-section"),
                WorkspaceProject(id: "demo-notes", name: "設計メモ", source: "/デモ/設計メモ",
                    permission: .readOnly, pinned: false, sectionID: "demo-section")],
            sections: [WorkspaceSection(id: "demo-section", name: "設計")],
            conversations: [WorkspaceConversation(id: "demo-chat", projectID: "demo-project",
                title: "貸出データの設計 · デモ", model: cloud, messages: [
                    WorkspaceMessage(id: "m1", author: "あなた · デモ", text: "予約が重複しないよう、データの構成を整理したい。"),
                    WorkspaceMessage(id: "m2", author: "polaris · デモ", text: "利用者、機材、予約、貸出を分けて整理します。\n\n右側の資料と作業状態は操作確認用の表示例です。実際のファイルやモデルには接続していません。")]),
                WorkspaceConversation(id: "demo-chat-b", projectID: "demo-project", title: "保存した決定 · デモ", model: local, messages: [])],
            models: [cloud, local], activities: [activity(id: "demo-chat", count: 12)])
    }
    static func activity(id: String, count: Int) -> WorkspaceActivity {
        let statuses: [WorkspaceTaskStatus] = [.completed, .completed, .implementing, .review, .retry, .pending, .cancelled, .failed, .unknown]
        let titles = ["利用者と機材を分ける", "予約の状態を整理", "予約の制約を定義", "設計のレビュー", "指摘の再確認"]
        let tasks = (0..<max(0, count)).map { index in
            WorkspaceTask(id: "task-\(index)", title: index < titles.count ? titles[index] : "確認項目 \(index + 1)",
                status: statuses[index % statuses.count], detail: "デモの作業詳細。受入条件と観測結果をここへ表示します。取消・失敗・不明は完了に数えません。",
                fileID: "schema")
        }
        return WorkspaceActivity(conversationID: id, planRevision: "デモ版1 · \(count)件", status: "表示例",
            tasks: tasks, agents: count == 0 ? [] : [
                WorkspaceAgent(id: "attempt-1", title: "決定の抽出", summary: "保存した決定と前提を整理する表示例。",
                    status: .succeeded, model: local, taskID: "task-0", attempt: 1),
                WorkspaceAgent(id: "attempt-2", title: "制約の照合", summary: "予約期間と取消時の条件を確認する表示例。",
                    status: .running, model: local, taskID: "task-\(min(2, count - 1))", attempt: 2)],
            files: [WorkspaceFile(id: "schema", path: "schema.sql", body: "-- デモの本文\nCREATE TABLE reservations (\n    id INTEGER PRIMARY KEY,\n    equipment_id INTEGER NOT NULL\n);",
                unstagedDiff: "-- worktree 対 index（デモ）\n+ equipment_id INTEGER NOT NULL", stagedDiff: "-- index 対 HEAD（デモ）\n+ id INTEGER PRIMARY KEY",
                modified: true, staged: true, unsaved: false, unavailableReason: nil),
                WorkspaceFile(id: "design", path: "docs/設計.md", body: "# 貸出データの設計\n\n利用者、機材、予約、貸出を分けて管理する。\n\nこれは表示例です。",
                    unstagedDiff: nil, stagedDiff: nil, modified: false, staged: false, unsaved: true, unavailableReason: nil)],
            git: WorkspaceGit(branch: "feature/data-model（デモ）", head: "3f7b1d2", changeSummary: "+41 −7（例）",
                revisions: [WorkspaceRevision(id: "3f7b1d2", parent: "2e6a0c1", title: "予約テーブルの構成 · デモ", fileID: "schema",
                    body: "-- 履歴の本文（デモ）\nCREATE TABLE reservations (id INTEGER PRIMARY KEY);",
                    diff: "-- 3f7b1d2 対 2e6a0c1（デモ）\n+ CREATE TABLE reservations (id INTEGER PRIMARY KEY);")]))
    }
}
