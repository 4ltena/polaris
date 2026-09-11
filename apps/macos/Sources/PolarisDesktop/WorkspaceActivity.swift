import SwiftUI

struct WorkspaceActivityView: View {
    let activity: WorkspaceActivity
    @ObservedObject var state: WorkspaceState
    @State private var tasksOpen = false
    @State private var agentsOpen = false
    @State private var historyOpen = false
    @State private var inspectedTaskID: String?
    @State private var expandedTaskID: String?
    @FocusState private var focused: String?
    private var inspected: WorkspaceTask? { activity.tasks.first { $0.id == inspectedTaskID } }
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text("現在の作業").font(.headline)
                Text(activity.status).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                Spacer(minLength: 0)
                Button("子 \(activity.runningCount)") { agentsOpen.toggle() }
                    .focused($focused, equals: "agents")
                    .accessibilityLabel("実行中の子\(activity.runningCount)件、累計呼出\(activity.agents.count)回")
                    .popover(isPresented: $agentsOpen, arrowEdge: .top) { agentPopover }
                Button("作業一覧") { tasksOpen.toggle() }.focused($focused, equals: "tasks")
                    .popover(isPresented: $tasksOpen, arrowEdge: .top) { taskPopover }
            }
            HStack {
                Text("\(activity.completedCount) / \(activity.tasks.count) 完了")
                Spacer()
                Text("残り \(activity.tasks.count - activity.completedCount) 件")
            }.font(.caption).foregroundStyle(.secondary)
            HStack(spacing: 5) {
                ForEach(Array(activity.tasks.prefix(16))) { task in
                    Button {
                        inspectedTaskID = inspectedTaskID == task.id ? nil : task.id
                    } label: { WorkspaceTaskSquare(status: task.status) }
                        .buttonStyle(.plain).padding(2).contentShape(Rectangle())
                        .accessibilityLabel("\(task.title)、\(task.status.title)")
                        .help("\(task.title)：\(task.status.title)")
                }
                if activity.tasks.count > 16 {
                    Button("ほか\(activity.tasks.count - 16)件") { tasksOpen = true }
                        .font(.caption)
                }
                if activity.tasks.isEmpty { Text("作業はまだありません").font(.caption).foregroundStyle(.secondary) }
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    if let inspected {
                        HStack {
                            Text("\(inspected.title) · \(inspected.status.title)")
                            Spacer()
                            Button("詳細") { expandedTaskID = inspected.id; tasksOpen = true }
                        }
                    } else if let current = activity.tasks.first(where: { $0.status == .implementing }) {
                        Label(current.title, systemImage: "circle.dotted").font(.headline)
                    }
                    ForEach(Array(activity.agents.filter { $0.status == .running || $0.status == .cancelling }.prefix(3))) { agent in
                        HStack {
                            Text(agent.title).lineLimit(1)
                            Spacer()
                            Text(agent.status.title).foregroundStyle(.secondary)
                        }.font(.caption)
                    }
                    Text("計画版：\(activity.planRevision)").font(.caption2).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
            Divider()
            if let git = activity.git {
                Button { historyOpen.toggle() } label: {
                    HStack {
                        Label(git.branch, systemImage: "arrow.triangle.branch").lineLimit(1)
                        Spacer(minLength: 3)
                        Text(git.changeSummary).lineLimit(1)
                        Text("\(git.head) · HEAD").font(.caption).lineLimit(1)
                    }
                }.buttonStyle(WorkspaceRowStyle()).focused($focused, equals: "git")
                    .accessibilityLabel("Git履歴を表示。\(git.branch)、\(git.changeSummary)、HEAD \(git.head)")
                    .popover(isPresented: $historyOpen, arrowEdge: .top) { historyPopover(git) }
            } else { Text("Git情報は未取得です").font(.caption).foregroundStyle(.secondary) }
        }.padding(12)
        .onChange(of: activity.conversationID) { _ in
            tasksOpen = false; agentsOpen = false; historyOpen = false
            inspectedTaskID = nil; expandedTaskID = nil
        }
    }
    private var taskPopover: some View {
        popover("作業一覧 · \(activity.tasks.count)件", close: closeTasks) {
            if activity.tasks.isEmpty { Text("作業はまだありません").padding(12) }
            ForEach(activity.tasks) { task in
                VStack(alignment: .leading, spacing: 8) {
                    Button { expandedTaskID = expandedTaskID == task.id ? nil : task.id } label: {
                        HStack {
                            WorkspaceTaskSquare(status: task.status)
                            Text(task.title)
                            Spacer()
                            Text(task.status.title).foregroundStyle(.secondary)
                        }
                    }.buttonStyle(WorkspaceRowStyle(selected: expandedTaskID == task.id))
                    if expandedTaskID == task.id {
                        Text(task.detail).textSelection(.enabled).padding(.horizontal, 10)
                        if let fileID = task.fileID {
                            Button("関連ファイルを開く") {
                                tasksOpen = false
                                state.openFile(fileID, conversationID: activity.conversationID)
                            }.padding(.horizontal, 10)
                        }
                    }
                }
            }
        }
    }
    private var agentPopover: some View {
        popover("子の実行 · 累計\(activity.agents.count)回", close: {
            agentsOpen = false; focused = "agents"
        }) {
            if activity.agents.isEmpty { Text("子の呼出はありません").padding(12) }
            ForEach(activity.agents) { agent in
                VStack(alignment: .leading, spacing: 6) {
                    Text(agent.title).font(.headline)
                    Text(agent.summary)
                    Text("\(agent.status.title) · " + (agent.attempt > 0 ? "試行\(agent.attempt)" : "試行番号未取得")).foregroundStyle(.secondary)
                    Text("\(agent.model.title) · \(agent.model.destination) · \(agent.model.effort)").font(.caption)
                    Text("呼出ID：\(agent.id)").font(.caption2).textSelection(.enabled)
                    Button("関連する作業") {
                        expandedTaskID = agent.taskID; agentsOpen = false; tasksOpen = true
                    }
                }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
                Divider()
            }
        }
    }
    private func historyPopover(_ git: WorkspaceGit) -> some View {
        popover("Git履歴", close: { historyOpen = false; focused = "git" }) {
            if git.revisions.isEmpty { Text("履歴は未取得です").padding(12) }
            ForEach(git.revisions) { revision in
                Button {
                    historyOpen = false
                    state.openFile(revision.fileID, conversationID: activity.conversationID, revisionID: revision.id)
                    var selected = state.fileSelections[activity.conversationID] ?? WorkspaceFileSelection()
                    selected.historyDiff.insert(revision.fileID)
                    state.fileSelections[activity.conversationID] = selected
                } label: {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(revision.title)
                        Text("\(revision.id) 対 親 \(revision.parent)").font(.caption).foregroundStyle(.secondary)
                    }
                }.buttonStyle(WorkspaceRowStyle())
            }
        }
    }
    private func closeTasks() { tasksOpen = false; focused = "tasks" }
    private func popover<Content: View>(_ title: String, close: @escaping () -> Void,
                                       @ViewBuilder content: () -> Content) -> some View {
        VStack(spacing: 0) {
            HStack {
                Text(title).font(.headline)
                Spacer()
                Button("閉じる", action: close).keyboardShortcut(.cancelAction)
            }.padding(14)
            Divider()
            ScrollView { LazyVStack(alignment: .leading, spacing: 4, content: content).padding(8) }
        }.frame(width: 420, height: 410).onExitCommand(perform: close)
    }
}

struct WorkspaceTaskSquare: View {
    let status: WorkspaceTaskStatus
    var body: some View {
        Rectangle().fill(fill).frame(width: 16, height: 16)
            .overlay(Rectangle().strokeBorder(border, style: StrokeStyle(lineWidth: 2,
                dash: status == .unknown || status == .cancelled ? [2, 2] : [])))
            .accessibilityLabel(status.title)
    }
    private var fill: Color {
        switch status {
        case .completed: .green
        case .review: .yellow
        case .retry, .failed: .red
        case .pending: Color(nsColor: .tertiaryLabelColor)
        case .implementing, .cancelled, .unknown: .clear
        }
    }
    private var border: Color {
        switch status {
        case .implementing: .blue
        case .cancelled, .unknown: .secondary
        default: .clear
        }
    }
}
