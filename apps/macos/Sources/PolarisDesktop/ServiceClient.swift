import Darwin
import Foundation
import PolarisSettings

/// Owned pipe transport for an explicitly supplied helper. Production launches
/// accept only typed store coordinates, never arbitrary arguments or environment.
/// A cancelled send disconnects this transport; it is NOT a run.cancel ACK.
actor ServiceClient {
  enum RecoveryMode: Sendable { case disabled, resultOnly }

  struct PersistentStore: Sendable {
    let root: URL
    let projectID: String
    let sessionID: String
    private let directory: MetadataDirectory

    init(root: URL, projectID: String, sessionID: String) throws {
      guard root.isFileURL, root.path.hasPrefix("/"), root.path != "/",
        root.query == nil, root.fragment == nil, !root.path.contains("\0"),
        root.path.utf8.count <= 4096
      else { throw ServiceError.schema }
      try ServiceSchema.id.check(.string(projectID))
      try ServiceSchema.id.check(.string(sessionID))
      // Walk every metadata component with O_NOFOLLOW, retain its identity,
      // and check it again before launch. Foundation path rewriting is not an
      // identity check. Only the fixed macOS /tmp and /var aliases are accepted.
      do { directory = try MetadataDirectory(url: root, create: false) }
      catch { throw ServiceError.schema }
      self.root = directory.url
      self.projectID = projectID
      self.sessionID = sessionID
    }

    fileprivate func verify() throws { try directory.verify() }

    fileprivate var arguments: [String] {
      ["--store-root", root.path, "--project-id", projectID, "--session-id", sessionID]
    }
  }
  struct Configuration: Sendable {
    var helloTimeout: Duration = .seconds(5)
    var requestTimeout: Duration = .seconds(5)
    var frameTimeout: Duration = .seconds(5)
    var exitGrace: Duration = .seconds(1)
    var maxPending = 8
    var maxEventCount = 256
    var maxEventBytes = 4_194_304
  }
  struct Exit: Sendable, Equatable {
    let status: Int32
    let forced: Bool
  }
  private struct Pending {
    let method: String
    let session: String?
    let snapshotGeneration: UUID?
    let readRequest: ServiceReadRequest?
    let deadline: ContinuousClock.Instant
    let continuation: CheckedContinuation<ServiceReply, Error>
  }
  private struct Output {
    var bytes: Data
    var offset = 0
  }
  private let helperURL: URL
  private let clientID: String
  private let configuration: Configuration
  private let persistentStore: PersistentStore?
  private let recoveryMode: RecoveryMode
  private let ownerLaunch: OwnerServiceLaunch?
  private var recovery: SourceRecoveryClient?
  private var ready = false
  private var process: Process?
  private var input: FileHandle?
  private var output: FileHandle?
  private var codec = ServiceCodec()
  private var pending: [Data: Pending] = [:]
  private var outbox: [Output] = []
  private var outputBytes = 0
  private var events: [(ServiceValue, Int)] = []
  private var eventBytes = 0
  private var subscriptions: [Data: (session: String, sequence: UInt64)] = [:]
  private struct HistoryAnchor {
    let context: ServiceReadContext
    var nextCursor: String?
    var cursors: Set<Data> = []
    var messageIDs: Set<Data> = []
  }
  private var snapshotGenerations: [Data: UUID] = [:]
  private var historyAnchors: [Data: HistoryAnchor] = [:]
  private var snapshotRevisions: [Data: UInt64] = [:]
  private var frameStarted: ContinuousClock.Instant?
  private var closingAt: ContinuousClock.Instant?
  private var terminatedAt: ContinuousClock.Instant?
  private var killed = false
  private var started = false
  private var hello: ServiceHello?
  private var failure: ServiceError?
  private var exit: Exit?
  private var orderlyExit = false
  private var exitWaiters: [CheckedContinuation<Exit?, Never>] = []

  init(
    helperURL: URL, clientID: String = UUID().uuidString,
    configuration: Configuration = Configuration(), persistentStore: PersistentStore? = nil,
    recoveryMode: RecoveryMode = .disabled, ownerLaunch: OwnerServiceLaunch? = nil
  ) throws {
    guard helperURL.isFileURL, helperURL.path.hasPrefix("/"), helperURL.query == nil,
      helperURL.fragment == nil
    else {
      throw ServiceError.invalidHelper
    }
    try ServiceSchema.id.check(.string(clientID))
    guard configuration.helloTimeout > .zero, configuration.helloTimeout <= .seconds(30),
      configuration.requestTimeout > .zero, configuration.requestTimeout <= .seconds(30),
      configuration.frameTimeout > .zero, configuration.frameTimeout <= .seconds(30),
      configuration.exitGrace > .zero, configuration.exitGrace <= .seconds(5),
      (1...8).contains(configuration.maxPending), (1...256).contains(configuration.maxEventCount),
      (1...4_194_304).contains(configuration.maxEventBytes)
    else { throw ServiceError.capacity }
    guard recoveryMode == .disabled || persistentStore != nil else { throw ServiceError.schema }
    guard ownerLaunch == nil || (recoveryMode == .resultOnly && persistentStore != nil) else { throw ServiceError.schema }
    self.ownerLaunch = ownerLaunch
    self.recoveryMode = recoveryMode
    self.helperURL = helperURL
    self.clientID = clientID
    self.configuration = configuration
    self.persistentStore = persistentStore
  }

  func start() async throws -> ServiceHello {
    try await start(afterLaunch: {})
  }

  func start(afterLaunch: @Sendable () -> Void) async throws -> ServiceHello {
    guard !started, closingAt == nil else { throw ServiceError.notReady }
    try Task.checkCancellation()
    try persistentStore?.verify()
    try ownerLaunch?.verify()
    let channel: SourceRecoveryClient.Channel?
    if recoveryMode == .resultOnly, let store = persistentStore {
      channel = try SourceRecoveryClient.makeChannel(projectID: store.projectID, sessionID: store.sessionID)
    } else { channel = nil }
    recovery = channel?.client
    started = true
    let child = Process()
    let stdin = Pipe()
    let stdout = Pipe()
    child.executableURL = helperURL
    child.arguments = ownerLaunch?.arguments ?? ((persistentStore?.arguments ?? []) + (channel == nil ? [] : ["--source-recovery-fd2"]))
    child.environment = [:]
    child.standardInput = stdin
    child.standardOutput = stdout
    child.standardError = channel?.childEndpoint ?? FileHandle.nullDevice
    do {
      try child.run()
    } catch {
      try? channel?.childEndpoint.close()
      await recovery?.close()
      try? stdin.fileHandleForReading.close()
      try? stdin.fileHandleForWriting.close()
      try? stdout.fileHandleForReading.close()
      try? stdout.fileHandleForWriting.close()
      failure = .launch
      throw ServiceError.launch
    }
    process = child
    try? channel?.childEndpoint.close()
    try? stdin.fileHandleForReading.close()
    try? stdout.fileHandleForWriting.close()
    input = stdin.fileHandleForWriting
    output = stdout.fileHandleForReading
    // F_SETNOSIGPIPE is per descriptor, never a process-wide signal change.
    let configured =
      fcntl(input!.fileDescriptor, F_SETFL, O_NONBLOCK) != -1
      && fcntl(input!.fileDescriptor, F_SETNOSIGPIPE, 1) != -1
      && fcntl(output!.fileDescriptor, F_SETFL, O_NONBLOCK) != -1
    if !configured { beginClose(.io) }
    // The pump owns the child until exit even when a caller drops/cancels its
    // public future. No blocking FileHandle read/write runs on the actor.
    Task { await self.pump() }
    guard configured else { throw ServiceError.io }
    // Internal observation seam for cancellation at the post-launch boundary.
    afterLaunch()
    do {
      let id = UUID().uuidString
      let wire = ServiceValue.object(
        try ServiceRequest.envelope(clientID: clientID, requestID: id, method: "hello", params: [:])
      )
      let reply = try await exchange(id: id, method: "hello", session: nil, wire: wire)
      try Task.checkCancellation()
      guard closingAt == nil else { throw failure ?? .closed }
      guard let payload = reply.payload else { throw ServiceError.schema }
      let accepted = try ServiceHello(payload)
      hello = accepted
      if recovery != nil {
        _ = try await recoveryHello(requestID: UUID().uuidString)
        try Task.checkCancellation()
        guard closingAt == nil, exit == nil else { throw failure ?? .closed }
      }
      ready = true
      return accepted
    } catch {
      // Also covers cancellation before exchange registers a pending request.
      beginClose((error as? ServiceError) ?? (error is CancellationError ? .cancelled : .schema))
      throw error
    }
  }

  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    guard ready, let hello, closingAt == nil, exit == nil else { throw failure ?? .notReady }
    guard hello.capabilities.contains(request.capability) else { throw ServiceError.notReady }
    if case .shutdown(let epoch) = request, !epoch.utf8.elementsEqual(hello.epoch.utf8) {
      throw ServiceError.schema
    }
    let wire = try request.wire(clientID: clientID, requestID: requestID)
    if let store = persistentStore, let session = wire["session_id"],
      !session.matchesID(store.sessionID) { throw ServiceError.correlation }
    return try await exchange(
      id: requestID, method: request.method, session: wire["session_id"]?.string, wire: wire)
  }

  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
    guard ready, let hello, closingAt == nil, exit == nil else { throw failure ?? .notReady }
    guard hello.capabilities.contains(request.capability) else { throw ServiceError.notReady }
    let context = request.context
    guard context.epoch.utf8.elementsEqual(hello.epoch.utf8),
      let anchor = historyAnchors[Data(context.sessionID.utf8)], anchor.context.matches(context)
    else { throw ServiceError.correlation }
    if case .history(_, let cursor, _) = request {
      guard anchor.nextCursor?.utf8.elementsEqual(cursor.utf8) == true,
        !pending.values.contains(where: {
          $0.method == "history.page" && $0.session?.utf8.elementsEqual(context.sessionID.utf8) == true
        })
      else { throw ServiceError.correlation }
    }
    let wire = try request.wire(clientID: clientID, requestID: requestID)
    return try await exchange(id: requestID, method: request.method, session: context.sessionID,
                              wire: wire, readRequest: request)
  }

  /// Consumer pull avoids an unbounded AsyncStream/task mailbox. After overflow
  /// callers must obtain a fresh snapshot on a new connection.
  func takeEvents() throws -> [ServiceValue] {
    if let failure { throw failure }
    let result = events.map(\.0)
    events.removeAll(keepingCapacity: false)
    eventBytes = 0
    return result
  }
  var inFlightRequestCount: Int { pending.count }
  func terminalError() -> ServiceError? { failure }
  func observedExit() -> Exit? { exit }
  /// Only an orderly exit of this owned child is eligible for startup diagnostics.
  /// Hello EOF can arrive just before Foundation records termination, so wait a
  /// short bounded interval for the already-owned process observer.
  func ownedOrderlyExitStatus() async -> Int32? {
    for _ in 0..<20 {
      if let exit { return orderlyExit ? exit.status : nil }
      guard process != nil else { return nil }
      try? await Task.sleep(for: .milliseconds(10))
    }
    return nil
  }
  // Queried after close: closingAt then prevents any subsequent launch. A nil
  // process covers both a reaped child and cancellation before Process.run.
  func hasUnreapedChild() async -> Bool { process != nil }

  /// Caller owns request IDs, including the initial control hello if its ACK was lost.
  func recoveryUnknownRequestID() async -> String? { await recovery?.uncertainRequestID }

  func recoveryHello(requestID: String) async throws -> SourceRecoveryClient.Status {
    guard let recovery, process != nil, exit == nil else { throw ServiceError.notReady }
    let status = try await recovery.hello(requestID: requestID)
    try validateRecovery(status)
    return status
  }

  func retryResultSave(requestID: String, target: SourceRecoveryClient.Target) async throws -> SourceRecoveryClient.Status {
    guard let recovery, process != nil, exit == nil else { throw ServiceError.notReady }
    if let hello, !target.engineEpoch.utf8.elementsEqual(hello.epoch.utf8) { throw ServiceError.correlation }
    let status = try await recovery.retryResultSave(requestID: requestID, target: target)
    try validateRecovery(status)
    return status
  }

  private func validateRecovery(_ status: SourceRecoveryClient.Status) throws {
    guard process != nil, exit == nil, let store = persistentStore,
          status.projectID.utf8.elementsEqual(store.projectID.utf8),
          status.sessionID.utf8.elementsEqual(store.sessionID.utf8),
          hello == nil || status.engineEpoch.utf8.elementsEqual(hello!.epoch.utf8)
    else { throw ServiceError.correlation }
    // A typed status (including readyToExit) never substitutes for observed exit.
  }

  /// Closes stdin, then escalates only for this owned fake child if it does not
  /// exit. Completion means Process observed/reaped exit, not just a stop signal.
  /// Run shutdown ready is separately obtained through send(.shutdown(...)).
  func close() async -> Exit? {
    if let exit { return exit }
    guard process != nil else {
      beginClose(.closed)
      return nil
    }
    beginClose(.closed)
    // Result-only owners remain connected to control until the helper actually exits.
    // No implicit hello/retry or signal is permitted, even after channel failure.
    if recoveryMode == .resultOnly { return nil }
    // A cancelled caller still waits for the independently owned pump.
    return await withCheckedContinuation { exitWaiters.append($0) }
  }

  private func exchange(id: String, method: String, session: String?, wire: ServiceValue,
                        readRequest: ServiceReadRequest? = nil)
    async throws -> ServiceReply
  {
    try Task.checkCancellation()
    guard closingAt == nil else { throw failure ?? .closed }
    guard pending[Data(id.utf8)] == nil else { throw ServiceError.correlation }
    guard pending.count < configuration.maxPending else { throw ServiceError.capacity }
    if let session, method == "session.snapshot" || method == "session.subscribe",
      snapshotGenerations[Data(session.utf8)] == nil, snapshotGenerations.count >= 8 {
      throw ServiceError.capacity
    }
    let bytes = try ServiceCodec.encode(wire)
    guard bytes.count - 4 <= (hello?.frameBytes ?? ServiceCodec.maxFrameBytes),
      bytes.count <= 4_194_304 - outputBytes
    else { throw ServiceError.capacity }
    return try await withTaskCancellationHandler {
      try Task.checkCancellation()
      return try await withCheckedThrowingContinuation { continuation in
        if let session, method == "session.snapshot" || method == "session.subscribe" {
          let key = Data(session.utf8)
          // Supersede immediately: an old page arriving before the new snapshot
          // is still stale. Failed refreshes require an explicit fresh snapshot.
          snapshotGenerations[key] = UUID()
          historyAnchors[key] = nil
        }
        pending[Data(id.utf8)] = Pending(
          method: method, session: session,
          snapshotGeneration: session.flatMap { snapshotGenerations[Data($0.utf8)] },
          readRequest: readRequest,
          deadline: .now
            + (method == "hello" ? configuration.helloTimeout : configuration.requestTimeout),
          continuation: continuation)
        outbox.append(Output(bytes: bytes))
        outputBytes += bytes.count
      }
    } onCancel: {
      Task { await self.cancelExchange(id) }
    }
  }
  private func cancelExchange(_ id: String) {
    if pending[Data(id.utf8)] != nil { beginClose(.cancelled) }
  }

  private func beginClose(_ error: ServiceError) {
    guard closingAt == nil else { return }
    closingAt = .now
    failure = error
    ready = false
    if recoveryMode == .resultOnly {
      try? output?.close()
      output = nil
    }
    try? input?.close()
    input = nil
    outbox.removeAll()
    outputBytes = 0
    events.removeAll()
    eventBytes = 0
    historyAnchors.removeAll()
    snapshotGenerations.removeAll()
    snapshotRevisions.removeAll()
    let requests = pending.values
    pending.removeAll()
    for request in requests { request.continuation.resume(throwing: error) }
  }

  private func pump() async {
    while let child = process {
      do {
        try readAvailable()
        if closingAt == nil {
          try writeAvailable()
          if pending.values.contains(where: { $0.deadline <= .now }) { throw ServiceError.timeout }
          if let frameStarted, frameStarted + configuration.frameTimeout <= .now {
            throw ServiceError.timeout
          }
        }
      } catch { beginClose((error as? ServiceError) ?? .io) }
      if !child.isRunning {
        // Foundation reaps the child; isRunning false precedes no blocking wait.
        // Descendants may retain stdout. Their EOF is not the helper's exit,
        // and this Exit claims only the owned helper, never shutdown.ready.
        let status = child.terminationStatus
        orderlyExit = child.terminationReason == .exit
        let forced = terminatedAt != nil
        if closingAt == nil { beginClose(.eof) }
        try? output?.close()
        output = nil
        exit = Exit(status: status, forced: forced)
        process = nil
        await recovery?.close()
        let waiters = exitWaiters
        exitWaiters.removeAll()
        for waiter in waiters { waiter.resume(returning: exit) }
        return
      }
      if recoveryMode == .disabled, let closingAt, closingAt + configuration.exitGrace <= .now, terminatedAt == nil {
        child.terminate()
        terminatedAt = .now
      }
      if recoveryMode == .disabled, let terminatedAt, terminatedAt + configuration.exitGrace <= .now, !killed {
        _ = Darwin.kill(child.processIdentifier, SIGKILL)
        killed = true
      }
      // This pump is never tied to caller cancellation.
      try? await Task.sleep(for: .milliseconds(5))
    }
  }

  private func readAvailable() throws {
    guard let output else { return }
    var bytes = [UInt8](repeating: 0, count: 16_384)
    // A per-tick quantum keeps writes, cancellation and deadlines responsive.
    for _ in 0..<16 {
      let count = Darwin.read(output.fileDescriptor, &bytes, bytes.count)
      if count == 0 {
        try codec.finish()
        if closingAt == nil { beginClose(.eof) }
        try? output.close()
        self.output = nil
        return
      }
      if count < 0 {
        if errno == EAGAIN || errno == EWOULDBLOCK { return }
        if errno == EINTR { continue }
        throw ServiceError.io
      }
      if closingAt != nil { continue }
      let wasPartial = codec.hasPartialFrame
      let values = try codec.receive(Data(bytes.prefix(count)))
      for value in values { try received(value) }
      if !codec.hasPartialFrame {
        frameStarted = nil
      } else if !wasPartial || !values.isEmpty {
        frameStarted = .now
      }
    }
  }

  private func writeAvailable() throws {
    guard let input, !outbox.isEmpty else { return }
    let sent = outbox[0].bytes.withUnsafeBytes { raw in
      Darwin.write(
        input.fileDescriptor, raw.baseAddress!.advanced(by: outbox[0].offset),
        raw.count - outbox[0].offset)
    }
    if sent < 0 {
      if errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR { return }
      throw ServiceError.io
    }
    outbox[0].offset += sent
    if outbox[0].offset == outbox[0].bytes.count {
      outputBytes -= outbox[0].bytes.count
      outbox.removeFirst()
    }
  }

  private func received(_ value: ServiceValue) throws {
    if value["kind"] == .string("response") {
      guard let id = value["request_id"]?.string, let waiting = pending[Data(id.utf8)] else {
        throw ServiceError.correlation
      }
      let reply = try ServiceSchema.response(value, clientID: clientID, method: waiting.method)
      if let session = waiting.session,
        waiting.method == "session.snapshot" || waiting.method == "session.subscribe" || waiting.readRequest != nil
      {
        guard waiting.snapshotGeneration == snapshotGenerations[Data(session.utf8)] else {
          throw ServiceError.correlation
        }
      }
      if waiting.method == "hello", let payload = reply.payload {
        hello = try ServiceHello(payload)
      }
      if let session = waiting.session, let payload = reply.payload,
        waiting.method == "session.snapshot" || waiting.method == "session.subscribe"
      {
        guard payload["session_id"]?.matchesID(session) == true,
          payload["position"]?["engine_epoch"]?.matchesID(hello!.epoch) == true
        else { throw ServiceError.correlation }
        let context = try ServiceReadContext(snapshot: payload, epoch: hello!.epoch, sessionID: session)
        let sessionKey = Data(session.utf8)
        if let revision = snapshotRevisions[sessionKey], context.sessionRevision < revision {
          throw ServiceError.correlation
        }
        snapshotRevisions[sessionKey] = context.sessionRevision
        historyAnchors[Data(session.utf8)] = HistoryAnchor(
          context: context, nextCursor: payload["history_start_cursor"]!.string!)
        if waiting.method == "session.subscribe" {
          let id = payload["position"]!["subscription_id"]!.string!
          let key = Data(id.utf8)
          var retained = subscriptions.filter {
            !Data($0.value.session.utf8).elementsEqual(session.utf8)
          }
          // One current subscription per session; IDs must not alias another session.
          guard retained[key] == nil else { throw ServiceError.correlation }
          guard retained.count < 8 else {
            throw ServiceError.capacity
          }
          retained[key] = (session, payload["position"]!["event_seq"]!.decimal!)
          subscriptions = retained
          // The replacement snapshot supersedes already-buffered old events.
          events.removeAll { $0.0["session_id"]?.matchesID(session) == true }
          eventBytes = events.reduce(0) { $0 + $1.1 }
        }
      }
      if let request = waiting.readRequest, let payload = reply.payload {
        let context = request.context
        let key = Data(context.sessionID.utf8)
        guard context.epoch.utf8.elementsEqual(hello!.epoch.utf8),
          var anchor = historyAnchors[key], anchor.context.matches(context)
        else { throw ServiceError.correlation }
        switch request {
        case .history(_, let cursor, let limit):
          let page = try ServiceHistoryPage(payload)
          guard page.snapshotID.utf8.elementsEqual(context.snapshotID.utf8),
            page.sessionRevision == context.sessionRevision,
            anchor.nextCursor?.utf8.elementsEqual(cursor.utf8) == true,
            page.messages.count <= limit, anchor.cursors.insert(Data(cursor.utf8)).inserted
          else { throw ServiceError.correlation }
          if let next = page.nextCursor {
            guard !page.messages.isEmpty, !anchor.cursors.contains(Data(next.utf8)) else {
              throw ServiceError.correlation
            }
          }
          for message in page.messages {
            guard anchor.messageIDs.insert(Data(message.id.utf8)).inserted else {
              throw ServiceError.correlation
            }
          }
          guard anchor.messageIDs.count <= 65_536 else { throw ServiceError.capacity }
          anchor.nextCursor = page.nextCursor
          historyAnchors[key] = anchor
        case .status:
          _ = try ServiceRequestStatus(payload)
        }
      }
      if waiting.method == "shutdown.request", let payload = reply.payload,
        payload["engine_epoch"]?.matchesID(hello!.epoch) != true
      {
        throw ServiceError.correlation
      }
      pending.removeValue(forKey: Data(id.utf8))
      waiting.continuation.resume(returning: reply)
    } else {
      guard let hello else { throw ServiceError.notReady }
      try ServiceSchema.validateEvent(value, epoch: hello.epoch)
      let id = value["subscription_id"]!.string!
      guard let prior = subscriptions[Data(id.utf8)], value["session_id"]!.matchesID(prior.session),
        prior.sequence < UInt64.max, value["event_seq"]!.decimal == prior.sequence + 1
      else { throw ServiceError.correlation }
      let size = try ServiceCodec.encode(value).count
      guard events.count < min(configuration.maxEventCount, hello.eventCount),
        size <= min(configuration.maxEventBytes, hello.eventBytes) - eventBytes
      else { throw ServiceError.capacity }
      subscriptions[Data(id.utf8)] = (prior.session, prior.sequence + 1)
      events.append((value, size))
      eventBytes += size
    }
  }
}
