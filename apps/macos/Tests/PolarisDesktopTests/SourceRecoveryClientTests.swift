import Darwin
import Foundation
import XCTest
@testable import PolarisDesktop

final class SourceRecoveryClientTests: XCTestCase {
  private let base = #"{"version":1,"request_id":"hello","engine_epoch":"epoch","project_id":"project","session_id":"session","state":"working"}"#

  private func reap(_ process: Process) async {
    while process.isRunning { try? await Task.sleep(for: .milliseconds(10)) }
    // Foundation has observed and reaped exit before isRunning becomes false.
    _ = process.terminationStatus
  }

  func testStrictCommonStatusSchema() throws {
    XCTAssertEqual(try SourceRecoveryClient.decode(Data(base.utf8)).state, .working)
    for extra in [#", "error":null"#, #", "path":"x""#, #", "request_id":"other""#, #", "r\u0065quest_id":"other""#] {
      XCTAssertThrowsError(try SourceRecoveryClient.decode(Data((base.dropLast() + extra + "}").utf8)))
    }
    for text in [base.replacingOccurrences(of: "\"working\"", with: "\"report_pending\""), base.replacingOccurrences(of: "\"version\":1", with: "\"version\":\"1\""), base.replacingOccurrences(of: "\"hello\"", with: "\"\"")] {
      XCTAssertThrowsError(try SourceRecoveryClient.decode(Data(text.utf8)))
    }
    XCTAssertThrowsError(try SourceRecoveryClient.decode(Data(repeating: 32, count: 8193)))
  }

  /// Synthetic child has no service/store/auth/network dependencies. All children exit
  /// on channel EOF and are synchronously reaped by the test, including failure paths.
  private func fixture(_ body: String) throws -> (SourceRecoveryClient.Channel, Process) {
    let channel = try SourceRecoveryClient.makeChannel(projectID: "project", sessionID: "session")
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
    process.arguments = ["-c", """
import os, socket, struct, json, fcntl, signal
signal.alarm(12)
fd=fcntl.fcntl(2, fcntl.F_DUPFD_CLOEXEC, 3)
n=os.open('/dev/null',os.O_RDWR); os.dup2(n,2); os.close(n)
os.close(0); os.close(1)
s=socket.socket(fileno=fd)
def exact(n):
 b=b''
 while len(b)<n:
  p=s.recv(n-len(b))
  if not p: raise EOFError()
  b+=p
 return b
def read(): return json.loads(exact(struct.unpack('>I',exact(4))[0]))
def response(r, **kw):
 d=dict(version=1,request_id=r['request_id'],engine_epoch='epoch',project_id='project',session_id='session',state='working'); d.update(kw); return d
def send(d):
 b=json.dumps(d,separators=(',',':')).encode(); s.sendall(struct.pack('>I',len(b))+b)
try:
\(body.split(separator: "\n", omittingEmptySubsequences: false).map { " " + $0 }.joined(separator: "\n"))
 while s.recv(4096): pass
except (EOFError,BrokenPipeError,ConnectionResetError): pass
s.close()
"""]
    process.standardInput = FileHandle.nullDevice
    process.standardOutput = FileHandle.nullDevice
    process.standardError = channel.childEndpoint
    do { try process.run(); try channel.childEndpoint.close() }
    catch { try? channel.childEndpoint.close(); throw error }
    return (channel, process)
  }

  func testIndependentFD2AndDelayedDuplicateCannotCompleteNewRequest() async throws {
    let (c, p) = try fixture("""
a=read(); send(response(a)); b=read(); send(response(a)); send(response(b,state='ready_to_exit'))
""")
    do {
      let first = try await c.client.hello(requestID: "first")
      let second = try await c.client.hello(requestID: "second")
      XCTAssertEqual(first.state, .working)
      XCTAssertEqual(second.requestID, "second")
      XCTAssertEqual(second.state, .readyToExit)
    } catch { XCTFail("\(error)") }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testDuplicateACKDrainIsBounded() async throws {
    let (c, p) = try fixture("a=read(); send(response(a)); b=read()\nfor i in range(17): send(response(a))")
    do {
      _ = try await c.client.hello(requestID: "first")
      _ = try await c.client.hello(requestID: "second")
      XCTFail("duplicate limit")
    } catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .correlation) }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testChangedOldStateDoesNotCompleteNewRequest() async throws {
    let (c, p) = try fixture("a=read(); send(response(a)); b=read(); send(response(a,state='ready_to_exit')); send(response(b))")
    do {
      _ = try await c.client.hello(requestID: "first")
      let status = try await c.client.hello(requestID: "second")
      XCTAssertEqual(status.requestID, "second")
      XCTAssertEqual(status.state, .working)
    } catch { XCTFail("\(error)") }
    await c.client.close(); await reap(p)
  }

