import Foundation
import XCTest
@testable import PolarisDesktop

/// Synthetic helper replies only; the real `--browse-home` helper would read the account home.
private actor HomeTransport: HomeBrowseTransport {
  var replies: [ServiceValue] = []
  var calls: [ServiceRequest] = []
  var closed = false
  func queue(_ values: [ServiceValue]) { replies += values }
  func start() throws -> ServiceHello {
    try ServiceHello(.object([
      "protocol_version": .number("1"), "engine_epoch": .string("browse-1"),
      "capabilities": .array([.string("home_browse"), .string("shutdown")]),
      "limits": .object(["frame_bytes": .string("1048576"), "subscription_events": .string("256"),
        "subscription_bytes": .string("4194304"), "text_batch_bytes": .string("32768"), "text_batch_ms": .string("50")])]))
  }
  func send(requestID: String, request: ServiceRequest) throws -> ServiceReply {
    calls.append(request)
    guard !replies.isEmpty else { throw ServiceError.io }
    return .init(requestID: requestID, method: request.method, payload: replies.removeFirst(), rejection: nil)
  }
  func close() -> ServiceClient.Exit? { closed = true; return .init(status: 0, forced: false) }
}

@MainActor
final class HomeBrowseModelTests: XCTestCase {
  private func list(_ directory: String, _ entries: [(String, String)], snapshot: String = "s1",
                    next: String? = nil, epoch: String = "browse-1", paths: [String]? = nil) -> ServiceValue {
    var fields: [String: ServiceValue] = [
      "browse_epoch": .string(epoch), "snapshot_id": .string(snapshot), "directory": .string(directory),
      "omitted": .string("0"), "capacity": .bool(false),
      "entries": .array(entries.enumerated().map { index, entry in
        let path = paths?[index] ?? (directory.isEmpty ? entry.0 : directory + "/" + entry.0)
        return .object(["name": .string(entry.0), "path": .string(path), "kind": .string(entry.1),
                        "device": .string("1"), "inode": .string(String(index + 10))])
      }),
    ]
    if let next { fields["next_cursor"] = .string(next) }
    return .object(fields)
  }

  func testTreeExpandsLazilyPagesWithinOneSnapshotAndReadsBoundFiles() async throws {
    let transport = HomeTransport()
    await transport.queue([
      list("", [("docs", "directory"), ("note.txt", "file")]),
      list("docs", [("a.md", "file")], next: "c1"),
      list("docs", [("b.md", "file")]),
      .object(["browse_epoch": .string("browse-1"), "path": .string("docs/a.md"), "device": .string("1"),
               "inode": .string("10"), "content": .object(["state": .string("ready"), "text": .string("本文")])]),
      .object(["browse_epoch": .string("browse-1"), "path": .string("note.txt"), "device": .string("1"),
               "inode": .string("11"), "content": .object(["state": .string("unavailable"), "reason": .string("too_large")])]),
    ])
    let model = HomeBrowseModel(makeTransport: { transport })
    await model.start()
    XCTAssertEqual(model.rows.map(\.id), ["e:docs", "e:note.txt"])
    let docs = try XCTUnwrap(model.directories[""]?.entries.first)
    await model.toggle(docs)
    XCTAssertEqual(model.rows.map(\.id), ["e:docs", "e:docs/a.md", "m:docs", "e:note.txt"])
    await model.loadMore("docs")
    XCTAssertEqual(model.directories["docs"]?.entries.map(\.path), ["docs/a.md", "docs/b.md"])
    await model.open(try XCTUnwrap(model.directories["docs"]?.entries.first))
    XCTAssertEqual(model.content, .ready("本文"))
    await model.open(try XCTUnwrap(model.directories[""]?.entries.last))
    XCTAssertEqual(model.content, .unavailable("64KiBを超えるため表示しません。"))
    let calls = await transport.calls
    guard case .browseList("docs", "c1") = calls[2] else { return XCTFail("\(calls[2])") }
    await model.shutdown()
    let closed = await transport.closed
    XCTAssertTrue(closed)
  }

  func testForeignSnapshotsPathsAndEpochsAreNeverShown() async throws {
    let transport = HomeTransport()
    await transport.queue([
      list("", [("docs", "directory")], next: "c1"),
      list("", [("late", "file")], snapshot: "s2"),
      list("", [("x", "file")], paths: ["elsewhere/x"]),
      list("", [("y", "file")], epoch: "browse-old"),
    ])
    let model = HomeBrowseModel(makeTransport: { transport })
    await model.start()
    await model.loadMore("")
    XCTAssertEqual(model.directories[""]?.entries.map(\.name), ["docs"])
    XCTAssertNil(model.directories[""]?.nextCursor)
    XCTAssertNotNil(model.directories[""]?.notice)
    for _ in 0..<2 {
      await model.reload("")
      XCTAssertEqual(model.directories[""]?.entries.map(\.name), ["docs"])
      XCTAssertNotNil(model.directories[""]?.notice)
    }
  }
}
