import AppKit
import SwiftUI
import XCTest
@testable import PolarisDesktop

@MainActor
private final class TaskStripFixture: ObservableObject {
    @Published var selectedID: String?
}

private struct TaskStripFixtureView: View {
    @ObservedObject var fixture: TaskStripFixture
    let tasks: [WorkspaceTask]
    var body: some View {
        WorkspaceTaskStrip(tasks: tasks, inspectedTaskID: $fixture.selectedID)
            .background(Color(nsColor: .windowBackgroundColor))
    }
}

@MainActor
final class WorkspaceActivityLayoutTests: XCTestCase {
    private func horizontalScroll(in view: NSView) -> NSScrollView? {
        if let scroll = view as? NSScrollView,
           let document = scroll.documentView,
           document.frame.width > scroll.contentSize.width + 100 { return scroll }
        for child in view.subviews {
            if let scroll = horizontalScroll(in: child) { return scroll }
        }
        return nil
    }

    private func capture(_ view: NSView, name: String) throws -> Data {
        view.layoutSubtreeIfNeeded()
        let bitmap = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
        view.cacheDisplay(in: view.bounds, to: bitmap)
        let data = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
        if let path = ProcessInfo.processInfo.environment["POLARIS_UI_PREVIEW_DIRECTORY"] {
            try data.write(to: URL(fileURLWithPath: path).appendingPathComponent(name + ".png"))
        }
        return data
    }

    func testTaskSelectionIsVisibleAndSurvivesScrollingInBothThemes() async throws {
        _ = NSApplication.shared
        for (name, appearance) in [("dark", NSAppearance.Name.darkAqua), ("light", .aqua)] {
            let fixture = TaskStripFixture()
            let host = NSHostingView(rootView: TaskStripFixtureView(fixture: fixture,
                tasks: WorkspaceFixture.activity(id: "selection", count: 200).tasks))
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 80),
                styleMask: [.titled], backing: .buffered, defer: false)
            window.isReleasedWhenClosed = false
            window.appearance = NSAppearance(named: appearance)
            window.contentView = host
            window.orderFront(nil)
            defer { window.close() }
            for _ in 0..<100 {
                host.layoutSubtreeIfNeeded()
                if horizontalScroll(in: host) != nil { break }
                try await Task.sleep(for: .milliseconds(10))
            }
            let scroll = try XCTUnwrap(horizontalScroll(in: host))
            try await Task.sleep(for: .milliseconds(100))
            let before = try capture(host, name: "task-strip-\(name)-unselected")
            fixture.selectedID = "task-0"
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(fixture.selectedID, "task-0")
            let selected = try capture(host, name: "task-strip-\(name)-selected")
            XCTAssertNotEqual(selected, before, "選択したタスクに視覚的な目印が必要です")
            let cg = try XCTUnwrap(CGEvent(scrollWheelEvent2Source: nil, units: .line,
                wheelCount: 2, wheel1: -10_000, wheel2: 0, wheel3: 0))
            cg.flags = .maskShift
            scroll.scrollWheel(with: try XCTUnwrap(NSEvent(cgEvent: cg)))
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertGreaterThan(scroll.contentView.bounds.minX, 100)
            XCTAssertEqual(fixture.selectedID, "task-0")
            scroll.contentView.scroll(to: .zero)
            scroll.reflectScrolledClipView(scroll.contentView)
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(fixture.selectedID, "task-0")
            fixture.selectedID = nil
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertNil(fixture.selectedID)
            XCTAssertEqual(try capture(host, name: "task-strip-\(name)-cleared"), before)
        }
    }

    func testTaskStripSupportsHorizontalAndShiftWheelAtNarrowWidth() async throws {
        _ = NSApplication.shared
        let state = WorkspaceState(conversationID: "scroll-test")
        let host = NSHostingView(rootView: WorkspaceActivityView(
            activity: WorkspaceFixture.activity(id: "scroll-test", count: 200), state: state))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 300),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFront(nil)
        defer { window.close() }
        for _ in 0..<100 {
            host.layoutSubtreeIfNeeded()
            if horizontalScroll(in: host) != nil { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        let scroll = try XCTUnwrap(horizontalScroll(in: host))
        let document = try XCTUnwrap(scroll.documentView)
        XCTAssertGreaterThan(document.frame.width, 2000)
        func wheel(horizontal: Int32, vertical: Int32, shift: Bool) throws {
            let cg = try XCTUnwrap(CGEvent(scrollWheelEvent2Source: nil, units: .line,
                wheelCount: 2, wheel1: vertical, wheel2: horizontal, wheel3: 0))
            cg.flags = shift ? .maskShift : []
            scroll.scrollWheel(with: try XCTUnwrap(NSEvent(cgEvent: cg)))
        }
        let start = scroll.contentView.bounds.minX
        try wheel(horizontal: -10, vertical: 0, shift: false)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertGreaterThan(scroll.contentView.bounds.minX, start)
        let afterHorizontal = scroll.contentView.bounds.minX
        try wheel(horizontal: 0, vertical: -10, shift: true)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertGreaterThan(scroll.contentView.bounds.minX, afterHorizontal)
        try wheel(horizontal: -10_000, vertical: 0, shift: false)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertGreaterThanOrEqual(scroll.contentView.bounds.maxX, document.frame.maxX - 1)
        try wheel(horizontal: 0, vertical: 10_000, shift: true)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(scroll.contentView.bounds.minX, start, accuracy: 1)
    }
}
