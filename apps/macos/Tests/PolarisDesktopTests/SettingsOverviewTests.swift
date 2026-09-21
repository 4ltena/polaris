import AppKit
import SwiftUI
import XCTest
import PolarisSettings
@testable import PolarisDesktop

@MainActor
final class SettingsOverviewTests: XCTestCase {
    func testCatalogCoversCategoriesAndPlannedAppsWithoutDuplicateIDs() {
        XCTAssertEqual(Set(SettingsCatalog.items.map(\.id)).count, SettingsCatalog.items.count)
        for category in SettingsCategory.allCases {
            XCTAssertFalse(SettingsCatalog.matching("", category: category).isEmpty)
        }
        let history = SettingsCatalog.items.first { $0.id == "memory.history" }
        XCTAssertEqual(history?.title, "会話で参照する過去の範囲")
        XCTAssertEqual(history?.delivery, .existing)
        XCTAssertEqual(history?.control, .history)
        let workflow = SettingsCatalog.items.first { $0.id == "workflow.stages" }
        XCTAssertEqual(workflow?.title, "作業内容に合わせた情報の使い方")
        XCTAssertEqual(workflow?.delivery, .existing)
        XCTAssertEqual(workflow?.control, .workflow)
        for name in ["Blender", "DaVinci Resolve", "Unity", "Unreal Engine"] {
            let item = SettingsCatalog.matching(name).first { $0.title == name }
            XCTAssertEqual(item?.delivery, .planned)
            XCTAssertEqual(item?.control, .information)
        }
    }

    func testSearchMatchesJapaneseAliasesWidthAndAllWords() {
        XCTAssertEqual(SettingsCatalog.matching("ｍｃｐ STDIO").map(\.id), ["integrations.stdio"])
        XCTAssertTrue(SettingsCatalog.matching("固有名詞").contains { $0.id == "memory.structured" })
        XCTAssertTrue(SettingsCatalog.matching("翻訳").contains { $0.id == "general.documentation" })
        XCTAssertTrue(SettingsCatalog.matching("存在しない設定zzzz").isEmpty)
    }

    func testSearchAcrossCategoriesAndClearingRestoresSelectedCategory() {
        let model = SettingsPresentation(preferences: Preferences())
        model.select(.appearance)
        model.query = "Unity"
        XCTAssertEqual(model.visibleItems.map(\.id), ["integrations.unity"])
        model.query = ""
        XCTAssertEqual(Set(model.visibleItems.map(\.category)), [.appearance])
        model.query = "MCP"
        model.select(.memory)
        XCTAssertFalse(model.isSearching)
        XCTAssertEqual(Set(model.visibleItems.map(\.category)), [.memory])
    }

    func testUnconnectedActionsDoNothingAndHostAcknowledgesChanges() {
        let disconnected = SettingsPresentation(preferences: Preferences(), availableActions: [.theme])
        XCTAssertFalse(disconnected.canPerform(.theme))
        disconnected.perform(.theme(.dark))
        XCTAssertEqual(disconnected.preferences.theme, .system)
        var requests: [SettingsAction] = []
        let connected = SettingsPresentation(preferences: Preferences(), availableActions: [.theme],
            onAction: { requests.append($0) })
        connected.perform(.project)
        XCTAssertTrue(requests.isEmpty)
        connected.perform(.theme(.dark))
        XCTAssertEqual(requests.count, 1)
        if case .theme(.dark) = requests[0] {} else { XCTFail("選択した値をhostへ渡していません") }
        XCTAssertEqual(connected.preferences.theme, .system, "hostの確定前に保存済みの値を変更しない")
        connected.preferences.theme = .dark
        XCTAssertEqual(connected.preferences.theme, .dark)

        var workflowRequests: [SettingsAction] = []
        let workflow = SettingsPresentation(preferences: Preferences(), workflowMode: "manual",
            availableActions: [.workflow], onAction: { workflowRequests.append($0) })
        workflow.perform(.workflow("adaptive"))
        XCTAssertEqual(workflowRequests.count, 1)
        if case .workflow("adaptive") = workflowRequests[0] {} else {
            XCTFail("選択したworkflowをhostへ渡していません")
        }
    }

    func testWindowReusesIdentitySelectionAndSizeAfterClosing() {
        _ = NSApplication.shared
        let model = SettingsPresentation(preferences: Preferences(), isPreview: true)
        let controller = SettingsWindowController(presentation: model)
        controller.present()
        guard let window = controller.window else { return XCTFail("設定ウィンドウがありません") }
        defer { window.close() }
        XCTAssertEqual(window.contentMinSize, NSSize(width: 820, height: 560))
        XCTAssertTrue(window.styleMask.contains(.resizable))
        model.select(.integrations)
        model.query = "Unity"
        window.setContentSize(NSSize(width: 900, height: 620))
        let frame = window.frame
        window.close()
        controller.present()
        XCTAssertTrue(controller.window === window)
        XCTAssertEqual(window.frame, frame)
        XCTAssertEqual(model.query, "Unity")
        XCTAssertEqual(model.category, .integrations)
    }

    func testNativeSettingsLayoutInBothThemesAndMinimumWidth() async throws {
        _ = NSApplication.shared
        for (name, appearance, width, category) in [
            ("general-dark", NSAppearance.Name.darkAqua, 1040.0, SettingsCategory.general),
            ("integrations-dark", .darkAqua, 1040.0, .integrations),
            ("workflow-dark", .darkAqua, 1040.0, .workflow),
            ("memory-light", .aqua, 820.0, .memory),
        ] {
            let model = SettingsPresentation(preferences: Preferences(), workflowMode: "adaptive",
                releaseTitle: "UI確認用", isPreview: true)
            model.select(category)
            let controller = SettingsWindowController(presentation: model)
            let window = try XCTUnwrap(controller.window)
            window.appearance = NSAppearance(named: appearance)
            window.setContentSize(NSSize(width: width, height: 740))
            controller.present()
            defer { window.close() }
            let content = try XCTUnwrap(window.contentView)
            try await Task.sleep(for: .milliseconds(150))
            content.layoutSubtreeIfNeeded()
            XCTAssertEqual(content.frame.width, width, accuracy: 1)
            let bitmap = try XCTUnwrap(content.bitmapImageRepForCachingDisplay(in: content.bounds))
            content.cacheDisplay(in: content.bounds, to: bitmap)
            let png = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
            if let directory = ProcessInfo.processInfo.environment["POLARIS_SETTINGS_PREVIEW_DIRECTORY"] {
                try png.write(to: URL(fileURLWithPath: directory).appendingPathComponent("settings-\(name).png"))
            }
        }
    }
}
