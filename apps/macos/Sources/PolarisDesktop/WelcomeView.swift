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
                if model.document.isEditing { HStack {
                    Text("polaris").font(.system(size: 24, weight: .medium))
                    Spacer()
                    Text(model.document.isEditing ? "初回セットアップ" : "保存済みの設定").foregroundStyle(.secondary)
                }.padding(28) }
                if model.document.isEditing {
                    WelcomeView(model: model)
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

struct WelcomeView: View {
    @ObservedObject var model: DesktopModel
    private var page: WelcomePage { model.document.draft.page }
    private var preferences: Preferences { model.document.draft.preferences }

    private func binding<T>(_ key: WritableKeyPath<Preferences, T>) -> Binding<T> {
        Binding(get: { model.document.draft.preferences[keyPath: key] },
                set: { value in model.change { $0.preferences[keyPath: key] = value } })
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .top, spacing: 20) {
                VStack(alignment: .leading, spacing: 8) {
                    ProgressMark(page: page).frame(width: 86, height: 96).padding(.bottom, 18)
                    ForEach(WelcomePage.allCases, id: \.self) { item in
                        Button { model.change { $0.go(to: item) } } label: {
                            Text(item.title).frame(maxWidth: .infinity, alignment: .leading)
                                .padding(.horizontal, 12).padding(.vertical, 10)
                                .background(page == item ? Color.primary.opacity(0.07) : .clear)
                                .clipShape(RoundedRectangle(cornerRadius: 8))
                        }.buttonStyle(.plain)
                            .foregroundStyle(page == item ? .primary : .secondary)
                            .accessibilityAddTraits(page == item ? [.isSelected] : [])
                    }
                    Spacer()
                }.frame(width: 205).padding(.leading, 28)
                ScrollView {
                    VStack(alignment: .leading, spacing: 20) {
                        Text(heading).font(.system(size: 26, weight: .semibold)).accessibilityAddTraits(.isHeader)
                        Text(description).foregroundStyle(.secondary).lineSpacing(6)
                            .fixedSize(horizontal: false, vertical: true)
                        fields
                    }.padding(.trailing, 32).padding(.bottom, 24).frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            Divider()
            HStack {
                Text("\(page.rawValue + 1) / 6").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("戻る") { model.change { $0.back() } }.disabled(page == .welcome)
                Button(page == .review ? "この設定ではじめる" : page == .welcome ? "セットアップを始める" : "次へ") {
                    if page == .review { model.finish() } else { model.change { $0.next() } }
                }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            }.padding(24)
        }
    }

    @ViewBuilder private var fields: some View {
        switch page {
        case .welcome:
            EarthArc().frame(height: 172).accessibilityLabel("地球の真円の輪郭")
            Text("設定はいつでも変更できます。").font(.caption).foregroundStyle(.secondary)
        case .gpt:
            Picker("接続方法の希望", selection: binding(\.gpt)) {
                ForEach([GPTPreference.chatGPT, .apiKey, .later], id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.radioGroup)
            Text("接続は未実施です。ここでは希望だけを保存します。ログインやAPIキーの入力はまだ行いません。")
                .font(.callout).foregroundStyle(.secondary)
            if preferences.gpt != .later {
                TextField("モデル", text: Binding(
                    get: { preferences.executionBinding?.model ?? "gpt-6-astra" },
                    set: { name in model.change { state in
                        state.preferences.executionBinding = try? ExecutionBinding(
                            provider: state.preferences.gpt == .apiKey ? .openai : .codex,
                            model: name, effort: "medium")
                    }}))
                Button("このモデルを使用") {
                    model.change { state in
                        state.preferences.executionBinding = try? ExecutionBinding(
                            provider: state.preferences.gpt == .apiKey ? .openai : .codex,
                            model: state.preferences.executionBinding?.model ?? "gpt-6-astra", effort: "medium")
                    }
                }
                Text("polaris login または保存済みのAPIキーを使用します。接続は主画面で開始します。")
                    .font(.caption).foregroundStyle(.secondary)
            }
        case .local:
            Picker("モデル連携の希望", selection: binding(\.local)) {
                ForEach([LocalPreference.linkInstalled, .later], id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.radioGroup)
            Text("モデルの検出・接続は未実施です。連携の希望だけを保存します。")
                .font(.callout).foregroundStyle(.secondary)
        case .project:
            VStack(alignment: .leading, spacing: 12) {
                Text("プロジェクトのフォルダ").font(.headline)
                Text(preferences.projectPath ?? "未選択（後で設定）")
                    .foregroundStyle(.secondary).lineLimit(nil).textSelection(.enabled)
                HStack {
                    Button("フォルダを選択…") { model.selectFolder() }
                    if preferences.projectPath != nil {
                        Button("選択を解除") { model.change { $0.preferences.projectPath = nil } }
                    }
                }
            }
            Picker("フォルダ内の自動許可（希望）", selection: binding(\.permission)) {
                ForEach(PermissionPreference.allCases, id: \.self) { Text($0.title).tag(Optional($0)) }
            }.pickerStyle(.radioGroup)
            HStack {
                if preferences.permission == nil { Text("未選択").foregroundStyle(.secondary) }
                else { Button("未選択に戻す") { model.change { $0.preferences.permission = nil } } }
            }
            Text("段階の希望のみを保存します。操作権限はまだ適用されません。第4段階のブラウザー拡張連携・ローカルアプリ操作は、接続と対象範囲の確認待ちです。OSの許可は変更しません。")
                .font(.caption).foregroundStyle(.secondary)
        case .theme:
            Picker("テーマ", selection: binding(\.theme)) {
                ForEach(ThemePreference.allCases, id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.radioGroup)
        case .review:
            SettingsSummary(preferences: preferences)
        }
    }

    private var heading: String {
        switch page {
        case .welcome: "polarisへようこそ"
        case .gpt: "使いたいGPTにつなぐ"
        case .local: "ローカルモデルを使う"
        case .project: "作業場所と権限を選ぶ"
        case .theme: "見やすいテーマを選ぶ"
        case .review: "設定を確認しましょう"
        }
    }
    private var description: String {
        switch page {
        case .welcome: "polarisは、企画から実装・レビューまでを支える軽量ハーネスです。\nあなたの作業に合わせて、polarisを設定しましょう。"
        case .gpt: "ChatGPTアカウントかAPIキーの接続方法を選びます。後から設定することもできます。"
        case .local: "このデバイスのモデルとの連携を希望するか選びます。連携は後からでも設定できます。"
        case .project: "作業するフォルダと、自動許可の希望を選びます。"
        case .theme: "システムの設定に合わせるか、ライト・ダークを選べます。"
        case .review: "変更したい項目には、左の一覧から戻れます。完了すると、保存した設定を表示します。"
        }
    }
}

struct SettingsSummary: View {
    let preferences: Preferences
    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 24, verticalSpacing: 20) {
            row("GPT接続", preferences.gpt.title + (preferences.gpt == .later ? "" : "（接続待ち）"))
            row("ローカルモデル", preferences.local.title + (preferences.local == .later ? "" : "（接続待ち）"))
            row("プロジェクト", preferences.projectPath ?? "後で設定")
            row("自動許可の希望", preferences.permission?.title ?? "未選択")
            row("テーマ", preferences.theme.title)
        }
        Text("実際の接続・操作権限はまだ有効になっていません。").font(.caption).foregroundStyle(.secondary)
    }
    private func row(_ title: String, _ value: String) -> some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            Text(value).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
        }
    }
}

