/// Decodes bounded admission and subagent responses without updating workspace state.
enum WorkspaceAdmissionCodec {
  static func subagentWait(_ payload: ServiceValue, root: WorkspaceRun, minimumRevision: UInt64,
                           afterCursor: UInt64?) throws -> WorkspaceSubagentWaitResult {
    guard payload["root_run_id"]?.matchesID(root.id) == true,
          payload["root_attempt_id"]?.matchesID(root.attemptID) == true,
          let revision = payload["revision"]?.decimal, revision >= minimumRevision,
          let cursor = payload["cursor"]?.decimal, afterCursor.map({ cursor >= $0 }) ?? true,
          let stateText = payload["state"]?.string, let state = WorkspaceSubagentWaitState(rawValue: stateText),
          let total = payload["child_total"]?.decimal, let omitted = payload["child_omitted"],
          case .bool(let childOmitted) = omitted, case .array(let rows) = payload["children"], rows.count <= 32
    else { throw WorkspaceServiceModel.Failure.stale }
    let children = try rows.map { row -> WorkspaceSubagentProjection in
      guard row["root_run_id"]?.matchesID(root.id) == true,
            row["root_attempt_id"]?.matchesID(root.attemptID) == true,
            let controlRevision = row["control_revision"]?.decimal, controlRevision >= minimumRevision,
            let logical = row["logical_child_id"]?.string, let attempt = row["attempt_id"]?.string,
            let name = row["display_name"]?.string, let role = row["role"]?.string,
            let stateText = row["state"]?.string, let childState = WorkspaceSubagentState(rawValue: stateText) else {
        throw ServiceError.schema
      }
      return .init(id: attempt, rootRunID: root.id, rootAttemptID: root.attemptID, controlRevision: controlRevision,
                   logicalChildID: logical, displayName: name,
                   role: role, state: childState, activity: row["activity"]?.string)
    }
    guard Set(children.map(\.id)).count == children.count, total >= UInt64(children.count) else {
      throw WorkspaceServiceModel.Failure.stale
    }
    return .init(rootRunID: root.id, rootAttemptID: root.attemptID, revision: revision, cursor: cursor,
                 state: state, childTotal: total, childOmitted: childOmitted, children: children)
  }

  static func subagentCancel(_ payload: ServiceValue, root: WorkspaceRun,
                             child: WorkspaceSubagentProjection, minimumRevision: UInt64) throws -> WorkspaceSubagentCancelResult {
    guard payload["root_run_id"]?.matchesID(root.id) == true,
          payload["root_attempt_id"]?.matchesID(root.attemptID) == true,
          payload["logical_child_id"]?.matchesID(child.logicalChildID) == true,
          payload["child_attempt_id"]?.matchesID(child.id) == true,
          let revision = payload["revision"]?.decimal, revision >= minimumRevision,
          let cursor = payload["cursor"]?.decimal,
          let dispositionText = payload["disposition"]?.string,
          let disposition = WorkspaceSubagentCancelDisposition(rawValue: dispositionText),
          let stateText = payload["state"]?.string, let state = WorkspaceSubagentState(rawValue: stateText),
          let effectText = payload["runtime_effect"]?.string,
          let effect = WorkspaceSubagentRuntimeEffect(rawValue: effectText)
    else { throw WorkspaceServiceModel.Failure.stale }
    return .init(rootRunID: root.id, rootAttemptID: root.attemptID, logicalChildID: child.logicalChildID,
                 childAttemptID: child.id, revision: revision, cursor: cursor,
                 disposition: disposition, state: state, runtimeEffect: effect)
  }

  static func inputAdmission(_ payload: ServiceValue, fallbackHash: String? = nil) throws -> WorkspaceInputAdmission? {
    guard let id = payload["admission_id"]?.string, let hash = payload["input_hash"]?.string ?? fallbackHash,
          let stateText = payload["state"]?.string, let state = WorkspaceInputAdmissionState(rawValue: stateText),
          let revision = payload["session_revision"]?.decimal else { return nil }
    let run = payload["run"]
    guard let route = payload["route"]?.string else { return nil }
    return .init(id: id, inputHash: hash, state: state, sessionRevision: revision,
                 route: route, routeReason: payload["route_reason"]?.string,
                 destinationSessionID: payload["destination_session_id"]?.string,
                 runID: run?["run_id"]?.string, attemptID: run?["attempt_id"]?.string)
  }

  static func currentSessionAdmissionCoordinates(_ payload: ServiceValue, id: String)
    throws -> (managerRevision: UInt64, sessionRevision: UInt64, goalRevision: UInt64)? {
    guard let managerRevision = payload["manager_revision"]?.decimal,
          case .array(let sessions) = payload["sessions"] else { return nil }
    let row = sessions.first { $0["session_id"]?.matchesID(id) == true }
    guard let row, let sessionRevision = row["session_revision"]?.decimal,
          let goalRevision = row["goal_revision"]?.decimal else { return nil }
    return (managerRevision, sessionRevision, goalRevision)
  }
}
