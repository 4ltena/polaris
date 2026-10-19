import XCTest
import Foundation
import Darwin
import CryptoKit
@testable import PolarisDesktop

final class MCPProductionEntryLeaseTests: XCTestCase {
    private final class Fixture {
        let root: URL
        let home: URL
        let binary: URL
        let parent: URL
        init() throws {
            let temporary = FileManager.default.temporaryDirectory.path
            guard let resolved = realpath(temporary, nil) else { throw MCPProductionEntryLease.Failure.refused }
            defer { free(resolved) }
            root = URL(fileURLWithPath: String(cString: resolved), isDirectory: true)
                .appendingPathComponent("polaris-mcp-entry-\(UUID().uuidString)")
            home = root.appendingPathComponent("home")
            binary = root.appendingPathComponent("entry")
            parent = home.appendingPathComponent(".polaris")
            for directory in [root, home, parent] {
                try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
                                                       attributes: [.posixPermissions: 0o700])
            }
            try Data("synthetic entry".utf8).write(to: binary)
            XCTAssertEqual(chmod(binary.path, 0o755), 0)
        }
        deinit { try? FileManager.default.removeItem(at: root) }
        func write(_ name: String, _ data: Data) throws {
            let path = parent.appendingPathComponent(name)
            try data.write(to: path)
            XCTAssertEqual(chmod(path.path, 0o600), 0)
        }
        func active() throws {
            var info = stat()
            XCTAssertEqual(lstat(binary.path, &info), 0)
            let path = String(data: try JSONSerialization.data(
                withJSONObject: binary.path, options: [.fragmentsAllowed, .withoutEscapingSlashes]), encoding: .utf8)!
            let digest = SHA256.hash(data: try Data(contentsOf: binary))
                .map { String(format: "%02x", $0) }.joined()
            // Exact serde struct field order: this is also the Rust wire fixture.
            let manifest = Data("{\"protocol_version\":2,\"entries\":[{\"kind\":\"desktop_app\",\"binary\":{\"path\":\(path),\"anchor\":{\"device\":\(UInt64(UInt32(bitPattern: info.st_dev))),\"inode\":\(info.st_ino),\"uid\":\(info.st_uid),\"mode\":\(info.st_mode & 0o7777),\"links\":\(info.st_nlink)},\"sha256\":\"\(digest)\"}}],\"runtime_copy_source_hashes\":[]}".utf8)
            let hash = SHA256.hash(data: manifest).map { String(format: "%02x", $0) }.joined()
            let gate = Data("{\"state\":\"active_v2\",\"protocol_version\":2,\"migration_id\":\"\(String(repeating: "a", count: 64))\",\"entry_manifest_hash\":\"\(hash)\",\"active_intent_hash\":\"\(String(repeating: "b", count: 64))\"}".utf8)
            try write("mcp-entry-gate-v2.lock", Data())
            try write("mcp-entry-identities-v2.json", manifest)
            try write("mcp-entry-gate-v2.json", gate)
        }
        func enter() throws -> MCPProductionEntryLease {
            return try MCPProductionEntryLease(home: home.path, executable: binary.path)
        }
    }

    func testAbsentGateDoesNotCreateStateAndPartialGateRefuses() throws {
        let fixture = try Fixture()
        let before = try FileManager.default.contentsOfDirectory(atPath: fixture.parent.path)
        let lease = try fixture.enter()
        try lease.verify()
        XCTAssertEqual(before, try FileManager.default.contentsOfDirectory(atPath: fixture.parent.path))
        try fixture.write("mcp-entry-gate-v2.lock", Data())
        XCTAssertThrowsError(try fixture.enter())
        XCTAssertThrowsError(try lease.verify())
    }

    func testCanonicalRustWireAdmitsOwnerWritableBinaryAndDetectsChange() throws {
        let fixture = try Fixture()
        try fixture.active()
        let lease = try fixture.enter()
        try lease.verify()
        try Data("changed bytes".utf8).write(to: fixture.binary)
        XCTAssertThrowsError(try lease.verify())
    }

    func testSharedLockPreventsMigrationAndNamedLockReplacementRefuses() throws {
        let fixture = try Fixture()
        try fixture.active()
        let lease = try fixture.enter()
        let path = fixture.parent.appendingPathComponent("mcp-entry-gate-v2.lock")
        let writer = open(path.path, O_RDONLY | O_NOFOLLOW)
        XCTAssertGreaterThanOrEqual(writer, 0)
        defer { close(writer) }
        XCTAssertNotEqual(flock(writer, LOCK_EX | LOCK_NB), 0)
        try FileManager.default.moveItem(at: path, to: path.appendingPathExtension("retired"))
        try fixture.write("mcp-entry-gate-v2.lock", Data())
        XCTAssertThrowsError(try lease.verify())
    }

    func testMigratingUnknownFieldAndOversizeRefuseWithoutRepair() throws {
        let fixture = try Fixture()
        try fixture.active()
        let path = fixture.parent.appendingPathComponent("mcp-entry-gate-v2.json")
        let original = try Data(contentsOf: path)
        var object = try XCTUnwrap(JSONSerialization.jsonObject(with: original) as? [String: Any])
        object["state"] = "migrating_v2"
        try fixture.write("mcp-entry-gate-v2.json", JSONSerialization.data(withJSONObject: object))
        XCTAssertThrowsError(try fixture.enter())
        object["state"] = "active_v2"; object["unknown"] = true
        let unknown = try JSONSerialization.data(withJSONObject: object)
        try fixture.write("mcp-entry-gate-v2.json", unknown)
        XCTAssertThrowsError(try fixture.enter())
        XCTAssertEqual(try Data(contentsOf: path), unknown)
        try fixture.write("mcp-entry-gate-v2.json", Data(repeating: 32, count: 65537))
        XCTAssertThrowsError(try fixture.enter())
    }

    func testSymlinkHardlinkAndParentReplacementRefuse() throws {
        let fixture = try Fixture()
        try fixture.active()
        let lease = try fixture.enter()
        let alias = fixture.root.appendingPathComponent("alias")
        XCTAssertEqual(link(fixture.binary.path, alias.path), 0)
        XCTAssertThrowsError(try lease.verify())
        try FileManager.default.removeItem(at: alias)
        try lease.verify()
        let moved = fixture.home.appendingPathComponent(".polaris-retired")
        try FileManager.default.moveItem(at: fixture.parent, to: moved)
        try FileManager.default.createDirectory(at: fixture.parent, withIntermediateDirectories: false,
                                               attributes: [.posixPermissions: 0o700])
        XCTAssertThrowsError(try lease.verify())
        try FileManager.default.removeItem(at: fixture.parent)
        XCTAssertEqual(symlink(moved.path, fixture.parent.path), 0)
        XCTAssertThrowsError(try fixture.enter())
    }
}
