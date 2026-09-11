import Foundation
import AppKit
import SwiftUI
import XCTest
import PolarisSettings
@testable import PolarisDesktop

final class DesktopModelTests: XCTestCase {
    private actor ConnectionGate {
        var continuation: CheckedContinuation<StartupConnections.Outcome, Never>?
        func wait(started: XCTestExpectation) async -> StartupConnections.Outcome {
            await withCheckedContinuation { continuation in
                self.continuation = continuation
                started.fulfill()
            }
        }
        func release() {
            continuation?.resume(returning: .reachable)
            continuation = nil
        }
    }

    @MainActor
    func testDrawingAndConnectionDeadlineBothGateStartup() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-connection-gate-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        var document = SettingsDocument()
        document.draft.preferences.gpt = .chatGPT
        document.finish()
        try JSONEncoder().encode(document).write(to: url)
        let started = expectation(description: "確認開始")
        let gate = ConnectionGate()
        let checks = StartupConnections(operation: { _ in await gate.wait(started: started) }, deadline: .milliseconds(100))
        let model = DesktopModel(arguments: ["--settings-path", url.path], startupConnections: checks)
        await model.start()
        await fulfillment(of: [started], timeout: 2)
        model.screenPrepared(try XCTUnwrap(model.preparationID), succeeded: true)
        XCTAssertTrue(model.isStarting)
        XCTAssertEqual(model.stage, .connections)
        for _ in 0..<200 where model.isStarting {
            try await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertFalse(model.isStarting)
        XCTAssertTrue(model.connectionSummary.contains("時間切れ"))
        let settled = model.connectionSummary
        await gate.release()
        for _ in 0..<200 where checks.cleanupPending {
            try await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertEqual(model.connectionSummary, settled)
        XCTAssertFalse(checks.cleanupPending)
    }

    @MainActor
    func testDrawFailureCancelsConnectionWithoutRevealingScreen() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-connection-cancel-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        var document = SettingsDocument()
        document.draft.preferences.gpt = .chatGPT
        document.finish()
        try JSONEncoder().encode(document).write(to: url)
        let started = expectation(description: "確認開始")
        let gate = ConnectionGate()
        let checks = StartupConnections(operation: { _ in await gate.wait(started: started) })
        let model = DesktopModel(arguments: ["--settings-path", url.path], startupConnections: checks)
        await model.start()
        await fulfillment(of: [started], timeout: 2)
        model.screenPrepared(try XCTUnwrap(model.preparationID), succeeded: false)
        XCTAssertNotNil(model.startupError)
        XCTAssertTrue(model.isStarting)
        XCTAssertEqual(checks.results.gpt.state, .cancelled)
        await gate.release()
    }

