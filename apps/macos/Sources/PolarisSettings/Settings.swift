import Foundation
import Darwin

public enum GPTPreference: String, Codable, CaseIterable, Sendable {
    case later, chatGPT, apiKey
    public var title: String {
        switch self { case .later: "後で設定"; case .chatGPT: "ChatGPTアカウント"; case .apiKey: "APIキー" }
    }
}
public enum LocalPreference: String, Codable, CaseIterable, Sendable {
    case later, linkInstalled
    public var title: String { self == .later ? "後で設定" : "導入済みモデルを連携" }
}
public enum ThemePreference: String, Codable, CaseIterable, Sendable {
    case system, light, dark
    public var title: String {
        switch self { case .system: "システム"; case .light: "ライト"; case .dark: "ダーク" }
    }
}
/// Request-history construction is an explicit saved preference. It never
/// changes the raw transcript retention policy.
public enum HistoryMode: String, Codable, CaseIterable, Sendable {
    case legacy, strict10
    public var title: String { self == .legacy ? "従来の履歴" : "strict10" }
}
// 希望する段階だけを保存する。実権限・編集可否・OS許可はこの型から導出しない。
public enum PermissionPreference: String, Codable, CaseIterable, Sendable {
    case readOnly, readCreate, readCreateBuild, readCreateBuildExternal
    public var title: String {
        switch self {
        case .readOnly: "閲覧のみ"
        case .readCreate: "閲覧・作成・編集"
        case .readCreateBuild: "閲覧・作成・編集・ビルド"
        case .readCreateBuildExternal: "閲覧・作成・編集・ビルド（外部ツール使用可）"
        }
    }
}
public struct Preferences: Codable, Equatable, Sendable {
    public var gpt: GPTPreference = .later
    public var local: LocalPreference = .later
    public var projectPath: String? = nil
    public var permission: PermissionPreference? = nil
    public var theme: ThemePreference = .system
    public var executionBinding: ExecutionBinding? = nil
    public var historyMode: HistoryMode = .legacy
    public init() {}
    private enum CodingKeys: String, CodingKey { case gpt, local, projectPath, permission, theme, executionBinding, historyMode }
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        gpt = try c.decodeIfPresent(GPTPreference.self, forKey: .gpt) ?? .later
        local = try c.decodeIfPresent(LocalPreference.self, forKey: .local) ?? .later
        projectPath = try c.decodeIfPresent(String.self, forKey: .projectPath)
        permission = try c.decodeIfPresent(PermissionPreference.self, forKey: .permission)
        theme = try c.decodeIfPresent(ThemePreference.self, forKey: .theme) ?? .system
        executionBinding = try c.decodeIfPresent(ExecutionBinding.self, forKey: .executionBinding)
        historyMode = try c.decodeIfPresent(HistoryMode.self, forKey: .historyMode) ?? .legacy
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(gpt, forKey: .gpt); try c.encode(local, forKey: .local)
        try c.encodeIfPresent(projectPath, forKey: .projectPath); try c.encodeIfPresent(permission, forKey: .permission)
        try c.encode(theme, forKey: .theme); try c.encodeIfPresent(executionBinding, forKey: .executionBinding)
        if historyMode != .legacy { try c.encode(historyMode, forKey: .historyMode) }
    }
}
public enum WelcomePage: Int, Codable, CaseIterable, Sendable {
    case welcome, gpt, local, project, theme, review
    public var title: String {
        switch self {
        case .welcome: "ようこそ"; case .gpt: "GPT接続"; case .local: "ローカルモデル"
        case .project: "プロジェクトと権限"; case .theme: "テーマ"; case .review: "確認して開始"
        }
    }
}
public struct WelcomeState: Codable, Equatable, Sendable {
    public var page: WelcomePage = .welcome
    public var preferences = Preferences()
    public init() {}
    public mutating func go(to page: WelcomePage) { self.page = page }
    public mutating func next() { page = WelcomePage(rawValue: page.rawValue + 1) ?? page }
    public mutating func back() { page = WelcomePage(rawValue: page.rawValue - 1) ?? page }
    public mutating func selectFolder(_ url: URL?) {
        guard let url else { return } // NSOpenPanelの取消で既存値を消さない。
        preferences.projectPath = url.path
    }
}
public struct SettingsDocument: Codable, Equatable, Sendable {
    public let schemaVersion: Int
    public var workspace: WorkspaceBinding? = nil
    public var pendingConfiguration: ConfigurationIntent? = nil
    public var pendingRoleConfiguration: RoleConfigurationIntent? = nil
    public var bootstrapProof: PublicationProof? = nil
    public var draft: WelcomeState
    public var preferences: Preferences?
    public var isEditing: Bool
    public var isComplete: Bool { preferences != nil }
    public init() {
        schemaVersion = 2; draft = WelcomeState(); preferences = nil; isEditing = true
    }
    private enum CodingKeys: String, CodingKey { case schemaVersion, draft, preferences, isEditing, workspace, pendingConfiguration, pendingRoleConfiguration, bootstrapProof }
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let version = try c.decode(Int.self, forKey: .schemaVersion)
        guard version == 1 || version == 2 else { throw SettingsError.invalidDocument }
        schemaVersion = 2
        draft = try c.decode(WelcomeState.self, forKey: .draft)
        preferences = try c.decodeIfPresent(Preferences.self, forKey: .preferences)
        isEditing = try c.decode(Bool.self, forKey: .isEditing)
        if c.contains(.workspace) {
            guard version == 2 else { throw SettingsError.invalidDocument }
            workspace = try c.decode(WorkspaceBinding.self, forKey: .workspace)
        }
        bootstrapProof = try c.decodeIfPresent(PublicationProof.self, forKey: .bootstrapProof)
        pendingConfiguration = try c.decodeIfPresent(ConfigurationIntent.self, forKey: .pendingConfiguration)
        pendingRoleConfiguration = try c.decodeIfPresent(RoleConfigurationIntent.self, forKey: .pendingRoleConfiguration)
        try validate()
    }
    public mutating func finish() { preferences = draft.preferences; isEditing = false }
    public mutating func reopen() {
        if let preferences { draft.preferences = preferences }
        draft.page = .welcome; isEditing = true
    }
    public func validate() throws {
        guard schemaVersion == 2, isEditing || isComplete else { throw SettingsError.invalidDocument }
        try workspace?.validate()
        try pendingConfiguration?.validate()
        try pendingRoleConfiguration?.validate()
        if let pending = pendingConfiguration {
            guard let workspace, Data(pending.projectID.utf8) == Data(workspace.projectID.utf8),
                  Data(pending.sessionID.utf8) == Data(workspace.sessionID.utf8),
                  Data(pending.clientID.utf8) == Data(workspace.clientID.utf8) else { throw SettingsError.invalidDocument }
        }
        if let pending = pendingRoleConfiguration {
            guard let workspace, Data(pending.projectID.utf8) == Data(workspace.projectID.utf8),
                  Data(pending.sessionID.utf8) == Data(workspace.sessionID.utf8),
                  Data(pending.clientID.utf8) == Data(workspace.clientID.utf8) else { throw SettingsError.invalidDocument }
        }
        guard workspace == nil || preferences?.projectPath != nil else { throw SettingsError.invalidDocument }
        for value in [draft.preferences, preferences].compactMap({ $0 }) {
            try value.executionBinding?.validate()
            if let path = value.projectPath, !path.hasPrefix("/") || path.contains("\0") {
                throw SettingsError.invalidDocument
            }
        }
    }
}
public enum SettingsError: Error, LocalizedError, Equatable {
    case invalidDocument, invalidArguments, unavailableLocation, readFailed, saveFailed, inUse
    case sourceChanged, metadataInSource
    public var errorDescription: String? {
        switch self {
        case .sourceChanged: "登録したフォルダを確認できません。保存済みの会話を保持しています。フォルダの差替えは行いません。"
        case .metadataInSource: "会話の保存先はプロジェクトの外に置いてください。"
        case .inUse: "この設定は別のpolarisで使用中です。そちらを閉じてから再試行してください。"
        case .invalidDocument: "設定ファイルの形式または版を確認できません。元のファイルを保持しています。"
        case .invalidArguments: "起動引数を確認してください。--settings-path の後に絶対ファイルパスを指定します。"
        case .unavailableLocation: "設定の保存先を取得できません。"
        case .readFailed: "設定を読み込めませんでした。ファイルとアクセス権を確認して再試行してください。"
        case .saveFailed: "設定を保存できませんでした。変更は未保存です。保存先と空き容量を確認して再試行してください。"
        }
    }
}
public enum SettingsLocation {
    public static func resolve(arguments: [String], applicationSupport: URL? = nil) throws -> URL {
        if !arguments.isEmpty {
            guard arguments.count == 2, arguments[0] == "--settings-path",
                  arguments[1].hasPrefix("/"), !arguments[1].hasSuffix("/"),
                  !arguments[1].contains("\0") else { throw SettingsError.invalidArguments }
            return URL(fileURLWithPath: arguments[1])
        }
        guard let base = applicationSupport ?? FileManager.default.urls(for: .applicationSupportDirectory,
                                                                        in: .userDomainMask).first else {
            throw SettingsError.unavailableLocation
        }
        return base.appendingPathComponent("Polaris/desktop/settings.json")
    }
}
public enum StartupStage: String, CaseIterable, Sendable {
    case configuration, settings, interface, connections, ready
    public var title: String {
        switch self {
        case .configuration: "リリース情報を読み込んでいます"
        case .settings: "デスクトップ設定を読み込んでいます"
        case .interface: "画面の素材とレイアウトを準備しています"
        case .connections: "接続状態を確認しています"
        case .ready: "画面の準備ができました"
        }
    }
}
public struct ReleaseConfiguration: Codable, Equatable, Sendable {
    public let version: String
    public let codename: String
    public var title: String { "v\(version) · \(codename)" }
    public static func load(from url: URL) throws -> Self {
        let value = try JSONDecoder().decode(Self.self, from: Data(contentsOf: url))
        guard !value.version.isEmpty, !value.codename.isEmpty else { throw SettingsError.invalidDocument }
        return value
    }
}

