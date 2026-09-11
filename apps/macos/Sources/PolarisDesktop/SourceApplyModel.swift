import Foundation

/// Saved source-apply records are review material. They never carry bytes or
/// paths to write, and a persisted result is not treated as a successful apply.
struct SourceApplyCandidate: Identifiable, Equatable {
  struct Version: Equatable { let hash: String; let mode: UInt64 }
  struct Entry: Identifiable, Equatable {
    let relativePath: String
    let before: Version?
    let after: Version?
    var id: Data { Data(relativePath.utf8) }
  }
  struct Result: Equatable {
    let id: String
    let status: String
    let failureKind: String?
    let installedCount: UInt64
    let deletedCount: UInt64
    let restoredCount: UInt64
  }

  let runID: String
  let attemptID: String
  let id: String
  let operationID: String
  let policyRevision: UInt64
  let expiresAtUnixMS: UInt64
  let payloadHash: String
  let sourcePath: String
  let recoveryParentPath: String
  let entryCount: UInt64
  let decision: ServiceApprovalDecision?
  let invalidated: Bool
  let intentCommitted: Bool
  let resultSaved: Bool
  let result: Result?
  var key: Data { Data(id.utf8) }
  func isExpired(at now: Date = Date()) -> Bool {
    Double(expiresAtUnixMS) <= now.timeIntervalSince1970 * 1000
  }
  func shouldShowExpiry(at now: Date = Date()) -> Bool {
    isExpired(at: now) && decision == nil && !intentCommitted && !resultSaved && result == nil
  }

  init(_ value: ServiceValue) throws {
    try ServiceSchema.sourceApplySummary.check(value)
    runID = value["run_id"]!.string!; attemptID = value["attempt_id"]!.string!
    id = value["approval_id"]!.string!; operationID = value["operation_id"]!.string!
    policyRevision = value["policy_revision"]!.decimal!; expiresAtUnixMS = value["expires_at_unix_ms"]!.decimal!
    payloadHash = value["payload_hash"]!.string!; sourcePath = value["source_path"]!.string!
    recoveryParentPath = value["recovery_parent_path"]!.string!; entryCount = value["entry_count"]!.decimal!
    decision = value["decision"]?.string.flatMap(ServiceApprovalDecision.init(rawValue:))
    invalidated = value["invalidated"] == .bool(true); intentCommitted = value["intent_committed"] == .bool(true)
    resultSaved = value["result_saved"] == .bool(true)
    if let payload = value["result"] {
      result = .init(id: payload["result_id"]!.string!, status: payload["status"]!.string!,
        failureKind: payload["failure_kind"]?.string, installedCount: payload["installed_count"]!.decimal!,
        deletedCount: payload["deleted_count"]!.decimal!, restoredCount: payload["restored_count"]!.decimal!)
    } else { result = nil }
    guard result == nil || resultSaved else { throw ServiceError.correlation }
  }
}

struct SourceApplyPage: Equatable {
  let approvalID: String
  let payloadHash: String
  let sessionRevision: UInt64
  let entries: [SourceApplyCandidate.Entry]
  let nextOffset: UInt64?

  init(approvalID: String, payloadHash: String, sessionRevision: UInt64,
       entries: [SourceApplyCandidate.Entry], nextOffset: UInt64?) {
    self.approvalID = approvalID; self.payloadHash = payloadHash; self.sessionRevision = sessionRevision
    self.entries = entries; self.nextOffset = nextOffset
  }

  init(_ value: ServiceValue) throws {
    try ServiceSchema.sourceApplyPage.check(value)
    approvalID = value["approval_id"]!.string!; payloadHash = value["payload_hash"]!.string!
    sessionRevision = value["session_revision"]!.decimal!; nextOffset = value["next_offset"]?.decimal
    guard case .array(let values) = value["entries"] else { throw ServiceError.schema }
    var ids: Set<Data> = []
    entries = try values.map { item in
      let path = item["relative_path"]!.string!
      guard !path.isEmpty, ids.insert(Data(path.utf8)).inserted else { throw ServiceError.correlation }
      func version(_ key: String) -> SourceApplyCandidate.Version? {
        guard let value = item[key] else { return nil }
        guard case .number(let raw) = value["mode"], let mode = UInt32(raw) else { return nil }
        return .init(hash: value["hash"]!.string!, mode: UInt64(mode))
      }
      if item["before"] != nil && version("before") == nil { throw ServiceError.schema }
      if item["after"] != nil && version("after") == nil { throw ServiceError.schema }
      return .init(relativePath: path, before: version("before"), after: version("after"))
    }
  }
}
