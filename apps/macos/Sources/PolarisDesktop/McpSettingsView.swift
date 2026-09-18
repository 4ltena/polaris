import SwiftUI

/// Registered MCP servers for the connected project. No field takes a command,
/// URL or secret; registrations come only from detected sources.
@MainActor
final class McpSettingsModel: ObservableObject {
  struct Candidate: Identifiable, Equatable {
    let id: String
    let name: String
    let description: String?
    let reason: String?
  }
  static let capabilities: [(key: String, title: String)] = [
    ("catalog_metadata", "一覧の取得"), ("resource_read", "リソースの読取り"),
    ("prompt_selection", "プロンプトの選択"), ("tool_call", "ツールの実行"),
  ]

  @Published private(set) var servers: [[String: ServiceValue]] = []
  @Published private(set) var candidates: [Candidate] = []
  @Published private(set) var checks: [String: String] = [:]
  @Published private(set) var references: [String] = []
  @Published private(set) var unchecked: Set<String> = []
  @Published private(set) var pendingRestart = false
  @Published private(set) var notice: String?
  @Published private(set) var working = false
  @Published private(set) var checking: String?
  private var revision: UInt64?
  let service: WorkspaceServiceModel
  let project: String

  init(service: WorkspaceServiceModel) {
    self.service = service
    project = service.store.projectID
  }

  func load() async { await run { try await self.service.mcpSettings(.mcpRegistryRead(session: self.service.store.sessionID)) } }

  func isEnabled(_ server: [String: ServiceValue]) -> Bool {
    guard case .array(let projects) = server["enabled_projects"] else { return false }
    return projects.contains(.string(project))
  }

  func destination(_ server: [String: ServiceValue]) -> String {
    let transport = server["transport"]
    return transport?["url"]?.string ?? transport?["command"]?.string ?? "不明な接続先"
  }

  func setEnabled(_ id: String, _ enabled: Bool) {
    update(id) { server in
      guard case .array(var projects) = server["enabled_projects"] else { return }
      projects.removeAll { $0 == .string(project) }
      if enabled { projects.append(.string(project)) }
      server["enabled_projects"] = .array(projects)
    }
  }

  func setCapability(_ id: String, _ key: String, _ enabled: Bool) {
    update(id) { server in
      guard case .object(var capabilities) = server["capabilities"] else { return }
      capabilities[key] = .bool(enabled)
      server["capabilities"] = .object(capabilities)
    }
  }

  func remove(_ id: String) { servers.removeAll { $0["server_id"]?.string == id } }

  func save() async {
    guard let revision else { return }
    let documents = servers.map(ServiceValue.object)
    await run { try await self.service.mcpSettings(.mcpRegistrySave(session: self.service.store.sessionID,
                                                                    expected: revision, servers: documents)) }
  }

  func check(_ id: String) async {
    guard !working else { return }
    working = true; checking = id; checks[id] = "確認しています。"
    defer { working = false; checking = nil }
    do {
      let result = try await service.mcpSettings(.mcpCatalogCheck(session: service.store.sessionID, server: id))
      guard result["server_id"]?.string == id else { throw ServiceError.correlation }
      let tools = result["tools"]?.decimal ?? 0
      checks[id] = switch result["outcome"]?.string {
      case "checked": "ツール\(tools)件を確認しました。"
      case "partial": "上限のため一部（ツール\(tools)件）だけ確認しました。"
      case "cancelled": "確認を取り消しました。"
      default: "確認できませんでした（\(result["failure"]?.string ?? "不明")）。"
      }
      unchecked.remove(id)
    } catch {
      checks[id] = Self.message(error)
    }
  }

  /// Allowed while the check itself is waiting; the delayed check reply reports the outcome.
  func cancelCheck() async {
    guard let id = checking else { return }
    _ = try? await service.mcpSettings(.mcpCatalogCancel(session: service.store.sessionID, server: id))
  }

  func detect() async {
    guard !working else { return }
    working = true; notice = nil
    defer { working = false }
    do {
      let result = try await service.mcpSettings(.mcpSetupDetect(session: service.store.sessionID))
      guard case .array(let items) = result["candidates"] else { throw ServiceError.schema }
      candidates = items.map { item in
        let status = item["status"]
        return Candidate(id: item["candidate_id"]!.string!, name: item["name"]!.string!,
                         description: status?["description"]?.string,
                         reason: status?["reason"]?.string.map(Self.reason))
      }
      if candidates.isEmpty { notice = "CodexとClaudeの設定にMCPサーバーは見つかりませんでした。" }
    } catch {
      notice = Self.message(error)
    }
  }

  /// The card shown before this button is the approval; the owner saves only
  /// the candidate it detected, bound to the same revisions.
  func register(_ candidate: Candidate) async {
    await run { try await self.service.mcpSettings(.mcpSetupRegister(session: self.service.store.sessionID,
                                                                     candidate: candidate.id)) }
    candidates.removeAll { $0.id == candidate.id }
  }

  private func update(_ id: String, _ change: (inout [String: ServiceValue]) -> Void) {
    guard let index = servers.firstIndex(where: { $0["server_id"]?.string == id }) else { return }
    change(&servers[index])
  }

