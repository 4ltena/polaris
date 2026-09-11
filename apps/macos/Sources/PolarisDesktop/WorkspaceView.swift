import AppKit
import SwiftUI
import PolarisSettings

struct WorkspaceView: View {
    let snapshot: WorkspaceSnapshot
    @ObservedObject var state: WorkspaceState
    let logo: NSImage?
    let release: ReleaseConfiguration?
    let onAction: (WorkspaceAction) -> Void
    let onSend: @MainActor (WorkspaceSubmission) async -> WorkspaceSendResult
    var onVoice: ((String, WorkspaceVoiceAction) -> Void)? = nil

    var nativeSidebar: AnyView? = nil
    var nativeConversation: AnyView? = nil
    var nativeActivity: AnyView? = nil
    var sourceUnavailableReason: String? = nil
    @Environment(\.colorScheme) private var colorScheme

    private var conversation: WorkspaceConversation? {
        snapshot.conversations.first { $0.id == state.selectedConversationID }
    }
    var body: some View {
        VStack(spacing: 0) {
            GeometryReader { geometry in
                let layout = state.layout.fitted(width: geometry.size.width, height: geometry.size.height)
                HStack(spacing: 0) {
                    VStack(alignment: .leading, spacing: 0) {
                        VStack(alignment: .leading, spacing: 0) {
                            if let logo {
                                // 背景を持たない原本SVGをサイドメニューへ重ねる。
                                Image(nsImage: logo).resizable().renderingMode(.template).scaledToFit()
                                    .foregroundStyle(.primary).frame(width: 140, height: 35)
                                    .accessibilityLabel("polaris")
                            } else { Text("polaris").font(.title2) }
                            if let release {
                                Text(release.title).font(.caption2).foregroundStyle(.secondary)
                                    .padding(.horizontal, 10)
                            }
                        }.padding(.horizontal, 10).padding(.top, 14).padding(.bottom, 18)
                        if snapshot.isDemo {
                            Text("画面デモ · 実行・保存・送信なし")
                                .font(.caption).foregroundStyle(.secondary).padding(.horizontal, 14)
                        }
                        Group {
                            if let nativeSidebar { nativeSidebar }
                            else { WorkspaceSidebar(snapshot: snapshot, state: state, onAction: onAction) }
                        }.frame(maxHeight: .infinity)
                        Button("配置を戻す") { state.layout = WorkspaceLayout() }
                            .help("4つの境界を初期配置へ戻す")
                            .padding(.horizontal, 8).padding(.bottom, 10)
                    }.frame(width: layout.projects)
                        .background((colorScheme == .dark ? Color(white: 0.16) : Color(white: 0.98))
                            .ignoresSafeArea(edges: .top))
                    divider("プロジェクト一覧の幅", value: layout.projects, keyPath: \.projects)
                    if let nativeConversation {
                        nativeConversation.frame(width: layout.chat)
                    } else if let conversation {
                        WorkspaceConversationView(conversation: conversation, snapshot: snapshot,
                            state: state, onSend: onSend, onVoice: onVoice)
                            .id(conversation.id).frame(width: layout.chat)
                    } else {
                        VStack(spacing: 12) {
                            Text("会話を選択してください").foregroundStyle(.secondary)
                            Button("新しい会話") { onAction(.newConversation(projectID: nil)) }
                        }.frame(width: layout.chat, height: geometry.size.height)
                    }
                    divider("会話とファイルの幅", value: layout.chat, keyPath: \.chat)
                    VStack(spacing: 0) {
                        let id = conversation?.id ?? ""
                        let activity = snapshot.activity(for: id)
                        WorkspaceFilesView(activity: activity, state: state, treeWidth: layout.tree,
                            onResizeTree: { state.layout.tree = $0 },
                            unavailableReason: sourceUnavailableReason ?? (snapshot.isDemo || !activity.files.isEmpty ? nil : "ファイル・Git は未接続です"))
                            .frame(maxHeight: .infinity)
                        WorkspaceDivider(title: "現在の作業の高さ", value: layout.work, horizontal: true,
                            reversed: true) { state.layout.work = $0 }
                        Group {
                            if let nativeActivity { nativeActivity }
                            else { WorkspaceActivityView(activity: activity, state: state) }
                        }.frame(height: layout.work)
                    }.frame(maxWidth: .infinity)
                }
            }
        }
        .background(Color(nsColor: DesktopTheme.surfaceColor))
        .buttonStyle(WorkspaceSeamlessButtonStyle())
        .menuStyle(.borderlessButton)
        .frame(minWidth: WorkspaceLayout.minimumWidth, minHeight: WorkspaceLayout.minimumHeight)
    }
    private func divider(_ title: String, value: Double, keyPath: WritableKeyPath<WorkspaceLayout, Double>) -> some View {
        WorkspaceDivider(title: title, value: value) { state.layout[keyPath: keyPath] = $0 }
    }
}

