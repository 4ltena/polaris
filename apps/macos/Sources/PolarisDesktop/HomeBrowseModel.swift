import Foundation
import SwiftUI

protocol HomeBrowseTransport: Sendable {
  func start() async throws -> ServiceHello
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply
  func close() async -> ServiceClient.Exit?
}
extension ServiceClient: HomeBrowseTransport {}

struct HomeEntry: Identifiable, Equatable {
  let name: String
  let path: String
  let kind: String
  let device: UInt64
  let inode: UInt64
  var id: String { path }
}

struct HomeDirectory: Equatable {
  var snapshotID: String?
  var entries: [HomeEntry] = []
  var nextCursor: String?
  var omitted: UInt64 = 0
  var capacity = false
  var loading = false
  var notice: String?
}

enum HomeContent: Equatable {
  case loading
  case ready(String)
  case unavailable(String)
}

/// The home tree from the read-only `--browse-home` helper. It never shares
/// selection, tabs or Git with a project, and saves nothing it shows.
@MainActor
final class HomeBrowseModel: ObservableObject {
  enum Row: Identifiable, Equatable {
    case entry(HomeEntry, depth: Int)
    case more(path: String, depth: Int)
    case notice(path: String, text: String, depth: Int)
    var id: String {
      switch self {
      case .entry(let entry, _): "e:" + entry.path
      case .more(let path, _): "m:" + path
      case .notice(let path, _, _): "n:" + path
      }
    }
  }

  @Published private(set) var directories: [String: HomeDirectory] = [:]
  @Published private(set) var expanded: Set<String> = [""]
  @Published private(set) var selected: HomeEntry?
  @Published private(set) var content: HomeContent?
  @Published private(set) var notice: String?
  private let makeTransport: () throws -> any HomeBrowseTransport
  private var transport: (any HomeBrowseTransport)?
  private var epoch: String?
  private var connection = UUID()
  private var directoryTokens: [String: UUID] = [:]
  private var readToken = UUID()
  private var starting = false

  init(makeTransport: @escaping () throws -> any HomeBrowseTransport) {
    self.makeTransport = makeTransport
  }

  convenience init(helper: @escaping () throws -> URL) {
    self.init(makeTransport: { try ServiceClient(helperURL: helper(), homeBrowse: true) })
  }

  /// Starts once; a failure waits for an explicit reconnect, never a loop.
  func start() async {
    guard transport == nil, !starting else { return }
    starting = true; defer { starting = false }
    let token = UUID()
    connection = token
    do {
      let client = try makeTransport()
      let hello = try await client.start()
      guard connection == token else { _ = await client.close(); return }
      guard hello.capabilities.contains("home_browse") else { _ = await client.close(); throw ServiceError.schema }
      transport = client; epoch = hello.epoch; notice = nil
      await list("", cursor: nil)
    } catch {
      guard connection == token else { return }
      notice = "ホームフォルダを表示できません。再接続してください。"
    }
  }

  func reconnect() async {
    await shutdown()
    directories = [:]; expanded = [""]; selected = nil; content = nil
    await start()
  }

  func shutdown() async {
    connection = UUID()
    if let transport { _ = await transport.close() }
    transport = nil; epoch = nil
  }

  func toggle(_ entry: HomeEntry) async {
    guard entry.kind == "directory" else { return }
    if expanded.remove(entry.path) != nil { return }
    expanded.insert(entry.path)
    if directories[entry.path]?.snapshotID == nil { await list(entry.path, cursor: nil) }
  }

  func loadMore(_ path: String) async {
    guard let cursor = directories[path]?.nextCursor else { return }
    await list(path, cursor: cursor)
  }

  func reload(_ path: String) async { await list(path, cursor: nil) }

  var rows: [Row] {
    var result: [Row] = []
    func walk(_ path: String, depth: Int) {
      guard let directory = directories[path] else { return }
      for entry in directory.entries {
        result.append(.entry(entry, depth: depth))
        if entry.kind == "directory", expanded.contains(entry.path) { walk(entry.path, depth: depth + 1) }
      }
      if let text = directory.notice ?? (directory.capacity ? "項目が多いため一部だけ表示しています。" : nil)
        ?? (directory.omitted > 0 ? "表示できない名前の項目が\(directory.omitted)件あります。" : nil) {
        result.append(.notice(path: path, text: text, depth: depth))
      }
      if directory.nextCursor != nil { result.append(.more(path: path, depth: depth)) }
    }
    walk("", depth: 0)
    return result
  }

