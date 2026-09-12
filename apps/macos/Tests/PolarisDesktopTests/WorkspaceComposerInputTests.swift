import Foundation
import AppKit
import SwiftUI
import XCTest
@testable import PolarisDesktop

final class WorkspaceComposerInputTests: XCTestCase {
    func testAttachmentPreviewUsesOnlyCallerSuppliedDisplayValues() {
        let attachment = WorkspaceAttachment(url: URL(fileURLWithPath: "/not-imported/private.txt"))
        let preview = WorkspaceComposerAttachmentPreview(attachment: attachment,
                                                         detail: "UTF-8 text · 14 bytes",
                                                         text: "確認済みの本文")

        XCTAssertEqual(preview.id, attachment.id)
        XCTAssertEqual(preview.name, "private.txt")
        XCTAssertEqual(preview.detail, "UTF-8 text · 14 bytes")
        XCTAssertEqual(preview.text, "確認済みの本文")
    }
}

@MainActor
private final class ComposerLayoutFixture: ObservableObject {
    @Published var text = "短い指示"
    @Published var isComposing = false
}

private struct ComposerLayoutFixtureView: View {
    @ObservedObject var fixture: ComposerLayoutFixture
    var body: some View {
        WorkspaceComposerInput(text: $fixture.text, isComposing: $fixture.isComposing,
            attachments: [], models: [WorkspaceFixture.cloud], selectedModel: WorkspaceFixture.cloud,
            selectedPermission: .readOnly, phase: .idle, showsSend: true, canSend: true,
            attachmentPreview: { WorkspaceComposerAttachmentPreview(attachment: $0, detail: "表示例") },
            onSelectAttachments: nil, onRemoveAttachment: { _ in }, onSelectModel: { _ in },
            onSelectPermission: { _ in }, onSend: {}, onVoice: nil, voiceNote: nil,
            deferredVoice: nil, onApplyDeferredVoice: nil, onDiscardDeferredVoice: nil,
            attachmentNotice: nil)
    }
}

@MainActor
final class WorkspaceComposerLayoutTests: XCTestCase {
    private func editor(in view: NSView) -> WorkspaceInputTextView? {
        if let input = view as? WorkspaceInputTextView { return input }
        for child in view.subviews { if let input = editor(in: child) { return input } }
        return nil
    }

    private func clickExpansionControl(_ input: WorkspaceInputTextView, window: NSWindow) throws {
        let scroll = try XCTUnwrap(input.enclosingScrollView)
        let frame = scroll.convert(scroll.bounds, to: nil)
        let location = NSPoint(x: frame.maxX + 14, y: frame.maxY - 14)
        func event(_ type: NSEvent.EventType) throws -> NSEvent {
            try XCTUnwrap(NSEvent.mouseEvent(with: type, location: location,
                modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: window.windowNumber, context: nil, eventNumber: 1,
                clickCount: 1, pressure: 1))
        }
        // AppKit's mouse tracking consumes the release from the application queue.
        NSApp.postEvent(try event(.leftMouseUp), atStart: true)
        window.sendEvent(try event(.leftMouseDown))
        NSApp.discardEvents(matching: .leftMouseUp, before: nil)
    }

    private func waitFor(_ predicate: () -> Bool) async throws {
        for _ in 0..<100 {
            if predicate() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw NSError(domain: "WorkspaceComposerLayoutTests", code: 1,
                      userInfo: [NSLocalizedDescriptionKey: "入力欄の表示が期限内に更新されませんでした"])
    }

    private func makeFixture(text: String, width: CGFloat = 480) async throws
        -> (ComposerLayoutFixture, NSHostingView<ComposerLayoutFixtureView>, NSWindow, WorkspaceInputTextView) {
        _ = NSApplication.shared
        let fixture = ComposerLayoutFixture()
        fixture.text = text
        let host = NSHostingView(rootView: ComposerLayoutFixtureView(fixture: fixture))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: width, height: 360),
                              styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderFront(nil)
        var ready = false
        defer { if !ready { window.close() } }
        try await waitFor {
            host.layoutSubtreeIfNeeded()
            return self.editor(in: host)?.enclosingScrollView?.frame.height ?? 0 > 0
        }
        let input = try XCTUnwrap(editor(in: host))
        ready = true
        return (fixture, host, window, input)
    }