  func testCancellationUnknownExplicitSameRequestConsumesLateACK() async throws {
    let trace = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-replay-\(UUID().uuidString).json")
    defer { try? FileManager.default.removeItem(at: trace) }
    let (c, p) = try fixture("""
a=read(); b=read()
assert a==b and a['method']=='hello' and a['request_id']=='first'
seen=[a['request_id'],b['request_id']]
send(response(a)); send(response(b))
while True:
 nextRequest=read(); seen.append(nextRequest['request_id'])
 assert nextRequest['method']=='hello'
 if nextRequest['request_id']=='first':
  assert nextRequest==a
  send(response(nextRequest))
  continue
 assert nextRequest['request_id']=='next'
 send(response(nextRequest))
 break
with open('\(trace.path)','x') as log: json.dump(seen,log)
""")
    do {
      let task = Task { try await c.client.hello(requestID: "first") }
      for _ in 0..<200 {
        if await c.client.uncertainRequestID != nil { break }
        try await Task.sleep(for: .milliseconds(5))
      }
      task.cancel()
      do { _ = try await task.value; XCTFail("cancel") }
      catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .cancelledOutcomeUnknown) }
      let unknown = await c.client.uncertainRequestID
      XCTAssertEqual(unknown, "first")
      do { _ = try await c.client.hello(requestID: "different"); XCTFail("must retain exact request") }
      catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .busy) }
      // The first ACK is gated on this explicit duplicate, not a wall-clock sleep.
      let status = try await c.client.hello(requestID: "first")
      XCTAssertEqual(status.requestID, "first")
      let next = try await c.client.hello(requestID: "next")
      XCTAssertEqual(next.requestID, "next")
    } catch { XCTFail("gated replay: \(error)") }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
    let ids = try JSONDecoder().decode([String].self, from: Data(contentsOf: trace))
    XCTAssertEqual(ids.map { Data($0.utf8) }, ["first", "first", "next"].map { Data($0.utf8) })
    print("synthetic replay IDs: \(ids)")
  }

  func testSavedProofWithNextPendingTargetAndStrictHash() async throws {
    let (c, p) = try fixture("""
a=read(); send(response(a)); r=read()
assert r['method']=='retry_result_save' and 'params' not in r and r['version']==1
send(response(r,state='report_pending',result=dict(operation_id=r['operation_id'],result_id='saved',saved_revision='11'),retry_target=dict(run_id='next',attempt_id='next',approval_id='next',operation_id='next',payload_hash='b'*64)))
""")
    do {
      _ = try await c.client.hello(requestID: "hello")
      let target = SourceRecoveryClient.Target(engineEpoch: "epoch", projectID: "project", sessionID: "session", runID: "run", attemptID: "attempt", approvalID: "approval", operationID: "operation", payloadHash: String(repeating: "a", count: 64))
      let status = try await c.client.retryResultSave(requestID: "save", target: target)
      XCTAssertEqual(status.result?.savedRevision, 11)
      XCTAssertEqual(status.retryTarget?.operationID, "next")
      XCTAssertEqual(status.state, .reportPending)
    } catch { XCTFail("\(error)") }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testByteDistinctSessionAndHelloResultRejected() async throws {
    for alteration in ["session_id='other'", "result=dict(operation_id='o',result_id='r',saved_revision='1')"] {
      let (c,p) = try fixture("a=read(); send(response(a,\(alteration)))")
      do { _ = try await c.client.hello(requestID: "hello"); XCTFail("correlation") }
      catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .correlation) }
      await c.client.close(); await reap(p)
    }
  }
  func testCancellationBeforeSendLeavesChannelUsable() async throws {
    let (c,p) = try fixture("a=read(); assert a['request_id']=='actual'; send(response(a))")
    let task = Task {
      withUnsafeCurrentTask { $0?.cancel() }
      return try await c.client.hello(requestID: "not-sent")
    }
    do { _ = try await task.value; XCTFail("cancel") }
    catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .cancelledBeforeSend) }
    let unknown = await c.client.uncertainRequestID
    XCTAssertNil(unknown)
    _ = try await c.client.hello(requestID: "actual")
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testFixedResponseTimeoutRetainsUnknownWithoutAutomaticRetry() async throws {
    let (c,p) = try fixture("a=read(); s.settimeout(5.4)\ntry:\n b=read(); raise AssertionError('automatic retry')\nexcept socket.timeout: send(response(a))")
    let start = ContinuousClock.now
    do { _ = try await c.client.hello(requestID: "timeout"); XCTFail("timeout") }
    catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .responseTimeout) }
    XCTAssertLessThan(ContinuousClock.now - start, .seconds(6))
    let unknown = await c.client.uncertainRequestID
    XCTAssertEqual(unknown, "timeout")
    try await Task.sleep(for: .milliseconds(600))
    let status = try await c.client.hello(requestID: "timeout")
    XCTAssertEqual(status.requestID, "timeout")
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testPartialFrameDeadlineClosesOnlyChannel() async throws {
    let (c,p) = try fixture("a=read(); s.sendall(bytes([0,0,0,100,123])); assert s.recv(1)==b''")
    let start = ContinuousClock.now
    do { _ = try await c.client.hello(requestID: "partial"); XCTFail("partial frame") }
    catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .frameTimeout) }
    XCTAssertLessThan(ContinuousClock.now - start, .seconds(4))
    let unknown = await c.client.uncertainRequestID
    XCTAssertEqual(unknown, "partial")
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testCanonicallyEquivalentRequestIDDoesNotMatch() async throws {
    let (c,p) = try fixture("a=read(); send(response(a,request_id='e'+chr(769)))")
    do { _ = try await c.client.hello(requestID: "é"); XCTFail("byte ID") }
    catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .correlation) }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testSameRequestStorageFailureThenSavedProofAndStateProgression() async throws {
    let (c,p) = try fixture("""
a=read(); send(response(a)); r=read()
send(response(r,state='recovery_required',error='storage_failed'))
r2=read(); assert r2==r
proof=dict(operation_id=r['operation_id'],result_id='saved',saved_revision='11')
send(response(r2,state='report_pending',result=proof,retry_target=dict(run_id='next',attempt_id='next',approval_id='next',operation_id='next',payload_hash='b'*64)))
r3=read(); assert r3==r
send(response(r3,state='ready_to_exit',result=proof))
b=read(); send(response(r3,state='working',result=proof)); send(response(b,state='recovery_required'))
""")
    do {
      _ = try await c.client.hello(requestID: "hello")
      let target = SourceRecoveryClient.Target(engineEpoch: "epoch", projectID: "project", sessionID: "session", runID: "run", attemptID: "attempt", approvalID: "approval", operationID: "operation", payloadHash: String(repeating: "a", count: 64))
      let failed = try await c.client.retryResultSave(requestID: "save", target: target)
      XCTAssertEqual(failed.error, .storageFailed)
      let saved = try await c.client.retryResultSave(requestID: "save", target: target)
      XCTAssertEqual(saved.result?.savedRevision, 11)
      XCTAssertEqual(saved.state, .reportPending)
      let advanced = try await c.client.retryResultSave(requestID: "save", target: target)
      XCTAssertEqual(advanced.state, .readyToExit)
      XCTAssertEqual(advanced.result, saved.result)
      let newer = try await c.client.hello(requestID: "newer")
      XCTAssertEqual(newer.requestID, "newer")
      XCTAssertEqual(newer.state, .recoveryRequired)
    } catch { XCTFail("\(error)") }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

  func testSameRequestChangedSavedProofRejected() async throws {
    let (c,p) = try fixture("""
a=read(); send(response(a)); r=read()
send(response(r,result=dict(operation_id=r['operation_id'],result_id='saved',saved_revision='11')))
r=read(); send(response(r,result=dict(operation_id=r['operation_id'],result_id='saved',saved_revision='12')))
""")
    do {
      _ = try await c.client.hello(requestID: "hello")
      let target = SourceRecoveryClient.Target(engineEpoch: "epoch", projectID: "project", sessionID: "session", runID: "run", attemptID: "attempt", approvalID: "approval", operationID: "operation", payloadHash: String(repeating: "a", count: 64))
      _ = try await c.client.retryResultSave(requestID: "save", target: target)
      _ = try await c.client.retryResultSave(requestID: "save", target: target)
      XCTFail("changed saved proof")
    } catch { XCTAssertEqual(error as? SourceRecoveryClient.Failure, .correlation) }
    await c.client.close(); await reap(p); XCTAssertEqual(p.terminationStatus, 0)
  }

}
