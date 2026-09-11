import Foundation
import PolarisSettings

/// Deliberately limited P5 API; no arbitrary RPC, provider, tool or auth entry point.
enum ServiceRequest: Sendable {
  case workspaceRead(session: String, selectedPath: String)
  case attachmentRead(session: String, path: String)
  case snapshot(session: String)
  case subscribe(session: String)
  case configure(session: String, revision: UInt64, selection: ExecutionBinding, historyMode: HistoryMode)
  case localModels(session: String, provider: ExecutionBinding.Provider, endpoint: String)
  case configureRoles(session: String, revision: UInt64, bindings: [LocalRoleBinding])
  case draft(session: String, revision: UInt64, text: String)
  case run(session: String, draft: UInt64, configuration: UInt64, policy: UInt64)
  case cancel(session: String, run: String, attempt: String)
  case approval(session: String, id: String, run: String, attempt: String, policy: UInt64, decision: ServiceApprovalDecision)
  case sourceApplyList(session: String, revision: UInt64, offset: UInt64, limit: Int)
  case sourceApplyPage(session: String, approval: String, hash: String, revision: UInt64, offset: UInt64, limit: Int)
  case sourceApplyResolve(session: String, run: String, attempt: String, approval: String, hash: String,
                          revision: UInt64, policy: UInt64, decision: ServiceApprovalDecision)
  case shutdown(epoch: String)

