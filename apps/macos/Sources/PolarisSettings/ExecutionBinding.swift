import Darwin
import Foundation

/// Saved selection only. Neither credentials nor an execution grant.
public struct ExecutionBinding: Codable, Equatable, Sendable {
    public enum Provider: String, Codable, Sendable { case codex, openai, ollama, lmstudio }
    public let provider: Provider
    public let model: String
    public let effort: String?
    public let localEndpoint: String?
    public var storedEffort: String { effort ?? "medium" }
    public init(provider: Provider, model: String, effort: String?, localEndpoint: String? = nil) throws {
        self.provider = provider; self.model = model; self.effort = effort; self.localEndpoint = localEndpoint
        try validate()
    }
    private enum CodingKeys: String, CodingKey { case provider, model, effort, localEndpoint }
    public init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(provider: fields.decode(Provider.self, forKey: .provider),
                      model: fields.decode(String.self, forKey: .model),
                      effort: fields.decodeIfPresent(String.self, forKey: .effort),
                      localEndpoint: fields.decodeIfPresent(String.self, forKey: .localEndpoint))
    }
    public func validate() throws {
        guard !model.isEmpty, model.utf8.count <= 512, !model.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else { throw SettingsError.invalidDocument }
        switch provider {
        case .codex, .openai:
            guard localEndpoint == nil, let effort, ["low", "medium", "high", "xhigh", "max", "ultra"].contains(effort) else { throw SettingsError.invalidDocument }
        case .ollama, .lmstudio:
            guard effort == nil, let endpoint = localEndpoint, endpoint.utf8.count <= 256,
                  !endpoint.contains(where: { $0.isWhitespace }),
                  !endpoint.contains(where: { "@?#%\\".contains($0) }),
                  let parts = URLComponents(string: endpoint), ["http", "https"].contains(parts.scheme),
                  parts.user == nil, parts.password == nil, parts.query == nil, parts.fragment == nil,
                  parts.path == "/", let host = parts.host,
                  parts.port == nil || (1...65535).contains(parts.port!), parts.string == endpoint else { throw SettingsError.invalidDocument }
            let bare = host.hasPrefix("[") && host.hasSuffix("]") ? String(host.dropFirst().dropLast()) : host
            var v4 = in_addr(); var v6 = in6_addr()
            let loopback4 = inet_pton(AF_INET, bare, &v4) == 1 && UInt32(bigEndian: v4.s_addr) >> 24 == 127
            let loopback6 = inet_pton(AF_INET6, bare, &v6) == 1 && withUnsafeBytes(of: v6) { Array($0) == Array(repeating: UInt8(0), count: 15) + [1] }
            guard loopback4 || loopback6 else { throw SettingsError.invalidDocument }
        }
    }
    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.provider == rhs.provider && Data(lhs.model.utf8) == Data(rhs.model.utf8)
          && lhs.effort == rhs.effort && lhs.localEndpoint == rhs.localEndpoint
    }
}

/// Durable before-send identity; an app restart must reconcile this ledger ID.
public struct ConfigurationIntent: Codable, Equatable, Sendable {
    public let requestID: String
    public let clientID: String
    public let projectID: String
    public let sessionID: String
    public let expectedRevision: String
    public let selection: ExecutionBinding
    public let historyMode: HistoryMode
    public init(requestID: String, clientID: String, projectID: String, sessionID: String,
                expectedRevision: UInt64, selection: ExecutionBinding, historyMode: HistoryMode = .legacy) {
        self.requestID = requestID; self.clientID = clientID; self.projectID = projectID
        self.sessionID = sessionID; self.expectedRevision = String(expectedRevision); self.selection = selection; self.historyMode = historyMode
    }
    private enum CodingKeys: String, CodingKey { case requestID, clientID, projectID, sessionID, expectedRevision, selection, historyMode }
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        requestID = try c.decode(String.self, forKey: .requestID); clientID = try c.decode(String.self, forKey: .clientID)
        projectID = try c.decode(String.self, forKey: .projectID); sessionID = try c.decode(String.self, forKey: .sessionID)
        expectedRevision = try c.decode(String.self, forKey: .expectedRevision)
        selection = try c.decode(ExecutionBinding.self, forKey: .selection)
        historyMode = try c.decodeIfPresent(HistoryMode.self, forKey: .historyMode) ?? .legacy
        try validate()
    }
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(requestID, forKey: .requestID); try c.encode(clientID, forKey: .clientID)
        try c.encode(projectID, forKey: .projectID); try c.encode(sessionID, forKey: .sessionID)
        try c.encode(expectedRevision, forKey: .expectedRevision); try c.encode(selection, forKey: .selection)
        if historyMode != .legacy { try c.encode(historyMode, forKey: .historyMode) }
    }
    public func validate() throws {
        try selection.validate()
        guard [requestID, clientID, projectID, sessionID].allSatisfy({ !$0.isEmpty && $0.utf8.count <= 128 }),
              let revision = UInt64(expectedRevision), String(revision) == expectedRevision else { throw SettingsError.invalidDocument }
    }
}