  private func run(_ request: @escaping () async throws -> ServiceValue) async {
    guard !working else { return }
    working = true; notice = nil
    defer { working = false }
    do {
      let state = try await request()
      guard case .array(let items) = state["servers"] else { throw ServiceError.schema }
      servers = items.compactMap { if case .object(let fields) = $0 { fields } else { nil } }
      revision = state["registry_revision"]?.decimal
      if case .array(let names) = state["secret_references"] { references = names.compactMap(\.string) }
      if case .array(let ids) = state["unchecked_servers"] { unchecked = Set(ids.compactMap(\.string)) }
      pendingRestart = state["registry_revision"] != state["active_registry_revision"]
        || state["secrets_revision"] != state["active_secrets_revision"]
    } catch {
      notice = Self.message(error)
    }
  }

  private static func message(_ error: Error) -> String {
    switch error {
    case WorkspaceServiceModel.Failure.rejected("revision_conflict"):
      "設定が別の操作で更新されました。読み込み直してから変更してください。"
    case WorkspaceServiceModel.Failure.rejected("session_busy"): "実行中は変更できません。実行の終了後に操作してください。"
    case WorkspaceServiceModel.Failure.rejected("invalid_request"): "保存できない内容です。読み込み直してください。"
    case ServiceError.notReady: "接続が準備できていないか、ほかの操作の完了を待っています。"
    default: "操作を完了できませんでした。読み込み直してください。"
    }
  }

  static func reason(_ code: String) -> String {
    switch code {
    case "launcher_not_supported": "起動のたびにパッケージを取得する形式のため取り込めません"
    case "environment_reference": "環境変数の名前だけが書かれていて、値が見つかりません"
    case "credential_in_arguments": "引数やURLに認証情報らしき値があります"
    case "unsupported_transport": "対応していない接続方式です"
    case "unsupported_auth": "対応していない認証方式です"
    case "command_not_found": "コマンドが見つかりません"
    case "unsupported_install_location": "導入場所が共有の場所やhome直下のため取り込めません"
    case "already_registered": "同じ内容で登録済みです"
    default: "設定を読み取れません"
    }
  }
}

struct McpSettingsView: View {
  @StateObject var model: McpSettingsModel
  @Environment(\.dismiss) private var dismiss

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Text("MCPサーバー").font(.title2)
        Spacer()
        Button("読み込み直す") { Task { await model.load() } }.disabled(model.working)
        Button("閉じる") { dismiss() }
      }
      if model.pendingRestart {
        Text("保存した変更は、次に接続し直したときから反映されます。").font(.caption).foregroundStyle(.orange)
      }
      if let notice = model.notice { Text(notice).font(.caption).foregroundStyle(.secondary) }
      List {
        Section("登録済み") {
          if model.servers.isEmpty { Text("登録済みのサーバーはありません。").foregroundStyle(.secondary) }
          ForEach(model.servers.indices, id: \.self) { index in
            serverRow(model.servers[index])
          }
          HStack {
            Spacer()
            Button("変更を保存") { Task { await model.save() } }.disabled(model.working)
          }
        }
        Section("CodexとClaudeの設定から追加") {
          Button("設定を検出") { Task { await model.detect() } }.disabled(model.working)
          ForEach(model.candidates) { candidate in
            VStack(alignment: .leading, spacing: 6) {
              Text(candidate.name).font(.headline)
              if let description = candidate.description {
                Text(description).font(.caption.monospaced()).textSelection(.enabled)
                Button("この内容で登録する") { Task { await model.register(candidate) } }.disabled(model.working)
              } else if let reason = candidate.reason {
                Text("取り込めません：" + reason).font(.caption).foregroundStyle(.secondary)
              }
            }.padding(.vertical, 4)
          }
        }
        if !model.references.isEmpty {
          Section("保存済みの認証情報（値は表示しません）") {
            ForEach(model.references, id: \.self) { Text($0).font(.caption.monospaced()) }
          }
        }
      }
    }
    .padding(16)
    .frame(minWidth: 560, minHeight: 480)
    .task { await model.load() }
    .onDisappear { Task { await model.cancelCheck() } }
  }

  @ViewBuilder private func serverRow(_ server: [String: ServiceValue]) -> some View {
    let id = server["server_id"]?.string ?? ""
    VStack(alignment: .leading, spacing: 6) {
      HStack {
        Text(id).font(.headline)
        if model.unchecked.contains(id) { Text("カタログ未確認").font(.caption).foregroundStyle(.orange) }
        Spacer()
        if model.checking == id {
          Button("確認を取り消す") { Task { await model.cancelCheck() } }
        } else {
          Button("カタログを確認") { Task { await model.check(id) } }.disabled(model.working)
        }
        Button("削除", role: .destructive) { model.remove(id) }.disabled(model.working)
      }
      Text(model.destination(server)).font(.caption.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
      Toggle("このプロジェクトで使う", isOn: Binding(get: { model.isEnabled(server) },
                                               set: { model.setEnabled(id, $0) }))
      HStack {
        ForEach(McpSettingsModel.capabilities, id: \.key) { capability in
          Toggle(capability.title, isOn: Binding(
            get: { server["capabilities"]?[capability.key] == .bool(true) },
            set: { model.setCapability(id, capability.key, $0) })).toggleStyle(.checkbox)
        }
      }
      if let check = model.checks[id] { Text(check).font(.caption).foregroundStyle(.secondary) }
    }.padding(.vertical, 4)
  }
}
