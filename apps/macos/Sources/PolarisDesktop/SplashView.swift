import SwiftUI
import AppKit
import PolarisSettings

struct SplashView: View {
    @ObservedObject var model: DesktopModel
    @State private var presentsStartupError = false
    var body: some View {
        GeometryReader { proxy in
            let width = proxy.size.width
            let height = proxy.size.height
            let edgeInset = width * 0.03142857
            ZStack(alignment: .bottomTrailing) {
                Color.black
                if let stars = model.assets["astra-a-stars-layer"] {
                    Image(nsImage: stars).resizable().scaledToFill()
                        .frame(width: width, height: height, alignment: .top).clipped().accessibilityHidden(true)
                }
                if let orbits = model.assets["astra-a-orbits-layer"] {
                    Image(nsImage: orbits).resizable().scaledToFill()
                        .frame(width: width, height: height, alignment: .top).clipped().accessibilityHidden(true)
                }
                if let logo = model.assets["polaris-banner-white"] {
                    Image(nsImage: logo).resizable().scaledToFit()
                        .frame(width: width * 0.57142857, height: width * 0.14285714)
                        .position(x: width * 0.785714285, y: height * 0.8465)
                        .accessibilityLabel("polaris")
                }
                HStack(alignment: .firstTextBaseline, spacing: 16) {
                    Text(model.startupError == nil ? model.stage.title : "起動処理を停止しました")
                        .font(.system(size: max(10, width * 0.0135)))
                        .foregroundStyle(Color(red: 127 / 255, green: 133 / 255, blue: 139 / 255))
                        .textSelection(.disabled)
                        .lineLimit(1).truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if let release = model.release {
                        Text(release.title).font(.system(size: width * 0.02285714))
                            .foregroundStyle(Color(white: 0.90)).textSelection(.disabled)
                            .fixedSize()
                    }
                }.padding(.horizontal, edgeInset).padding(.bottom, edgeInset)
                if model.startupError == nil {
                    // 読取時間は不明。段階数を架空の完了率に換算しない。
                    ProgressView().progressViewStyle(.linear).tint(Color(red: 0.57, green: 0.71, blue: 0.81))
                        .frame(height: 3).clipped().accessibilityLabel("起動の進捗（所要時間は不明）")
                } else {
                    Color.red.opacity(0.5).frame(height: 3)
                }
            }.foregroundStyle(.white).clipped()
        }
        .onAppear { presentsStartupError = model.startupError != nil }
        .onChange(of: model.startupError) { error in presentsStartupError = error != nil }
        .alert("起動を続けられません", isPresented: $presentsStartupError) {
            Button("再試行") {
                Task {
                    await model.start()
                    // 同じ同期エラーはonChangeへ届かない。alertのdismiss後に再提示する。
                    DispatchQueue.main.async {
                        presentsStartupError = model.startupError != nil
                    }
                }
            }
            Button("閉じる", role: .cancel) { NSApp.terminate(nil) }
        } message: {
            Text(model.startupError ?? "起動処理を停止しました。")
        }
    }
}

