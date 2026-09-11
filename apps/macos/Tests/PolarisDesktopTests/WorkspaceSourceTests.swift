import Foundation
import XCTest
@testable import PolarisDesktop

final class WorkspaceSourceTests: XCTestCase {
    private func ready(_ id: String = "src/main.swift", _ body: String = "print(1)") -> WorkspaceSourceReader.Reply {
        .init(state: .ready, files: [.init(id: id, path: id, body: body, unavailableReason: nil, unstagedDiff: nil, stagedDiff: nil, modified: true, staged: false, unsaved: false)],
              git: .init(state: "unavailable", reason: "Git情報を安全に取得できません", branch: nil, head: nil, changeSummary: nil, revisions: []), notices: [])
    }
    func testDecodesBoundedServiceProjection() throws {
        let reply = try WorkspaceSourceReader.decode(JSONEncoder().encode(ready()))
        XCTAssertEqual(reply.files.map(\.id), ["src/main.swift"]); XCTAssertEqual(reply.files.first?.body, "print(1)")
    }
    func testTypedServicePayloadDecodesWithoutTransportHeader() throws {
        let value = try ServiceCodec.json(JSONEncoder().encode(ready()))
        let reply = try WorkspaceSourceReader.decode(WorkspaceSourceReader.jsonPayload(value))
        XCTAssertEqual(reply.files.first?.body, "print(1)")
    }
    func testRejectsAbsoluteTraversalAndOversizedBody() throws {
        XCTAssertThrowsError(try WorkspaceSourceReader.decode(JSONEncoder().encode(ready("/secret"))))
        XCTAssertThrowsError(try WorkspaceSourceReader.decode(JSONEncoder().encode(ready("x", String(repeating: "a", count: WorkspaceSourceReader.maximumBodyBytes + 1)))))
    }
    @MainActor func testLateReplyDoesNotReplaceCurrentProjection() throws {
        let model = WorkspaceSourceModel(); let old = model.beginReload(); let current = model.beginReload()
        let data = try JSONEncoder().encode(ready("a", "new")); model.apply(data, generation: old); XCTAssertTrue(model.files.isEmpty)
        model.apply(data, generation: current); XCTAssertEqual(model.files.first?.body, "new")
    }
}
