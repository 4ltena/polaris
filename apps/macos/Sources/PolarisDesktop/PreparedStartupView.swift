import AppKit
import SwiftUI

/// 同じhosting viewを準備中から表示後まで保持する。
struct PreparedStartupView: NSViewRepresentable {
    let model: DesktopModel
    let generation: UUID
    let openServiceDemo: () -> Void
    let openWorkspace: () -> Void

    func makeNSView(context: Context) -> PreparedHost {
        PreparedHost(rootView: content)
    }
    func updateNSView(_ view: PreparedHost, context: Context) {
        view.rootView = content
        view.prepare(generation: generation, size: model.preparedSize) { [weak model] success in
            model?.screenPrepared(generation, succeeded: success)
        }
    }
    private var content: StartupContentView {
        StartupContentView(model: model, openServiceDemo: openServiceDemo, openWorkspace: openWorkspace)
    }

    final class PreparedHost: NSHostingView<StartupContentView> {
        private var generation: UUID?
        private var scheduled: UUID?
        private var completed: UUID?
        private var completion: ((Bool) -> Void)?
        private(set) var rasterizedSize: NSSize?

        private var preparationSize = NSSize(width: 960, height: 680)
        func prepare(generation: UUID, size: NSSize = NSSize(width: 960, height: 680), completion: @escaping (Bool) -> Void) {
            self.preparationSize = size
            self.generation = generation
            self.completion = completion
            schedulePreparation()
        }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            schedulePreparation()
        }
        private func schedulePreparation() {
            guard window != nil, let generation, completed != generation, scheduled != generation else { return }
            scheduled = generation
            DispatchQueue.main.async { [weak self] in
                guard let self, self.generation == generation else { return }
                self.scheduled = nil
                guard self.window != nil else { return }
                // 最終サイズでレイアウトし、画像デコードを含む実際の描画経路を通す。
                self.setFrameSize(self.preparationSize)
                self.layoutSubtreeIfNeeded()
                let bitmap = self.bitmapImageRepForCachingDisplay(in: self.bounds)
                if let bitmap {
                    self.cacheDisplay(in: self.bounds, to: bitmap)
                    self.rasterizedSize = self.bounds.size
                }
                self.completed = generation
                // updateから離れたrun loop内で、描画と通知を一緒に完了する。
                self.completion?(bitmap != nil)
            }
        }
    }
}