// 細い表示線と広い操作領域を分離し、メニューがdragを横取りしないようにする。
struct WorkspaceDivider: View {
    let title: String
    let value: Double
    var horizontal = false
    var reversed = false
    let onChange: (Double) -> Void
    @State private var dragOrigin: Double?
    @FocusState private var isFocused: Bool
    var body: some View {
        Color.clear
            .frame(width: horizontal ? nil : 7, height: horizontal ? 7 : nil)
            .overlay {
                Rectangle().fill(isFocused ? Color.accentColor : Color(nsColor: .separatorColor).opacity(0.45))
                    .frame(width: horizontal ? nil : 0.5, height: horizontal ? 0.5 : nil)
                    .allowsHitTesting(false)
            }
            .contentShape(Rectangle())
            .focusable().focused($isFocused)
            .onMoveCommand { direction in
                let step: Double
                switch direction {
                case .left: step = horizontal ? 0 : -24
                case .right: step = horizontal ? 0 : 24
                case .up: step = horizontal ? -24 : 0
                case .down: step = horizontal ? 24 : 0
                @unknown default: step = 0
                }
                onChange(value + step * (reversed ? -1 : 1))
            }
            .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .global).onChanged { gesture in
                if dragOrigin == nil { dragOrigin = value }
                let delta = horizontal ? gesture.translation.height : gesture.translation.width
                onChange((dragOrigin ?? value) + delta * (reversed ? -1 : 1))
            }.onEnded { _ in dragOrigin = nil })
            .onHover { inside in
                if inside { (horizontal ? NSCursor.resizeUpDown : NSCursor.resizeLeftRight).set() }
                else { NSCursor.arrow.set() }
            }
            .contextMenu {
                Button("広げる") { onChange(value + 24) }
                Button("狭める") { onChange(value - 24) }
            }
            .help("\(title)：ドラッグ、またはメニューから調整")
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(title).accessibilityValue("\(Int(value))ポイント")
            .accessibilityAdjustableAction { direction in
                switch direction {
                case .increment: onChange(value + 24)
                case .decrement: onChange(value - 24)
                @unknown default: break
                }
            }
    }
}

struct WorkspaceSeamlessButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .padding(.horizontal, 8).padding(.vertical, 5)
            .contentShape(RoundedRectangle(cornerRadius: 5))
            .background(configuration.isPressed ? Color.primary.opacity(0.08) : .clear,
                        in: RoundedRectangle(cornerRadius: 5))
            .opacity(isEnabled ? 1 : 0.4)
    }
}

struct WorkspaceRowStyle: ButtonStyle {
    var selected = false
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.padding(.horizontal, 8).padding(.vertical, 7)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(selected || configuration.isPressed ? Color.accentColor.opacity(0.14) : .clear)
            .clipShape(RoundedRectangle(cornerRadius: 5)).contentShape(Rectangle())
    }
}
