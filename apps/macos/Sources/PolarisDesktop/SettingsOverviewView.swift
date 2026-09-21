import SwiftUI
import PolarisSettings

struct SettingsOverviewView: View {
    @ObservedObject var presentation: SettingsPresentation
    @FocusState private var searchFocused: Bool
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 16) {
                HStack(spacing: 6) {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("設定を検索", text: $presentation.query)
                        .textFieldStyle(.plain).focused($searchFocused)
                        .accessibilityIdentifier("settings-search")
                    if !presentation.query.isEmpty {
                        Button { presentation.query = ""; searchFocused = true } label: {
                            Image(systemName: "xmark.circle.fill")
                        }.buttonStyle(.plain).accessibilityLabel("検索を消去")
                    }
                }.padding(9).background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 8))
                    .padding(.horizontal, 14).padding(.top, 18)
                ScrollView {
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(SettingsCategory.allCases) { category in
                            Button { presentation.select(category) } label: {
                                Label(category.title, systemImage: category.symbol)
                                    .font(.system(size: 13)).lineLimit(2)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .padding(.horizontal, 10).padding(.vertical, 9)
                                    .background(!presentation.isSearching && presentation.category == category
                                        ? Color.primary.opacity(0.07) : .clear, in: RoundedRectangle(cornerRadius: 6))
                                    .contentShape(Rectangle())
                            }.buttonStyle(.plain)
                                .accessibilityAddTraits(!presentation.isSearching && presentation.category == category ? .isSelected : [])
                                .accessibilityIdentifier("settings-category-\(category.id)")
                        }
                    }.padding(.horizontal, 10)
                }
                Text("polaris").font(.headline).foregroundStyle(.secondary).padding(20)
            }.frame(width: 228)
                .background(colorScheme == .dark ? Color(white: 0.16) : Color(white: 0.97))
            Rectangle().fill(Color(nsColor: .separatorColor).opacity(0.45)).frame(width: 0.5)
            VStack(alignment: .leading, spacing: 0) {
                HStack(alignment: .firstTextBaseline) {
                    Text(presentation.isSearching ? "検索結果" : presentation.category.title)
                        .font(.system(size: 24, weight: .semibold)).accessibilityAddTraits(.isHeader)
                    Spacer()
                    if presentation.isSearching {
                        Text("\(presentation.visibleItems.count)件").foregroundStyle(.secondary)
                    }
                }.padding(.horizontal, 34).padding(.top, 30).padding(.bottom, 18)
                if presentation.isPreview {
                    Text("画面プレビュー · 変更は保存されません")
                        .font(.caption).foregroundStyle(.secondary).padding(.horizontal, 34).padding(.bottom, 12)
                }
                if let error = presentation.error {
                    Label(error, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.red).textSelection(.enabled).padding(.horizontal, 34).padding(.bottom, 12)
                }
                ScrollViewReader { proxy in
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            Color.clear.frame(height: 0).id("settings-top")
                            if presentation.visibleItems.isEmpty {
                                VStack(alignment: .leading, spacing: 8) {
                                    Text("一致する設定がありません").font(.headline)
                                    Text("機能名や別のキーワードで検索してください。")
                                        .foregroundStyle(.secondary)
                                }.padding(.vertical, 30)
                            }
                            ForEach(presentation.visibleItems) { item in
                                settingRow(item)
                                if item.id != presentation.visibleItems.last?.id {
                                    Rectangle().fill(Color(nsColor: .separatorColor).opacity(0.45)).frame(height: 0.5)
                                }
                            }
                        }.padding(.horizontal, 34).padding(.bottom, 28)
                    }
                    .onChange(of: presentation.category) { _ in proxy.scrollTo("settings-top") }
                    .onChange(of: presentation.query) { _ in proxy.scrollTo("settings-top") }
                }
                Text("準備中・実装予定の項目は、対応後に操作できるようになります。")
                    .font(.caption).foregroundStyle(.secondary).padding(.horizontal, 34).padding(.vertical, 14)
            }.frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .frame(minWidth: 820, minHeight: 560)
        .buttonStyle(.borderless)
        .background {
            Button("設定を検索") { searchFocused = true }
                .keyboardShortcut("f", modifiers: .command).hidden().accessibilityHidden(true)
        }
    }

    private func settingRow(_ item: SettingsItem) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                Text(item.title).font(.system(size: 14, weight: .medium)).accessibilityAddTraits(.isHeader)
                Spacer(minLength: 8)
                Text(item.delivery.title).font(.caption).foregroundStyle(.secondary)
            }
            if presentation.isSearching {
                Text(item.category.title).font(.caption).foregroundStyle(.secondary)
            }
            Text(item.detail).font(.system(size: 13)).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
            control(item)
        }.frame(maxWidth: .infinity, alignment: .leading).padding(.vertical, 20)
            .accessibilityElement(children: .contain)
            .accessibilityIdentifier("settings-item-\(item.id)")
    }

    @ViewBuilder private func control(_ item: SettingsItem) -> some View {
        switch item.control {
        case .theme:
            Picker("テーマ", selection: Binding(get: { presentation.preferences.theme },
                set: { presentation.perform(.theme($0)) })) {
                ForEach(ThemePreference.allCases, id: \.self) { Text($0.title).tag($0) }
            }.pickerStyle(.segmented).frame(maxWidth: 310).disabled(!presentation.canPerform(.theme))
            unavailable(.theme)
        case .history:
            Picker("会話で参照する過去の範囲", selection: Binding(get: { presentation.preferences.historyMode },
                set: { presentation.perform(.history($0)) })) {
                ForEach(HistoryMode.allCases, id: \.self) { Text($0.title).tag($0) }
            }.labelsHidden().frame(maxWidth: 310, alignment: .leading).disabled(!presentation.canPerform(.history))
            unavailable(.history)
        case .workflow:
            Picker("作業内容に合わせた情報の使い方", selection: Binding(
                get: { presentation.workflowMode ?? "disabled" },
                set: { presentation.perform(.workflow($0)) })) {
                Text("作業内容に合わせて自動調整").tag("adaptive")
                Text("現在の段階に固定").tag("manual")
                Text("段階による調整を使わない").tag("disabled")
            }.labelsHidden().frame(maxWidth: 310, alignment: .leading)
                .disabled(!presentation.canPerform(.workflow))
            unavailable(.workflow)
        case .permission:
            Picker("自動許可の希望", selection: Binding(get: { presentation.preferences.permission },
                set: { presentation.perform(.permission($0)) })) {
                Text("未選択").tag(Optional<PermissionPreference>.none)
                ForEach(PermissionPreference.allCases, id: \.self) { Text($0.title).tag(Optional($0)) }
            }.labelsHidden().disabled(!presentation.canPerform(.permission))
            unavailable(.permission)
        case .model:
            Text(presentation.preferences.executionBinding.map { "\($0.model) · \($0.provider.rawValue) · \($0.storedEffort)" } ?? "モデルは未選択です")
                .font(.callout).textSelection(.enabled)
            action("モデル設定を開く", .model)
        case .localModel:
            action("ローカルモデル設定を開く", .localModel)
        case .project:
            Text(presentation.preferences.projectPath ?? "フォルダは未選択です")
                .font(.callout).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            action("フォルダを選択", .project)
        case .information:
            if item.id == "general.version" {
                Text(presentation.releaseTitle ?? "バージョン情報は未取得です").textSelection(.enabled)
            }
        }
    }

    private func action(_ title: String, _ action: SettingsAction) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Button(title) { presentation.perform(action) }.disabled(!presentation.canPerform(action.kind))
            unavailable(action.kind)
        }
    }
    @ViewBuilder private func unavailable(_ kind: SettingsActionKind) -> some View {
        if !presentation.canPerform(kind) {
            if presentation.isApplying && presentation.availableActions.contains(kind) {
                Text("保存しています…").font(.caption).foregroundStyle(.secondary)
            } else if kind == .history || kind == .workflow {
                Text("作業中の会話を開くと変更できます").font(.caption).foregroundStyle(.secondary)
            } else {
                Text("この画面からの変更は準備中です").font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}
