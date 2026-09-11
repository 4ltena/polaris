import Darwin
import Foundation

/// Independent, result-only channel. It never owns or terminates a Process.
actor SourceRecoveryClient {
  enum State: String, Sendable { case working, reportPending = "report_pending", recoveryRequired = "recovery_required", readyToExit = "ready_to_exit" }
  enum RemoteError: String, Sendable { case busy, targetMismatch = "target_mismatch", noRetainedReport = "no_retained_report", requestConflict = "request_conflict", storageFailed = "storage_failed" }
  enum Failure: Error, Equatable {
    case busy, closed, invalidRequest, invalidResponse, correlation, frameTooLarge, io
    case writeTimeout, frameTimeout, responseTimeout, cancelledBeforeSend, cancelledOutcomeUnknown
  }
  struct Target: Sendable, Equatable {
    let engineEpoch: String
    let projectID: String
    let sessionID: String
    let runID: String
    let attemptID: String
    let approvalID: String
    let operationID: String
    let payloadHash: String
  }
  struct RetryTarget: Sendable, Equatable {
    let runID: String
    let attemptID: String
    let approvalID: String
    let operationID: String
    let payloadHash: String
  }
  struct SavedResult: Sendable, Equatable {
    let operationID: String
    let resultID: String
    let savedRevision: UInt64
  }
  /// readyToExit is not a run-completion or child-exit claim.
  struct Status: Sendable, Equatable {
    let requestID: String
    let engineEpoch: String
    let projectID: String
    let sessionID: String
    let state: State
    let result: SavedResult?
    let retryTarget: RetryTarget?
    let error: RemoteError?
  }
  struct Channel {
    let client: SourceRecoveryClient
    /// Assign to Process.standardError at launch, then close this parent copy.
    /// The trusted helper must duplicate fd2 CLOEXEC and redirect fd2 to null.
    let childEndpoint: FileHandle
  }

  nonisolated static func makeChannel(projectID: String, sessionID: String) throws -> Channel {
    try validateID(projectID); try validateID(sessionID)
    var pair: [Int32] = [-1, -1]
    guard socketpair(AF_UNIX, SOCK_STREAM, 0, &pair) == 0 else { throw Failure.io }
    do {
      for fd in pair { guard fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 else { throw Failure.io } }
      let flags = fcntl(pair[0], F_GETFL)
      guard flags >= 0, fcntl(pair[0], F_SETFL, flags | O_NONBLOCK) == 0 else { throw Failure.io }
      var one: Int32 = 1
      guard setsockopt(pair[0], SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size)) == 0 else { throw Failure.io }
      return Channel(client: SourceRecoveryClient(fd: pair[0], projectID: projectID, sessionID: sessionID),
                     childEndpoint: FileHandle(fileDescriptor: pair[1], closeOnDealloc: true))
    } catch {
      Darwin.close(pair[0]); Darwin.close(pair[1]); throw error
    }
  }

  private var fd: Int32
  private let projectID: String
  private let sessionID: String
  private var engineEpoch: String?
  private var busy = false
  private var buffer = Data()
  private var frameStarted: ContinuousClock.Instant?
  private struct Request {
    let id: Data
    let wire: Data
    let target: Target?
  }
  private var unresolved: Request?
  private var completed: Request?
  private var completedProof: SavedResult?
  var uncertainRequestID: String? { unresolved.map { String(decoding: $0.id, as: UTF8.self) } }

  private init(fd: Int32, projectID: String, sessionID: String) {
    self.fd = fd; self.projectID = projectID; self.sessionID = sessionID
  }
  deinit { if fd >= 0 { Darwin.close(fd) } }
  func close() {
    if fd >= 0 { Darwin.close(fd); fd = -1 }
    buffer = Data(); frameStarted = nil
  }

  func hello(requestID: String) async throws -> Status {
    try await exchange(requestID: requestID,
      fields: ["version": .number("1"), "method": .string("hello"), "request_id": .string(requestID)], target: nil)
  }
  func retryResultSave(requestID: String, target: Target) async throws -> Status {
    guard target.projectID.utf8.elementsEqual(projectID.utf8), target.sessionID.utf8.elementsEqual(sessionID.utf8),
          engineEpoch?.utf8.elementsEqual(target.engineEpoch.utf8) == true else { throw Failure.invalidRequest }
    for id in [target.engineEpoch, target.projectID, target.sessionID, target.runID, target.attemptID, target.approvalID, target.operationID] {
      try Self.validateID(id)
    }
    try Self.validateHash(target.payloadHash)
    return try await exchange(requestID: requestID, fields: [
      "version": .number("1"), "method": .string("retry_result_save"), "request_id": .string(requestID),
      "engine_epoch": .string(target.engineEpoch), "project_id": .string(target.projectID), "session_id": .string(target.sessionID),
      "run_id": .string(target.runID), "attempt_id": .string(target.attemptID), "approval_id": .string(target.approvalID),
      "operation_id": .string(target.operationID), "payload_hash": .string(target.payloadHash)], target: target)
  }

  private func exchange(requestID: String, fields: [String: ServiceValue], target: Target?) async throws -> Status {
    try Self.validateID(requestID)
    guard !busy else { throw Failure.busy }
    guard fd >= 0 else { throw Failure.closed }
    try checkCancellation(written: 0)
    let wire = try ServiceCodec.encode(.object(fields))
    guard wire.count <= 8192 + 4 else { throw Failure.frameTooLarge }
    let request = Request(id: Data(requestID.utf8), wire: wire, target: target)
    if let unresolved, unresolved.id != request.id || unresolved.wire != wire { throw Failure.busy }
    if let completed, completed.id == request.id, completed.wire != wire { throw Failure.invalidRequest }
    busy = true
    defer { busy = false }
    var written = 0
    var drained = 0
    let responseDeadline = ContinuousClock.now + .seconds(5)
    do {
      // An explicit repeat may first consume an already-arrived original ACK.
      if unresolved != nil, let status = try readStatus(request: request, deadline: responseDeadline, drained: &drained) {
        completed = request; unresolved = nil; return status
      }
      let writeDeadline = ContinuousClock.now + .seconds(2)
      while written < wire.count {
        try checkCancellation(written: written)
        guard fd >= 0 else { throw Failure.closed }
        guard ContinuousClock.now < writeDeadline else { throw Failure.writeTimeout }
        let count = wire.withUnsafeBytes { bytes in
          Darwin.write(fd, bytes.baseAddress!.advanced(by: written), wire.count - written)
        }
        if count > 0 { written += count; unresolved = request }
        else if count < 0 && errno == EINTR { continue }
        else if count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK) { try await pause(written: written) }
        else { throw Failure.io }
      }
      while true {
        try checkCancellation(written: written)
        if let status = try readStatus(request: request, deadline: responseDeadline, drained: &drained) {
          completed = request; unresolved = nil; return status
        }
        guard ContinuousClock.now < responseDeadline else { throw Failure.responseTimeout }
        try await pause(written: written)
      }
    } catch {
      // A whole written request can be explicitly repeated with identical bytes.
      // A partial write/frame failure cannot safely reuse the framing channel.
      let failure = (error as? Failure) ?? .invalidResponse
      let retain = written == wire.count || (written == 0 && unresolved != nil)
      if failure != .cancelledBeforeSend && (!retain || ![Failure.responseTimeout, .cancelledOutcomeUnknown].contains(failure)) { close() }
      throw failure
    }
  }
  private func checkCancellation(written: Int) throws {
    if Task.isCancelled { throw written == 0 && unresolved == nil ? Failure.cancelledBeforeSend : Failure.cancelledOutcomeUnknown }
  }
  private func pause(written: Int) async throws {
    do { try await Task.sleep(for: .milliseconds(5)) }
    catch { throw written == 0 && unresolved == nil ? Failure.cancelledBeforeSend : Failure.cancelledOutcomeUnknown }
  }

  private func readStatus(request: Request, deadline: ContinuousClock.Instant, drained: inout Int) throws -> Status? {
    while true {
      guard ContinuousClock.now < deadline else { throw Failure.responseTimeout }
      try checkCancellation(written: 0)
      if let started = frameStarted, ContinuousClock.now - started >= .seconds(2) { throw Failure.frameTimeout }
      if buffer.count >= 4 {
        let length = buffer.prefix(4).reduce(0) { ($0 << 8) | Int($1) }
        guard length > 0, length <= 8192 else { throw Failure.frameTooLarge }
        if buffer.count >= length + 4 {
          let body = Data(buffer[4..<(length + 4)])
          buffer = Data(buffer.dropFirst(length + 4))
          frameStarted = buffer.isEmpty ? nil : frameStarted
          let status = try Self.decode(body)
          if Data(status.requestID.utf8) == request.id {
            try validate(status, target: request.target)
            if completed?.id == request.id {
              try validateSavedProof(status.result)
            } else {
              completedProof = status.result
            }
            return status
          }
          guard let completed, completed.id == Data(status.requestID.utf8),
                drained < 16 else { throw Failure.correlation }
          try validate(status, target: completed.target)
          try validateSavedProof(status.result)
          drained += 1 // Mutable old status never completes the current request.
          continue
        }
      }
      guard fd >= 0 else { throw Failure.closed }
      var bytes = [UInt8](repeating: 0, count: 4096)
      let count = Darwin.read(fd, &bytes, bytes.count)
      if count > 0 {
        if buffer.isEmpty { frameStarted = .now }
        guard buffer.count + count <= 2 * (8192 + 4) else { throw Failure.frameTooLarge }
        buffer.append(contentsOf: bytes.prefix(count))
      } else if count == 0 { throw Failure.closed }
      else if errno == EINTR { continue }
      else if errno == EAGAIN || errno == EWOULDBLOCK { return nil }
      else { throw Failure.io }
    }
  }
  // State/error and the next pending target describe current global state.
  // Only an already-observed saved proof is immutable across ACKs.
  private func validateSavedProof(_ incoming: SavedResult?) throws {
    guard let incoming else { return }
    if let prior = completedProof {
      guard Data(prior.operationID.utf8) == Data(incoming.operationID.utf8),
            Data(prior.resultID.utf8) == Data(incoming.resultID.utf8),
            prior.savedRevision == incoming.savedRevision else { throw Failure.correlation }
    } else {
      completedProof = incoming
    }
  }

  private func validate(_ status: Status, target: Target?) throws {
    guard status.projectID.utf8.elementsEqual(projectID.utf8), status.sessionID.utf8.elementsEqual(sessionID.utf8),
          engineEpoch == nil || engineEpoch!.utf8.elementsEqual(status.engineEpoch.utf8) else { throw Failure.correlation }
    if let target {
      if status.error == nil {
        guard let result = status.result,
              result.operationID.utf8.elementsEqual(target.operationID.utf8),
              status.engineEpoch.utf8.elementsEqual(target.engineEpoch.utf8) else { throw Failure.correlation }
      }
    } else if status.result != nil { throw Failure.correlation }
    engineEpoch = status.engineEpoch
  }
  private nonisolated static func validateID(_ id: String) throws {
    guard !id.isEmpty, id.utf8.count <= 128 else { throw Failure.invalidRequest }
  }

  private nonisolated static func validateHash(_ hash: String) throws {
    guard hash.utf8.count == 64, hash.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else { throw Failure.invalidRequest }
  }

  nonisolated static func decode(_ body: Data) throws -> Status {
    guard body.count <= 8192 else { throw Failure.frameTooLarge }
    do {
      let value = try ServiceCodec.json(body)
      try ServiceSchema.optionalFields([
        "version": .version, "request_id": .id, "engine_epoch": .id, "project_id": .id, "session_id": .id,
        "state": .oneOf(["working", "report_pending", "recovery_required", "ready_to_exit"])], [
        "result": .object(["operation_id": .id, "result_id": .id, "saved_revision": .decimal]),
        "retry_target": .object(["run_id": .id, "attempt_id": .id, "approval_id": .id, "operation_id": .id, "payload_hash": .text]),
        "error": .oneOf(["busy", "target_mismatch", "no_retained_report", "request_conflict", "storage_failed"])
      ]).check(value)
      let status = Status(requestID: value["request_id"]!.string!, engineEpoch: value["engine_epoch"]!.string!,
        projectID: value["project_id"]!.string!, sessionID: value["session_id"]!.string!, state: State(rawValue: value["state"]!.string!)!,
        result: value["result"].map { SavedResult(operationID: $0["operation_id"]!.string!, resultID: $0["result_id"]!.string!, savedRevision: $0["saved_revision"]!.decimal!) },
        retryTarget: value["retry_target"].map { RetryTarget(runID: $0["run_id"]!.string!, attemptID: $0["attempt_id"]!.string!, approvalID: $0["approval_id"]!.string!, operationID: $0["operation_id"]!.string!, payloadHash: $0["payload_hash"]!.string!) },
        error: value["error"]?.string.flatMap(RemoteError.init(rawValue:)))
      guard (status.state == .reportPending) == (status.retryTarget != nil),
            status.result == nil || status.error == nil else { throw Failure.invalidResponse }
      if let target = status.retryTarget {
        do { try validateHash(target.payloadHash) } catch { throw Failure.invalidResponse }
      }
      return status
    } catch let error as Failure { throw error }
    catch { throw Failure.invalidResponse }
  }
}