  var method: String {
    switch self {
    case .workspaceRead: "workspace.read"
    case .attachmentRead: "attachment.read"
    case .snapshot: "session.snapshot"
    case .subscribe: "session.subscribe"
    case .configure: "session.configure"
    case .localModels: "local.models"
    case .configureRoles: "session.roles.configure"
    case .draft: "draft.update"
    case .run: "run.start"
    case .cancel: "run.cancel"
    case .approval: "approval.resolve"
    case .sourceApplyList: "source_apply.list"
    case .sourceApplyPage: "source_apply.page"
    case .sourceApplyResolve: "source_apply.resolve"
    case .shutdown: "shutdown.request"
    }
  }
  var capability: String {
    switch self {
    case .workspaceRead: "workspace_read"
    case .attachmentRead: "attachment_read"
    case .snapshot, .subscribe: "session_read"
    case .configure: "session_configure"
    case .localModels: "local_models"
    case .configureRoles: "role_configure"
    case .draft: "draft_update"
    case .run: "run_start"
    case .cancel: "run_cancel"
    case .approval: "approval_resolve"
    case .sourceApplyList, .sourceApplyPage: "source_apply_read"
    case .sourceApplyResolve: "source_apply_resolve"
    case .shutdown: "shutdown"
    }
  }
  func wire(clientID: String, requestID: String) throws -> ServiceValue {
    var params: [String: ServiceValue] = [:]
    var session: String?
    switch self {
    case .workspaceRead(let s, let path):
      guard path.utf8.count <= 4096 else { throw ServiceError.capacity }
      session = s; params = ["selected_path": .string(path)]
    case .attachmentRead(let s, let path):
      guard path.hasPrefix("/"), !path.contains("\0"), path.utf8.count <= 4096 else { throw ServiceError.schema }
      session = s; params = ["path": .string(path)]
    case .snapshot(let s), .subscribe(let s): session = s
    case .configure(let s, let revision, let selection, let historyMode):
      try selection.validate()
      session = s
      params = ["expected_configuration_revision": .string(String(revision)), "provider": .string(selection.provider.rawValue),
                "model": .string(selection.model), "effort": .string(selection.storedEffort)]
      if historyMode != .legacy { params["history_mode"] = .string(historyMode.rawValue) }
    case .localModels(let s, let provider, let endpoint):
      guard provider == .ollama || provider == .lmstudio else { throw ServiceError.schema }
      session = s
      params = ["provider": .string(provider.rawValue), "endpoint": .string(endpoint)]
    case .configureRoles(let s, let revision, let bindings):
      session = s
      var seen: Set<Data> = []
      let values = try bindings.map { binding -> ServiceValue in
        try binding.validate()
        guard seen.insert(Data(binding.role.utf8)).inserted else { throw ServiceError.schema }
        return .object([
          "role": .string(binding.role), "provider": .string(binding.selection.provider.rawValue),
          "endpoint": .string(binding.selection.localEndpoint!), "model": .string(binding.selection.model),
          "observed_tool_support": .string(binding.observedToolSupport.rawValue),
        ])
      }
      params = ["expected_configuration_revision": .string(String(revision)), "bindings": .array(values)]
    case .draft(let s, let revision, let text):
      session = s
      params = ["expected_draft_revision": .string(String(revision)), "text": .string(text)]
    case .run(let s, let draft, let configuration, let policy):
      session = s
      params = [
        "expected_draft_revision": .string(String(draft)),
        "expected_configuration_revision": .string(String(configuration)),
        "expected_policy_revision": .string(String(policy)),
      ]
    case .cancel(let s, let run, let attempt):
      try ServiceSchema.id.check(.string(run))
      try ServiceSchema.id.check(.string(attempt))
      session = s
      params = ["run_id": .string(run), "attempt_id": .string(attempt)]
    case .approval(let s, let id, let run, let attempt, let policy, let decision):
      session = s
      for value in [id, run, attempt] { try ServiceSchema.id.check(.string(value)) }
      params = ["approval_id": .string(id), "run_id": .string(run), "attempt_id": .string(attempt),
                "policy_revision": .string(String(policy)), "decision": .string(decision.rawValue)]
    case .sourceApplyList(let s, let revision, let offset, let limit):
      guard (1...32).contains(limit) else { throw ServiceError.capacity }
      session = s
      params = ["expected_session_revision": .string(String(revision)), "offset": .string(String(offset)),
                "limit": .number(String(limit))]
    case .sourceApplyPage(let s, let approval, let hash, let revision, let offset, let limit):
      guard (1...32).contains(limit), !hash.isEmpty, hash.utf8.count <= 256 else { throw ServiceError.capacity }
      try ServiceSchema.id.check(.string(approval))
      session = s
      params = ["approval_id": .string(approval), "payload_hash": .string(hash),
                "expected_session_revision": .string(String(revision)), "offset": .string(String(offset)),
                "limit": .number(String(limit))]
    case .sourceApplyResolve(let s, let run, let attempt, let approval, let hash, let revision, let policy, let decision):
      guard !hash.isEmpty, hash.utf8.count <= 256 else { throw ServiceError.capacity }
      for value in [run, attempt, approval] { try ServiceSchema.id.check(.string(value)) }
      session = s
      params = ["run_id": .string(run), "attempt_id": .string(attempt), "approval_id": .string(approval),
                "payload_hash": .string(hash), "expected_session_revision": .string(String(revision)),
                "expected_policy_revision": .string(String(policy)), "decision": .string(decision.rawValue)]
    case .shutdown(let epoch):
      try ServiceSchema.id.check(.string(epoch))
      params = ["engine_epoch": .string(epoch)]
    }
    var fields = try Self.envelope(
      clientID: clientID, requestID: requestID, method: method, params: params)
    if let session {
      try ServiceSchema.id.check(.string(session))
      fields["session_id"] = .string(session)
    }
    return .object(fields)
  }
  static func envelope(
    clientID: String, requestID: String, method: String, params: [String: ServiceValue]
  ) throws -> [String: ServiceValue] {
    try ServiceSchema.id.check(.string(clientID))
    try ServiceSchema.id.check(.string(requestID))
    return [
      "protocol_version": .number("1"), "kind": .string("request"), "client_id": .string(clientID),
      "request_id": .string(requestID), "method": .string(method), "params": .object(params),
    ]
  }
}

struct ServiceHello: Sendable, Equatable {
  let epoch: String
  let capabilities: Set<String>
  let frameBytes: Int
  let eventCount: Int
  let eventBytes: Int

