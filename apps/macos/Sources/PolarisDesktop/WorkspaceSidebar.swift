import SwiftUI
import AppKit
import PolarisSettings

struct WorkspaceSidebar: View {
    let snapshot: WorkspaceSnapshot
    @ObservedObject var state: WorkspaceState
    let onAction: (WorkspaceAction) -> Void
    @State private var editing: WorkspaceProject?
    @State private var sectionProject: WorkspaceProject?
    @State private var sectionName = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("プロジェクト").font(.headline)
                Spacer()
                Button { editing = WorkspaceProject(id: UUID().uuidString, name: "", source: "",
                    permission: nil, pinned: false, sectionID: nil) } label: {
                    Image(systemName: "folder.badge.plus")
                }.buttonStyle(.borderless).accessibilityLabel("プロジェクトを追加")
            }.padding(.horizontal, 12).padding(.top, 14)
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    group("ピン留め", id: "pinned")
                    ForEach(snapshot.sections) { group($0.name, id: $0.id) }
                    group("プロジェクト", id: "projects")
                    let loose = snapshot.conversations.filter { conversation in
                        !snapshot.projects.contains { $0.id == conversation.projectID }
                    }
                    if !loose.isEmpty {
                        VStack(alignment: .leading, spacing: 3) {
                            Text("未分類の会話").font(.caption).foregroundStyle(.secondary).padding(.horizontal, 8)
                            ForEach(loose) { chat($0) }
                        }
                    }
                }.padding(6)
            }
            Button("新しい会話") { onAction(.newConversation(projectID: nil)) }
                .buttonStyle(WorkspaceRowStyle()).padding(6)
        }
        .sheet(item: $editing) { project in
            WorkspaceProjectEditor(project: project, isDemo: snapshot.isDemo) { candidate in
                onAction(.saveProject(candidate)); editing = nil
            } onCancel: { editing = nil }
        }
        .sheet(item: $sectionProject) { project in
            VStack(alignment: .leading, spacing: 18) {
                Text("セクションを作成").font(.headline)
                TextField("名前", text: $sectionName).onSubmit { createSection(project) }
                HStack {
                    Button("キャンセル") { sectionProject = nil }.keyboardShortcut(.cancelAction)
                    Spacer()
                    Button("作成") { createSection(project) }
                        .disabled(sectionName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        .keyboardShortcut(.defaultAction)
                }
            }.padding(24).frame(width: 340)
        }
    }
    private func createSection(_ project: WorkspaceProject) {
        let name = sectionName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else { return }
        onAction(.createSection(projectID: project.id, name: name)); sectionProject = nil
    }
    private func group(_ name: String, id: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(name).font(.caption).foregroundStyle(.secondary).padding(.horizontal, 8)
            ForEach(snapshot.projects(in: id)) { project in
                VStack(spacing: 2) {
                    HStack(spacing: 2) {
                        Button {
                            if !state.expandedProjects.insert(project.id).inserted { state.expandedProjects.remove(project.id) }
                        } label: {
                            Label(project.name, systemImage: state.expandedProjects.contains(project.id) ? "folder.fill" : "folder")
                                .lineLimit(1).frame(maxWidth: .infinity, alignment: .leading)
                        }.buttonStyle(WorkspaceRowStyle())
                            .accessibilityValue(state.expandedProjects.contains(project.id) ? "展開" : "折りたたみ")
                        Menu { projectMenu(project) } label: { Image(systemName: "ellipsis") }
                            .menuStyle(.borderlessButton).menuIndicator(.hidden).frame(width: 22)
                            .accessibilityLabel("\(project.name)のメニュー")
                        Button { onAction(.newConversation(projectID: project.id)) } label: {
                            Image(systemName: "square.and.pencil")
                        }.buttonStyle(.borderless).frame(width: 22)
                            .accessibilityLabel("\(project.name)で新しい会話")
                    }.contextMenu { projectMenu(project) }
                    if state.expandedProjects.contains(project.id) {
                        Text(project.source).font(.caption2).foregroundStyle(.secondary)
                            .lineLimit(1).truncationMode(.middle).help(project.source)
                            .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                        ForEach(snapshot.conversations.filter { $0.projectID == project.id }) { chat($0).padding(.leading, 10) }
                    }
                }
            }
        }
    }
    private func chat(_ conversation: WorkspaceConversation) -> some View {
        Button {
            state.selectedConversationID = conversation.id
            onAction(.selectConversation(conversation.id))
        } label: { Text(conversation.title).lineLimit(2) }
            .buttonStyle(WorkspaceRowStyle(selected: state.selectedConversationID == conversation.id))
    }
    @ViewBuilder private func projectMenu(_ project: WorkspaceProject) -> some View {
        Button(project.pinned ? "ピン留めを外す" : "ピン留め") { onAction(.pin(projectID: project.id, pinned: !project.pinned)) }
        Button("編集…") { editing = project }
        Menu("セクション") {
            ForEach(snapshot.sections) { section in
                Button(section.name) { onAction(.assignSection(projectID: project.id, sectionID: section.id)) }
            }
            Button("セクションから外す") { onAction(.assignSection(projectID: project.id, sectionID: nil)) }
            Button("新しいセクション…") { sectionName = ""; sectionProject = project }
        }
        Button("フォルダを開く") { onAction(.openFolder(project.id)) }
        Divider()
        Button("プロジェクトを外す") { onAction(.removeProject(project.id)) }
    }
}

private struct WorkspaceProjectEditor: View {
    @State var project: WorkspaceProject
    let isDemo: Bool
    let onSave: (WorkspaceProject) -> Void
    let onCancel: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("プロジェクトの設定").font(.title2)
            if isDemo { Text("デモ内の設定です。実際の権限は変更しません。").foregroundStyle(.secondary) }
            TextField("表示名", text: $project.name)
            HStack {
                TextField("参照元フォルダ", text: $project.source)
                Button("選択…") {
                    let panel = NSOpenPanel()
                    panel.canChooseFiles = false; panel.canChooseDirectories = true
                    panel.canCreateDirectories = false
                    panel.begin { response in
                        if response == .OK, let url = panel.url { project.source = url.path }
                    }
                }
            }
            Picker("自動許可の希望", selection: $project.permission) {
                Text("選択してください").tag(PermissionPreference?.none)
                ForEach(PermissionPreference.allCases, id: \.self) { Text($0.title).tag(Optional($0)) }
            }.pickerStyle(.radioGroup)
            Text("登録解除はファイルや会話を削除しません。").font(.caption).foregroundStyle(.secondary)
            HStack {
                Button("キャンセル", action: onCancel).keyboardShortcut(.cancelAction)
                Spacer()
                Button("保存") { onSave(project) }.keyboardShortcut(.defaultAction)
                    .disabled(project.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                        || project.source.isEmpty || project.permission == nil)
            }
        }.padding(24).frame(width: 500)
    }
}
