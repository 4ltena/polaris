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
        XCTAssertEqual(WelcomePage.allCases.count, 6)
        var state = WelcomeState()
        XCTAssertNil(state.preferences.permission)
        state.preferences.gpt = .apiKey
        state.preferences.jevEnabled = true
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
        XCTAssertTrue(state.preferences.jevEnabled)
        XCTAssertEqual(state.preferences.permission, .readCreateBuildExternal)
    }

    func testLocalConnectionRoundTripsAndRejectsNonLoopbackEndpoints() throws {
        var document = SettingsDocument()
        document.draft.preferences.local = .linkInstalled
        document.draft.preferences.localConnection = try LocalConnectionPreference(
            runtime: .ollama, endpoint: "http://127.0.0.1:11434/", model: "qwen:small", verified: true)
        let encoded = try JSONEncoder().encode(document)
        XCTAssertEqual(try JSONDecoder().decode(SettingsDocument.self, from: encoded), document)
        XCTAssertThrowsError(try LocalConnectionPreference(
            runtime: .ollama, endpoint: "https://example.com/", model: nil, verified: false))
        XCTAssertThrowsError(try LocalConnectionPreference(
            runtime: .lmstudio, endpoint: "http://localhost:1234/", model: nil, verified: false))
        XCTAssertThrowsError(try LocalConnectionPreference(
            runtime: .llamacpp, endpoint: nil, model: nil, verified: true))
        XCTAssertNoThrow(try LocalConnectionPreference(
            runtime: .llamacpp, endpoint: nil, model: "model.gguf", verified: true))
    }

    func testLegacyPreferencesDecodeWithoutInventingLocalConnection() throws {
        let data = Data(#"{"gpt":"later","local":"later","theme":"system"}"#.utf8)
        let preferences = try JSONDecoder().decode(Preferences.self, from: data)
        XCTAssertNil(preferences.localConnection)
        XCTAssertEqual(preferences.presentationLanguage, .unspecified)
        XCTAssertEqual(preferences.answerFinalizer, AnswerFinalizerPreference())
    }

    func testTypedAnswerFinalizerPreferencesRoundTripWithoutGrantingBenchmarkLock() throws {
        var preferences = Preferences()
        preferences.presentationLanguage = .ja
        preferences.answerFinalizer.mode = .enforce
        preferences.answerFinalizer.judgeProfile = .fulfillmentV1
        preferences.answerFinalizer.translation.activation = .requested
        preferences.answerFinalizer.translation.profile = .catTranslate3_3b

        let restored = try JSONDecoder().decode(
            Preferences.self,
            from: JSONEncoder().encode(preferences)
        )
        XCTAssertEqual(restored, preferences)
        XCTAssertTrue(restored.answerFinalizer.requiresBenchmarkLock)
        XCTAssertEqual(restored.answerFinalizer.translation.profile, .catTranslate3_3b)
    }

    func testAnswerFinalizerPreferencesFailClosedForUnknownOrIncompleteValues() throws {
        for json in [
            #"{"presentationLanguage":"auto"}"#,
            #"{"presentationLanguage":"unspecified"}"#,
            #"{"answerFinalizer":{"mode":"observe"}}"#,
            #"{"answerFinalizer":{"mode":"shadow"}}"#,
            #"{"answerFinalizer":{"translation":{"activation":"requested"}}}"#,
            #"{"answerFinalizer":{"translation":{"profile":"project-model"}}}"#
        ] {
            XCTAssertThrowsError(try JSONDecoder().decode(Preferences.self, from: Data(json.utf8)), json)
        }
    }

    func testPartialLegacyFinalizerObjectKeepsSafeDefaults() throws {
        let preferences = try JSONDecoder().decode(
            Preferences.self,
            from: Data(#"{"answerFinalizer":{}}"#.utf8)
        )
        XCTAssertEqual(preferences.answerFinalizer, AnswerFinalizerPreference())
    }

    func testDraftCompletionAndReopenRoundTrip() throws {
        try isolated { url in
            let store = try SettingsStore(url: url)
            var document = try store.load()
            XCTAssertFalse(document.isComplete)
            XCTAssertTrue(document.isEditing)
            document.draft.preferences.theme = .dark
            document.draft.preferences.gpt = .chatGPT
            document.draft.preferences.jevEnabled = true
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
            XCTAssertEqual(resumed.preferences?.jevEnabled, true)
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

    func testManagedPreferencesWritesExactPrivateTOMLAndRoundTrips() throws {
        try isolated { url in
            let managedURL = url.deletingLastPathComponent().appendingPathComponent("managed-preferences.toml")
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: managedURL.deletingLastPathComponent().path)
            let store = try ManagedPreferencesStore(url: managedURL)
            var finalizer = AnswerFinalizerPreference()
            finalizer.mode = .shadow
            finalizer.judgeProfile = .fulfillmentV1
            finalizer.externalDisclosure = true
            finalizer.translation.activation = .requested
            finalizer.translation.profile = .catTranslate3_3b
            let expected = try ManagedPreferences(language: .ja, finalizer: finalizer)
            try store.save(expected)
            XCTAssertEqual(try String(contentsOf: managedURL, encoding: .utf8), """
            [presentation]
            language = "ja"

            [answer_finalizer]
            mode = "shadow"
            judge_profile = "fulfillment-v1"
            external_disclosure = true

            [answer_finalizer.translation]
            enabled = true
            profile = "cat-translate-3.3b"

            """)
            XCTAssertEqual(try store.load(), expected)
            let attributes = try FileManager.default.attributesOfItem(atPath: managedURL.path)
            XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        }
    }

    func testManagedPreferencesRejectInvalidCombinationsAndPreservesExistingFile() throws {
        try isolated { url in
            let managedURL = url.deletingLastPathComponent().appendingPathComponent("managed-preferences.toml")
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: managedURL.deletingLastPathComponent().path)
            let store = try ManagedPreferencesStore(url: managedURL)
            let original = ManagedPreferences()
            try store.save(original)
            var invalid = AnswerFinalizerPreference()
            invalid.mode = .shadow
            invalid.judgeProfile = .fulfillmentV1
            invalid.translation.activation = .requested
            invalid.translation.profile = .catTranslate3_3b
            XCTAssertThrowsError(try ManagedPreferences(language: .en, finalizer: invalid))
            invalid.mode = .enforce
            XCTAssertThrowsError(try ManagedPreferences(language: .ja, finalizer: invalid))
            invalid.mode = .off
            invalid.judgeProfile = nil
            invalid.externalDisclosure = true
            XCTAssertThrowsError(try ManagedPreferences(language: .ja, finalizer: invalid))
            XCTAssertEqual(try store.load(), original)
            try FileManager.default.removeItem(at: managedURL)
            try FileManager.default.createSymbolicLink(at: managedURL, withDestinationURL: url)
            XCTAssertThrowsError(try store.save(original))
        }
    }

    func testManagedPreferencesOmitsExternalDisclosureUntilShadowConsent() throws {
        let disabled = ManagedPreferencesStore.encode(ManagedPreferences())
        XCTAssertEqual(disabled, "[answer_finalizer]\nmode = \"off\"\n")
        var finalizer = AnswerFinalizerPreference()
        finalizer.mode = .shadow
        finalizer.judgeProfile = .fulfillmentV1
        let shadow = try ManagedPreferences(language: .unspecified, finalizer: finalizer)
        XCTAssertFalse(ManagedPreferencesStore.encode(shadow).contains("external_disclosure"))
        finalizer.externalDisclosure = true
        XCTAssertTrue(ManagedPreferencesStore.encode(try ManagedPreferences(language: .unspecified, finalizer: finalizer))
            .contains("external_disclosure = true"))
    }
}