  init(_ payload: ServiceValue) throws {
    try ServiceSchema.hello.check(payload)
    guard payload["protocol_version"] == .number("1") else { throw ServiceError.version }
    epoch = payload["engine_epoch"]!.string!
    guard case .array(let capabilities) = payload["capabilities"] else { throw ServiceError.schema }
    self.capabilities = Set(capabilities.compactMap(\.string))
    let limits = payload["limits"]!
    guard let frames = limits["frame_bytes"]?.decimal, frames > 0,
      frames <= ServiceCodec.maxFrameBytes,
      let events = limits["subscription_events"]?.decimal, events > 0, events <= 256,
      let bytes = limits["subscription_bytes"]?.decimal, bytes > 0, bytes <= 4_194_304,
      let batch = limits["text_batch_bytes"]?.decimal, batch > 0, batch <= frames,
      let ms = limits["text_batch_ms"]?.decimal, ms > 0
    else { throw ServiceError.schema }
    frameBytes = Int(frames)
    eventCount = Int(events)
    eventBytes = Int(bytes)
  }
}

struct ServiceReply: Sendable, Equatable {
  let requestID: String
  let method: String?
  let payload: ServiceValue?
  let rejection: ServiceValue?
}

/// Closed schemas for P5's requests and all P1 state events. Unknown state means
/// resync/connection failure, never silently dropping an unrecognised update.
indirect enum ServiceSchema: Sendable {
  case text, id, decimal, uint32Number, version, bool
  case oneOf(Set<String>)
  case array(ServiceSchema)
  case object([String: ServiceSchema])
  case optionalFields([String: ServiceSchema], [String: ServiceSchema])
  case alternatives([ServiceSchema])

  func check(_ value: ServiceValue) throws {
    switch (self, value) {
    case (.text, .string): break
    case (.id, .string(let s)) where !s.isEmpty && s.utf8.count <= 128: break
    case (.decimal, _) where value.decimal != nil: break
    case (.uint32Number, .number(let number)) where Self.isCanonicalUInt(number, maximum: UInt64(UInt32.max)): break
    case (.version, .number("1")): break
    case (.bool, .bool): break
    case (.oneOf(let choices), .string(let s)) where choices.contains(s): break
    case (.array(let schema), .array(let items)): for item in items { try schema.check(item) }
    case (.object(let fields), .object(let object)) where Set(fields.keys) == Set(object.keys):
      for (key, schema) in fields { try schema.check(object[key]!) }
    case (.optionalFields(let required, let optional), .object(let object)):
      guard Set(required.keys).isSubset(of: Set(object.keys)),
        Set(object.keys).isSubset(of: Set(required.keys).union(optional.keys))
      else { throw ServiceError.schema }
      for (key, schema) in required { try schema.check(object[key]!) }
      for (key, schema) in optional { if let value = object[key] { try schema.check(value) } }
    case (.alternatives(let schemas), _):
      guard schemas.contains(where: { (try? $0.check(value)) != nil }) else { throw ServiceError.schema }
    default: throw ServiceError.schema
    }
  }
  private static func isCanonicalUInt(_ value: String, maximum: UInt64) -> Bool {
    guard !value.isEmpty, value == "0" || !value.hasPrefix("0"),
      value.utf8.allSatisfy({ (48...57).contains($0) }), let parsed = UInt64(value)
    else { return false }
    return parsed <= maximum
  }
  static let capabilities: Set<String> = [
    "workspace_read", "attachment_read", "session_read", "history_read", "draft_update", "session_configure", "role_configure", "run_start", "run_cancel", "local_models",
    "approval_resolve", "request_status", "shutdown", "source_apply_read", "source_apply_resolve",
  ]
  static let hello: Self = .object([
    "protocol_version": .version, "engine_epoch": .id, "capabilities": .array(.oneOf(capabilities)),
    "limits": .object([
      "frame_bytes": .decimal, "subscription_events": .decimal, "subscription_bytes": .decimal,
      "text_batch_bytes": .decimal, "text_batch_ms": .decimal,
    ]),
  ])
  static let position: Self = .object([
    "engine_epoch": .id, "subscription_id": .id, "event_seq": .decimal,
  ])
  static let draft: Self = .object([
    "draft_revision": .decimal, "text": .text, "attachment_ids": .array(.id),
  ])
  static let configuration: Self = .optionalFields([
    "configuration_revision": .decimal, "provider": .text, "model": .text, "effort": .text,
  ], ["history_mode": .oneOf(["legacy", "strict10"])])
  static let memoryUsage: Self = .object([
    "input_tokens": .decimal, "output_tokens": .decimal, "cached_tokens": .decimal,
    "reported_responses": .decimal, "missing_responses": .decimal, "failed_requests": .decimal,
  ])
  static let embeddingUsage: Self = .object([
    "requests": .decimal, "completed": .decimal, "failed": .decimal, "unknown": .decimal, "input_tokens": .decimal,
  ])
  static let memory: Self = .optionalFields([
    "run_id": .id, "phase": .oneOf(["preparing", "ready", "failed", "outcome_unknown"]), "detail": .text,
    "recent_raw_turns": .decimal, "retrieval_sources": .array(.text), "reference_tokens": .decimal,
  ], ["summary_usage": memoryUsage, "main_usage": memoryUsage, "embedding_usage": embeddingUsage, "total_usage": memoryUsage])
  static let roleBinding: Self = .object([
    "role": .text, "provider": .oneOf(["ollama", "lmstudio"]), "endpoint": .text,
    "model": .text, "observed_tool_support": .oneOf(["supported", "unsupported", "unknown"]),
  ])
  static let roleDescriptor: Self = .object(["role": .text, "requires_tools": .bool])
  static let observedCapability: Self = .oneOf(["supported", "unsupported", "unknown"])
  static let localModel: Self = .optionalFields([
    "model_id": .text, "completion": observedCapability, "tools": observedCapability,
    "vision": observedCapability, "reasoning": observedCapability,
    "execution_location": .oneOf(["remote", "unknown"]),
  ], ["digest": .text, "variant": .text, "max_context_length": .decimal,
      "load_state": .oneOf(["loaded", "unloaded", "unknown"])])
  static let localModels: Self = .object([
    "provider": .oneOf(["ollama", "lmstudio"]), "endpoint": .text,
    "availability": .oneOf(["available", "unreachable", "invalid_response"]),
    "models": .array(localModel),
  ])
  static let states: Set<String> = [
    "queued", "loading", "running", "awaiting_approval", "cancelling", "succeeded", "failed",
    "cancelled", "interrupted", "outcome_unknown",
  ]
  static let run: Self = .object([
    "run_id": .id, "attempt_id": .id, "state": .oneOf(states), "task_ids": .array(.id),
  ])
  static let child: Self = .object([
    "run_id": .id, "attempt_id": .id, "parent_run_id": .id, "state": .oneOf(states),
    "task_ids": .array(.id),
  ])
  static let task: Self = .object([
    "task_id": .id, "title": .text,
    "state": .oneOf(["pending", "implementing", "review_pending", "retry_pending", "completed"]),
    "acceptance": .array(
      .object([
        "criterion": .text, "state": .oneOf(["pending", "passed", "recheck_required"]),
        "evidence": .text,
      ])),
    "blockers": .array(
      .object([
        "run_id": .id, "attempt_id": .id,
        "kind": .oneOf(["cancelled", "failed", "interrupted", "outcome_unknown"]), "detail": .text,
      ])),
  ])
  static let approval: Self = .object([
    "approval_id": .id, "run_id": .id, "attempt_id": .id, "operation_id": .id, "operation": .text,
    "scope": .text, "payload_hash": .text,
    "policy_revision": .decimal, "expires_at_unix_ms": .decimal, "state": .oneOf(["pending"]),
    "display": .object([
      "title": .text, "description": .text, "choices": .array(.oneOf(["allow", "deny"])),
    ]),
  ])
  static let resolved: Self = .object([
    "approval_id": .id, "state": .oneOf(["resolved"]), "decision": .oneOf(["allow", "deny"]),
  ])
  static let sourceApplyVersion: Self = .object(["hash": .text, "mode": .uint32Number])
  static let sourceApplyEntry: Self = .optionalFields([
    "relative_path": .text,
  ], ["before": sourceApplyVersion, "after": sourceApplyVersion])
  static let sourceApplyResult: Self = .optionalFields([
    "result_id": .id, "status": .oneOf(["applied", "failed", "partial", "unknown"]),
    "installed_count": .decimal, "deleted_count": .decimal, "restored_count": .decimal,
  ], ["failure_kind": .oneOf(["protection_unavailable", "invalid_change_set", "unsupported_entry", "conflict",
      "secret", "ancestor_changed", "cross_device", "io", "restore_conflict"])])
  static let sourceApplySummary: Self = .optionalFields([
    "run_id": .id, "attempt_id": .id, "approval_id": .id, "operation_id": .id,
    "policy_revision": .decimal, "expires_at_unix_ms": .decimal, "payload_hash": .text,
    "source_path": .text, "recovery_parent_path": .text, "entry_count": .decimal,
    "invalidated": .bool, "intent_committed": .bool, "result_saved": .bool,
  ], ["decision": .oneOf(["allow", "deny"]), "result": sourceApplyResult])
  static let sourceApplyList: Self = .optionalFields([
    "session_revision": .decimal, "items": .array(sourceApplySummary),
  ], ["next_offset": .decimal])
  static let sourceApplyPage: Self = .optionalFields([
    "approval_id": .id, "payload_hash": .text, "session_revision": .decimal,
    "entries": .array(sourceApplyEntry),
  ], ["next_offset": .decimal])
  static let snapshot: Self = .optionalFields([
    "snapshot_id": .id, "history_start_cursor": .id, "session_id": .id, "summary": .text,
    "session_revision": .decimal, "content_revision": .decimal, "plan_revision": .decimal,
    "policy_revision": .decimal,
    "position": position, "draft": draft, "configuration": configuration,
    "role_bindings": .array(roleBinding), "role_catalog": .array(roleDescriptor), "tasks": .array(task),
    "runs": .array(run),
    "children": .array(child), "child_attempt_count": .decimal,
    "unresolved_approvals": .array(approval),
  ], ["memory": memory])
  static func result(_ type: String) throws -> Self {
    switch type {
    case "workspace.read": .object([
      "state": .oneOf(["ready", "unavailable"]), "files": .array(.optionalFields([
        "id": .text, "path": .text, "body": .text,
        "modified": .bool, "staged": .bool, "unsaved": .bool
      ], ["unavailableReason": .text, "unstagedDiff": .text, "stagedDiff": .text])),
      "git": .optionalFields(["state": .oneOf(["ready", "unavailable"]), "revisions": .array(.object([
        "id": .text, "parent": .text, "title": .text, "fileID": .text, "body": .text, "diff": .text
      ]))], ["reason": .text, "branch": .text, "head": .text, "changeSummary": .text]),
      "notices": .array(.text)
    ])
    case "attachment.read": .optionalFields(["state": .oneOf(["ready", "unavailable"])], ["name": .text, "text": .text, "reason": .text])
    case "hello": hello
    case "session.snapshot", "session.subscribe": snapshot
    case "history.page": ServiceHistoryPage.schema
    case "request.status": ServiceRequestStatus.schema
    case "session.configure": .object(["session_revision": .decimal, "configuration": configuration])
    case "local.models": localModels
    case "session.roles.configure": .object([
      "configuration_revision": .decimal, "bindings": .array(roleBinding),
    ])
    case "draft.update": .object(["session_revision": .decimal, "draft_revision": .decimal])
    case "run.start":
      .object([
        "session_revision": .decimal, "run_id": .id, "attempt_id": .id, "state": .oneOf(["queued"]),
      ])
    case "approval.resolve": resolved
    case "source_apply.list": sourceApplyList
    case "source_apply.page": sourceApplyPage
    case "source_apply.resolve": resolved
    case "run.cancel": .object(["status": .oneOf(["cancel_requested"])])
    case "shutdown.request":
      .object(["engine_epoch": .id, "state": .oneOf(["draining", "ready"])])
    default: throw ServiceError.schema
    }
  }
  static func event(_ type: String) throws -> Self {
    switch type {
    case "message.delta":
      .object([
        "message_id": .id, "byte_offset": .decimal, "text": .text,
        "durability": .oneOf(["tentative", "saved"]),
      ])
    case "run.state": run
    case "child.updated": child
    case "task.updated": task
    case "draft.updated": draft
    case "configuration.updated": configuration
    case "memory.updated": memory
    case "role_bindings.updated": .object([
      "configuration_revision": .decimal, "bindings": .array(roleBinding),
    ])
    case "approval.requested": approval
    case "approval.resolved": resolved
    case "approval.expired":
      .object([
        "approval_id": .id,
        "reason": .oneOf(["cancelled", "shutdown", "expired", "policy_revoked", "engine_restarted"]
        ),
      ])
    default: throw ServiceError.schema
    }
  }
  static func response(_ value: ServiceValue, clientID: String, method: String) throws
    -> ServiceReply
  {
    guard value["protocol_version"] == .number("1") else { throw ServiceError.version }
    guard value["client_id"]?.matchesID(clientID) == true else { throw ServiceError.correlation }
    let base: [String: Self] = [
      "protocol_version": .version, "kind": .oneOf(["response"]), "client_id": .id,
      "request_id": .id,
    ]
    if let rejection = value["error"] {
      let error: Self = .object([
        "code": .oneOf([
          "unsupported_version", "invalid_request", "not_found", "permission_denied",
          "revision_conflict", "session_busy", "capability_unavailable", "storage_failed",
          "recovery_required",
        ]), "message": .text,
      ])
      try Self.object(base.merging(["error": error]) { _, rhs in rhs }).check(value)
      return ServiceReply(
        requestID: value["request_id"]!.string!, method: nil, payload: nil, rejection: rejection)
    }
    guard value["result"]?["type"] == .string(method) else { throw ServiceError.correlation }
    let result = try Self.object(["type": .oneOf([method]), "payload": result(method)])
    try Self.object(base.merging(["result": result]) { _, rhs in rhs }).check(value)
    return ServiceReply(
      requestID: value["request_id"]!.string!, method: method, payload: value["result"]?["payload"],
      rejection: nil)
  }
  static func validateEvent(_ value: ServiceValue, epoch: String) throws {
    guard value["protocol_version"] == .number("1") else { throw ServiceError.version }
    guard value["engine_epoch"]?.matchesID(epoch) == true, let type = value["type"]?.string else {
      throw ServiceError.schema
    }
    try Self.object([
      "protocol_version": .version, "kind": .oneOf(["event"]), "engine_epoch": .id,
      "subscription_id": .id, "event_seq": .decimal, "session_id": .id,
      "session_revision": .decimal,
      "type": .oneOf([type]), "payload": event(type),
    ]).check(value)
  }
}

