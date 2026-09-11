import Foundation
import XCTest
@testable import PolarisDesktop

/// Deterministic durable-session replies; all fabricated values stay in tests.
private actor WorkspaceTransportFixture: WorkspaceServiceTransport {
  var snapshot: ServiceValue
  var pages: [ServiceValue]
  var status: ServiceValue = .object(["status": .string("not_found")])
  var requests: [String] = []
  var heldMethod: String?
  var waiting: CheckedContinuation<Void, Never>?
  var rejection: String?
  var nextFailure: ServiceError?
  var exit: ServiceClient.Exit? = .init(status: 0, forced: false)
  var shutdownState = "ready"
  var pendingCloses = 0
  init(snapshot: ServiceValue, pages: [ServiceValue]) { self.snapshot = snapshot; self.pages = pages }
  func start() throws -> ServiceHello {
    try ServiceHello(ServiceCodec.json(Data(#"{"protocol_version":1,"engine_epoch":"e","capabilities":["session_read","history_read","draft_update","request_status","shutdown"],"limits":{"frame_bytes":"1048576","subscription_events":"256","subscription_bytes":"4194304","text_batch_bytes":"32768","text_batch_ms":"50"}}"#.utf8)))
  }
  func hold(_ method: String) { heldMethod = method }
  func isWaiting() -> Bool { waiting != nil }
  func release() { heldMethod = nil; waiting?.resume(); waiting = nil }
  func setSnapshot(_ value: ServiceValue) { snapshot = value }
  func setPages(_ value: [ServiceValue]) { pages = value }
  func reject(_ code: String) { rejection = code }
  func failNext(_ value: ServiceError) { nextFailure = value }
  func setStatus(_ value: ServiceValue) { status = value }
  func setExit(_ value: ServiceClient.Exit?) { exit = value }
  func deferExitForCloses(_ count: Int) { pendingCloses = count }
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply {
    requests.append(request.method)
    if let nextFailure { self.nextFailure = nil; throw nextFailure }
    let captured = snapshot
    if heldMethod == request.method { await withCheckedContinuation { waiting = $0 } }
    if let rejection {
      self.rejection = nil
      return .init(requestID: requestID, method: nil, payload: nil,
                   rejection: .object(["code": .string(rejection), "message": .string("合成拒否")]))
    }
    let payload: ServiceValue
    switch request {
    case .snapshot, .subscribe: payload = captured
    case .draft(_, let revision, _):
      payload = .object(["session_revision": .string("11"), "draft_revision": .string(String(revision + 1))])
    case .shutdown: payload = .object(["engine_epoch": .string("e"), "state": .string(shutdownState)])
    default: throw ServiceError.notReady
    }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply {
    requests.append(request.method)
    let payload: ServiceValue
    switch request {
    case .history(_, let cursor, _):
      if cursor.hasPrefix("page-"), let index = Int(cursor.dropFirst(5)), pages.indices.contains(index) {
        payload = pages[index]
      } else { payload = cursor == "first" ? pages[0] : (cursor == "third" && pages.count > 2 ? pages[2] : pages[1]) }
    case .status: payload = status
    }
    if heldMethod == request.method { await withCheckedContinuation { waiting = $0 } }
    return .init(requestID: requestID, method: request.method, payload: payload, rejection: nil)
  }
  func takeEvents() -> [ServiceValue] { [] }
  func close() async -> ServiceClient.Exit? {
    requests.append("transport.close")
    if pendingCloses > 0 { pendingCloses -= 1; return nil }
    if heldMethod == "transport.close" { await withCheckedContinuation { waiting = $0 } }
    return exit
  }
}

@MainActor
final class WorkspaceServiceTests: XCTestCase {
  private var repository: URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
      .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
  }
  private func snapshot(session: String = "s", epoch: String = "e", id: String = "snap",
                        revision: String = "10", draftRevision: String = "4", text: String = "保存した下書き") throws -> ServiceValue {
    let url = repository.appendingPathComponent("crates/polaris-desktop-protocol/tests/fixtures/snapshot.json")
    guard case .object(var fields) = try ServiceCodec.json(Data(contentsOf: url)) else { throw ServiceError.schema }
    fields["session_id"] = .string(session); fields["snapshot_id"] = .string(id)
    fields["session_revision"] = .string(revision); fields["history_start_cursor"] = .string("first")
    fields["position"] = .object(["engine_epoch": .string(epoch), "subscription_id": .string("sub"), "event_seq": .string("0")])
    fields["draft"] = .object(["draft_revision": .string(draftRevision), "text": .string(text), "attachment_ids": .array([])])
    return .object(fields)
  }
  private func page(id: String = "snap", revision: String = "10", messageID: String = "m1",
                    role: String = "tool", text: String = "日本語", next: String? = nil) -> ServiceValue {
    var fields: [String: ServiceValue] = [
      "snapshot_id": .string(id), "session_revision": .string(revision),
      "messages": .array([.object(["message_id": .string(messageID), "role": .string(role),
        "text": .string(text), "saved_byte_offset": .string(String(text.utf8.count))])]),
    ]
    if let next { fields["next_cursor"] = .string(next) }
    return .object(fields)
  }
  private func model(_ fixture: WorkspaceTransportFixture,
                     recoveryMode: ServiceClient.RecoveryMode = .disabled) throws -> WorkspaceServiceModel {
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory,
                                                 projectID: "p", sessionID: "s")
    return try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"),
                                      store: store, clientID: "native", makeTransport: { fixture }, recoveryMode: recoveryMode)
  }
  func testReadyOwnerCloseWaitsForObservedExitWithoutRecoveryRequests() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture, recoveryMode: .resultOnly)
    await model.start()
    await fixture.deferExitForCloses(2)
    let closed = await model.saveAndClose()
    XCTAssertTrue(closed)
    XCTAssertEqual(model.phase, .closed)
    XCTAssertEqual(model.lastExit, .init(status: 0, forced: false))
    let requests = await fixture.requests
    XCTAssertEqual(requests.suffix(4), ["shutdown.request", "transport.close", "transport.close", "transport.close"])
  }
  private func wait(_ fixture: WorkspaceTransportFixture) async throws {
    let deadline = ContinuousClock.now + .seconds(3)
    while !(await fixture.isWaiting()), ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    let waiting = await fixture.isWaiting()
    XCTAssertTrue(waiting)
  }

  func testTypedHistoryAndAllLedgerStatesRejectInvalidShapes() throws {
    let decoded = try ServiceHistoryPage(page())
    XCTAssertEqual(decoded.messages[0].role, .tool)
    XCTAssertEqual(decoded.messages[0].savedByteOffset, 9)
    let inputs = [
      #"{"status":"not_found"}"#,
      #"{"status":"accepted","session_revision":"18446744073709551615"}"#,
      #"{"status":"accepted","session_revision":"2","run":{"run_id":"r","attempt_id":"a"}}"#,
      #"{"status":"completed","session_revision":"3","result_id":"result"}"#,
      #"{"status":"outcome_unknown","session_revision":"4","run_id":"r","attempt_id":"a"}"#,
    ]
    let values = try inputs.map { try ServiceRequestStatus(ServiceCodec.json(Data($0.utf8))) }
    XCTAssertEqual(values[0], .notFound)
    XCTAssertEqual(values[1], .accepted(revision: UInt64.max, run: nil))
    XCTAssertEqual(values[2], .accepted(revision: 2, run: .init(id: "r", attemptID: "a")))
    XCTAssertEqual(values[3], .completed(revision: 3, resultID: "result"))
    XCTAssertEqual(values[4], .outcomeUnknown(revision: 4, run: .init(id: "r", attemptID: "a")))
    for invalid in [#"{"status":"not_found","session_revision":"1"}"#,
                    #"{"status":"accepted","session_revision":1}"#,
                    #"{"status":"completed","session_revision":"1"}"#,
                    #"{"status":"accepted","session_revision":"1","run":null}"#] {
      XCTAssertThrowsError(try ServiceRequestStatus(ServiceCodec.json(Data(invalid.utf8))))
    }
    XCTAssertThrowsError(try ServiceHistoryPage(page(role: "invented")))
  }

  func testReadWireUsesExactSnapshotCursorAndLedgerOwner() throws {
    let context = try ServiceReadContext(snapshot: snapshot(), epoch: "e", sessionID: "s")
    let wire = try ServiceReadRequest.history(context: context, cursor: "opaque", limit: 4)
      .wire(clientID: "native", requestID: "q")
    XCTAssertEqual(wire["params"]?["limit"], .number("4"))
    XCTAssertEqual(wire["params"]?["cursor"], .string("opaque"))
    XCTAssertEqual(wire["session_id"], .string("s"))
    XCTAssertThrowsError(try ServiceReadRequest.history(context: context, cursor: "x", limit: 0).wire(clientID: "native", requestID: "q"))
    let status = try ServiceReadRequest.status(context: context, clientID: "original", requestID: "lost")
      .wire(clientID: "native", requestID: "lookup")
    XCTAssertEqual(status["params"]?["client_id"], .string("original"))
    XCTAssertEqual(status["params"]?["request_id"], .string("lost"))
    XCTAssertThrowsError(try ServiceReadContext(snapshot: snapshot(epoch: "old"), epoch: "e", sessionID: "s"))
    XCTAssertThrowsError(try ServiceReadContext(snapshot: snapshot(session: "other"), epoch: "e", sessionID: "s"))
    XCTAssertThrowsError(try ServiceReadContext(snapshot: snapshot(session: "é"), epoch: "e", sessionID: "e\u{301}"))
  }

  func testCatchUpHistoryBeyond516MessagesKeepsTailAfterRefresh() async throws {
    let pages = (0..<520).map { index in
      page(messageID: "message-\(index)", role: index % 2 == 0 ? "user" : "assistant",
           next: index < 519 ? "page-\(index + 1)" : nil)
    }
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: pages)
    let model = try model(fixture)
    await model.start()
    await model.catchUpHistory()
    XCTAssertEqual(model.messages.count, 520)
    XCTAssertEqual(model.messages.last?.id, "message-519")
    XCTAssertNil(model.nextHistoryCursor)
    await model.refresh()
    XCTAssertEqual(model.messages.count, 520)
    XCTAssertEqual(model.messages.last?.id, "message-519")
    XCTAssertTrue(model.retainedHistory.isEmpty)
    _ = await model.close()
  }

  func testCatchUpHistoryReachesMessagesBeyondFirstPageWithoutSending() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page(next: "second"), page(messageID: "m2", role: "user", next: "third"), page(messageID: "m3", role: "assistant")])
    let model = try model(fixture)
    await model.start()
    await model.catchUpHistory()
    XCTAssertEqual(model.messages.map(\.id), ["m1", "m2", "m3"])
    XCTAssertNil(model.nextHistoryCursor)
    let requests = await fixture.requests
    XCTAssertEqual(requests, ["session.subscribe", "history.page", "history.page", "history.page"])
    await model.refresh()
    XCTAssertEqual(model.messages.map(\.id), ["m1", "m2", "m3"])
    XCTAssertTrue(model.retainedHistory.isEmpty)
    XCTAssertNil(model.nextHistoryCursor)
    _ = await model.close()
  }

  func testLoadsFirstPageThenExplicitHistoryPreservesRolesAndDoesNotInventData() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page(next: "second"), page(messageID: "m2", role: "assistant")])
    let model = try model(fixture)
    await model.start()
    XCTAssertEqual(model.phase, .ready)
    XCTAssertEqual(model.messages.map(\.id), ["m1"])
    XCTAssertEqual(model.nextHistoryCursor, "second")
    await model.loadNextHistoryPage()
    XCTAssertEqual(model.messages.map(\.id), ["m1", "m2"])
    XCTAssertEqual(model.messages.map(\.role), [.tool, .assistant])
    XCTAssertEqual(model.draftText, "保存した下書き")
    XCTAssertEqual(model.snapshot, try snapshot())
    XCTAssertFalse(model.canSend)
    await model.lookupRequest(clientID: "original", requestID: "lost")
    XCTAssertEqual(model.requestStatus, .notFound)
    let requests = await fixture.requests
    XCTAssertEqual(requests, ["session.subscribe", "history.page", "history.page", "request.status"])
    let closed = await model.close(); XCTAssertTrue(closed)
  }

  func testStaleSessionEpochSnapshotRevisionAndCursorNeverPublish() async throws {
    let scenarios: [(ServiceValue, [ServiceValue])] = try [
      (snapshot(session: "other"), [page()]), (snapshot(epoch: "old"), [page()]),
      (snapshot(), [page(id: "old")]), (snapshot(), [page(revision: "9")]),
      (snapshot(), [page(next: "first")]),

    ]
    for (snapshot, pages) in scenarios {
      let fixture = WorkspaceTransportFixture(snapshot: snapshot, pages: pages)
      let model = try model(fixture)
      await model.start()
      XCTAssertEqual(model.phase, .unavailable)
      XCTAssertNil(model.snapshot); XCTAssertTrue(model.messages.isEmpty)
      _ = await model.close()
    }
  }

  func testLateSaveACKKeepsNextEditAndBlocksConcurrentSave() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await model.start(); model.editDraft("保存要求の本文")
    await fixture.hold("draft.update")
    let save = Task { await model.saveDraft() }
    try await wait(fixture)
    model.editDraft("ACK待ち中の追加入力")
    await model.saveDraft()
    await fixture.release(); await save.value
    XCTAssertEqual(model.draftText, "ACK待ち中の追加入力")
    XCTAssertEqual(model.savedDraftRevision, 5)
    XCTAssertTrue(model.isDirty); XCTAssertTrue(model.canSaveDraft)
    XCTAssertFalse(model.draftOutcomeUnknown)
    let requests = await fixture.requests
    XCTAssertEqual(requests.filter { $0 == "draft.update" }.count, 1)
    let closed = await model.close(); XCTAssertFalse(closed)
  }

  func testRefreshDoesNotOverwriteDirtyDraftOrRebaseAcrossExternalSave() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await model.start(); model.editDraft("ローカル編集中")
    await fixture.setSnapshot(try snapshot(draftRevision: "5", text: "外部の保存"))
    await model.refresh()
    XCTAssertEqual(model.draftText, "ローカル編集中")
    XCTAssertEqual(model.savedDraftRevision, 4)
    XCTAssertTrue(model.draftConflict); XCTAssertFalse(model.canSaveDraft)
    model.useObservedDraft()
    XCTAssertEqual(model.draftText, "外部の保存")
    XCTAssertFalse(model.isDirty); XCTAssertFalse(model.draftConflict)
    _ = await model.close()
  }

  func testRejectedSaveKeepsTextAndRequiresExplicitConflictResolution() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await model.start(); model.editDraft("消してはいけない本文")
    await fixture.reject("revision_conflict")
    await model.saveDraft()
    XCTAssertEqual(model.draftText, "消してはいけない本文")
    XCTAssertEqual(model.failure, .rejected("revision_conflict"))
    XCTAssertFalse(model.draftOutcomeUnknown); XCTAssertTrue(model.draftConflict)
    XCTAssertFalse(model.canSaveDraft)
    _ = await model.close()
  }

  func testCloseInvalidatesHeldHistoryAndRetainsUncertainSave() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await fixture.hold("history.page")
    let start = Task { await model.start() }
    try await wait(fixture)
    _ = await model.close()
    await fixture.release(); await start.value
    XCTAssertNil(model.snapshot)
    XCTAssertNotEqual(model.phase, .ready)

    let second = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let saving = try self.model(second)
    await saving.start(); saving.editDraft("保存中")
    await second.hold("draft.update")
    let save = Task { await saving.saveDraft() }
    try await wait(second)
    let closed = await saving.close(); XCTAssertFalse(closed)
    await second.release(); await save.value
    XCTAssertTrue(saving.draftOutcomeUnknown)
    XCTAssertTrue(saving.isDirty)
    XCTAssertEqual(saving.draftText, "保存中")
  }

  func testShutdownReadyWithoutExitDoesNotReleaseOwner() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await model.start(); await fixture.setExit(nil)
    let closed = await model.close(); XCTAssertFalse(closed)
    XCTAssertEqual(model.phase, .unavailable)
    await model.start()
    XCTAssertEqual(model.phase, .unavailable)
    await fixture.setExit(.init(status: 0, forced: false))
    _ = await model.close()
  }
}

