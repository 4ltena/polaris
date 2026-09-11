import Foundation
import XCTest
@testable import PolarisSettings

final class WorkspaceSettingsTests: XCTestCase {
    private func isolated(_ body: (URL, URL) throws -> Void) throws {
        let base = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-binding-\(UUID())")
        try FileManager.default.createDirectory(at: base, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: base) }
        let source = base.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        try body(base.appendingPathComponent("metadata/settings.json"), source)
    }
    func testVersionOneMigratesWithoutInventingSelectionOrIDs() throws {
        var old = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(SettingsDocument())) as? [String: Any])
        old["schemaVersion"] = 1
        let migrated = try JSONDecoder().decode(SettingsDocument.self, from: JSONSerialization.data(withJSONObject: old))
        XCTAssertEqual(migrated.schemaVersion, 2)
        XCTAssertNil(migrated.workspace); XCTAssertNil(migrated.preferences?.projectPath)
        try isolated { url, _ in
            let store = try SettingsStore(url: url)
            let (blank, root) = try store.prepareWorkspace(migrated)
            XCTAssertNil(root); XCTAssertNil(blank.workspace)
            XCTAssertFalse(FileManager.default.fileExists(atPath: url.path + ".workspace"))
        }
    }
    func testBindingIsDurableStablePrivateAndOutsideSource() throws {
        try isolated { url, source in
            var store: SettingsStore? = try SettingsStore(url: url)
            var document = SettingsDocument()
            document.draft.selectFolder(source); document.finish()
            let (first, root) = try store!.prepareWorkspace(document)
            XCTAssertEqual(try store!.load(), first)
            XCTAssertNotNil(first.workspace)
            let metadata = try XCTUnwrap(root)
            XCTAssertFalse(metadata.pathComponents.starts(with: source.pathComponents))
            let mode = try FileManager.default.attributesOfItem(atPath: metadata.path)[.posixPermissions] as? NSNumber
            XCTAssertEqual(mode?.intValue, 0o700)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: source.path), [])
            store = nil
            let reopened = try SettingsStore(url: url)
            let (again, againRoot) = try reopened.prepareWorkspace(reopened.load())
            XCTAssertEqual(first.workspace, again.workspace); XCTAssertEqual(root, againRoot)
        }
    }
    func testMissingOrReplacedSourceNeverRebindsSavedIDs() throws {
        try isolated { url, source in
            let store = try SettingsStore(url: url)
            var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
            let (bound, _) = try store.prepareWorkspace(document)
            try FileManager.default.moveItem(at: source, to: source.appendingPathExtension("original"))
            XCTAssertThrowsError(try store.prepareWorkspace(bound))
            try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
            XCTAssertThrowsError(try store.prepareWorkspace(bound))
            XCTAssertEqual(try store.load(), bound)
        }
    }
    func testMetadataSymlinkAncestorAndRootAreRejectedWithoutTargetWrite() throws {
        try isolated { url, source in
            let alias = url.deletingLastPathComponent()
            try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: source)
            XCTAssertThrowsError(try SettingsStore(url: url))
            XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: source.path).isEmpty)
            try FileManager.default.removeItem(at: alias)
            let store = try SettingsStore(url: url)
            let root = URL(fileURLWithPath: url.path + ".workspace")
            try FileManager.default.createSymbolicLink(at: root, withDestinationURL: source)
            var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
            XCTAssertThrowsError(try store.prepareWorkspace(document))
            XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: source.path).isEmpty)
            XCTAssertNil(try store.load().workspace)
        }
    }
    func testMetadataInsideSourceAndMissingSavedRootFailClosed() throws {
        try isolated { url, source in
            let inside = try SettingsStore(url: source.appendingPathComponent("settings.json"))
            var document = SettingsDocument(); document.draft.selectFolder(source); document.finish()
            XCTAssertThrowsError(try inside.prepareWorkspace(document)) { XCTAssertEqual($0 as? SettingsError, .metadataInSource) }
            let outside = try SettingsStore(url: url)
            let (bound, root) = try outside.prepareWorkspace(document)
            try FileManager.default.removeItem(at: XCTUnwrap(root))
            XCTAssertThrowsError(try outside.prepareWorkspace(bound))
            XCTAssertEqual(try outside.load(), bound)
        }
    }
}