/// Read-only additions do not expand the demo's closed mutation API.
/// Coordinates come from an observed snapshot, never a guessed cursor or epoch.
struct ServiceReadContext: Sendable, Equatable {
  let epoch: String
  let sessionID: String
  let snapshotID: String
  let sessionRevision: UInt64

  init(snapshot: ServiceValue, epoch: String, sessionID: String) throws {
    try ServiceSchema.snapshot.check(snapshot)
    guard snapshot["session_id"]!.matchesID(sessionID),
      snapshot["position"]!["engine_epoch"]!.matchesID(epoch)
    else { throw ServiceError.correlation }
    self.epoch = epoch
    self.sessionID = sessionID
    snapshotID = snapshot["snapshot_id"]!.string!
    sessionRevision = snapshot["session_revision"]!.decimal!
  }
  func matches(_ other: Self) -> Bool {
    epoch.utf8.elementsEqual(other.epoch.utf8)
      && sessionID.utf8.elementsEqual(other.sessionID.utf8)
      && snapshotID.utf8.elementsEqual(other.snapshotID.utf8)
      && sessionRevision == other.sessionRevision
  }
}

enum ServiceReadRequest: Sendable {
  case history(context: ServiceReadContext, cursor: String, limit: Int)
  case status(context: ServiceReadContext, clientID: String, requestID: String)