    @MainActor
    func testRetryDuringCleanupDoesNotReuseEarlierSuccess() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-connection-retry-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        var document = SettingsDocument()
        document.draft.preferences.gpt = .chatGPT
        document.draft.preferences.local = .linkInstalled
        document.finish()
        try JSONEncoder().encode(document).write(to: url)
        let started = expectation(description: "local確認開始")
        let gate = ConnectionGate()
        let checks = StartupConnections(operation: { connection in
            if connection == .gpt { return .reachable }
            return await gate.wait(started: started)
        })
        let model = DesktopModel(arguments: ["--settings-path", url.path], startupConnections: checks)
        await model.start()
        await fulfillment(of: [started], timeout: 2)
        for _ in 0..<200 where checks.results.gpt.state == .checking {
            try await Task.sleep(for: .milliseconds(5))
        }
        XCTAssertEqual(checks.results.gpt.state, .reachable)
        model.screenPrepared(try XCTUnwrap(model.preparationID), succeeded: false)
        await model.start()
        model.screenPrepared(try XCTUnwrap(model.preparationID), succeeded: true)
        XCTAssertFalse(model.isStarting)
        XCTAssertTrue(model.connectionSummary.contains("今回の接続状態は未確認"))
        XCTAssertFalse(model.connectionSummary.contains("到達を確認"))
        await gate.release()
    }

    @MainActor
    func testSettingsLoadDoesNotRevealAnUnpreparedScreen() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-readiness-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = DesktopModel(arguments: ["--settings-path", directory.appendingPathComponent("settings.json").path])
        await model.start()
        XCTAssertNil(model.startupError)
        XCTAssertTrue(model.isStarting, "設定の読込だけで未描画の画面へ切り替えてはいけない")
    }

    @MainActor
    func testPreparedScreenDoesNotSetSplashLayoutSize() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-splash-layout-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        var document = SettingsDocument()
        document.draft.preferences.gpt = .chatGPT
        document.finish()
        try JSONEncoder().encode(document).write(to: url)
        let started = expectation(description: "接続確認開始")
        let gate = ConnectionGate()
        let checks = StartupConnections(operation: { _ in await gate.wait(started: started) })
        let model = DesktopModel(arguments: ["--settings-path", url.path], startupConnections: checks)
        await model.start()
        await fulfillment(of: [started], timeout: 2)
        XCTAssertNotNil(model.preparationID)

        let host = NSHostingView(rootView: RootView(model: model))
        host.layoutSubtreeIfNeeded()
        XCTAssertEqual(host.fittingSize, NSSize(width: 668, height: 413))

        model.cancelStartupChecks()
        await gate.release()
    }

    @MainActor
    func testObsoleteDrawCannotCompleteRetryAndDrawFailureStaysOnSplash() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-stale-draw-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = DesktopModel(arguments: ["--settings-path", directory.appendingPathComponent("settings.json").path])
        await model.start()
        let oldID = try XCTUnwrap(model.preparationID)
        model.screenPrepared(oldID, succeeded: false)
        XCTAssertTrue(model.isStarting)
        XCTAssertNotNil(model.startupError)
        await model.start()
        let newID = try XCTUnwrap(model.preparationID)
        model.screenPrepared(oldID, succeeded: true)
        XCTAssertTrue(model.isStarting)
        model.screenPrepared(newID, succeeded: true)
        XCTAssertFalse(model.isStarting)
        XCTAssertNotNil(model.preparationSeconds)
    }

    @MainActor
    func testActualHostDrawsFinalSizeBeforeRevealAndSurvivesReveal() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-host-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let model = DesktopModel(arguments: ["--settings-path", directory.appendingPathComponent("settings.json").path])
        await model.start()
        let id = try XCTUnwrap(model.preparationID)
        let host = PreparedStartupView.PreparedHost(rootView: StartupContentView(model: model, openServiceDemo: {}, openWorkspace: {}))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 960, height: 680), styleMask: [], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        let drawn = expectation(description: "実描画後の通知")
        host.prepare(generation: id) { success in
            XCTAssertTrue(success)
            model.screenPrepared(id, succeeded: success)
            drawn.fulfill()
        }
        await fulfillment(of: [drawn], timeout: 10)
        XCTAssertEqual(host.rasterizedSize, NSSize(width: 960, height: 680))
        XCTAssertFalse(model.isStarting)
        XCTAssertTrue(window.contentView === host)
        host.prepare(generation: id) { _ in XCTFail("同じ世代を再準備しない") }
        await Task.yield()
    }

    @MainActor
    func testDetachedPreparationResumesWhenHostReturns() async throws {
        let model = DesktopModel(arguments: ["--invalid"])
        let host = PreparedStartupView.PreparedHost(rootView: StartupContentView(model: model, openServiceDemo: {}, openWorkspace: {}))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 960, height: 680), styleMask: [], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = host
        let drawn = expectation(description: "再接続後の描画通知")
        host.prepare(generation: UUID()) { success in
            XCTAssertTrue(success)
            drawn.fulfill()
        }
        window.contentView = nil
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async { continuation.resume() }
        }
        XCTAssertNil(host.rasterizedSize)
        window.contentView = host
        await fulfillment(of: [drawn], timeout: 10)
        XCTAssertEqual(host.rasterizedSize, NSSize(width: 960, height: 680))
    }

    @MainActor
    func testViewReattachmentDoesNotRestartStartupButExplicitRetryDoes() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-start-once-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        let model = DesktopModel(arguments: ["--settings-path", url.path])
        await model.startOnce()
        if let id = model.preparationID { model.screenPrepared(id, succeeded: true) }
        XCTAssertFalse(model.isStarting)
        XCTAssertNil(model.startupError)
        try Data("broken".utf8).write(to: url)
        await model.startOnce()
        if let id = model.preparationID { model.screenPrepared(id, succeeded: true) }
        XCTAssertFalse(model.isStarting)
        XCTAssertNil(model.startupError)
        await model.start()
        XCTAssertTrue(model.isStarting)
        XCTAssertNotNil(model.startupError)
    }

    @MainActor
    func testStartupHidesTitleBarThenRestoresFixedSetupWindow() async {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 960, height: 680),
                              styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let view = StartupWindowSize.PhaseView()
        window.contentView = view
        XCTAssertEqual(window.alphaValue, 0)
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async { continuation.resume() }
        }
        // The initial restored frame was hidden until the attached run loop set
        // the splash geometry. The representable must remain attached so later
        // phase changes still reach this same window.
        XCTAssertEqual(window.contentRect(forFrameRect: window.frame).size, NSSize(width: 668, height: 413))
        XCTAssertFalse(window.styleMask.contains(.titled))
        XCTAssertFalse(window.styleMask.contains(.fullSizeContentView))
        XCTAssertFalse(window.titlebarAppearsTransparent)
        XCTAssertEqual(window.alphaValue, 1)
        XCTAssertTrue(view.window === window)
        view.isStarting = false
        view.scheduleSize()
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async { continuation.resume() }
        }
        XCTAssertEqual(window.contentRect(forFrameRect: window.frame).size, NSSize(width: 960, height: 680))
        XCTAssertTrue(window.styleMask.contains(.titled))
        XCTAssertTrue(window.styleMask.contains(.fullSizeContentView))
        XCTAssertFalse(window.styleMask.contains(.resizable))
        XCTAssertTrue(window.titlebarAppearsTransparent)
        XCTAssertEqual(window.backgroundColor, DesktopTheme.surfaceColor)
        window.setContentSize(NSSize(width: 1000, height: 700))
        view.scheduleSize()
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async { continuation.resume() }
        }
        XCTAssertEqual(window.contentRect(forFrameRect: window.frame).size, NSSize(width: 1000, height: 700))
    }

    @MainActor
    func testInitialHiddenFrameRevealsWhenPhaseChangesBeforeQueuedSizing() async {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1280, height: 780),
                              styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let view = StartupWindowSize.PhaseView()
        window.contentView = view
        XCTAssertEqual(window.alphaValue, 0)
        view.isStarting = false
        view.scheduleSize()
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async { continuation.resume() }
        }
        XCTAssertEqual(window.alphaValue, 1)
        XCTAssertEqual(window.contentRect(forFrameRect: window.frame).size, NSSize(width: 960, height: 680))
        XCTAssertTrue(window.styleMask.contains(.titled))
        XCTAssertTrue(window.styleMask.contains(.fullSizeContentView))
    }

    @MainActor
    func testDetachingBeforeInitialSizingRestoresWindowAlpha() {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1280, height: 780),
                              styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let view = StartupWindowSize.PhaseView()
        window.contentView = view
        XCTAssertEqual(window.alphaValue, 0)
        window.contentView = nil
        XCTAssertEqual(window.alphaValue, 1)
    }

    @MainActor
    func testSystemThemeClearsLightAndDarkOverrides() {
        let app = NSApplication.shared
        let original = app.appearance
        defer { app.appearance = original }
        app.appearance = nil
        let system = app.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua])
        for (theme, name) in [(ThemePreference.light, NSAppearance.Name.aqua), (.dark, .darkAqua)] {
            DesktopTheme.apply(theme)
            XCTAssertEqual(app.appearance?.name, name)
            DesktopTheme.apply(.system)
            XCTAssertNil(app.appearance)
            XCTAssertEqual(app.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]), system)
        }
    }

    @MainActor
    func testBundledSplashIsReadyBeforeSettingsIO() {
        let model = DesktopModel(arguments: ["--invalid"])
        XCTAssertNil(model.startupError)
        XCTAssertTrue(model.isStarting)
        XCTAssertEqual(model.release?.version, "0.12.0")
        XCTAssertEqual(model.release?.codename, "Alrescha")
        XCTAssertEqual(model.release?.title, "v0.12.0 · Alrescha")
        XCTAssertEqual(model.assets.count, 3)
        for image in model.assets.values { XCTAssertTrue(image.isValid) }
    }

    @MainActor
    func testReadFailureKeepsArtAndRetryRestoresExactDraft() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-model-test-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        try Data("broken".utf8).write(to: url)
        let model = DesktopModel(arguments: ["--settings-path", url.path])
        await model.start()
        XCTAssertNotNil(model.startupError)
        XCTAssertTrue(model.isStarting)
        XCTAssertEqual(model.assets.count, 3)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), "broken")
        var document = SettingsDocument()
        document.draft.preferences.theme = .dark
        document.finish()
        document.reopen()
        document.draft.preferences.theme = .light
        document.draft.page = .project
        try JSONEncoder().encode(document).write(to: url)
        await model.start()
        XCTAssertNil(model.startupError)
        if let id = model.preparationID { model.screenPrepared(id, succeeded: true) }
        XCTAssertFalse(model.isStarting)
        XCTAssertEqual(model.document, document)
        XCTAssertEqual(model.document.draft.preferences.theme, .light)
        XCTAssertEqual(model.document.preferences?.theme, .dark)
        XCTAssertThrowsError(try SettingsStore(url: url)) { XCTAssertEqual($0 as? SettingsError, .inUse) }
    }

    @MainActor
    func testFailedDraftAndCompletionSaveRemainRetryable() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-model-test-\(UUID())")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let url = directory.appendingPathComponent("settings.json")
        let model = DesktopModel(arguments: ["--settings-path", url.path])
        await model.start()
        XCTAssertNil(model.startupError)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        model.change { $0.preferences.theme = .dark }
        XCTAssertNotNil(model.saveError)
        XCTAssertEqual(model.document.draft.preferences.theme, .dark)
        try FileManager.default.removeItem(at: url)
        model.retrySave()
        XCTAssertNil(model.saveError)
        XCTAssertEqual(try JSONDecoder().decode(SettingsDocument.self, from: Data(contentsOf: url)), model.document)
        try FileManager.default.removeItem(at: url)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        model.finish()
        XCTAssertNotNil(model.saveError)
        XCTAssertFalse(model.document.isComplete)
        XCTAssertTrue(model.document.isEditing)
        try FileManager.default.removeItem(at: url)
        model.retrySave()
        XCTAssertNil(model.saveError)
        XCTAssertTrue(model.document.isComplete)
        XCTAssertFalse(model.document.isEditing)
        XCTAssertNil(model.document.preferences?.projectPath)
    }
}
