import XCTest

@testable import PolarisDesktop

final class OrchestrationProtocolTests: XCTestCase {
  private func orchestration() -> ServiceValue {
    .object([
      "workflow": .object([
        "phase": .string("implement"), "step": .string("executing"),
        "active_task": .object(["artifact_id": .string("task-1"), "content_hash": .string(String(repeating: "a", count: 64))]),
        "acceptance": .array([.object(["criterion_id": .string("criterion-1"), "state": .string("passed"), "evidence_count": .string("1")])]),
        "evidence": .object(["passed": .string("1"), "failed": .string("0"), "stale": .string("0"), "unavailable": .string("0")]),
        "convergence": .object(["state": .string("pending"), "required_criteria": .string("1"), "unresolved_gaps": .string("0")]),
      ]),
      "session_manager": .object([
        "manager_revision": .string("1"), "session_count": .string("2"), "session_total": .string("2"), "session_omitted": .bool(false),
        "sessions": .array([
          .object(["session_id": .string("main"), "kind": .string("current"), "state": .string("running")]),
          .object(["session_id": .string("side"), "kind": .string("sidechat"), "state": .string("idle"), "source_session_id": .string("main")]),
        ]), "sidechat": .string("active"), "queued_admissions": .string("1"), "active_runs": .string("1"),
      ]),
      "subagents": .array([.object([
        "root_run_id": .string("run-1"), "root_attempt_id": .string("attempt-root-1"), "control_revision": .string("7"),
        "logical_child_id": .string("child-1"), "attempt_id": .string("attempt-1"), "display_name": .string("Sirius"),
        "role": .string("reviewer"), "state": .string("running"), "activity": .string("read(CURRENT.md)"),
      ])]), "subagent_total": .string("1"), "subagent_omitted": .bool(false),
      "mcp_leases": .array([.object([
        "lease_id": .string("lease-1"), "server_id": .string("mcp-1"), "holder_run_id": .string("run-1"),
        "holder_attempt_id": .string("attempt-1"), "state": .string("active"), "expires_at_unix_ms": .string("1"),
      ])]), "mcp_lease_total": .string("1"), "mcp_lease_omitted": .bool(false),
      "common_runner": .object([
        "state": .string("running"), "case_id": .string("case-1"), "completed_cases": .string("1"),
        "failed_cases": .string("0"), "cleanup_pending": .bool(false),
      ]),
      "clarification": .object(["state": .string("pending"), "questions": .array([.object([
        "question_id": .string("question-1"), "text": .string("進めますか"), "options": .array([.object(["option_id": .string("yes"), "label": .string("進める")])]),
        "free_text": .object(["allowed": .bool(true), "required": .bool(false), "max_bytes": .string("128")]), "answered": .bool(false),
      ])]), "unanswered_count": .string("1")]),
    ])
  }

  func testV1AndV2AreAcceptedAndOrchestrationEventIsClosed() throws {
    let event = ServiceValue.object([
      "protocol_version": .number("2"), "kind": .string("event"), "engine_epoch": .string("epoch"),
      "subscription_id": .string("subscription"), "event_seq": .string("1"), "session_id": .string("main"),
      "session_revision": .string("2"), "type": .string("orchestration.updated"), "payload": orchestration(),
    ])
    XCTAssertNoThrow(try ServiceSchema.validateEvent(event, epoch: "epoch"))
    let v1 = ServiceValue.object([
      "protocol_version": .number("1"), "kind": .string("event"), "engine_epoch": .string("epoch"),
      "subscription_id": .string("subscription"), "event_seq": .string("1"), "session_id": .string("main"),
      "session_revision": .string("2"), "type": .string("orchestration.updated"), "payload": orchestration(),
    ])
    XCTAssertNoThrow(try ServiceSchema.validateEvent(v1, epoch: "epoch"))
    var leaked = event
    guard case .object(var fields) = leaked, case .object(var payload) = fields["payload"]! else { return XCTFail("fixture") }
    payload["credential"] = .string("forbidden")
    fields["payload"] = .object(payload); leaked = .object(fields)
    XCTAssertThrowsError(try ServiceSchema.validateEvent(leaked, epoch: "epoch"))
  }