  var context: ServiceReadContext {
    switch self { case .history(let c, _, _), .status(let c, _, _): c }
  }
  var method: String {
    switch self { case .history: "history.page"; case .status: "request.status" }
  }
  var capability: String {
    switch self { case .history: "history_read"; case .status: "request_status" }
  }
  func wire(clientID: String, requestID: String) throws -> ServiceValue {
    for id in [context.epoch, context.sessionID, context.snapshotID] {
      try ServiceSchema.id.check(.string(id))
    }
    let params: [String: ServiceValue]
    switch self {
    case .history(let context, let cursor, let limit):
      try ServiceSchema.id.check(.string(cursor))
      guard (1...256).contains(limit) else { throw ServiceError.schema }
      params = ["snapshot_id": .string(context.snapshotID), "cursor": .string(cursor),
                "limit": .number(String(limit))]
    case .status(_, let owner, let target):
      try ServiceSchema.id.check(.string(owner))
      try ServiceSchema.id.check(.string(target))
      params = ["client_id": .string(owner), "request_id": .string(target)]
    }
    var fields = try ServiceRequest.envelope(clientID: clientID, requestID: requestID,
                                            method: method, params: params)
    fields["session_id"] = .string(context.sessionID)
    return .object(fields)
  }
}

struct ServiceHistoryMessage: Sendable, Equatable {
  enum Role: String, Sendable { case user, assistant, tool, system }
  let id: String
  // SwiftUI identity must preserve the protocol's opaque UTF-8 bytes.
  var key: Data { Data(id.utf8) }
  let role: Role
  let text: String
  let savedByteOffset: UInt64
}
struct ServiceHistoryPage: Sendable, Equatable {
  let snapshotID: String
  let sessionRevision: UInt64
  let messages: [ServiceHistoryMessage]
  let nextCursor: String?
  static let schema: ServiceSchema = .optionalFields([
    "snapshot_id": .id, "session_revision": .decimal,
    "messages": .array(.object([
      "message_id": .id, "role": .oneOf(["user", "assistant", "tool", "system"]),
      "text": .text, "saved_byte_offset": .decimal,
    ])),
  ], ["next_cursor": .id])

