import Combine
import Foundation

/// Test seam for the local P5 screen; ServiceClient itself remains review-owned.
protocol ServiceDemoTransport: Sendable {
  func start() async throws -> ServiceHello
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply
  func takeEvents() async throws -> [ServiceValue]
  func close() async -> ServiceClient.Exit?
}
extension ServiceClient: ServiceDemoTransport {}

/// Fixture controller, not a product session. The parent owns this object and
/// must await close() before permitting window/app termination. onDisappear is
/// only a fallback; it cannot delay NSApplication termination.
@MainActor
final class ServiceDemoModel: ObservableObject {
  enum Phase: Equatable {
    case idle, starting, connected, closing, stopped, unknown
    var label: String {
      switch self {
      case .idle: "未起動"
      case .starting: "Helloを確認中"
      case .connected: "検証用helperに接続中"
      case .closing: "helperの終了を回収中"
      case .stopped: "helperの終了を確認"
      case .unknown: "通信状態不明・再開不可"
      }
    }
  }
  struct Run: Equatable {
    let id: String
    let attempt: String
    var state: String
    var isTerminal: Bool {
      ["succeeded", "failed", "cancelled", "interrupted", "outcome_unknown"].contains(state)
    }
    var label: String {
      switch state {
      case "queued": "待機中"
      case "loading": "ロード中"
      case "running": "実行中"
      case "awaiting_approval": "承認待ち"
      case "cancelling": "取消中"
      case "succeeded": "成功を観測"
      case "failed": "失敗を観測"
      case "cancelled": "取消完了を観測"
      case "interrupted": "中断を観測"
      default: "結果不明"
      }
    }
  }

  @Published private(set) var helperURL: URL?
  @Published private(set) var phase: Phase = .idle
  @Published private(set) var hello: ServiceHello?
  @Published private(set) var snapshot: ServiceValue?
  @Published private(set) var run: Run?
  @Published private(set) var runStartPending = false
  @Published private(set) var cancelAccepted = false
  @Published private(set) var shutdownReady = false
  @Published private(set) var lastExit: ServiceClient.Exit?
  @Published private(set) var message = "検証用helperを選択してください。選択だけでは起動しません。"
  @Published private(set) var lastRequestID: String?
  @Published private(set) var isBusy = false
  @Published private(set) var needsClose = false
  @Published private(set) var responseText = ""
  @Published var draftText = "こんにちは。これはP5のローカル通信試験です。"

  private let makeClient: @Sendable (URL) throws -> any ServiceDemoTransport
  private var client: (any ServiceDemoTransport)?
  private var preview = ResponsePreview()
  private var subscribed = false
  private var lastEventSequence: UInt64 = 0
  private var generation = UUID()
  private var monitor: Task<Void, Never>?
  private var closeTask: Task<Bool, Never>?
  private let session = "fake-session"
  static let maxDraftBytes = 16_384
  nonisolated static let maxDisplayBytes = 32_768

  init(
    makeClient: @escaping @Sendable (URL) throws -> any ServiceDemoTransport = {
      try ServiceClient(helperURL: $0)
    }
  ) {
    self.makeClient = makeClient
  }

  var canSelectHelper: Bool { (phase == .idle || phase == .stopped) && !needsClose && !isBusy }
  var canStart: Bool { canSelectHelper && helperURL != nil }
  var canSnapshot: Bool {
    phase == .connected && !isBusy && hello?.capabilities.contains("session_read") == true
  }
  var runLabel: String { runStartPending ? "開始要求の結果未確認" : (run?.label ?? "未開始") }
  var canRun: Bool {
    !runStartPending && canSnapshot && snapshot != nil && (run == nil || run?.isTerminal == true)
      && run?.state != "outcome_unknown"
      && !draftText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
      && draftText.utf8.count <= Self.maxDraftBytes
      && hello?.capabilities.contains("draft_update") == true
      && hello?.capabilities.contains("run_start") == true
  }
  var canCancel: Bool {
    phase == .connected && !isBusy && run != nil && run?.isTerminal == false && !cancelAccepted
      && hello?.capabilities.contains("run_cancel") == true
  }