  func open(_ entry: HomeEntry) async {
    guard entry.kind == "file", let transport else {
      if entry.kind != "directory" {
        selected = entry; content = .unavailable("通常のファイルではないため表示しません。")
      }
      return
    }
    let token = UUID(), current = connection
    readToken = token; selected = entry; content = .loading
    do {
      let id = UUID().uuidString
      let reply = try await transport.send(requestID: id, request: .browseRead(path: entry.path, device: entry.device, inode: entry.inode))
      guard connection == current, readToken == token else { return }
      let payload = try Self.checked(reply, id: id, method: "browse.read")
      guard payload["browse_epoch"]?.string == epoch, payload["path"]?.string == entry.path,
            payload["device"]?.decimal == entry.device, payload["inode"]?.decimal == entry.inode,
            let body = payload["content"] else { throw ServiceError.correlation }
      if body["state"]?.string == "ready", let text = body["text"]?.string {
        content = .ready(text)
      } else {
        content = .unavailable(Self.readReason(body["reason"]?.string ?? ""))
      }
    } catch {
      guard connection == current, readToken == token else { return }
      content = .unavailable(Self.listNotice(error))
    }
  }

  private func list(_ path: String, cursor: String?) async {
    guard let transport else { return }
    let token = UUID(), current = connection
    directoryTokens[path] = token
    var directory = directories[path] ?? HomeDirectory()
    directory.loading = true; directory.notice = nil
    directories[path] = directory
    do {
      let id = UUID().uuidString
      let reply = try await transport.send(requestID: id, request: .browseList(path: path, cursor: cursor))
      guard connection == current, directoryTokens[path] == token else { return }
      let payload = try Self.checked(reply, id: id, method: "browse.list")
      guard payload["browse_epoch"]?.string == epoch, payload["directory"]?.string == path,
            let snapshot = payload["snapshot_id"]?.string,
            cursor == nil || directory.snapshotID == snapshot,
            case .array(let items) = payload["entries"] else { throw ServiceError.correlation }
      let entries = try items.map { item -> HomeEntry in
        let name = item["name"]!.string!, entryPath = item["path"]!.string!
        guard !name.isEmpty, !name.contains("/"), entryPath == (path.isEmpty ? name : path + "/" + name) else {
          throw ServiceError.correlation
        }
        return HomeEntry(name: name, path: entryPath, kind: item["kind"]!.string!,
                         device: item["device"]!.decimal!, inode: item["inode"]!.decimal!)
      }
      var next = cursor == nil ? HomeDirectory() : directory
      next.loading = false
      next.snapshotID = snapshot
      next.entries += entries
      next.nextCursor = payload["next_cursor"]?.string
      next.omitted = payload["omitted"]!.decimal!
      next.capacity = payload["capacity"] == .bool(true)
      directories[path] = next
    } catch {
      guard connection == current, directoryTokens[path] == token else { return }
      // Earlier pages stay visible but are never extended from another snapshot.
      directory.loading = false; directory.nextCursor = nil; directory.notice = Self.listNotice(error)
      directories[path] = directory
    }
  }

  private static func checked(_ reply: ServiceReply, id: String, method: String) throws -> ServiceValue {
    guard reply.requestID.utf8.elementsEqual(id.utf8) else { throw ServiceError.correlation }
    if let rejection = reply.rejection {
      throw WorkspaceServiceModel.Failure.rejected(rejection["code"]?.string ?? "")
    }
    guard reply.method == method, let payload = reply.payload else { throw ServiceError.correlation }
    try ServiceSchema.result(method).check(payload)
    return payload
  }

