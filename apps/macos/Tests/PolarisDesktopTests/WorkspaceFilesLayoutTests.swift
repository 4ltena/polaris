import AppKit
import SwiftUI
import XCTest
@testable import PolarisDesktop

@MainActor
final class WorkspaceFilesLayoutTests: XCTestCase {
    private func tabScroll(in view: NSView) -> NSScrollView? {
        if let scroll = view as? NSScrollView,
           scroll.frame.height <= 60,
           let document = scroll.documentView,
           document.frame.width > scroll.contentSize.width + 100 { return scroll }
        for child in view.subviews {
            if let scroll = tabScroll(in: child) { return scroll }
        }
        return nil
    }

    private func waitFor(_ predicate: () -> Bool) async throws {
        for _ in 0..<100 {
            if predicate() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("選択したタブが表示範囲へ移動しませんでした")
    }

    private func makeFixture(selectedLast: Bool) async throws -> (WorkspaceState, NSWindow, NSScrollView) {
        _ = NSApplication.shared
        let id = "tabs-test"
        let state = WorkspaceState(conversationID: id)
        let files = (0..<24).map { index in
            WorkspaceFile(id: "file-\(index)", path: "document-\(index).txt", body: "合成データ \(index)",
                unstagedDiff: nil, stagedDiff: nil, modified: false, staged: false,
                unsaved: false, unavailableReason: nil)
        }
        for file in files { state.openFile(file.id, conversationID: id) }
        if !selectedLast { state.openFile("file-0", conversationID: id) }
        let activity = WorkspaceActivity(conversationID: id, planRevision: "fixture", status: "表示例",
            tasks: [], agents: [], files: files, git: nil)
        let host = NSHostingView(rootView: WorkspaceFilesView(activity: activity, state: state,
            treeWidth: 140, onResizeTree: { _ in }))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 560, height: 420),
            styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFront(nil)
        var ready = false
        defer { if !ready { window.close() } }
        try await waitFor {
            host.layoutSubtreeIfNeeded()
            return self.tabScroll(in: host) != nil
        }
        let scroll = try XCTUnwrap(tabScroll(in: host))
        ready = true
        return (state, window, scroll)
    }

    func testOpeningAndClosingOffscreenTabsKeepsSelectionVisible() async throws {
        let (state, window, scroll) = try await makeFixture(selectedLast: false)
        defer { window.close() }
        let document = try XCTUnwrap(scroll.documentView)
        state.openFile("file-23", conversationID: "tabs-test")
        try await waitFor { scroll.contentView.bounds.maxX >= document.frame.maxX - 8 }
        state.openFile("file-0", conversationID: "tabs-test")
        try await waitFor { scroll.contentView.bounds.minX <= 8 }
        state.openFile("file-23", conversationID: "tabs-test")
        try await waitFor { scroll.contentView.bounds.maxX >= document.frame.maxX - 8 }
        state.closeFile("file-23", conversationID: "tabs-test")
        XCTAssertEqual(state.fileSelections["tabs-test"]?.selectedID, "file-22")
        try await waitFor { scroll.contentView.bounds.maxX >= document.frame.maxX - 8 }
        XCTAssertEqual(state.fileSelections["tabs-test"]?.openIDs.count, 23)
    }

    func testRestoredSelectionIsVisibleOnFirstLayout() async throws {
        let (_, window, scroll) = try await makeFixture(selectedLast: true)
        defer { window.close() }
        let document = try XCTUnwrap(scroll.documentView)
        try await waitFor { scroll.contentView.bounds.maxX >= document.frame.maxX - 8 }
    }

    func testReopeningSelectedFileRevealsHiddenTab() async throws {
        let (state, window, scroll) = try await makeFixture(selectedLast: true)
        defer { window.close() }
        let document = try XCTUnwrap(scroll.documentView)
        try await waitFor { scroll.contentView.bounds.maxX >= document.frame.maxX - 8 }
        let wheel = try XCTUnwrap(CGEvent(scrollWheelEvent2Source: nil, units: .line,
            wheelCount: 2, wheel1: 10_000, wheel2: 0, wheel3: 0))
        wheel.flags = .maskShift
        scroll.scrollWheel(with: try XCTUnwrap(NSEvent(cgEvent: wheel)))
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(scroll.contentView.bounds.minX, 0, accuracy: 1)
        state.openFile("file-23", conversationID: "tabs-test")
        try await waitFor { scroll.contentView.bounds.maxX >= document.frame.maxX - 8 }
        XCTAssertEqual(state.fileSelections["tabs-test"]?.openIDs.count, 24)
    }
}
