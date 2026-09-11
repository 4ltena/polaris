import AppKit
import SwiftUI

/// 検証用childを回収するまで、専用ウィンドウの閉鎖を保留する。
@MainActor
final class ServiceDemoWindowController: NSWindowController, NSWindowDelegate {
    let model = ServiceDemoModel()
    private var closing = false

    init() {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 700),
                              styleMask: [.titled, .closable, .miniaturizable, .resizable],
                              backing: .buffered, defer: false)
        super.init(window: window)
        window.isReleasedWhenClosed = false
        window.title = "polaris — ローカル通信の検証"
        window.contentView = NSHostingView(rootView: ServiceDemoView(model: model))
        window.delegate = self
        window.center()
    }

    required init?(coder: NSCoder) { return nil }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        guard model.needsClose else { return true }
        guard !closing else { return false }
        closing = true
        Task {
            let settled = await model.close()
            closing = false
            if settled { sender.close() }
        }
        return false
    }
}
