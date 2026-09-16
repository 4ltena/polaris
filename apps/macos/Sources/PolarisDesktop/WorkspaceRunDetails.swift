import Foundation

/// One `run.details` page. Text is display data only; never run as a command,
/// link or markup.
struct ServiceRunDetailsPage: Sendable, Equatable {
  struct Fragment: Sendable, Equatable {
    let entryID: String
    let kind: String
    let offset: UInt64
    let totalBytes: UInt64
    let text: String
  }
  let snapshotID: String
  let runID: String
  let attemptID: String
  let runState: String
  let historyGap: Bool
  let fragments: [Fragment]
  let nextCursor: String?

  static let kinds: Set<String> = [
    "user_input", "mcp_prompt", "assistant", "tool_call", "tool_result", "tool_result_unsaved", "hosted_web_search",
  ]
  static let schema: ServiceSchema = .optionalFields([
    "snapshot_id": .id, "run_id": .id, "attempt_id": .id, "session_revision": .decimal,
    "accepted_revision": .decimal, "run_state": .text, "history_gap": .bool,
    "fragments": .array(.object([
      "entry_id": .id, "kind": .oneOf(kinds), "offset": .decimal, "total_bytes": .decimal, "text": .text,
    ])),
  ], ["next_cursor": .id])

  init(_ value: ServiceValue) throws {
    try Self.schema.check(value)
    snapshotID = value["snapshot_id"]!.string!
    runID = value["run_id"]!.string!
    attemptID = value["attempt_id"]!.string!
    runState = value["run_state"]!.string!
    historyGap = value["history_gap"] == .bool(true)
    nextCursor = value["next_cursor"]?.string
    guard case .array(let items) = value["fragments"], items.count <= 4 else { throw ServiceError.schema }
    fragments = items.map {
      Fragment(entryID: $0["entry_id"]!.string!, kind: $0["kind"]!.string!, offset: $0["offset"]!.decimal!,
               totalBytes: $0["total_bytes"]!.decimal!, text: $0["text"]!.string!)
    }
  }
}

/// Pages appended in order for one run. Any gap, overlap or foreign page is
/// refused so stale or mixed text is never shown as one transcript.
struct WorkspaceRunDetails: Equatable {
  struct Entry: Equatable, Identifiable {
    let id: String
    let kind: String
    var text: String
    let totalBytes: UInt64
    var complete: Bool { UInt64(text.utf8.count) == totalBytes }
    var isPrimary: Bool { kind == "user_input" || kind == "assistant" }
    var title: String {
      switch kind {
      case "user_input": "指示"
      case "mcp_prompt": "MCPプロンプト（外部の内容）"
      case "assistant": "回答"
      case "tool_call": "ツール要求"
      case "tool_result": "ツール結果"
      case "tool_result_unsaved": "ツール結果（保存されていません）"
      default: "Web検索"
      }
    }
  }
  let runKey: Data
  let attemptID: String
  let snapshotID: String
  private(set) var runState: String
  private(set) var historyGap: Bool
  private(set) var entries: [Entry] = []
  private(set) var nextCursor: String?

  init(run: WorkspaceRun, first page: ServiceRunDetailsPage) throws {
    runKey = run.key
    attemptID = run.attemptID
    snapshotID = page.snapshotID
    runState = page.runState
    historyGap = page.historyGap
    try append(page)
  }

  mutating func append(_ page: ServiceRunDetailsPage) throws {
    guard Data(page.runID.utf8) == runKey, page.attemptID.utf8.elementsEqual(attemptID.utf8),
          page.snapshotID.utf8.elementsEqual(snapshotID.utf8) else { throw ServiceError.correlation }
    var next = entries
    for fragment in page.fragments {
      if let last = next.last, last.id.utf8.elementsEqual(fragment.entryID.utf8) {
        guard !last.complete, fragment.offset == UInt64(last.text.utf8.count), fragment.kind == last.kind,
              fragment.totalBytes == last.totalBytes else { throw ServiceError.correlation }
        next[next.count - 1].text += fragment.text
      } else {
        guard next.last?.complete ?? true, fragment.offset == 0,
              !next.contains(where: { $0.id.utf8.elementsEqual(fragment.entryID.utf8) }) else {
          throw ServiceError.correlation
        }
        next.append(Entry(id: fragment.entryID, kind: fragment.kind, text: fragment.text, totalBytes: fragment.totalBytes))
      }
      guard UInt64(next[next.count - 1].text.utf8.count) <= fragment.totalBytes else { throw ServiceError.correlation }
    }
    // A final page must leave every entry whole.
    guard page.nextCursor != nil || (next.last?.complete ?? true) else { throw ServiceError.correlation }
    entries = next
    nextCursor = page.nextCursor
  }
}