  private static func listNotice(_ error: Error) -> String {
    switch error {
    case WorkspaceServiceModel.Failure.rejected("revision_conflict"):
      "内容が更新されたか、ホームフォルダが変わりました。読み込み直してください。"
    case WorkspaceServiceModel.Failure.rejected("not_found"): "フォルダが見つかりません。"
    case WorkspaceServiceModel.Failure.rejected("permission_denied"): "開く権限がないか、保護された場所です。"
    default: "読み込めませんでした。読み込み直してください。"
    }
  }

  private static func readReason(_ code: String) -> String {
    switch code {
    case "deleted": "ファイルが削除されました。"
    case "replaced": "ファイルが置き換えられました。一覧を読み込み直してください。"
    case "revision_conflict": "読み取り中にファイルが変更されました。もう一度開いてください。"
    case "too_large": "64KiBを超えるため表示しません。"
    case "binary": "テキストではないため表示しません。"
    case "permission_denied": "読み取る権限がありません。"
    case "protected": "保護された場所のため表示しません。"
    default: "通常のファイルではないため表示しません。"
    }
  }
}

struct HomeBrowseView: View {
  @ObservedObject var model: HomeBrowseModel
  var body: some View {
    HStack(spacing: 0) {
      VStack(alignment: .leading, spacing: 8) {
        if let selected = model.selected {
          Text("~/" + selected.path).font(.caption).foregroundStyle(.secondary).lineLimit(2).textSelection(.enabled)
          Divider()
          switch model.content {
          case .ready(let text):
            ScrollView([.horizontal, .vertical]) {
              Text(text).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .topLeading).padding(6)
            }.accessibilityLabel("ホームのファイルの本文")
          case .unavailable(let reason):
            Text(reason).foregroundStyle(.secondary)
            if selected.kind == "file" { Button("もう一度開く") { Task { await model.open(selected) } } }
          case .loading, .none:
            Text("読み込んでいます。").foregroundStyle(.secondary)
          }
          Spacer(minLength: 0)
        } else {
          Spacer()
          Text("ツリーからファイルを選択").foregroundStyle(.secondary).frame(maxWidth: .infinity)
          Spacer()
        }
      }.padding(12).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
      Divider()
      VStack(alignment: .leading, spacing: 0) {
        HStack {
          Text("ホーム").font(.headline)
          Spacer()
          Button("読み込み直す") { Task { await model.reconnect() } }.buttonStyle(.borderless)
        }.padding(12)
        if let notice = model.notice {
          Text(notice).font(.caption).foregroundStyle(.secondary).padding(.horizontal, 12)
        }
        ScrollView {
          LazyVStack(alignment: .leading, spacing: 2) {
            ForEach(model.rows) { row in
              switch row {
              case .entry(let entry, let depth):
                Button {
                  Task { entry.kind == "directory" ? await model.toggle(entry) : await model.open(entry) }
                } label: {
                  HStack(spacing: 4) {
                    Image(systemName: icon(entry))
                    Text(entry.name).lineLimit(1).truncationMode(.middle)
                    Spacer(minLength: 0)
                  }.padding(.leading, CGFloat(depth) * 14)
                }.buttonStyle(WorkspaceRowStyle(selected: model.selected == entry)).help("~/" + entry.path)
              case .more(let path, let depth):
                Button("続きを読み込む") { Task { await model.loadMore(path) } }
                  .buttonStyle(.borderless).font(.caption).padding(.leading, CGFloat(depth) * 14 + 8)
              case .notice(let path, let text, let depth):
                HStack {
                  Text(text).font(.caption).foregroundStyle(.secondary)
                  Button("読み込み直す") { Task { await model.reload(path) } }.buttonStyle(.borderless).font(.caption)
                }.padding(.leading, CGFloat(depth) * 14 + 8)
              }
            }
          }.padding(4)
        }
      }.frame(width: 260)
        .background(Color(nsColor: .underPageBackgroundColor).opacity(0.3))
    }
    .task { await model.start() }
  }

  private func icon(_ entry: HomeEntry) -> String {
    switch entry.kind {
    case "directory": model.expanded.contains(entry.path) ? "folder.fill" : "folder"
    case "file": "doc.text"
    case "symlink": "arrow.turn.up.right"
    default: "questionmark.square"
    }
  }
}