  /// Only NSOpenPanel's explicit choice reaches this method. No saved/default URL.
  func selectHelper(_ url: URL?) {
    guard canSelectHelper, let url else { return }
    guard url.isFileURL, FileManager.default.isExecutableFile(atPath: url.path),
      url.lastPathComponent == "polaris-fake-service"
    else {
      message = "実行可能なpolaris-fake-serviceを選択してください。"
      return
    }
    helperURL = url
    message = "選択した検証用helperを引数なしで起動します。環境変数は引き継ぎません。"
  }

  func start() async {
    guard canStart, let helperURL else { return }
    let token = UUID()
    generation = token
    phase = .starting
    isBusy = true
    hello = nil
    snapshot = nil
    run = nil
    lastExit = nil
    subscribed = false
    lastEventSequence = 0
    runStartPending = false
    cancelAccepted = false
    shutdownReady = false
    responseText = ""
    preview = ResponsePreview()
    do {
      let transport = try makeClient(helperURL)
      client = transport
      needsClose = true
      let observed = try await transport.start()
      guard generation == token else { return }
      hello = observed
      phase = .connected
      isBusy = false
      message = "Hello確認済み。snapshotを取得すると合成runを開始できます。"
      monitor = Task { [weak self, transport] in
        while !Task.isCancelled {
          do {
            let events = try await transport.takeEvents()
            guard !Task.isCancelled else { return }
            self?.receive(events, generation: token)
          } catch {
            guard !Task.isCancelled else { return }
            await self?.transportFailed(transport, generation: token)
            return
          }
          do { try await Task.sleep(for: .milliseconds(50)) } catch { return }
        }
      }
    } catch {
      guard generation == token else { return }
      isBusy = false
      if (error as? ServiceError) == .launch {
        if let client { _ = await client.close() }
        guard generation == token else { return }
        client = nil
        needsClose = false
        phase = .idle
        message = "helperを起動できませんでした。選択を確認してください。"
      } else if let client {
        await transportFailed(client, generation: token)
      } else {
        phase = .idle
        message = "helperを起動できませんでした。選択を確認してください。"
      }
    }
  }

  func fetchSnapshot() async {
    guard canSnapshot, let client else { return }
    let token = generation
    isBusy = true
    do {
      // Subscribe once so ACK and terminal events remain separate observations.
      let request: ServiceRequest =
        !subscribed ? .subscribe(session: session) : .snapshot(session: session)
      let reply = try await send(request, using: client)
      guard generation == token else { return }
      isBusy = false
      guard let payload = reply.payload else {
        rejected()
        return
      }
      snapshot = payload
      subscribed = true
      if (payload["position"]?["event_seq"]?.decimal ?? 0) >= lastEventSequence,
        case .array(let runs) = payload["runs"], let current = runs.last,
        let id = current["run_id"]?.string, let attempt = current["attempt_id"]?.string,
        let state = current["state"]?.string
      {
        run = Run(id: id, attempt: attempt, state: state)
      }
      message = "snapshotを取得しました。合成データだけを表示しています。"
    } catch { await transportFailed(client, generation: token) }
  }

