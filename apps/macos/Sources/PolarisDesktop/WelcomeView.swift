import AppKit
import SwiftUI
import PolarisSettings

struct RootView: View {
    @ObservedObject var model: DesktopModel
    var openServiceDemo: () -> Void = {}
    @Environment(\.openWindow) private var openWindow
    var body: some View {
        ZStack {
            if let id = model.preparationID {
                PreparedStartupView(model: model, generation: id,
                                    openServiceDemo: openServiceDemo,
                                    openWorkspace: { openWindow(id: "workspace-demo") })
                    .frame(width: model.isStarting ? model.preparedSize.width : nil,
                           height: model.isStarting ? model.preparedSize.height : nil)
                    // Keep the final screen at its actual size for the draw
                    // readiness check, without letting its hidden 960/1280-point
                    // geometry choose the splash window's initial size.
                    .frame(width: model.isStarting ? 668 : nil,
                           height: model.isStarting ? 413 : nil)
                    // Let the nested host paint sidebar background under the
                    // traffic lights; its own controls still use the safe area.
                    .ignoresSafeArea(edges: model.isStarting ? [] : .top)
                    .opacity(model.isStarting ? 0 : 1)
                    .allowsHitTesting(!model.isStarting)
                    .accessibilityHidden(model.isStarting)
            }
            if model.isStarting { SplashView(model: model) }
        }
        .background(Color(nsColor: DesktopTheme.surfaceColor).ignoresSafeArea())
    }
}

struct StartupContentView: View {
    @ObservedObject var model: DesktopModel
    var openServiceDemo: () -> Void
    var openWorkspace: () -> Void
    var body: some View {
            VStack(spacing: 0) {
                if model.document.isEditing {
                    SetupRequiredView()
                } else {
                    WorkspaceMainView(model: model, owner: model.workspace,
                                      openDemo: openWorkspace, openServiceDemo: openServiceDemo)
                }
                if let error = model.saveError {
                    HStack {
                        Image(systemName: "exclamationmark.triangle")
                        Text(error).fixedSize(horizontal: false, vertical: true)
                        Spacer()
                        Button("保存を再試行") { model.retrySave() }
                    }.padding().background(Color.red.opacity(0.08)).accessibilityElement(children: .combine)
                }
            }.background(Color(nsColor: .windowBackgroundColor))
    }
}