  init(_ value: ServiceValue) throws {
    try Self.schema.check(value)
    snapshotID = value["snapshot_id"]!.string!
    sessionRevision = value["session_revision"]!.decimal!
    nextCursor = value["next_cursor"]?.string
    guard case .array(let items) = value["messages"] else { throw ServiceError.schema }
    var seen: Set<Data> = []
    messages = try items.map { item in
      let id = item["message_id"]!.string!
      let text = item["text"]!.string!
      let offset = item["saved_byte_offset"]!.decimal!
      guard seen.insert(Data(id.utf8)).inserted, offset == UInt64(text.utf8.count) else {
        throw ServiceError.correlation
      }
      return ServiceHistoryMessage(id: id, role: ServiceHistoryMessage.Role(rawValue: item["role"]!.string!)!,
                                   text: text, savedByteOffset: offset)
    }
  }
}

enum ServiceRequestStatus: Sendable, Equatable {
  struct Run: Sendable, Equatable { let id: String; let attemptID: String }
  case notFound
  case accepted(revision: UInt64, run: Run?)
  case completed(revision: UInt64, resultID: String)
  case outcomeUnknown(revision: UInt64, run: Run)

  static let schema: ServiceSchema = .alternatives([
    .object(["status": .oneOf(["not_found"])]),
    .optionalFields(["status": .oneOf(["accepted"]), "session_revision": .decimal],
                    ["run": .object(["run_id": .id, "attempt_id": .id])]),
    .object(["status": .oneOf(["completed"]), "session_revision": .decimal, "result_id": .id]),
    .object(["status": .oneOf(["outcome_unknown"]), "session_revision": .decimal,
             "run_id": .id, "attempt_id": .id]),
  ])
  init(_ value: ServiceValue) throws {
    try Self.schema.check(value)
    switch value["status"]!.string! {
    case "not_found": self = .notFound
    case "accepted":
      self = .accepted(revision: value["session_revision"]!.decimal!, run: value["run"].map {
        Run(id: $0["run_id"]!.string!, attemptID: $0["attempt_id"]!.string!)
      })
    case "completed":
      self = .completed(revision: value["session_revision"]!.decimal!, resultID: value["result_id"]!.string!)
    default:
      self = .outcomeUnknown(revision: value["session_revision"]!.decimal!,
                            run: Run(id: value["run_id"]!.string!, attemptID: value["attempt_id"]!.string!))
    }
  }
}