  func testV2AdmissionRequestsAreBoundedAndDefaultToV2() throws {
    let request = try ServiceRequest.inputSubmit(session: "main", managerRevision: 0, sessionRevision: 1, goalRevision: 2,
                                                 route: "auto", text: "continue", attachments: ["attachment-1"], reference: nil, admission: "adm-1")
      .wire(clientID: "client", requestID: "request")
    XCTAssertEqual(request["protocol_version"], .number("2"))
    XCTAssertEqual(request["method"], .string("input.submit"))
    XCTAssertThrowsError(try ServiceRequest.inputSubmit(session: "main", managerRevision: 0, sessionRevision: 1, goalRevision: 2,
                                                         route: "auto", text: String(repeating: "x", count: 32_769), attachments: [], reference: nil, admission: nil)
      .wire(clientID: "client", requestID: "request"))
    XCTAssertNoThrow(try ServiceRequest.sessionList.wire(clientID: "client", requestID: "request"))
    XCTAssertEqual(try ServiceRequest.sessionSelect(session: "side").wire(clientID: "client", requestID: "request")["method"], .string("session.select"))
    XCTAssertEqual(try ServiceRequest.inputCancel(session: "main", managerRevision: 3, admission: "adm-1").wire(clientID: "client", requestID: "request")["method"], .string("input.cancel"))
  }

  func testM25ClarificationKeepsQuestionIdentityAndFreeTextContract() throws {
    let projected = try WorkspaceOrchestration(orchestration())
    let question = try XCTUnwrap(projected.clarification.questions.first)
    XCTAssertEqual(projected.clarification.state, .pending)
    XCTAssertEqual(question.id, "question-1")
    XCTAssertEqual(question.options.first?.id, "yes")
    XCTAssertTrue(question.freeText.allowed)
    XCTAssertFalse(question.freeText.required)
    XCTAssertEqual(question.freeText.maxBytes, 128)
    XCTAssertEqual(projected.subagents.first?.rootRunID, "run-1")
    XCTAssertEqual(projected.subagents.first?.rootAttemptID, "attempt-root-1")
    XCTAssertEqual(projected.subagents.first?.controlRevision, 7)

    var incomplete = orchestration()
    guard case .object(var fields) = incomplete, case .array(var rows) = fields["subagents"],
          case .object(var row) = rows.first else { return XCTFail("fixture") }
    row.removeValue(forKey: "control_revision"); rows[0] = .object(row); fields["subagents"] = .array(rows); incomplete = .object(fields)
    XCTAssertThrowsError(try ServiceSchema.orchestration.check(incomplete))
  }

  func testSubagentControlWiresExactCorrelationAndRejectsUnboundedResponses() throws {
    let wait = try ServiceRequest.subagentWait(session: "side", rootRun: "run-1", rootAttempt: "attempt-1",
                                                revision: 7, afterCursor: 3).wire(clientID: "client", requestID: "request")
    XCTAssertEqual(wait["method"], .string("subagent.wait"))
    XCTAssertEqual(wait["session_id"], .string("side"))
    XCTAssertEqual(wait["params"]?["root_run_id"], .string("run-1"))
    XCTAssertEqual(wait["params"]?["after_cursor"], .string("3"))
    let cancel = try ServiceRequest.subagentCancel(session: "side", rootRun: "run-1", rootAttempt: "attempt-1",
                                                    logicalChild: "child-1", childAttempt: "child-attempt-1", revision: 7)
      .wire(clientID: "client", requestID: "request")
    XCTAssertEqual(cancel["method"], .string("subagent.cancel"))
    XCTAssertEqual(cancel["params"]?["logical_child_id"], .string("child-1"))

    let reply = ServiceValue.object([
      "root_run_id": .string("run-1"), "root_attempt_id": .string("attempt-1"), "revision": .string("7"),
      "cursor": .string("4"), "state": .string("updated"), "child_total": .string("1"), "child_omitted": .bool(false),
      "children": .array([.object(["root_run_id": .string("run-1"), "root_attempt_id": .string("attempt-1"), "control_revision": .string("7"),
        "logical_child_id": .string("child-1"), "attempt_id": .string("child-attempt-1"),
        "display_name": .string("Sirius"), "role": .string("reviewer"), "state": .string("waiting")])]),
    ])
    XCTAssertNoThrow(try ServiceSchema.result("subagent.wait").check(reply))
    var oversized = reply
    guard case .object(var fields) = oversized, case .array(let child) = fields["children"] else { return XCTFail("fixture") }
    fields["children"] = .array(Array(repeating: child[0], count: 33)); oversized = .object(fields)
    XCTAssertThrowsError(try ServiceSchema.result("subagent.wait").check(oversized))
  }
}
