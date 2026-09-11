import Foundation
import XCTest
@testable import PolarisSettings

final class SettingsTests: XCTestCase {
    private func isolated(_ body: (URL) throws -> Void) throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-native-test-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        try body(directory.appendingPathComponent("settings.json"))
    }

    func testNavigationKeepsChoicesAndCancellationKeepsFolder() {
        var state = WelcomeState()
        XCTAssertNil(state.preferences.permission)
        state.preferences.gpt = .apiKey
        state.preferences.permission = .readCreateBuildExternal
        state.selectFolder(URL(fileURLWithPath: "/tmp/project"))
        state.go(to: .review); state.next()
        XCTAssertEqual(state.page, .review)
        state.back(); XCTAssertEqual(state.page, .theme)
        state.go(to: .welcome); state.back()
        XCTAssertEqual(state.page, .welcome)
        state.selectFolder(nil)
        XCTAssertEqual(state.preferences.projectPath, "/tmp/project")
        XCTAssertEqual(state.preferences.gpt, .apiKey)
        XCTAssertEqual(state.preferences.permission, .readCreateBuildExternal)
    }

    func testDraftCompletionAndReopenRoundTrip() throws {
        try isolated { url in
            let store = try SettingsStore(url: url)
            var document = try store.load()
            XCTAssertFalse(document.isComplete)
            XCTAssertTrue(document.isEditing)
            document.draft.preferences.theme = .dark
            document.draft.preferences.gpt = .chatGPT
            document.draft.preferences.local = .linkInstalled
            document.draft.preferences.permission = .readCreate
            document.draft.selectFolder(URL(fileURLWithPath: "/tmp/project 日本語"))
            document.draft.go(to: .project)
            try store.save(document)
            XCTAssertEqual(try store.load(), document)
            document.finish()
            try store.save(document)
            XCTAssertTrue(try store.load().isComplete)
            XCTAssertFalse(try store.load().isEditing)
            document.reopen()
            document.draft.preferences.theme = .light
            try store.save(document)
            let resumed = try store.load()
            XCTAssertTrue(resumed.isEditing)
            XCTAssertEqual(resumed.preferences?.theme, .dark)
            XCTAssertEqual(resumed.draft.preferences.theme, .light)
            XCTAssertEqual(resumed, document)
            let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
            XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        }
    }

    func testCorruptAndFutureFilesAreNotReset() throws {
        try isolated { url in
            let store = try SettingsStore(url: url)
            let corrupt = Data("{broken".utf8)
            try corrupt.write(to: url)
            XCTAssertThrowsError(try store.load()) { XCTAssertEqual($0 as? SettingsError, .invalidDocument) }
            XCTAssertEqual(try Data(contentsOf: url), corrupt)
            let current = try JSONEncoder().encode(SettingsDocument())
            var object = try XCTUnwrap(JSONSerialization.jsonObject(with: current) as? [String: Any])
            object["schemaVersion"] = 99
            let future = try JSONSerialization.data(withJSONObject: object)
            try future.write(to: url)
            XCTAssertThrowsError(try store.load())
            XCTAssertEqual(try Data(contentsOf: url), future)
        }
    }

    func testReadFailureAndDanglingSymlinkAreNotNewDraft() throws {
        try isolated { url in
            let store = try SettingsStore(url: url)
            try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
            XCTAssertThrowsError(try store.load()) { XCTAssertEqual($0 as? SettingsError, .readFailed) }
            try FileManager.default.removeItem(at: url)
            try FileManager.default.createSymbolicLink(at: url, withDestinationURL: url.appendingPathExtension("missing"))
            XCTAssertThrowsError(try store.load()) { XCTAssertEqual($0 as? SettingsError, .readFailed) }
        }
    }

    func testExclusiveOwnershipAndRelease() throws {
        try isolated { url in
            var first: SettingsStore? = try SettingsStore(url: url)
            XCTAssertNotNil(first)
            XCTAssertThrowsError(try SettingsStore(url: url)) { XCTAssertEqual($0 as? SettingsError, .inUse) }
            first = nil
            let second = try SettingsStore(url: url)
            try second.save(SettingsDocument())
            XCTAssertFalse(try second.load().isComplete)
        }
    }

    func testFailedSavePreservesTargetAndCanRetry() throws {
        try isolated { url in
            let store = try SettingsStore(url: url)
            try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
            let marker = url.appendingPathComponent("keep")
            try Data("keep".utf8).write(to: marker)
            var candidate = SettingsDocument()
            candidate.draft.preferences.theme = .dark
            candidate.finish()
            XCTAssertThrowsError(try store.save(candidate)) { XCTAssertEqual($0 as? SettingsError, .saveFailed) }
            XCTAssertEqual(try String(contentsOf: marker, encoding: .utf8), "keep")
            let children = try FileManager.default.contentsOfDirectory(atPath: url.deletingLastPathComponent().path)
            XCTAssertFalse(children.contains { $0.hasPrefix(".desktop-settings-") })
            try FileManager.default.removeItem(at: url)
            try store.save(candidate)
            XCTAssertEqual(try store.load(), candidate)
        }
    }

    func testExplicitLocationAndDedicatedDefault() throws {
        let base = URL(fileURLWithPath: "/tmp/support")
        XCTAssertEqual(try SettingsLocation.resolve(arguments: [], applicationSupport: base).path,
                       "/tmp/support/Polaris/desktop/settings.json")
        XCTAssertEqual(try SettingsLocation.resolve(arguments: ["--settings-path", "/tmp/demo.json"]).path,
                       "/tmp/demo.json")
        for arguments in [["--settings-path"], ["--settings-path", "relative"], ["--unknown"],
                          ["--settings-path", "/tmp/"], ["--settings-path", "/tmp/a", "extra"]] {
            XCTAssertThrowsError(try SettingsLocation.resolve(arguments: arguments))
        }
    }

    func testInvalidPathCannotReplaceSavedDocument() throws {
        try isolated { url in
            let store = try SettingsStore(url: url)
            let original = SettingsDocument()
            try store.save(original)
            var invalid = original
            invalid.draft.preferences.projectPath = "relative/path"
            XCTAssertThrowsError(try store.save(invalid))
            XCTAssertEqual(try store.load(), original)
        }
    }
}
