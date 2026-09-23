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
                     files: [WorkspaceFile] = [], git: WorkspaceGit? = nil,
                     orchestration projectedOrchestration: WorkspaceOrchestration? = nil) -> WorkspaceSnapshot {
        guard let binding else {
            return WorkspaceSnapshot(isDemo: false, projects: [], sections: [], conversations: [], models: [], activities: [], quota: nil)
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
        let orchestration = projectedOrchestration ?? payload?["orchestration"].flatMap { try? WorkspaceOrchestration($0) }
        let agents: [WorkspaceAgent]
        if let orchestration {
            agents = orchestration.subagents.map { child in
                let unknownModel = WorkspaceModelConfiguration(id: "unreported", title: "モデル未取得",
                    destination: "未取得", effort: "未取得", unavailableReason: "子の保存済みモデル情報は未取得です", supportsAttachments: false)
                return WorkspaceAgent(id: child.id, name: child.displayName, title: child.role,
                    summary: child.activity ?? "保存済みの子実行", status: agentStatus(child.state), model: unknownModel,
                    taskID: "", attempt: 0)
            }
        } else {
            agents = values(payload?["children"]).compactMap { value -> WorkspaceAgent? in
            guard let attempt = value["attempt_id"]?.string, let run = value["run_id"]?.string else { return nil }
            let state = WorkspaceAgentStatus(rawValue: value["state"]?.string ?? "") ?? .unknown
            // The snapshot has no per-child model. Never attribute today's main model to an older child.
            let unknownModel = WorkspaceModelConfiguration(id: "unreported", title: "モデル未取得",
                destination: "未取得", effort: "未取得", unavailableReason: "子の保存済みモデル情報は未取得です", supportsAttachments: false)
            return WorkspaceAgent(id: attempt, name: value["display_name"]?.string ?? run,
                title: "保存済みの子実行", summary: "run ID：\(run)", status: state,
                model: unknownModel, taskID: values(value["task_ids"]).first?.string ?? "", attempt: 0)
            }
        }
        let activity = WorkspaceActivity(conversationID: binding.sessionID,
            planRevision: payload?["plan_revision"]?.string ?? "未取得",
            status: payload == nil ? "保存状態を読み込み中" : "保存済みの作業",
            tasks: tasks, agents: agents, files: files, git: git,
            orchestration: orchestration)
        return WorkspaceSnapshot(isDemo: false,
            projects: [WorkspaceProject(id: binding.projectID, name: URL(fileURLWithPath: binding.sourcePath).lastPathComponent,
                source: binding.sourcePath, permission: nil, pinned: false, sectionID: nil)], sections: [],
            conversations: [WorkspaceConversation(id: binding.sessionID, projectID: binding.projectID,
                title: "保存済みの会話", model: configuration, messages: [])], models: [configuration],
            activities: [activity], quota: nil)
    }

    private static func values(_ value: ServiceValue?) -> [ServiceValue] {
        guard case .array(let values) = value else { return [] }
        return values
    }
    private static func agentStatus(_ state: WorkspaceSubagentState) -> WorkspaceAgentStatus {
        switch state {
        case .queued: .queued
        case .running: .running
        case .waiting: .loading
        case .cancelling: .cancelling
        case .succeeded: .succeeded
        case .failed: .failed
        case .cancelled: .cancelled
        case .outcomeUnknown: .unknown
        }
    }
}

