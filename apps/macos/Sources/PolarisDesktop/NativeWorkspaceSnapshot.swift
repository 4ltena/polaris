import Foundation
import PolarisSettings

/// Durable service values are the only source of task and run status.
enum NativeWorkspaceSnapshot {
    static func selectedModel(_ selection: ExecutionBinding?, fallback: ServiceValue?) -> WorkspaceModelConfiguration {
        guard let selection else { return model(fallback) }
        let provider = selection.provider.rawValue
        let id = selection.localEndpoint.map { provider + "|" + $0 + "|" + selection.model }
            ?? provider + "|" + selection.model
        return WorkspaceModelConfiguration(id: id, title: selection.model,
            destination: provider, effort: selection.storedEffort, unavailableReason: nil,
            supportsAttachments: true)
    }

    static func model(_ value: ServiceValue?) -> WorkspaceModelConfiguration {
        let name = value?["model"]?.string ?? "未設定"
        return WorkspaceModelConfiguration(id: name, title: name,
            destination: value?["provider"]?.string ?? "未設定",
            effort: value?["effort"]?.string ?? "未設定", unavailableReason: nil,
            supportsAttachments: true)
    }

    static func make(binding: WorkspaceBinding?, payload: ServiceValue?,
                     files: [WorkspaceFile] = [], git: WorkspaceGit? = nil) -> WorkspaceSnapshot {
        guard let binding else {
            return WorkspaceSnapshot(isDemo: false, projects: [], sections: [], conversations: [], models: [], activities: [])
        }
        let configuration = model(payload?["configuration"])
        let tasks = values(payload?["tasks"]).compactMap { value -> WorkspaceTask? in
            guard let id = value["task_id"]?.string, let title = value["title"]?.string else { return nil }
            let blockers = values(value["blockers"])
            let status: WorkspaceTaskStatus
            if let blocker = blockers.last?["kind"]?.string {
                status = blocker == "cancelled" ? .cancelled : (blocker == "failed" ? .failed : .unknown)
            } else {
                switch value["state"]?.string {
                case "pending": status = .pending
                case "implementing": status = .implementing
                case "review_pending": status = .review
                case "retry_pending": status = .retry
                case "completed": status = .completed
                default: status = .unknown
                }
            }
            let evidence = values(value["acceptance"]).map {
                [$0["criterion"]?.string, $0["state"]?.string, $0["evidence"]?.string].compactMap { $0 }.joined(separator: " · ")
            } + blockers.compactMap { $0["detail"]?.string }
            return WorkspaceTask(id: id, title: title, status: status, detail: evidence.joined(separator: "\n"), fileID: nil)
        }
        let agents = values(payload?["children"]).compactMap { value -> WorkspaceAgent? in
            guard let attempt = value["attempt_id"]?.string, let run = value["run_id"]?.string else { return nil }
            let state = WorkspaceAgentStatus(rawValue: value["state"]?.string ?? "") ?? .unknown
            // The snapshot has no per-child model. Never attribute today's main model to an older child.
            let unknownModel = WorkspaceModelConfiguration(id: "unreported", title: "モデル未取得",
                destination: "未取得", effort: "未取得", unavailableReason: "子の保存済みモデル情報は未取得です", supportsAttachments: false)
            return WorkspaceAgent(id: attempt, title: run, summary: "保存済みの子実行", status: state,
                model: unknownModel, taskID: values(value["task_ids"]).first?.string ?? "", attempt: 0)
        }
        let activity = WorkspaceActivity(conversationID: binding.sessionID,
            planRevision: payload?["plan_revision"]?.string ?? "未取得",
            status: payload == nil ? "保存状態を読み込み中" : "保存済みの作業",
            tasks: tasks, agents: agents, files: files, git: git)
        return WorkspaceSnapshot(isDemo: false,
            projects: [WorkspaceProject(id: binding.projectID, name: URL(fileURLWithPath: binding.sourcePath).lastPathComponent,
                source: binding.sourcePath, permission: nil, pinned: false, sectionID: nil)], sections: [],
            conversations: [WorkspaceConversation(id: binding.sessionID, projectID: binding.projectID,
                title: "保存済みの会話", model: configuration, messages: [])], models: [configuration], activities: [activity])
    }

    private static func values(_ value: ServiceValue?) -> [ServiceValue] {
        guard case .array(let values) = value else { return [] }
        return values
    }
}
