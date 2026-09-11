import SwiftUI

struct WorkspaceFilesView: View {
    let activity: WorkspaceActivity
    @ObservedObject var state: WorkspaceState
    let treeWidth: Double
    let onResizeTree: (Double) -> Void
    var unavailableReason: String? = nil
    @FocusState private var fileFocused: Bool
    private var selection: WorkspaceFileSelection { state.fileSelections[activity.conversationID] ?? WorkspaceFileSelection() }
    private var selected: WorkspaceFile? { activity.files.first { $0.id == selection.selectedID } }
    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 0) {
                ScrollView(.horizontal) {
                    HStack(spacing: 2) {
                        ForEach(selection.openIDs, id: \.self) { id in
                            let file = activity.files.first { $0.id == id }
                            HStack(spacing: 2) {
                                Button {
                                    state.openFile(id, conversationID: activity.conversationID)
                                } label: {
                                    HStack(spacing: 4) {
                                        Text(file?.path.components(separatedBy: "/").last ?? "対象なし")
                                        if let file { WorkspaceFileMarks(file: file) }
                                    }
                                }.buttonStyle(WorkspaceRowStyle(selected: selection.selectedID == id))
                                Button { state.closeFile(id, conversationID: activity.conversationID) } label: {
                                    Image(systemName: "xmark").font(.caption)
                                }.buttonStyle(.borderless).padding(.trailing, 6)
                                    .accessibilityLabel("\(file?.path ?? id)のタブを閉じる")
                            }
                        }
                    }.padding(6)
                }.frame(height: 45)
                Divider()
                if let file = selected {
                    fileContent(file)
                } else {
                    VStack(spacing: 8) {
                        Image(systemName: "doc.text").font(.largeTitle).foregroundStyle(.tertiary)
                        Text(unavailableReason ?? (selection.selectedID == nil ? "ツリーからファイルを選択" : "選択したファイルは現在のデータにありません"))
                            .foregroundStyle(.secondary)
                    }.frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }.frame(maxWidth: .infinity)
            WorkspaceDivider(title: "ファイルツリーの幅", value: treeWidth, reversed: true, onChange: onResizeTree)
            VStack(alignment: .leading, spacing: 0) {
                Text("ファイル").font(.headline).padding(12)
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        if activity.files.isEmpty { Text(unavailableReason ?? "ファイルはありません").font(.caption).padding(12).foregroundStyle(.secondary) }
                        OutlineGroup(tree, children: \.children) { node in
                            if let file = node.file {
                                Button {
                                    state.openFile(file.id, conversationID: activity.conversationID)
                                    fileFocused = true
                                } label: {
                                    HStack(spacing: 4) {
                                        Image(systemName: "doc.text")
                                        Text(node.name).lineLimit(1).truncationMode(.middle)
                                        Spacer(minLength: 0)
                                        WorkspaceFileMarks(file: file)
                                    }
                                }.buttonStyle(WorkspaceRowStyle(selected: selection.selectedID == file.id))
                                    .help(file.path)
                            } else { Label(node.name, systemImage: "folder").lineLimit(1) }
                        }
                    }.padding(4)
                }
            }.frame(width: treeWidth)
                .background(Color(nsColor: .underPageBackgroundColor).opacity(0.3))
        }
        .onChange(of: state.fileFocusRequest) { _ in fileFocused = true }
    }
    private var tree: [WorkspaceTreeNode] { WorkspaceTreeNode.make(activity.files) }
    private func fileContent(_ file: WorkspaceFile) -> some View {
        let version = selection.versions[file.id] ?? .current
        let revision = activity.git?.revisions.first { $0.id == selection.revisions[file.id] && $0.fileID == file.id }
        return VStack(alignment: .leading, spacing: 8) {
            Text(file.path).font(.caption).foregroundStyle(.secondary).lineLimit(2).textSelection(.enabled)
            HStack {
                Picker("表示対象", selection: Binding(get: { version }, set: { value in
                    var next = selection; next.versions[file.id] = value
                    state.fileSelections[activity.conversationID] = next
                })) {
                    ForEach(WorkspaceFileVersion.allCases, id: \.self) { item in
                        Text(item.title).tag(item)
                    }
                }.labelsHidden().frame(maxWidth: 270)
                if version != .current {
                    Button("現在へ") {
                        var next = selection; next.versions[file.id] = .current
                        state.fileSelections[activity.conversationID] = next
                    }
                }
            }
            if version == .history {
                Text(revision.map { "\($0.id) 対 親 \($0.parent)" } ?? "右下のGit履歴から版を選択してください")
                    .font(.caption).foregroundStyle(.secondary)
                Toggle("履歴の差分を表示", isOn: Binding(get: { selection.historyDiff.contains(file.id) }, set: { enabled in
                    var next = selection
                    if enabled { next.historyDiff.insert(file.id) } else { next.historyDiff.remove(file.id) }
                    state.fileSelections[activity.conversationID] = next
                })).toggleStyle(.checkbox)
            }
            Divider()
            ScrollView([.horizontal, .vertical]) {
                Text(content(file: file, version: version, revision: revision))
                    .font(.system(.body, design: .monospaced)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .topLeading).padding(6)
            }.focusable().focused($fileFocused).accessibilityLabel("選択ファイルの本文")
        }.padding(12)
    }
    private func content(file: WorkspaceFile, version: WorkspaceFileVersion, revision: WorkspaceRevision?) -> String {
        if let reason = file.unavailableReason { return reason }
        switch version {
        case .current: return file.body
        case .unstaged: return file.unstagedDiff ?? "未ステージの差分はありません"
        case .staged: return file.stagedDiff ?? "ステージ済みの差分はありません"
        case .history:
            guard let revision else { return "履歴の内容は未取得です" }
            return selection.historyDiff.contains(file.id) ? revision.diff : revision.body
        }
    }
}

struct WorkspaceFileMarks: View {
    let file: WorkspaceFile
    var body: some View {
        HStack(spacing: 3) {
            if file.modified { Text("M").foregroundStyle(.orange).accessibilityLabel("作業ツリーに変更あり") }
            if file.staged { Circle().fill(Color.green).frame(width: 5, height: 5).accessibilityLabel("ステージ済み") }
            if file.unsaved { Text("○").accessibilityLabel("未保存の編集あり") }
        }.font(.caption)
    }
}

struct WorkspaceTreeNode: Identifiable {
    let id: String
    let name: String
    let file: WorkspaceFile?
    let children: [WorkspaceTreeNode]?
    static func make(_ files: [WorkspaceFile], prefix: String = "") -> [Self] {
        let direct = files.filter { !$0.path.dropFirst(prefix.count).contains("/") }
        let groups = Dictionary(grouping: files.filter { $0.path.dropFirst(prefix.count).contains("/") }) {
            String($0.path.dropFirst(prefix.count).split(separator: "/", maxSplits: 1)[0])
        }
        return groups.keys.sorted().map { name in
            let path = prefix + name + "/"
            return Self(id: "folder:" + path, name: name, file: nil, children: make(groups[name] ?? [], prefix: path))
        } + direct.sorted { $0.path < $1.path }.map {
            Self(id: $0.id, name: String($0.path.dropFirst(prefix.count)), file: $0, children: nil)
        }
    }
}