extension WorkspaceOrchestration {
    /// Decode only the already schema-validated, bounded v2 payload.  A missing
    /// projection is the normal v1 compatibility case; malformed values are not
    /// converted into a plausible status.
    init(_ value: ServiceValue) throws {
        func text(_ object: ServiceValue, _ key: String) throws -> String {
            guard let value = object[key]?.string else { throw ServiceError.schema }
            return value
        }
        func number(_ object: ServiceValue, _ key: String) throws -> UInt64 {
            guard let value = object[key]?.decimal else { throw ServiceError.schema }
            return value
        }
        func object(_ object: ServiceValue, _ key: String) throws -> ServiceValue {
            guard let value = object[key] else { throw ServiceError.schema }; return value
        }
        func array(_ object: ServiceValue, _ key: String) throws -> [ServiceValue] {
            guard case .array(let values) = object[key] else { throw ServiceError.schema }; return values
        }
        func artifact(_ value: ServiceValue?) throws -> WorkspaceArtifactReference? {
            guard let value else { return nil }
            return .init(id: try text(value, "artifact_id"), contentHash: try text(value, "content_hash"))
        }

        let workflowValue = try object(value, "workflow")
        let acceptance = try array(workflowValue, "acceptance").map {
            WorkspaceAcceptanceEvidence(id: try text($0, "criterion_id"), state: try text($0, "state"),
                                        evidenceCount: try number($0, "evidence_count"))
        }
        let evidenceValue = try object(workflowValue, "evidence")
        let convergenceValue = try object(workflowValue, "convergence")
        guard let convergenceState = WorkspaceConvergenceState(rawValue: try text(convergenceValue, "state")) else {
            throw ServiceError.schema
        }
        workflow = .init(phase: try text(workflowValue, "phase"), step: try text(workflowValue, "step"),
                         activeFeature: try artifact(workflowValue["active_feature"]),
                         activePlan: try artifact(workflowValue["active_plan"]),
                         activeTask: try artifact(workflowValue["active_task"]), acceptance: acceptance,
                         evidence: .init(passed: try number(evidenceValue, "passed"), failed: try number(evidenceValue, "failed"),
                                         stale: try number(evidenceValue, "stale"), unavailable: try number(evidenceValue, "unavailable")),
                         convergence: .init(state: convergenceState, requiredCriteria: try number(convergenceValue, "required_criteria"),
                                            unresolvedGaps: try number(convergenceValue, "unresolved_gaps")))

        let manager = try object(value, "session_manager")
        let sessions = try array(manager, "sessions").map { row -> WorkspaceManagedSession in
            guard let kind = WorkspaceManagedSessionKind(rawValue: try text(row, "kind")),
                  let state = WorkspaceManagedSessionState(rawValue: try text(row, "state")) else { throw ServiceError.schema }
            return .init(id: try text(row, "session_id"), kind: kind, state: state, sourceSessionID: row["source_session_id"]?.string,
                         sessionRevision: row["session_revision"]?.decimal, goalRevision: row["goal_revision"]?.decimal)
        }
        sessionManager = .init(revision: try number(manager, "manager_revision"), sessions: sessions,
                               sessionTotal: try number(manager, "session_total"), sessionOmitted: manager["session_omitted"] == .bool(true),
                               sidechat: try text(manager, "sidechat"), queuedAdmissions: try number(manager, "queued_admissions"),
                               activeRuns: try number(manager, "active_runs"))

        subagents = try array(value, "subagents").map { row in
            guard let state = WorkspaceSubagentState(rawValue: try text(row, "state")) else { throw ServiceError.schema }
            return .init(id: try text(row, "attempt_id"), rootRunID: try text(row, "root_run_id"),
                         rootAttemptID: try text(row, "root_attempt_id"), controlRevision: try number(row, "control_revision"),
                         logicalChildID: try text(row, "logical_child_id"),
                         displayName: try text(row, "display_name"), role: try text(row, "role"), state: state,
                         activity: row["activity"]?.string)
        }
        subagentTotal = try number(value, "subagent_total")
        subagentOmitted = value["subagent_omitted"] == .bool(true)
        mcpLeases = try array(value, "mcp_leases").map { row in
            guard let state = WorkspaceMcpLeaseState(rawValue: try text(row, "state")) else { throw ServiceError.schema }
            return .init(id: try text(row, "lease_id"), serverID: try text(row, "server_id"), holderRunID: try text(row, "holder_run_id"),
                         holderAttemptID: try text(row, "holder_attempt_id"), state: state,
                         expiresAtUnixMilliseconds: try number(row, "expires_at_unix_ms"))
        }
        mcpLeaseTotal = try number(value, "mcp_lease_total")
        mcpLeaseOmitted = value["mcp_lease_omitted"] == .bool(true)
        if let runner = value["common_runner"] {
            guard let state = WorkspaceCommonRunnerState(rawValue: try text(runner, "state")) else { throw ServiceError.schema }
            commonRunner = .init(state: state, caseID: runner["case_id"]?.string,
                                 completedCases: try number(runner, "completed_cases"), failedCases: try number(runner, "failed_cases"),
                                 cleanupPending: runner["cleanup_pending"] == .bool(true))
        } else { commonRunner = nil }
        let clarificationValue = try object(value, "clarification")
        guard let clarificationState = WorkspaceClarificationState(rawValue: try text(clarificationValue, "state")) else {
            throw ServiceError.schema
        }
        let questions = try array(clarificationValue, "questions").map { row -> WorkspaceClarificationQuestion in
            let free = try object(row, "free_text")
            return .init(id: try text(row, "question_id"), text: try text(row, "text"),
                         options: try array(row, "options").map { option in
                             .init(id: try text(option, "option_id"), label: try text(option, "label"))
                         }, freeText: .init(allowed: row["free_text"]?["allowed"]?.bool == true,
                                             required: free["required"]?.bool == true,
                                             maxBytes: try number(free, "max_bytes")),
                         answered: row["answered"]?.bool == true)
        }
        clarification = .init(state: clarificationState, questions: questions,
                              unansweredCount: try number(clarificationValue, "unanswered_count"))
    }
}