extension WorkspaceServiceTests {
  /// Real framed IPC exercises the actor's request registration and response
  /// correlation, not just the injected model seam. The script owns no user data.
  private func pipeFixture(_ behavior: String) throws -> URL {
    let json = String(decoding: try ServiceCodec.encode(snapshot()).dropFirst(4), as: UTF8.self)
    let quoted = String(decoding: try JSONEncoder().encode(json), as: UTF8.self)
    let source = #"""
    #!/usr/bin/python3
    import sys, struct, json
    def read():
        h = sys.stdin.buffer.read(4)
        if len(h) != 4: sys.exit(0)
        return json.loads(sys.stdin.buffer.read(struct.unpack('>I', h)[0]))
    def reply(r, payload):
        v = {'protocol_version':1,'kind':'response','client_id':r['client_id'],'request_id':r['request_id'],
             'result':{'type':r['method'],'payload':payload}}
        b = json.dumps(v).encode()
        sys.stdout.buffer.write(struct.pack('>I',len(b))+b); sys.stdout.buffer.flush()
    r = read()
    reply(r, {'protocol_version':1,'engine_epoch':'e','capabilities':['session_read','history_read','request_status','shutdown'],
              'limits':{'frame_bytes':'1048576','subscription_events':'256','subscription_bytes':'4194304','text_batch_bytes':'32768','text_batch_ms':'50'}})
    """# + "\nsnapshot = json.loads(" + quoted + ")\n" + behavior + "\nwhile True: read()\n"
    let url = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-workspace-ipc-\(UUID().uuidString).py")
    try Data(source.utf8).write(to: url)
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
    return url
  }
  private func pipeClient(_ url: URL) throws -> ServiceClient {
    var config = ServiceClient.Configuration()
    config.requestTimeout = .seconds(2); config.exitGrace = .milliseconds(100)
    return try ServiceClient(helperURL: url, clientID: "native", configuration: config)
  }
  private func pending(_ client: ServiceClient) async throws {
    let deadline = ContinuousClock.now + .seconds(2)
    while await client.inFlightRequestCount == 0, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    let count = await client.inFlightRequestCount
    XCTAssertGreaterThan(count, 0)
  }

  func testIPCRejectsWrongSnapshotSessionAndEpoch() async throws {
    for assignment in ["snapshot['session_id']='other'", "snapshot['position']['engine_epoch']='old'"] {
      let url = try pipeFixture("r=read()\n" + assignment + "\nreply(r,snapshot)")
      defer { try? FileManager.default.removeItem(at: url) }
      let client = try pipeClient(url)
      _ = try await client.start()
      do {
        _ = try await client.send(requestID: "snapshot", request: .snapshot(session: "s"))
        XCTFail("別session/epochのsnapshotを受け入れました")
      } catch { XCTAssertEqual(error as? ServiceError, .correlation) }
      let exit = await client.close(); XCTAssertNotNil(exit)
    }
  }

  func testIPCRejectsWrongHistorySnapshotRevisionLoopAndDuplicateMessages() async throws {
    for change in ["p['snapshot_id']='old'", "p['session_revision']='9'", "p['next_cursor']='first'",
                   "p['messages'].append(p['messages'][0])", "p['messages'][0]['saved_byte_offset']='0'"] {
      let url = try pipeFixture("""
      r=read(); reply(r,snapshot)
      r=read()
      p={'snapshot_id':'snap','session_revision':'10','messages':[{'message_id':'m','role':'user','text':'hello','saved_byte_offset':'5'}]}
      \(change)
      reply(r,p)
      """)
      defer { try? FileManager.default.removeItem(at: url) }
      let client = try pipeClient(url)
      _ = try await client.start()
      let reply = try await client.send(requestID: "snapshot", request: .snapshot(session: "s"))
      let context = try ServiceReadContext(snapshot: XCTUnwrap(reply.payload), epoch: "e", sessionID: "s")
      do {
        _ = try await client.send(requestID: "history", request: .history(context: context, cursor: "first", limit: 4))
        XCTFail("履歴の照合不一致を受け入れました")
      } catch { XCTAssertEqual(error as? ServiceError, .correlation) }
      let exit = await client.close(); XCTAssertNotNil(exit)
    }
  }

  func testIPCRejectsInventedCursorAndCrossSessionContextBeforeSending() async throws {
    let url = try pipeFixture("r=read(); reply(r,snapshot)")
    defer { try? FileManager.default.removeItem(at: url) }
    let client = try pipeClient(url)
    _ = try await client.start()
    let reply = try await client.send(requestID: "snapshot", request: .snapshot(session: "s"))
    let context = try ServiceReadContext(snapshot: XCTUnwrap(reply.payload), epoch: "e", sessionID: "s")
    let other = try ServiceReadContext(snapshot: snapshot(session: "other"), epoch: "e", sessionID: "other")
    let old = try ServiceReadContext(snapshot: snapshot(epoch: "old"), epoch: "old", sessionID: "s")
    for request in [ServiceReadRequest.history(context: context, cursor: "guessed", limit: 4),
                    .status(context: other, clientID: "native", requestID: "q"),
                    .status(context: old, clientID: "native", requestID: "q")] {
      do { _ = try await client.send(requestID: UUID().uuidString, request: request); XCTFail("古い/別sessionの座標を送信しました") }
      catch { XCTAssertEqual(error as? ServiceError, .correlation) }
    }
    let count = await client.inFlightRequestCount; XCTAssertEqual(count, 0)
    let failure = await client.terminalError(); XCTAssertNil(failure)
    _ = await client.close()
  }

  func testIPCSnapshotReplacementInvalidatesPendingHistoryAndStatus() async throws {
    for method in ["history.page", "request.status"] {
      let url = try pipeFixture("""
      r=read(); reply(r,snapshot)
      old=read()
      new=read()
      snapshot['snapshot_id']='new'
      reply(new,snapshot)
      if old['method']=='history.page':
          reply(old,{'snapshot_id':'snap','session_revision':'10','messages':[]})
      else:
          reply(old,{'status':'not_found'})
      """)
      defer { try? FileManager.default.removeItem(at: url) }
      let client = try pipeClient(url)
      _ = try await client.start()
      let reply = try await client.send(requestID: "snapshot", request: .snapshot(session: "s"))
      let context = try ServiceReadContext(snapshot: XCTUnwrap(reply.payload), epoch: "e", sessionID: "s")
      let request: ServiceReadRequest = method == "history.page"
        ? .history(context: context, cursor: "first", limit: 4)
        : .status(context: context, clientID: "native", requestID: "lost")
      let old = Task { try await client.send(requestID: "old", request: request) }
      try await pending(client)
      _ = try await client.send(requestID: "refresh", request: .snapshot(session: "s"))
      do { _ = try await old.value; XCTFail("置換前の応答を採用しました") }
      catch { XCTAssertEqual(error as? ServiceError, .correlation) }
      let exit = await client.close(); XCTAssertNotNil(exit)
    }
  }

  func testRealExistingPersistentHelperReopensDraftAndTypedLedger() async throws {
    let helper = repository.appendingPathComponent("target/debug/polaris-desktop-service")
    XCTAssertTrue(FileManager.default.isExecutableFile(atPath: helper.path), "既存service実行物が必要です。cargoは実行しません。")
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-workspace-store-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ServiceClient.PersistentStore(root: root, projectID: "native-project", sessionID: "native-session")
    let first = try WorkspaceServiceModel(helperURL: helper, store: store, clientID: "persist-owner")
    await first.start()
    XCTAssertEqual(first.phase, .ready)
    first.editDraft("別processへ残す日本語")
    await first.saveDraft()
    XCTAssertFalse(first.isDirty); XCTAssertNil(first.failure)
    XCTAssertFalse(first.canSend, "storage-only helper must not enable native inference")
    let request = try XCTUnwrap(first.lastDraftRequestID)
    let closed = await first.close(); XCTAssertTrue(closed)
    let second = try WorkspaceServiceModel(helperURL: helper, store: store, clientID: "persist-owner")
    await second.start()
    XCTAssertEqual(second.phase, .ready)
    XCTAssertEqual(second.draftText, "別processへ残す日本語")
    XCTAssertEqual(second.savedDraftRevision, 1)
    XCTAssertTrue(second.messages.isEmpty)
    await second.lookupRequest(clientID: "persist-owner", requestID: request)
    guard case .completed = second.requestStatus else {
      _ = await second.close(); return XCTFail("保存済み台帳の完了結果を取得できませんでした")
    }
    await second.lookupRequest(clientID: "persist-owner", requestID: "absent")
    XCTAssertEqual(second.requestStatus, .notFound)
    let closedAgain = await second.close(); XCTAssertTrue(closedAgain)
    XCTAssertNotEqual(first.snapshot?["position"]?["engine_epoch"], second.snapshot?["position"]?["engine_epoch"])
  }
}

extension WorkspaceServiceTests {
  func testEditingDuringInitialLoadKeepsTextAndRequiresStoredDraftChoice() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await fixture.hold("history.page")
    let start = Task { await model.start() }
    try await wait(fixture)
    model.editDraft("復元を待つ間の入力")
    await fixture.release(); await start.value
    XCTAssertEqual(model.draftText, "復元を待つ間の入力")
    XCTAssertTrue(model.draftConflict)
    XCTAssertFalse(model.canSaveDraft)
    _ = await model.close()
  }

  func testAttachmentIDsRemainObservedAndCannotBeSilentlyDroppedByTextSave() async throws {
    guard case .object(var value) = try snapshot(), case .object(var draft) = value["draft"] else {
      return XCTFail("合成snapshotが不正です")
    }
    draft["attachment_ids"] = .array([.string("saved-attachment")]); value["draft"] = .object(draft)
    let fixture = WorkspaceTransportFixture(snapshot: .object(value), pages: [page()])
    let model = try model(fixture)
    await model.start(); model.editDraft("添付を保持する本文")
    await model.saveDraft()
    XCTAssertEqual(model.attachmentIDs, ["saved-attachment"])
    XCTAssertFalse(model.canSaveDraft)
    let requests = await fixture.requests
    XCTAssertFalse(requests.contains("draft.update"))
    _ = await model.close()
  }

  func testIPCOlderSnapshotResponseCannotReplaceNewerRequest() async throws {
    let url = try pipeFixture("""
    old=read(); new=read()
    snapshot['snapshot_id']='new'; reply(new,snapshot)
    snapshot['snapshot_id']='old'; reply(old,snapshot)
    """)
    defer { try? FileManager.default.removeItem(at: url) }
    let client = try pipeClient(url)
    _ = try await client.start()
    let old = Task { try await client.send(requestID: "old", request: .snapshot(session: "s")) }
    try await pending(client)
    let newest = try await client.send(requestID: "new", request: .snapshot(session: "s"))
    XCTAssertEqual(newest.payload?["snapshot_id"], .string("new"))
    do { _ = try await old.value; XCTFail("古いsnapshot応答を採用しました") }
    catch { XCTAssertEqual(error as? ServiceError, .correlation) }
    _ = await client.close()
  }
}

extension WorkspaceServiceTests {
  func testAmbiguousPublicationRejectionRetainsUnknownAndCannotResave() async throws {
    for code in ["storage_failed", "recovery_required"] {
      let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
      let model = try model(fixture)
      await model.start(); model.editDraft("公開成否が不明な本文")
      await fixture.reject(code); await model.saveDraft()
      let originalID = model.lastDraftRequestID
      XCTAssertEqual(model.failure, .rejected(code))
      XCTAssertEqual(model.phase, .unavailable)
      XCTAssertTrue(model.draftOutcomeUnknown); XCTAssertTrue(model.isDirty)
      XCTAssertFalse(model.canSaveDraft)
      await model.saveDraft()
      XCTAssertEqual(model.lastDraftRequestID, originalID)
      XCTAssertEqual(model.draftText, "公開成否が不明な本文")
      let requests = await fixture.requests
      XCTAssertEqual(requests.filter { $0 == "draft.update" }.count, 1)
      _ = await model.close()
    }
  }
}


extension WorkspaceServiceTests {
  func testQuitSaveRejectionKeepsConnectedEditingAndLateEditCancelsQuit() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let model = try model(fixture)
    await model.start(); model.editDraft("保存前")
    await fixture.reject("revision_conflict")
    let rejected = await model.saveAndClose()
    XCTAssertFalse(rejected); XCTAssertEqual(model.phase, .ready)
    var requests = await fixture.requests
    XCTAssertFalse(requests.contains("shutdown.request"))
    await model.refresh(); model.useObservedDraft(); model.editDraft("保存する本文")
    await fixture.hold("draft.update")
    let quit = Task { await model.saveAndClose() }
    try await wait(fixture); model.editDraft("保存待ち中の追加入力")
    await fixture.release()
    let late = await quit.value
    XCTAssertFalse(late); XCTAssertEqual(model.phase, .ready); XCTAssertTrue(model.isDirty)
    requests = await fixture.requests
    XCTAssertFalse(requests.contains("shutdown.request"))
    _ = await model.close()
  }

  func testLazyDuplicatePageDoesNotReplaceValidatedHistory() async throws {
    let fixture = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page(next: "second"), page()])
    let model = try model(fixture)
    await model.start()
    XCTAssertEqual(model.phase, .ready)
    await model.loadNextHistoryPage()
    XCTAssertEqual(model.phase, .unavailable)
    XCTAssertEqual(model.messages.map(\.id), ["m1"])
    _ = await model.close()
  }
}


private final class RecoveryTransportSequence: @unchecked Sendable {
  private let lock = NSLock()
  private var values: [WorkspaceTransportFixture]
  private var count = 0
  init(_ values: [WorkspaceTransportFixture]) { self.values = values }
  func next() throws -> any WorkspaceServiceTransport {
    try lock.withLock {
      guard !values.isEmpty else { throw ServiceError.launch }
      count += 1
      return values.removeFirst()
    }
  }
  var launches: Int { lock.withLock { count } }
}

extension WorkspaceServiceTests {
  private func recoveryModel(_ sequence: RecoveryTransportSequence) throws -> WorkspaceServiceModel {
    let store = try ServiceClient.PersistentStore(root: FileManager.default.temporaryDirectory, projectID: "p", sessionID: "s")
    return try WorkspaceServiceModel(helperURL: URL(fileURLWithPath: "/usr/bin/false"), store: store,
                                     clientID: "native", makeTransport: { try sequence.next() })
  }

  func testReconnectReapsBeforeNewTransportAndKeepsUnsavedDraft() async throws {
    let first = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let second = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let sequence = RecoveryTransportSequence([first, second])
    let model = try recoveryModel(sequence)
    await model.start(); model.editDraft("未送信のローカル本文")
    await first.failNext(.io); await model.refresh()
    XCTAssertEqual(model.phase, .unavailable)
    await first.hold("transport.close")
    let recovery = Task { await model.reconnect() }
    try await wait(first)
    XCTAssertEqual(sequence.launches, 1); XCTAssertEqual(model.draftText, "未送信のローカル本文")
    await first.release(); await recovery.value
    XCTAssertEqual(sequence.launches, 2); XCTAssertEqual(model.phase, .ready)
    XCTAssertTrue(model.isDirty); XCTAssertTrue(model.canSaveDraft)
    let requests = await second.requests
    XCTAssertFalse(requests.contains("draft.update"))
    await model.saveDraft()
    XCTAssertFalse(model.isDirty)
    _ = await model.close()
  }

  func testReconnectCannotReplaceChildWithoutConfirmedExit() async throws {
    let first = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let second = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let sequence = RecoveryTransportSequence([first, second])
    let model = try recoveryModel(sequence)
    await model.start(); model.editDraft("回収待ちの本文")
    await first.failNext(.io); await model.refresh(); await first.setExit(nil)
    await model.reconnect()
    XCTAssertEqual(sequence.launches, 1); XCTAssertEqual(model.phase, .unavailable)
    XCTAssertEqual(model.draftText, "回収待ちの本文")
    await first.setExit(.init(status: 0, forced: false))
    await model.reconnect()
    XCTAssertEqual(sequence.launches, 2); XCTAssertEqual(model.phase, .ready)
    _ = await model.close()
  }

  func testReconnectCompletedOriginalRequestKeepsLateEditWithoutResending() async throws {
    let first = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let second = try WorkspaceTransportFixture(snapshot: snapshot(revision: "11", draftRevision: "5", text: "送った本文"),
                                                pages: [page(revision: "11")])
    await second.setStatus(.object(["status": .string("completed"), "session_revision": .string("11"), "result_id": .string("saved")]))
    let model = try recoveryModel(RecoveryTransportSequence([first, second]))
    await model.start(); model.editDraft("送った本文")
    await first.hold("draft.update"); await first.reject("storage_failed")
    let saving = Task { await model.saveDraft() }
    try await wait(first); model.editDraft("送信後の追加入力")
    await first.release(); await saving.value
    let requestID = model.lastDraftRequestID
    await model.reconnect()
    XCTAssertEqual(model.phase, .ready); XCTAssertFalse(model.draftOutcomeUnknown)
    XCTAssertEqual(model.lastDraftRequestID, requestID)
    XCTAssertEqual(model.draftText, "送信後の追加入力"); XCTAssertTrue(model.isDirty)
    XCTAssertEqual(model.savedDraftRevision, 5); XCTAssertTrue(model.canSaveDraft)
    let requests = await second.requests
    XCTAssertEqual(requests, ["session.subscribe", "history.page", "request.status"])
    _ = await model.close()
  }

  func testReconnectDoesNotTreatMissingAcceptedOrUnknownLedgerAsSafeRetry() async throws {
    let statuses: [ServiceValue] = [
      .object(["status": .string("not_found")]),
      .object(["status": .string("accepted"), "session_revision": .string("10")]),
      .object(["status": .string("outcome_unknown"), "session_revision": .string("10"), "run_id": .string("r"), "attempt_id": .string("a")])
    ]
    for status in statuses {
      let first = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
      let second = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
      await second.setStatus(status)
      let model = try recoveryModel(RecoveryTransportSequence([first, second]))
      await model.start(); model.editDraft("成否不明の本文")
      await first.reject("recovery_required"); await model.saveDraft()
      let requestID = model.lastDraftRequestID
      await model.reconnect(); model.useObservedDraft(); await model.saveDraft()
      XCTAssertEqual(model.phase, .ready); XCTAssertTrue(model.draftOutcomeUnknown)
      XCTAssertTrue(model.isDirty); XCTAssertFalse(model.canSaveDraft)
      XCTAssertEqual(model.lastDraftRequestID, requestID); XCTAssertEqual(model.draftText, "成否不明の本文")
      let requests = await second.requests
      XCTAssertEqual(requests, ["session.subscribe", "history.page", "request.status"])
      _ = await model.close()
    }
  }
}


extension WorkspaceServiceTests {
  func testExplicitReconciliationRefreshesStaleSnapshotBeforeClearingUnknown() async throws {
    let first = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let second = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    await second.setStatus(.object(["status": .string("completed"), "session_revision": .string("11"), "result_id": .string("saved")]))
    let model = try recoveryModel(RecoveryTransportSequence([first, second]))
    await model.start(); model.editDraft("照合対象の本文")
    await first.failNext(.io); await model.saveDraft()
    let originalID = model.lastDraftRequestID
    await model.reconnect()
    XCTAssertTrue(model.draftOutcomeUnknown); XCTAssertTrue(model.isDirty)
    await second.setSnapshot(try snapshot(revision: "11", draftRevision: "5", text: "照合対象の本文"))
    await second.setPages([page(revision: "11")])
    await model.reconcileDraft()
    XCTAssertFalse(model.draftOutcomeUnknown); XCTAssertFalse(model.isDirty)
    XCTAssertEqual(model.lastDraftRequestID, originalID)
    let requests = await second.requests
    XCTAssertFalse(requests.contains("draft.update"))
    _ = await model.close()
  }

  func testCompletedPublicationWithNewerRemoteDraftRequiresExplicitConflictChoice() async throws {
    let first = try WorkspaceTransportFixture(snapshot: snapshot(), pages: [page()])
    let second = try WorkspaceTransportFixture(snapshot: snapshot(revision: "12", draftRevision: "6", text: "別の保存済み本文"),
                                                pages: [page(revision: "12")])
    await second.setStatus(.object(["status": .string("completed"), "session_revision": .string("11"), "result_id": .string("saved")]))
    let model = try recoveryModel(RecoveryTransportSequence([first, second]))
    await model.start(); model.editDraft("自分の保存要求本文")
    await first.failNext(.io); await model.saveDraft()
    let originalID = model.lastDraftRequestID
    await model.reconnect()
    XCTAssertFalse(model.draftOutcomeUnknown); XCTAssertTrue(model.draftConflict)
    XCTAssertTrue(model.isDirty); XCTAssertFalse(model.canSaveDraft)
    XCTAssertEqual(model.draftText, "自分の保存要求本文"); XCTAssertEqual(model.lastDraftRequestID, originalID)
    model.useObservedDraft()
    XCTAssertEqual(model.draftText, "別の保存済み本文"); XCTAssertFalse(model.isDirty)
    let requests = await second.requests
    XCTAssertFalse(requests.contains("draft.update"))
    _ = await model.close()
  }
}
