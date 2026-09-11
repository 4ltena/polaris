import Foundation

public enum ObservedToolSupport: String, Codable, CaseIterable, Sendable {
    case supported, unsupported, unknown
}

/// A durable next-run choice. The service revalidates the model before a run;
/// this value does not grant endpoint access or prove current availability.
public struct LocalRoleBinding: Codable, Equatable, Identifiable, Sendable {
    public var id: String { role }
    public let role: String
    public let selection: ExecutionBinding
    public let observedToolSupport: ObservedToolSupport

    public init(role: String, selection: ExecutionBinding,
                observedToolSupport: ObservedToolSupport) throws {
        self.role = role
        self.selection = selection
        self.observedToolSupport = observedToolSupport
        try validate()
    }

    public func validate() throws {
        guard !role.isEmpty, role.utf8.count <= 128,
              !role.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains),
              selection.provider == .ollama || selection.provider == .lmstudio,
              selection.effort == nil, selection.localEndpoint != nil
        else { throw SettingsError.invalidDocument }
        try selection.validate()
    }
}

/// Durable before-send identity for role CAS reconciliation after an unknown
/// response. Bindings are canonicalized so readback comparison is exact.
public struct RoleConfigurationIntent: Codable, Equatable, Sendable {
    public let requestID: String
    public let clientID: String
    public let projectID: String
    public let sessionID: String
    public let expectedRevision: String
    public let bindings: [LocalRoleBinding]

    public init(requestID: String, clientID: String, projectID: String, sessionID: String,
                expectedRevision: UInt64, bindings: [LocalRoleBinding]) throws {
        self.requestID = requestID
        self.clientID = clientID
        self.projectID = projectID
        self.sessionID = sessionID
        self.expectedRevision = String(expectedRevision)
        self.bindings = bindings.sorted { Data($0.role.utf8).lexicographicallyPrecedes(Data($1.role.utf8)) }
        try validate()
    }

    public func validate() throws {
        guard [requestID, clientID, projectID, sessionID].allSatisfy({
            !$0.isEmpty && $0.utf8.count <= 128
                && !$0.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        }), let revision = UInt64(expectedRevision), String(revision) == expectedRevision,
        bindings.count <= 32 else { throw SettingsError.invalidDocument }
        for binding in bindings { try binding.validate() }
        let roles = bindings.map { Data($0.role.utf8) }
        let sorted = roles.sorted { $0.lexicographicallyPrecedes($1) }
        guard Set(roles).count == roles.count, roles.elementsEqual(sorted) else {
            throw SettingsError.invalidDocument
        }
    }
}