  func startRun() async {
    guard canRun, let client, let snapshot,
      let draft = snapshot["draft"]?["draft_revision"]?.decimal,
      let configuration = snapshot["configuration"]?["configuration_revision"]?.decimal,
      let policy = snapshot["policy_revision"]?.decimal
    else { return }
    let token = generation
    isBusy = true
    do {
      let saved = try await send(
        .draft(session: session, revision: draft, text: draftText), using: client)
      guard generation == token else { return }
      guard let nextDraft = saved.payload?["draft_revision"]?.decimal else {
        isBusy = false
        rejected()
        return
      }
      cancelAccepted = false
      responseText = ""
      preview.beginRun()
      runStartPending = true
      let accepted = try await send(
        .run(session: session, draft: nextDraft, configuration: configuration, policy: policy),
        using: client)
      guard generation == token else { return }
      isBusy = false
      guard let payload = accepted.payload, let id = payload["run_id"]?.string,
        let attempt = payload["attempt_id"]?.string
      else {
        // A protocol rejection is known; a thrown transport failure is not.
        if accepted.rejection != nil { runStartPending = false }
        rejected()
        return
      }
      runStartPending = false
      // An event can arrive before this continuation resumes. Never downgrade it to queued.
      if run?.id != id || run?.attempt != attempt {
        run = Run(id: id, attempt: attempt, state: "queued")
      }
      self.snapshot = nil  // An explicit fresh snapshot is required before another run.
      message = "run開始を受け付けました。終端はイベントで確認します。"
    } catch { await transportFailed(client, generation: token) }
  }

  func cancelRun() async {
    guard canCancel, let client, let run else { return }
    let token = generation
    isBusy = true
    do {
      let reply = try await send(
        .cancel(session: session, run: run.id, attempt: run.attempt), using: client)
      guard generation == token else { return }
      isBusy = false
      guard reply.payload?["status"] == .string("cancel_requested") else {
        rejected()
        return
      }
      cancelAccepted = true
      message = "取消要求を受け付けました。取消完了は終端イベントで確認します。"
    } catch { await transportFailed(client, generation: token) }
  }

  /// Parent Window/App integration MUST await this before allowing destruction.
  /// Concurrent/cancelled callers share cleanup; true requires observed child exit
  /// (or no child was ever owned). Shutdown ready alone never suffices.
  func close() async -> Bool {
    if let closeTask { return await closeTask.value }
    guard let transport = client else {
      if phase == .unknown && lastExit != nil { phase = .stopped }
      return phase != .unknown
    }
    let epoch = phase == .connected ? hello?.epoch : nil
    generation = UUID()
    monitor?.cancel()
    monitor = nil
    phase = .closing
    isBusy = true
    let task = Task { await self.finishClose(transport, epoch: epoch) }
    closeTask = task
    let result = await task.value
    closeTask = nil
    needsClose = client != nil
    return result
  }

  private func finishClose(
    _ transport: any ServiceDemoTransport, epoch: String?, keepUnknown: Bool = false
  ) async -> Bool {
    if let epoch {
      do {
        let reply = try await send(.shutdown(epoch: epoch), using: transport)
        shutdownReady = reply.payload?["state"] == .string("ready")
      } catch { shutdownReady = false }
    }
    let exit = await transport.close()
    lastExit = exit
    client = exit == nil ? transport : nil
    isBusy = false
    if run?.isTerminal == false { run?.state = "outcome_unknown" }
    phase = exit == nil || keepUnknown ? .unknown : .stopped
    message =
      exit == nil ? "helper終了を確認できません。再起動可能とは扱いません。" : "helperの終了を回収しました。runの結果不明はそのまま保持します。"
    return exit != nil
  }

  private func send(_ request: ServiceRequest, using transport: any ServiceDemoTransport)
    async throws -> ServiceReply
  {
    let id = UUID().uuidString
    lastRequestID = id
    return try await transport.send(requestID: id, request: request)
  }
  private func rejected() {
    snapshot = nil
    message = "要求が拒否されました。snapshotで状態を確認してください。自動再送はしません。"
  }
  private func transportFailed(_ transport: any ServiceDemoTransport, generation token: UUID) async
  {
    guard generation == token, phase != .closing else { return }
    generation = UUID()
    monitor?.cancel()
    monitor = nil
    phase = .unknown
    isBusy = true
    if run?.isTerminal == false { run?.state = "outcome_unknown" }
    message = "通信状態が不明です。helperの終了を回収しています。要求は自動再送しません。"
    // Retain ownership until close completes, independently of the public UI task.
    let task = Task { await self.finishClose(transport, epoch: nil, keepUnknown: true) }
    closeTask = task
    _ = await task.value
    closeTask = nil
    needsClose = client != nil
    phase = .unknown
    message = "通信状態は不明です。" + (lastExit == nil ? "helper終了も未確認です。" : "helper終了のみ確認済みです。")
  }