    func testShortInputUsesCompactHeight() async throws {
        let (fixture, _, window, input) = try await makeFixture(text: "短い指示")
        defer { window.close() }
        let scroll = try XCTUnwrap(input.enclosingScrollView)
        XCTAssertEqual(scroll.frame.height, 50, accuracy: 1)
        XCTAssertEqual(input.string, fixture.text)
    }

    func testExpansionAndClearPreserveNativeEditorAndDraft() async throws {
        let text = (1...12).map { "確認する項目 \($0)" }.joined(separator: "\n")
        let (fixture, host, window, input) = try await makeFixture(text: text)
        defer { window.close() }
        try await Task.sleep(for: .milliseconds(100))
        let selection = NSRange(location: 3, length: 2)
        input.setSelectedRange(selection)
        try clickExpansionControl(input, window: window)
        try await waitFor { input.enclosingScrollView?.frame.height == 180 }
        XCTAssertTrue(editor(in: host) === input)
        XCTAssertEqual(input.selectedRange(), selection)
        XCTAssertEqual(input.string, text)
        XCTAssertEqual(fixture.text, text)
        try clickExpansionControl(input, window: window)
        try await waitFor { input.enclosingScrollView?.frame.height == 50 }
        XCTAssertTrue(editor(in: host) === input)
        XCTAssertEqual(input.selectedRange(), selection)
        try clickExpansionControl(input, window: window)
        try await waitFor { input.enclosingScrollView?.frame.height == 180 }
        fixture.text = ""
        try await waitFor { input.string.isEmpty && input.enclosingScrollView?.frame.height == 50 }
        try clickExpansionControl(input, window: window)
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(input.enclosingScrollView?.frame.height, 50)
        XCTAssertTrue(editor(in: host) === input)
    }

    func testWidthChangeDetectsJapaneseWrappingWithoutLosingMarkedText() async throws {
        let text = String(repeating: "日本語の折り返しを確認します。", count: 3)
        let (fixture, host, window, input) = try await makeFixture(text: text, width: 700)
        defer { window.close() }
        try await Task.sleep(for: .milliseconds(30))
        try clickExpansionControl(input, window: window)
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(input.enclosingScrollView?.frame.height, 50)
        window.setContentSize(NSSize(width: 300, height: 360))
        try await waitFor { input.frame.width < 300 }
        try await Task.sleep(for: .milliseconds(30))
        try clickExpansionControl(input, window: window)
        try await waitFor { input.enclosingScrollView?.frame.height == 180 }
        XCTAssertEqual(input.string, text)
        input.setMarkedText("へんかん", selectedRange: NSRange(location: 4, length: 0),
                            replacementRange: NSRange(location: (text as NSString).length, length: 0))
        let marked = input.markedRange()
        let selection = input.selectedRange()
        window.setContentSize(NSSize(width: 700, height: 360))
        try await waitFor { input.frame.width > 500 }
        XCTAssertTrue(editor(in: host) === input)
        XCTAssertTrue(input.hasMarkedText())
        XCTAssertEqual(input.markedRange(), marked)
        XCTAssertEqual(input.selectedRange(), selection)
        XCTAssertTrue(input.string.hasSuffix("へんかん"))
        XCTAssertTrue(fixture.isComposing)
        try await Task.sleep(for: .milliseconds(30))
        try clickExpansionControl(input, window: window)
        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(input.enclosingScrollView?.frame.height, 180)
        XCTAssertEqual(input.markedRange(), marked)
        XCTAssertEqual(input.selectedRange(), selection)
        input.unmarkText()
        XCTAssertFalse(fixture.isComposing)
        try await Task.sleep(for: .milliseconds(30))
        try clickExpansionControl(input, window: window)
        try await waitFor { input.enclosingScrollView?.frame.height == 50 }
    }
}