// 承認済みSVGの6本と同一座標・曲線。選択ページに対応して色だけ変える。
struct ProgressMark: View {
    let page: WelcomePage
    var body: some View {
        Canvas { context, size in
            let scale = min(size.width / 81.21462043111528, size.height / 90.09184629803187)
            context.scaleBy(x: scale, y: scale)
            context.translateBy(x: -23.39268978444236, y: -18.95407685098407)
            for index in 0..<6 {
                var copy = context
                copy.translateBy(x: 64, y: 64)
                copy.rotate(by: .degrees(Double(index) * 60))
                var path = Path()
                path.move(to: CGPoint(x: -2.5, y: -45)); path.addLine(to: CGPoint(x: 2.5, y: -45))
                path.addQuadCurve(to: CGPoint(x: 4.5, y: -43), control: CGPoint(x: 4.5, y: -45))
                path.addLine(to: CGPoint(x: 4.5, y: -15))
                path.addQuadCurve(to: CGPoint(x: 2.5, y: -13), control: CGPoint(x: 4.5, y: -13))
                path.addLine(to: CGPoint(x: -2.5, y: -13))
                path.addQuadCurve(to: CGPoint(x: -4.5, y: -15), control: CGPoint(x: -4.5, y: -13))
                path.addLine(to: CGPoint(x: -4.5, y: -43))
                path.addQuadCurve(to: CGPoint(x: -2.5, y: -45), control: CGPoint(x: -4.5, y: -45))
                path.closeSubpath()
                copy.fill(path, with: .color(index == page.rawValue ? .accentColor : index < page.rawValue ? .primary : .secondary.opacity(0.25)))
            }
        }.accessibilityHidden(true)
    }
}

struct EarthArc: View {
    private let accent = Color(red: 0.525, green: 0.686, blue: 0.816)
    var body: some View {
        Canvas { context, size in
            let scale = min(size.width / 520, size.height / 172)
            context.translateBy(x: (size.width - 520 * scale) / 2, y: 0)
            context.scaleBy(x: scale, y: scale)
            let earth = Path(ellipseIn: CGRect(x: -190, y: 40, width: 1040, height: 1040))
            context.fill(earth, with: .linearGradient(Gradient(colors: [accent.opacity(0.14), accent.opacity(0)]), startPoint: CGPoint(x: 0, y: 40), endPoint: CGPoint(x: 0, y: 172)))
            let rim = Gradient(stops: [.init(color: accent.opacity(0), location: 0), .init(color: accent.opacity(0.8), location: 0.3), .init(color: accent.opacity(0.38), location: 0.7), .init(color: accent.opacity(0), location: 1)])
            var glow = context
            glow.addFilter(.blur(radius: 1.4)); glow.opacity = 0.5
            glow.stroke(earth, with: .linearGradient(rim, startPoint: CGPoint(x: 0, y: 170), endPoint: CGPoint(x: 520, y: 40)), lineWidth: 4)
            context.stroke(earth, with: .linearGradient(rim, startPoint: CGPoint(x: 0, y: 170), endPoint: CGPoint(x: 520, y: 40)), lineWidth: 0.85)
            context.fill(Path(ellipseIn: CGRect(x: 119, y: 60, width: 12, height: 12)), with: .radialGradient(Gradient(colors: [.primary.opacity(0.8), accent, Color(nsColor: .windowBackgroundColor)]), center: CGPoint(x: 122.36, y: 63), startRadius: 0, endRadius: 9.6))
        }.clipped()
    }
}
