import Foundation
import XCTest
@testable import PolarisDesktop

@MainActor
final class ServiceRecoveryLifecycleTests: XCTestCase {
  private struct Fixture {
    let root: URL
    let script: URL
    let store: ServiceClient.PersistentStore
  }
  private func fixture(_ body: String, controlEpoch: String = "epoch") throws -> Fixture {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-recovery-lifecycle-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    let store = try ServiceClient.PersistentStore(root: root, projectID: "project", sessionID: "session")
    let args = ["--store-root", store.root.path, "--project-id", "project", "--session-id", "session", "--source-recovery-fd2"]
    let encoded = String(decoding: try JSONSerialization.data(withJSONObject: args, options: [.withoutEscapingSlashes]), as: UTF8.self)
    let script = root.appendingPathComponent("helper.py")
    let source = """
#!/usr/bin/python3
import os, sys, socket, fcntl, struct, json, signal, time
signal.alarm(10)
assert sys.argv[1:]==\(encoded)
assert 'OPENAI_API_KEY' not in os.environ
fd=fcntl.fcntl(2,fcntl.F_DUPFD_CLOEXEC,3)
n=os.open('/dev/null',os.O_RDWR); os.dup2(n,2); os.close(n)
s=socket.socket(fileno=fd)
def exact(read,n):
 b=b''
 while len(b)<n:
  x=read(n-len(b))
  if not x: raise EOFError()
  b+=x
 return b
def receive(read): return json.loads(exact(read,struct.unpack('>I',exact(read,4))[0]))
def wire(v):
 b=json.dumps(v).encode(); return struct.pack('>I',len(b))+b
def control(): return receive(s.recv)
def status(r,**kw):
 v=dict(version=1,request_id=r['request_id'],engine_epoch='\(controlEpoch)',project_id='project',session_id='session',state='working'); v.update(kw); s.sendall(wire(v))
r=receive(lambda n:os.read(0,n))
assert r['method']=='hello'
os.write(1,wire(dict(protocol_version=1,kind='response',client_id=r['client_id'],request_id=r['request_id'],result=dict(type='hello',payload=dict(protocol_version=1,engine_epoch='epoch',capabilities=['shutdown'],limits=dict(frame_bytes='1048576',subscription_events='256',subscription_bytes='4194304',text_batch_bytes='32768',text_batch_ms='50'))))))
r=control(); assert r['method']=='hello'; status(r)
\(body)
s.close()
"""
    try Data(source.utf8).write(to: script)
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
    return Fixture(root: root, script: script, store: store)
  }
  private func client(_ f: Fixture) throws -> ServiceClient {
    var config = ServiceClient.Configuration()
    config.exitGrace = .milliseconds(50)
    return try ServiceClient(helperURL: f.script, configuration: config, persistentStore: f.store, recoveryMode: .resultOnly)
  }
  private func observedExit(_ client: ServiceClient) async throws -> ServiceClient.Exit? {
    let deadline = ContinuousClock.now + .seconds(11)
    while await client.observedExit() == nil && ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(10))
    }
    return await client.observedExit()
  }

  func testMainLossRetainsControlAndExplicitRetryUntilActualExit() async throws {
    let f = try fixture("""
r=control(); assert r['request_id']=='disconnect'; os.close(0); os.close(1); status(r)
r=control(); assert r['request_id']=='retry-exact' and r['method']=='retry_result_save'
assert r['engine_epoch']=='epoch' and r['operation_id']=='operation' and r['payload_hash']=='a'*64
status(r,state='ready_to_exit',result=dict(operation_id='operation',result_id='result',saved_revision='7'))
r=control(); assert r['request_id']=='finish'; status(r,state='ready_to_exit'); time.sleep(0.1)
""")
    defer { try? FileManager.default.removeItem(at: f.root) }
    let c = try client(f)
    let transport: any WorkspaceServiceTransport = c
    let beforeLaunch = await transport.hasUnreapedChild()
    XCTAssertFalse(beforeLaunch)
    do {
      _ = try await c.start()
      let running = await transport.hasUnreapedChild()
      XCTAssertTrue(running)
      _ = try await c.recoveryHello(requestID: "disconnect")
      try await Task.sleep(for: .milliseconds(250))
      let closeStart = ContinuousClock.now
      let pending = await c.close()
      XCTAssertNil(pending)
      XCTAssertLessThan(ContinuousClock.now - closeStart, .seconds(1))
      let retained = await transport.hasUnreapedChild()
      XCTAssertTrue(retained)
      let target = SourceRecoveryClient.Target(engineEpoch: "epoch", projectID: "project", sessionID: "session", runID: "run", attemptID: "attempt", approvalID: "approval", operationID: "operation", payloadHash: String(repeating: "a", count: 64))
      let result = try await c.retryResultSave(requestID: "retry-exact", target: target)
      XCTAssertEqual(result.result?.savedRevision, 7)
      XCTAssertEqual(result.state, .readyToExit)
      let stillPending = await c.close()
      XCTAssertNil(stillPending, "ready status is not child exit")
      _ = try await c.recoveryHello(requestID: "finish")
    } catch { XCTFail("\(error)") }
    let exit = try await observedExit(c)
    XCTAssertEqual(exit, .init(status: 0, forced: false))
    let afterExit = await transport.hasUnreapedChild()
    XCTAssertFalse(afterExit)
  }

  func testBrokenControlDoesNotTerminateRetainedChild() async throws {
    let f = try fixture("r=control(); os.close(0); os.close(1); s.close(); time.sleep(0.6)")
    defer { try? FileManager.default.removeItem(at: f.root) }
    let c = try client(f)
    _ = try await c.start()
    do { _ = try await c.recoveryHello(requestID: "break"); XCTFail("EOF") } catch {}
    let pending = await c.close()
    XCTAssertNil(pending)
    try await Task.sleep(for: .milliseconds(200))
    let retained = await c.hasUnreapedChild()
    XCTAssertTrue(retained)
    let exit = try await observedExit(c)
    XCTAssertEqual(exit, .init(status: 0, forced: false))
  }

  func testStartEpochMismatchRetainsChildAndRejectsWrongRecoveryCoordinates() async throws {
    let f = try fixture("r=control(); status(r); time.sleep(0.2)", controlEpoch: "wrong")
    defer { try? FileManager.default.removeItem(at: f.root) }
    let c = try client(f)
    do { _ = try await c.start(); XCTFail("epoch mismatch") }
    catch { XCTAssertEqual(error as? ServiceError, .correlation) }
    let pending = await c.close()
    XCTAssertNil(pending)
    let retained = await c.hasUnreapedChild()
    XCTAssertTrue(retained)
    do { _ = try await c.recoveryHello(requestID: "finish"); XCTFail("wrong coordinates") }
    catch { XCTAssertEqual(error as? ServiceError, .correlation) }
    let exit = try await observedExit(c)
    XCTAssertEqual(exit, .init(status: 0, forced: false))
  }

  func testOptInRequiresStoreAndFailedLaunchHasNoChild() async throws {
    XCTAssertThrowsError(try ServiceClient(helperURL: URL(fileURLWithPath: "/missing"), recoveryMode: .resultOnly))
    let f = try fixture("")
    defer { try? FileManager.default.removeItem(at: f.root) }
    let c = try ServiceClient(helperURL: f.root.appendingPathComponent("missing"), persistentStore: f.store, recoveryMode: .resultOnly)
    do { _ = try await c.start(); XCTFail("missing executable") }
    catch { XCTAssertEqual(error as? ServiceError, .launch) }
    let retained = await c.hasUnreapedChild()
    XCTAssertFalse(retained)
    let pending = await c.close()
    XCTAssertNil(pending)
    do { _ = try await c.recoveryHello(requestID: "no-child"); XCTFail("no child") }
    catch { XCTAssertEqual(error as? ServiceError, .notReady) }
  }
  func testPendingErrorAndCancelledControlKeepOriginalRequestAndChild() async throws {
    let f = try fixture("""
r=control(); assert r['request_id']=='pending'
status(r,state='report_pending',error='storage_failed',retry_target=dict(run_id='run',attempt_id='attempt',approval_id='approval',operation_id='operation',payload_hash='a'*64))
r=control(); assert r['request_id']=='unknown'; time.sleep(0.3); status(r,state='recovery_required')
r=control(); assert r['request_id']=='finish'; status(r,state='ready_to_exit'); time.sleep(0.1)
""")
    defer { try? FileManager.default.removeItem(at: f.root) }
    let c = try client(f)
    do {
      _ = try await c.start()
      let pending = try await c.recoveryHello(requestID: "pending")
      XCTAssertEqual(pending.error, .storageFailed)
      XCTAssertEqual(pending.state, .reportPending)
      let closed = await c.close()
      XCTAssertNil(closed)
      let request = Task { try await c.recoveryHello(requestID: "unknown") }
      let deadline = ContinuousClock.now + .seconds(2)
      while await c.recoveryUnknownRequestID() == nil && ContinuousClock.now < deadline {
        try await Task.sleep(for: .milliseconds(5))
      }
      request.cancel()
      do { _ = try await request.value; XCTFail("cancel") }
      catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .cancelledOutcomeUnknown) }
      let unknown = await c.recoveryUnknownRequestID()
      XCTAssertEqual(unknown, "unknown")
      do { _ = try await c.recoveryHello(requestID: "different"); XCTFail("new ID") }
      catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .busy) }
      try await Task.sleep(for: .milliseconds(400))
      let reconciled = try await c.recoveryHello(requestID: "unknown")
      XCTAssertEqual(reconciled.state, .recoveryRequired)
      _ = try await c.recoveryHello(requestID: "finish")
    } catch { XCTFail("\(error)") }
    let exit = try await observedExit(c)
    XCTAssertEqual(exit, .init(status: 0, forced: false))
  }

  func testCancelledBeforeLaunchHasNoChildThroughProtocol() async throws {
    let f = try fixture("")
    defer { try? FileManager.default.removeItem(at: f.root) }
    let c = try client(f)
    let start = Task {
      withUnsafeCurrentTask { $0?.cancel() }
      return try await c.start()
    }
    do { _ = try await start.value; XCTFail("cancel") }
    catch { XCTAssertTrue(error is CancellationError) }
    let transport: any WorkspaceServiceTransport = c
    let retained = await transport.hasUnreapedChild()
    XCTAssertFalse(retained)
    let pending = await c.close()
    XCTAssertNil(pending)
  }

}
