import Foundation
import PolarisSettings

/// Existing v1 wire values only. No provider defaults or inferred run ownership.
enum ServiceApprovalDecision: String, Sendable { case allow, deny }

struct WorkspaceMemoryUsage: Equatable {
  let inputTokens: UInt64
  let outputTokens: UInt64
  let cachedTokens: UInt64
  let reportedResponses: UInt64
  let missingResponses: UInt64
  let failedRequests: UInt64
  init(_ value: ServiceValue) throws {
    try ServiceSchema.memoryUsage.check(value)
    inputTokens = value["input_tokens"]!.decimal!; outputTokens = value["output_tokens"]!.decimal!
    cachedTokens = value["cached_tokens"]!.decimal!; reportedResponses = value["reported_responses"]!.decimal!
    missingResponses = value["missing_responses"]!.decimal!; failedRequests = value["failed_requests"]!.decimal!
  }
}

struct WorkspaceEmbeddingUsage: Equatable {
  let requests: UInt64; let completed: UInt64; let failed: UInt64; let unknown: UInt64; let inputTokens: UInt64
  init(_ value: ServiceValue) throws {
    try ServiceSchema.embeddingUsage.check(value)
    requests = value["requests"]!.decimal!; completed = value["completed"]!.decimal!
    failed = value["failed"]!.decimal!; unknown = value["unknown"]!.decimal!; inputTokens = value["input_tokens"]!.decimal!
  }
}

struct WorkspaceMemoryStatus: Equatable {
  let runID: String; let phase: String; let detail: String; let recentRawTurns: UInt64
  let retrievalSources: [String]; let referenceTokens: UInt64
  let summaryUsage: WorkspaceMemoryUsage?; let mainUsage: WorkspaceMemoryUsage?; let embeddingUsage: WorkspaceEmbeddingUsage?; let totalUsage: WorkspaceMemoryUsage?
  init(_ value: ServiceValue) throws {
    try ServiceSchema.memory.check(value)
    guard case .array(let sources) = value["retrieval_sources"], sources.count <= 3,
          sources.allSatisfy({ $0.string?.hasPrefix("conversation://") == true }),
          let tokens = value["reference_tokens"]?.decimal, tokens <= 768 else { throw ServiceError.schema }
    runID = value["run_id"]!.string!; phase = value["phase"]!.string!; detail = value["detail"]!.string!
    recentRawTurns = value["recent_raw_turns"]!.decimal!; retrievalSources = sources.map { $0.string! }; referenceTokens = tokens
    summaryUsage = try value["summary_usage"].map(WorkspaceMemoryUsage.init)
    mainUsage = try value["main_usage"].map(WorkspaceMemoryUsage.init)
    embeddingUsage = try value["embedding_usage"].map(WorkspaceEmbeddingUsage.init)
    totalUsage = try value["total_usage"].map(WorkspaceMemoryUsage.init)
  }
  var phaseTitle: String {
    switch phase { case "preparing": "準備中"; case "ready": "準備完了"; case "failed": "準備失敗"; default: "結果不明" }
  }
}

struct WorkspaceRun: Identifiable, Equatable {
  let id: String
  var key: Data { Data(id.utf8) }
  let attemptID: String
  let generation: UUID
  let state: String
  static func == (lhs: Self, rhs: Self) -> Bool {
    lhs.key == rhs.key && lhs.attemptID.utf8.elementsEqual(rhs.attemptID.utf8)
      && lhs.generation == rhs.generation && lhs.state == rhs.state
  }
  var isTerminal: Bool { ["succeeded", "failed", "cancelled", "interrupted", "outcome_unknown"].contains(state) }
  var blocksStart: Bool { !isTerminal || state == "outcome_unknown" }
  var title: String {
    switch state {
    case "queued": "待機中"
    case "loading": "読込中"
    case "running": "実行中"
    case "awaiting_approval": "承認待ち"
    case "cancelling": "取消処理中"
    case "succeeded": "完了"
    case "failed": "失敗"
    case "cancelled": "取消済み"
    case "interrupted": "中断"
    default: "結果不明"
    }
  }
  init(_ value: ServiceValue, generation: UUID) throws {
    self.generation = generation
    try ServiceSchema.run.check(value)
    id = value["run_id"]!.string!; attemptID = value["attempt_id"]!.string!; state = value["state"]!.string!
  }
}

struct WorkspaceApproval: Identifiable, Equatable {
  let id: String
  var key: Data { Data(id.utf8) }
  let runID: String
  let attemptID: String
  let generation: UUID
  let policy: UInt64
  let expires: UInt64
  let title: String
  let detail: String
  let scope: String
  let choices: [ServiceApprovalDecision]
  // Entire observed payload binds a click to this exact displayed operation.
  let payload: ServiceValue
  private let fingerprint: Data
  static func == (lhs: Self, rhs: Self) -> Bool {
    lhs.generation == rhs.generation && lhs.fingerprint == rhs.fingerprint
  }
  init(_ value: ServiceValue, generation: UUID) throws {
    self.generation = generation
    try ServiceSchema.approval.check(value)
    payload = value
    fingerprint = try ServiceCodec.encode(value)
    id = value["approval_id"]!.string!; runID = value["run_id"]!.string!; attemptID = value["attempt_id"]!.string!
    policy = value["policy_revision"]!.decimal!; expires = value["expires_at_unix_ms"]!.decimal!
    title = value["display"]!["title"]!.string!; detail = value["display"]!["description"]!.string!
    scope = value["scope"]!.string!
    guard case .array(let values) = value["display"]!["choices"] else { throw ServiceError.schema }
    choices = values.compactMap { $0.string.flatMap(ServiceApprovalDecision.init(rawValue:)) }
  }
}

struct WorkspaceStreamMessage: Identifiable, Equatable {
  let id: String
  var key: Data { Data(id.utf8) }
  private(set) var text = ""
  private(set) var saved = false
  mutating func apply(_ value: ServiceValue) throws {
    let incoming = Data(value["text"]!.string!.utf8)
    let offset = value["byte_offset"]!.decimal!
    let durable = value["durability"] == .string("saved")
    var bytes = Data(text.utf8)
    if durable && offset == 0 && !saved {
      guard incoming.count <= 1_048_576 else { throw ServiceError.capacity }
      bytes = incoming; saved = true
    } else {
      guard offset <= UInt64(bytes.count), offset <= UInt64(Int.max) else { throw ServiceError.correlation }
      let start = Int(offset), overlap = min(incoming.count, bytes.count - start)
      guard bytes.subdata(in: start..<(start + overlap)) == incoming.prefix(overlap),
            durable || !saved || incoming.count <= overlap else { throw ServiceError.correlation }
      guard incoming.count - overlap <= 1_048_576 - bytes.count else { throw ServiceError.capacity }
      bytes.append(incoming.dropFirst(overlap))
      saved = saved || durable
    }
    guard let decoded = String(data: bytes, encoding: .utf8) else { throw ServiceError.schema }
    text = decoded
  }
}