  private func receive(_ events: [ServiceValue], generation token: UUID) {
    guard generation == token, phase == .connected else { return }
    for event in events {
      guard event["session_id"]?.matchesID(session) == true else { continue }
      lastEventSequence = max(lastEventSequence, event["event_seq"]?.decimal ?? 0)
      let payload = event["payload"]
      if event["type"] == .string("run.state"), let id = payload?["run_id"]?.string,
        let attempt = payload?["attempt_id"]?.string, let state = payload?["state"]?.string
      {
        run = Run(id: id, attempt: attempt, state: state)
        if run?.isTerminal == true { preview.finishRun() }
      } else if event["type"] == .string("message.delta"), let payload {
        preview.apply(payload)
        responseText = preview.text

      }
    }
  }

  /// Fake service publishes saved offset-zero as the authoritative full message.
  /// P1 carries no run ID in TextDelta: bind one message within the ordered run
  /// window, and retain old IDs so known late messages never enter the next run.
  /// Unknown, previously unseen IDs cannot be independently attributed to a run.
  private struct ResponsePreview {
    var text = ""
    private var messageID: Data?
    private var retired: Set<Data> = []
    private var bytes = Data()
    private var length: UInt64 = 0
    private var saved = false
    private var accepting = false

    mutating func beginRun() {
      if let messageID { retired.insert(messageID) }
      messageID = nil
      bytes.removeAll()
      length = 0
      saved = false
      text = ""
      // Do not evict IDs and thereby admit old messages after a long demo session.
      accepting = retired.count < 256
    }
    mutating func finishRun() { accepting = false }

    mutating func apply(_ payload: ServiceValue) {
      guard let id = payload["message_id"]?.string, let offset = payload["byte_offset"]?.decimal,
        let content = payload["text"]?.string, let durability = payload["durability"]?.string,
        ["tentative", "saved"].contains(durability)
      else { return }
      let key = Data(id.utf8)
      guard !retired.contains(key) else { return }
      if let messageID {
        guard messageID == key else { return }
      } else {
        guard accepting, offset == 0 else { return }
        messageID = key
      }
      let incoming = Data(content.utf8)
      let (end, overflow) = offset.addingReportingOverflow(UInt64(incoming.count))
      guard !overflow else { return }
      if durability == "saved" && offset == 0 {
        // Replaces tentative content, including a shorter/corrected saved answer.
        bytes = Data(incoming.prefix(ServiceDemoModel.maxDisplayBytes))
        length = end
        saved = true
      } else {
        guard accepting, !saved, offset <= length else { return }
        // Overlaps/replays must agree with the retained UTF-8 bytes. A gap or
        // conflicting range is not appendable; a later saved full message recovers it.
        let visibleStart = min(Int(min(offset, UInt64(bytes.count))), bytes.count)
        let visibleEnd = Int(min(end, UInt64(bytes.count)))
        if visibleEnd > visibleStart {
          guard bytes[visibleStart..<visibleEnd] == incoming.prefix(visibleEnd - visibleStart)
          else { return }
        }
        if end > length {
          let skip = Int(length - offset)
          bytes.append(
            incoming.dropFirst(skip).prefix(ServiceDemoModel.maxDisplayBytes - bytes.count))
          length = end
        }
      }
      // Keep raw prefix bytes for subsequent overlap checks; trim only the view.
      var visible = bytes
      while !visible.isEmpty && String(data: visible, encoding: .utf8) == nil {
        visible.removeLast()
      }
      text = String(data: visible, encoding: .utf8) ?? ""
    }
  }

}