/// 起動中はタイトルバーを隠し、セットアップは固定サイズとする。
struct StartupWindowSize: NSViewRepresentable {
    let isStarting: Bool
    var isWorkspace = false
    func makeNSView(context: Context) -> PhaseView { PhaseView() }
    func updateNSView(_ view: PhaseView, context: Context) {
        view.isStarting = isStarting
        view.isWorkspace = isWorkspace
        view.scheduleSize()
    }
    final class PhaseView: NSView, NSWindowDelegate {
        // Objective-C delegate forwarding does not expose the delegate to tasks.
        nonisolated(unsafe) private weak var originalDelegate: (any NSWindowDelegate)?
        var requestTermination: () -> Void = { NSApp.terminate(nil) }
        func windowShouldClose(_ sender: NSWindow) -> Bool {
            confirmClose()
            return false
        }
        override nonisolated func responds(to selector: Selector!) -> Bool {
            if super.responds(to: selector) { return true }
            return MainActor.assumeIsolated { originalDelegate?.responds(to: selector) ?? false }
        }
        override nonisolated func forwardingTarget(for selector: Selector!) -> Any? {
            originalDelegate
        }
        var isStarting = true
        var isWorkspace = false
        private var appliedWorkspace: Bool?
        private var appliedPhase: Bool?
        private weak var appliedWindow: NSWindow?
        private weak var hiddenInitialWindow: NSWindow?
        private var showingCloseConfirmation = false
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            // Changing a window's style mask while its content view is being
            // attached can detach this representable. Hide that one initial
            // frame, then apply the size and style on the attached run loop.
            guard let window else {
                // Do not leave a window invisible if this representable is
                // detached before its queued first layout can run.
                hiddenInitialWindow?.alphaValue = 1
                hiddenInitialWindow = nil
                return
            }
            if appliedWindow !== window, isStarting {
                window.alphaValue = 0
                hiddenInitialWindow = window
            }
            scheduleSize()
        }
        func scheduleSize() {
            // Style and geometry transitions are deferred until AppKit has
            // completed attachment. The initial frame stays invisible above.
            DispatchQueue.main.async { [weak self] in self?.applySize() }
        }
        private func applySize() {
            guard let window,
                  appliedWindow !== window || appliedPhase != isStarting || appliedWorkspace != isWorkspace else { return }
            let revealsInitialWindow = hiddenInitialWindow === window
            if window.delegate !== self {
                originalDelegate = window.delegate
                window.delegate = self
            }
            appliedWindow = window
            appliedPhase = isStarting
            appliedWorkspace = isWorkspace
            var style = window.styleMask
            if isStarting {
                style.subtract([.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView])
            } else {
                // Transparency alone leaves the system title-bar material in
                // place. A full-size content view exposes RootView's shared
                // surface below the transparent title bar.
                style.formUnion([.titled, .closable, .miniaturizable, .fullSizeContentView])
                if isWorkspace { style.insert(.resizable) } else { style.remove(.resizable) }
            }
            if window.styleMask != style { window.styleMask = style }
            // Keep the standard title controls, but let the normal window
            // background continue through the title bar. Startup itself has
            // no title bar, so this only affects the prepared screen.
            window.titlebarAppearsTransparent = !isStarting
            window.backgroundColor = DesktopTheme.surfaceColor
            if !isStarting {
                window.standardWindowButton(.zoomButton)?.isHidden = true
                if let close = window.standardWindowButton(.closeButton) {
                    close.target = self
                    close.action = #selector(self.confirmClose)
                }
            }
            window.collectionBehavior.remove(.fullScreenPrimary)
            window.collectionBehavior.insert(.fullScreenNone)
            window.setContentSize(isStarting
                ? NSSize(width: 668, height: 413)
                : (isWorkspace ? NSSize(width: 1280, height: 780) : NSSize(width: 960, height: 680)))
            // The splash has no user-established position. Apply its final
            // size before centering so it cannot inherit a pre-layout origin.
            if revealsInitialWindow {
                window.center()
                window.alphaValue = 1
                hiddenInitialWindow = nil
            }
        }
        @objc private func confirmClose() {
            guard let window, !showingCloseConfirmation else { return }
            if isWorkspace { requestTermination(); return }
            showingCloseConfirmation = true
            let alert = NSAlert()
            alert.messageText = "セットアップ画面を閉じますか？"
            alert.informativeText = "保存済みの設定は、次回起動時に続きから変更できます。"
            alert.addButton(withTitle: "続ける")
            alert.addButton(withTitle: "閉じる")
            alert.beginSheetModal(for: window) { [weak self] response in
                self?.showingCloseConfirmation = false
                if response == .alertSecondButtonReturn { self?.requestTermination() }
            }
        }
    }
}
