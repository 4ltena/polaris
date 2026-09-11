import Foundation
import XCTest

@testable import PolarisDesktop

@MainActor
final class ServiceTests: XCTestCase {
  private var repository: URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
      .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
  }
  private var helper: URL { repository.appendingPathComponent("target/debug/polaris-fake-service") }
  private let common = #"""
    #!/usr/bin/python3
    import sys, struct, json, time, os, signal
    assert len(sys.argv) == 1
    assert 'OPENAI_API_KEY' not in os.environ
    def read():
        h = sys.stdin.buffer.read(4)
        if len(h) != 4: sys.exit(0)
        return json.loads(sys.stdin.buffer.read(struct.unpack('>I', h)[0]))
    def send(value):
        b = json.dumps(value, ensure_ascii=False).encode()
        sys.stdout.buffer.write(struct.pack('>I',len(b))+b); sys.stdout.buffer.flush()
    def reply(r, payload, kind=None):
        send({'protocol_version':1,'kind':'response','client_id':r['client_id'],'request_id':r['request_id'],
              'result':{'type':kind or r['method'],'payload':payload}})
    def hello():
        r=read()
        assert r['method']=='hello'
        reply(r, {'protocol_version':1,'engine_epoch':'e','capabilities':['session_read','draft_update','run_start','run_cancel','shutdown'],
                  'limits':{'frame_bytes':'1048576','subscription_events':'256','subscription_bytes':'4194304','text_batch_bytes':'32768','text_batch_ms':'50'}})
    """#

  private func fixture(_ source: String, persistentArgs: [String]? = nil) throws -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
      "polaris-service-\(UUID().uuidString).py")
    var prelude = common
    if let persistentArgs {
      let encoded = String(decoding: try JSONSerialization.data(withJSONObject: persistentArgs, options: [.withoutEscapingSlashes]), as: UTF8.self)
      prelude = prelude.replacingOccurrences(of: "assert len(sys.argv) == 1", with: "assert sys.argv[1:] == " + encoded)
    }
    try Data((prelude + "\n" + source + "\n").utf8).write(to: url)
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
    return url
  }
  private func fast() -> ServiceClient.Configuration {
    var config = ServiceClient.Configuration()
    config.requestTimeout = .milliseconds(500)
    config.frameTimeout = .milliseconds(300)
    config.exitGrace = .milliseconds(100)
    return config
  }
  private func assertReaped(_ client: ServiceClient) async {
    let result = await client.close()
    XCTAssertNotNil(result)
    let observed = await client.observedExit()
    XCTAssertEqual(observed, result)
  }

  private func awaitShutdownReady(_ client: ServiceClient, epoch: String) async throws {
    let deadline = ContinuousClock.now + .seconds(3)
    while ContinuousClock.now < deadline {
      let reply = try await client.send(requestID: "shutdown-ready", request: .shutdown(epoch: epoch))
      if reply.payload?["state"]?.string == "ready" { return }
      XCTAssertEqual(reply.payload?["state"]?.string, "draining")
      try await Task.sleep(for: .milliseconds(20))
    }
    XCTFail("終了準備完了を期限内に確認できませんでした")
    throw ServiceError.timeout
  }

  func testPersistentLaunchUsesOnlyTypedStoreCoordinates() async throws {
    let root = URL(fileURLWithPath: "/tmp/polaris 保存 \(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ServiceClient.PersistentStore(root: root, projectID: "project-1", sessionID: "session-1")
    let script = try fixture("hello()\nwhile True: read()", persistentArgs: [
      "--store-root", store.root.path, "--project-id", "project-1", "--session-id", "session-1",
    ])
    defer { try? FileManager.default.removeItem(at: script) }
    let client = try ServiceClient(helperURL: script, configuration: fast(), persistentStore: store)
    _ = try await client.start()
    await assertReaped(client)
    XCTAssertThrowsError(try ServiceClient.PersistentStore(root: URL(fileURLWithPath: "/"), projectID: "p", sessionID: "s"))
    XCTAssertThrowsError(try ServiceClient.PersistentStore(root: URL(string: "https://example.test/store")!, projectID: "p", sessionID: "s"))
    XCTAssertThrowsError(try ServiceClient.PersistentStore(root: root, projectID: "", sessionID: "s"))
    XCTAssertThrowsError(try ServiceClient.PersistentStore(root: root.appendingPathComponent("missing"), projectID: "p", sessionID: "s"))
  }

  func testRealPersistentServiceReopensSameStoreAndRetainsDraftLedger() async throws {
    let executable = repository.appendingPathComponent("target/debug/polaris-desktop-service")
    XCTAssertTrue(FileManager.default.isExecutableFile(atPath: executable.path),
      "先にcargo build -p polaris-desktop-service --bin polaris-desktop-serviceを実行")
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-native-store-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    defer { try? FileManager.default.removeItem(at: root) }
    let store = try ServiceClient.PersistentStore(root: root.resolvingSymlinksInPath(), projectID: "native-project", sessionID: "native-session")
    let canonical = try XCTUnwrap(realpath(store.root.path, nil))
    defer { free(canonical) }
    XCTAssertEqual(store.root.path, String(cString: canonical))
    let attributes = try FileManager.default.attributesOfItem(atPath: store.root.path)
    XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o700)
    let first = try ServiceClient(helperURL: executable, clientID: "native-persist", persistentStore: store)
    let firstHello: ServiceHello
    var stage = "first start"
    do {
      firstHello = try await first.start()
      XCTAssertFalse(firstHello.capabilities.contains("run_start"))
      stage = "draft save"
      let saved = try await first.send(requestID: "save-draft", request: .draft(session: "native-session", revision: 0, text: "再開後も保持する下書き"))
      XCTAssertEqual(saved.payload?["draft_revision"]?.decimal, 1)
      let competing = try ServiceClient(helperURL: executable, persistentStore: store)
      do { _ = try await competing.start(); XCTFail("同じstoreのwriterを重複起動しました") } catch {}
      _ = await competing.close()
      stage = "first shutdown"
      try await awaitShutdownReady(first, epoch: firstHello.epoch)
    } catch { _ = await first.close(); XCTFail("\(stage): \(error)"); throw error }
    let firstExit = await first.close()
    XCTAssertEqual(firstExit?.status, 0)
    XCTAssertEqual(firstExit?.forced, false)

    let second = try ServiceClient(helperURL: executable, clientID: "native-persist", persistentStore: store)
    do {
      let hello = try await second.start()
      XCTAssertNotEqual(firstHello.epoch, hello.epoch)
      let snapshot = try await second.send(requestID: "read", request: .snapshot(session: "native-session"))
      XCTAssertEqual(snapshot.payload?["draft"]?["text"]?.string, "再開後も保持する下書き")
      XCTAssertEqual(snapshot.payload?["draft"]?["draft_revision"]?.decimal, 1)
      let replay = try await second.send(requestID: "save-draft", request: .draft(session: "native-session", revision: 0, text: "再開後も保持する下書き"))
      XCTAssertEqual(replay.payload?["draft_revision"]?.decimal, 1)
      try await awaitShutdownReady(second, epoch: hello.epoch)
    } catch { _ = await second.close(); throw error }
    let secondExit = await second.close()
    XCTAssertEqual(secondExit?.status, 0)
    XCTAssertEqual(secondExit?.forced, false)
  }

  func testStrictCodecAndBigEndianFragmentation() throws {
    let value = ServiceValue.object([
      "text": .string("日本語🙂"), "revision": .string("18446744073709551615"),
    ])
    let frame = try ServiceCodec.encode(value)
    XCTAssertEqual(frame.prefix(4).reduce(0) { ($0 << 8) | Int($1) }, frame.count - 4)
    var codec = ServiceCodec()
    var decoded: [ServiceValue] = []
    for byte in frame { decoded += try codec.receive(Data([byte])) }
    XCTAssertEqual(decoded, [value])
    try codec.finish()
    XCTAssertEqual(value["revision"]?.decimal, UInt64.max)
    XCTAssertFalse(ServiceValue.string("é").matchesID("e\u{301}"))
    XCTAssertNil(ServiceValue.string("01").decimal)
    XCTAssertNil(ServiceValue.string("18446744073709551616").decimal)
    for bad in [
      #"{"a":1,"\u0061":2}"#, #"{"x":{"a":1,"a":2}}"#, #"{"x":[null]}"#, #"{"x":1,}"#, #"{"x":01}"#,
    ] {
      XCTAssertThrowsError(try ServiceCodec.json(Data(bad.utf8)), bad)
    }
    XCTAssertThrowsError(try ServiceCodec.json(Data([0xff])))
    var oversized = ServiceCodec()
    XCTAssertThrowsError(try oversized.receive(Data([0, 16, 0, 1])))
    XCTAssertThrowsError(
      try ServiceCodec.encode(
        .string(String(repeating: "x", count: ServiceCodec.maxFrameBytes + 1))))
    var partial = ServiceCodec()
    _ = try partial.receive(Data([0, 0, 0]))
    XCTAssertThrowsError(try partial.finish()) {
      XCTAssertEqual($0 as? ServiceError, .truncatedFrame)
    }
    var joined = ServiceCodec()
    XCTAssertEqual(try joined.receive(frame + frame), [value, value])
  }

  func testRustEventAndSnapshotFixturesUseSameSchemas() throws {
    let base = repository.appendingPathComponent("crates/polaris-desktop-protocol/tests/fixtures")
    let events = try ServiceCodec.json(Data(contentsOf: base.appendingPathComponent("events.json")))
    guard case .array(let frames) = events else { return XCTFail("fixture") }
    for frame in frames { try ServiceSchema.validateEvent(frame, epoch: "e") }
    let snapshot = try ServiceCodec.json(
      Data(contentsOf: base.appendingPathComponent("snapshot.json")))
    try ServiceSchema.snapshot.check(snapshot)
    XCTAssertThrowsError(try ServiceSchema.validateEvent(frames[0], epoch: "other"))
  }

  func testRealFakeHelperPipeRoundtripCancelAndShutdown() async throws {
    XCTAssertTrue(
      FileManager.default.isExecutableFile(atPath: helper.path),
      "先にcargo build -p polaris-desktop-service --bin polaris-fake-serviceを実行")
    let client = try ServiceClient(helperURL: helper)
    let hello = try await client.start()
    XCTAssertTrue(hello.capabilities.contains("run_cancel"))
    let snapshot = try await client.send(
      requestID: "subscribe", request: .subscribe(session: "fake-session"))
    XCTAssertEqual(snapshot.payload?["session_id"], .string("fake-session"))
    let draft = try await client.send(
      requestID: "draft", request: .draft(session: "fake-session", revision: 0, text: "nativeから送信"))
    XCTAssertEqual(draft.payload?["draft_revision"]?.decimal, 1)
    let run = try await client.send(
      requestID: "run",
      request: .run(session: "fake-session", draft: 1, configuration: 0, policy: 0))
    let runID = try XCTUnwrap(run.payload?["run_id"]?.string)
    let attempt = try XCTUnwrap(run.payload?["attempt_id"]?.string)
    let cancel = try await client.send(
      requestID: "cancel", request: .cancel(session: "fake-session", run: runID, attempt: attempt))
    XCTAssertEqual(cancel.payload?["status"], .string("cancel_requested"))
    // ACK is not terminal: collect an independently observed terminal event.
    let deadline = ContinuousClock.now + .seconds(3)
    var terminal = false
    while ContinuousClock.now < deadline && !terminal {
      let events = try await client.takeEvents()
      terminal = events.contains {
        $0["type"] == .string("run.state")
          && ["cancelled", "succeeded"].contains($0["payload"]?["state"]?.string ?? "")
      }
      if !terminal { try await Task.sleep(for: .milliseconds(10)) }
    }
    XCTAssertTrue(terminal)
    var ready = false
    for _ in 0..<30 {
      let shutdown = try await client.send(
        requestID: "shutdown", request: .shutdown(epoch: hello.epoch))
      if shutdown.payload?["state"] == .string("ready") {
        ready = true
        break
      }
      try await Task.sleep(for: .milliseconds(20))
    }
    XCTAssertTrue(ready)
    let exited = await client.close()
    XCTAssertEqual(exited?.status, 0)
    XCTAssertEqual(exited?.forced, false)
  }

  func testEOFClosesAndReapsRealHelperWithoutShutdownRPC() async throws {
    let client = try ServiceClient(helperURL: helper)
    _ = try await client.start()
    let exit = await client.close()
    XCTAssertEqual(exit, ServiceClient.Exit(status: 0, forced: false))
  }

  func testCancellationImmediatelyAfterLaunchReapsWithoutClose() async throws {
    let url = try fixture("while sys.stdin.buffer.read(1): pass")
    defer { try? FileManager.default.removeItem(at: url) }
    let client = try ServiceClient(helperURL: url, configuration: fast())
    let starting = Task {
      try await client.start(afterLaunch: {
        withUnsafeCurrentTask { $0?.cancel() }
      })
    }
    do {
      _ = try await starting.value
      XCTFail("起動取消を受理")
    } catch { XCTAssertTrue(error is CancellationError) }
    let deadline = ContinuousClock.now + .seconds(2)
    while await client.observedExit() == nil && ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(10))
    }
    let error = await client.terminalError()
    let exit = await client.observedExit()
    XCTAssertEqual(error, .cancelled)
    XCTAssertNotNil(exit, "closeを呼ばなくても起動済みhelperを回収する")
    await assertReaped(client)
  }

  func testExitedHelperIsObservedWhileDescendantHoldsStdout() async throws {
    let url = try fixture(
      """
      hello()
      r=read()
      if os.fork()==0:
          time.sleep(3)
          os._exit(0)
      os._exit(7)
      """)
    defer { try? FileManager.default.removeItem(at: url) }
    var config = fast()
    config.requestTimeout = .seconds(5)
    let client = try ServiceClient(helperURL: url, configuration: config)
    _ = try await client.start()
    let request = Task {
      try await client.send(requestID: "exit", request: .snapshot(session: "s"))
    }
    let deadline = ContinuousClock.now + .seconds(2)
    while await client.observedExit() == nil && ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(10))
    }
    let exit = await client.observedExit()
    XCTAssertEqual(exit, ServiceClient.Exit(status: 7, forced: false))
    let error = await client.terminalError()
    XCTAssertEqual(error, .eof)
    // On regression, close also releases the pending request so the test terminates.
    await assertReaped(client)
    do {
      _ = try await request.value
      XCTFail("終了したhelperの要求が成功")
    } catch { XCTAssertEqual(error as? ServiceError, .eof) }
    // The descendant is a finite fixture, not part of the helper Exit claim.
    try await Task.sleep(for: .seconds(3))
  }

  func testRepeatedSubscribeReplacesOldSessionIdentity() async throws {
    let client = try ServiceClient(helperURL: helper, configuration: fast())
    _ = try await client.start()
    do {
      var ids = Set<String>()
      for index in 0..<12 {
        let reply = try await client.send(
          requestID: "subscribe-\(index)", request: .subscribe(session: "fake-session"))
        let id = try XCTUnwrap(reply.payload?["position"]?["subscription_id"]?.string)
        XCTAssertTrue(ids.insert(id).inserted)
        _ = try await client.takeEvents()
      }
      _ = try await client.send(
        requestID: "after-subscribe",
        request: .draft(session: "fake-session", revision: 0, text: "更新"))
      let error = await client.terminalError()
      XCTAssertNil(error)
    } catch {
      XCTFail("再購読で接続が壊れた: \(error)")
    }
    await assertReaped(client)
  }

  func testResubscribeSupersedesBufferedEventsButPreservesOtherSessions() async throws {
    let base = repository.appendingPathComponent(
      "crates/polaris-desktop-protocol/tests/fixtures/snapshot.json")
    let snapshot = String(decoding: try Data(contentsOf: base), as: UTF8.self)
    let quoted = String(decoding: try JSONEncoder().encode(snapshot), as: UTF8.self)
    let url = try fixture(
      """
      template=json.loads(\(quoted))
      def snapshot(r, sub):
          s=dict(template)
          s['session_id']=r['session_id']
          s['position']={'engine_epoch':'e','subscription_id':sub,'event_seq':'0'}
          reply(r,s)
      def event(session, sub, seq):
          send({'protocol_version':1,'kind':'event','engine_epoch':'e','subscription_id':sub,
                'event_seq':str(seq),'session_id':session,'session_revision':'1','type':'message.delta',
                'payload':{'message_id':'m','byte_offset':'0','text':'fixture','durability':'tentative'}})
      hello()
      a=read(); snapshot(a,'old'); event('a','old',1)
      b=read(); snapshot(b,'other'); event('b','other',1)
      c=read(); event('a','old',2); snapshot(c,'new'); event('a','new',1)
      d=read(); reply(d,{'session_revision':'1','draft_revision':'1'}); event('a','old',3)
      while sys.stdin.buffer.read(1): pass
      """)
    defer { try? FileManager.default.removeItem(at: url) }
    let client = try ServiceClient(helperURL: url, configuration: fast())
    _ = try await client.start()
    for (id, session) in [("first", "a"), ("second", "b"), ("replace", "a")] {
      _ = try await client.send(requestID: id, request: .subscribe(session: session))
    }
    var events: [ServiceValue] = []
    let deadline = ContinuousClock.now + .seconds(2)
    while events.count < 2 && ContinuousClock.now < deadline {
      events += try await client.takeEvents()
      if events.count < 2 { try await Task.sleep(for: .milliseconds(10)) }
    }
    XCTAssertEqual(events.compactMap { $0["subscription_id"]?.string }, ["other", "new"])
    XCTAssertEqual(events.compactMap { $0["event_seq"]?.decimal }, [1, 1])
    // A late event from the retired identity must not be accepted as current.
    _ = try? await client.send(
      requestID: "late", request: .draft(session: "a", revision: 0, text: "fixture"))
    let failureDeadline = ContinuousClock.now + .seconds(2)
    while await client.terminalError() == nil && ContinuousClock.now < failureDeadline {
      try await Task.sleep(for: .milliseconds(10))
    }
    let error = await client.terminalError()
    XCTAssertEqual(error, .correlation)
    await assertReaped(client)
  }

  func testHelloVersionSchemaAndCorrelationFailuresReap() async throws {
    for source in [
      "r=read(); reply(r, {'protocol_version':2})",
      "r=read(); r['request_id']='wrong'; reply(r, {})",
      "r=read(); reply(r, {}, 'session.snapshot')",
      "sys.stdout.buffer.write(bytes([0,16,0,1])); sys.stdout.buffer.flush()",
      "sys.stdout.buffer.write(bytes([0,0,0,8])+b'{'); sys.stdout.buffer.flush()",
      "sys.exit(0)",
    ] {
      let url = try fixture(source)
      defer { try? FileManager.default.removeItem(at: url) }
      let client = try ServiceClient(helperURL: url, configuration: fast())
      do {
        _ = try await client.start()
        XCTFail("不正Helloを受理")
      } catch {}
      await assertReaped(client)
    }
  }

  func testOutOfOrderIDsAndDuplicatePendingAreNotConfused() async throws {
    let url = try fixture(
      "hello()\na=read(); b=read()\nreply(b, {'session_revision':'2','draft_revision':'2'})\nreply(a, {'session_revision':'1','draft_revision':'1'})\nwhile sys.stdin.buffer.read(1): pass"
    )
    defer { try? FileManager.default.removeItem(at: url) }
    let client = try ServiceClient(helperURL: url)
    _ = try await client.start()
    let first = Task {
      try await client.send(
        requestID: "first", request: .draft(session: "s", revision: 0, text: "A"))
    }
    try await Task.sleep(for: .milliseconds(30))
    do {
      _ = try await client.send(
        requestID: "first", request: .draft(session: "s", revision: 0, text: "wrong"))
      XCTFail("同時ID重複")
    } catch { XCTAssertEqual(error as? ServiceError, .correlation) }
    let second = try await client.send(
      requestID: "second", request: .draft(session: "s", revision: 1, text: "B"))
    let original = try await first.value
    XCTAssertEqual(original.requestID, "first")
    XCTAssertEqual(original.payload?["draft_revision"]?.decimal, 1)
    XCTAssertEqual(second.requestID, "second")
    XCTAssertEqual(second.payload?["draft_revision"]?.decimal, 2)
    await assertReaped(client)
  }

  func testUnreadInputTimeoutAndCancelledRequestStillReapChild() async throws {
    for cancel in [false, true] {
      let url = try fixture(
        "hello()\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\ntime.sleep(20)")
      defer { try? FileManager.default.removeItem(at: url) }
      let client = try ServiceClient(helperURL: url, configuration: fast())
      _ = try await client.start()
      let task = Task {
        try await client.send(
          requestID: "blocked",
          request: .draft(session: "s", revision: 0, text: String(repeating: "x", count: 900_000)))
      }
      if cancel {
        let deadline = ContinuousClock.now + .seconds(2)
        while await client.inFlightRequestCount == 0 && ContinuousClock.now < deadline {
          try await Task.sleep(for: .milliseconds(5))
        }
        let count = await client.inFlightRequestCount
        XCTAssertEqual(count, 1)
        task.cancel()
      }
      do {
        _ = try await task.value
        XCTFail("未読pipeが成功")
      } catch { XCTAssertEqual(error as? ServiceError, cancel ? .cancelled : .timeout) }
      let cleanupDeadline = ContinuousClock.now + .seconds(3)
      while await client.observedExit() == nil && ContinuousClock.now < cleanupDeadline {
        try await Task.sleep(for: .milliseconds(10))
      }
      let reapedWithoutClose = await client.observedExit()
      XCTAssertNotNil(reapedWithoutClose)
      let exit = await client.close()
      XCTAssertEqual(exit?.forced, true)
      XCTAssertNotNil(exit)
    }
  }

  func testPartialFrameDeadlineAndUnexpectedEOF() async throws {
    for delayed in [false, true] {
      let url = try fixture(
        "hello()\nr=read()\nsys.stdout.buffer.write(bytes([0,0,1,0])+b'{'); sys.stdout.buffer.flush()\n"
          + (delayed ? "time.sleep(20)" : "sys.exit(0)"))
      defer { try? FileManager.default.removeItem(at: url) }
      let client = try ServiceClient(helperURL: url, configuration: fast())
      _ = try await client.start()
      do {
        _ = try await client.send(requestID: "partial", request: .snapshot(session: "s"))
        XCTFail("途中frame")
      } catch { XCTAssertEqual(error as? ServiceError, delayed ? .timeout : .truncatedFrame) }
      await assertReaped(client)
    }
  }

  func testUnreadEventsHitBoundAndRequireNewSnapshot() async throws {
    var config = ServiceClient.Configuration()
    config.maxEventCount = 1
    let client = try ServiceClient(helperURL: helper, configuration: config)
    _ = try await client.start()
    _ = try await client.send(requestID: "subscribe", request: .subscribe(session: "fake-session"))
    _ = try await client.send(
      requestID: "draft", request: .draft(session: "fake-session", revision: 0, text: "未読"))
    do {
      _ = try await client.send(
        requestID: "run",
        request: .run(session: "fake-session", draft: 1, configuration: 0, policy: 0))
    } catch {}
    let deadline = ContinuousClock.now + .seconds(3)
    while await client.terminalError() == nil && ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(10))
    }
    let error = await client.terminalError()
    XCTAssertEqual(error, .capacity)
    do {
      _ = try await client.takeEvents()
      XCTFail("欠落を隠した")
    } catch { XCTAssertEqual(error as? ServiceError, .capacity) }
    await assertReaped(client)
  }
}