/// Native registration only. This is not an execution permission or source grant.
public struct WorkspaceBinding: Codable, Equatable, Sendable {
    public let projectID: String
    public let sessionID: String
    public let clientID: String
    public let sourcePath: String
    public let sourceDevice: String
    public let sourceInode: String
    struct Source { let path: String; let device: String; let inode: String }
    init(source: Source) {
        projectID = UUID().uuidString; sessionID = UUID().uuidString; clientID = UUID().uuidString
        sourcePath = source.path; sourceDevice = source.device; sourceInode = source.inode
    }
    func validate() throws {
        guard [projectID, sessionID, clientID].allSatisfy({ UUID(uuidString: $0) != nil }),
              sourcePath.hasPrefix("/"), !sourcePath.contains("\0"),
              UInt64(sourceDevice) != nil, UInt64(sourceInode) != nil else { throw SettingsError.invalidDocument }
    }
    static func sourceIdentity(path: String) throws -> Source {
        guard path.hasPrefix("/"), !path.contains("\0"), let resolved = realpath(path, nil) else {
            throw SettingsError.sourceChanged
        }
        defer { free(resolved) }
        let canonical = String(cString: resolved)
        let fd = open(canonical, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        guard fd >= 0 else { throw SettingsError.sourceChanged }
        defer { Darwin.close(fd) }
        var info = stat()
        guard fstat(fd, &info) == 0 else { throw SettingsError.sourceChanged }
        return Source(path: canonical, device: String(UInt64(UInt32(bitPattern: info.st_dev))), inode: String(info.st_ino))
    }
}
