import Foundation

enum WorkspaceSourceReplyError: Error, Equatable { case malformed, capacity }

/// Decodes the bounded service projection; the UI has no source filesystem API.
struct WorkspaceSourceReader {
    static let maximumFiles = 256
    static let maximumBodyBytes = 64 * 1024
    static let maximumPayloadBytes = 512 * 1024

    static func jsonPayload(_ value: ServiceValue) throws -> Data {
        // ServiceCodec emits a four-byte transport length before the JSON body.
        Data(try ServiceCodec.encode(value).dropFirst(4))
    }

    struct Reply: Codable, Equatable, Sendable {
        let state: State
        let files: [File]
        let git: Git
        let notices: [String]
    }
    enum State: String, Codable, Sendable { case ready, unavailable }
    struct File: Codable, Equatable, Sendable {
        let id: String; let path: String; let body: String; let unavailableReason: String?
        let unstagedDiff: String?; let stagedDiff: String?
        let modified: Bool; let staged: Bool; let unsaved: Bool
    }
    struct Git: Codable, Equatable, Sendable {
        let state: String; let reason: String?; let branch: String?; let head: String?
        let changeSummary: String?; let revisions: [Revision]
    }
    struct Revision: Codable, Equatable, Sendable {
        let id: String; let parent: String; let title: String; let fileID: String; let body: String; let diff: String
    }

    static func decode(_ data: Data) throws -> Reply {
        guard data.count <= maximumPayloadBytes else { throw WorkspaceSourceReplyError.capacity }
        let reply: Reply
        do { reply = try JSONDecoder().decode(Reply.self, from: data) }
        catch { throw WorkspaceSourceReplyError.malformed }
        guard reply.files.count <= maximumFiles, reply.git.revisions.count <= 64, reply.notices.count <= 64,
              reply.files.allSatisfy(valid), reply.git.revisions.allSatisfy(valid),
              reply.notices.allSatisfy({ $0.lengthOfBytes(using: .utf8) <= 256 }) else { throw WorkspaceSourceReplyError.capacity }
        return reply
    }
    private static func valid(_ file: File) -> Bool {
        validID(file.id) && file.id == file.path && file.body.lengthOfBytes(using: .utf8) <= maximumBodyBytes
            && (file.unstagedDiff?.lengthOfBytes(using: .utf8) ?? 0) <= maximumBodyBytes
            && (file.stagedDiff?.lengthOfBytes(using: .utf8) ?? 0) <= maximumBodyBytes
            && (file.unavailableReason?.lengthOfBytes(using: .utf8) ?? 0) <= 256
    }
    private static func valid(_ revision: Revision) -> Bool {
        validID(revision.id) && validID(revision.fileID)
            && [revision.parent, revision.title, revision.body, revision.diff].allSatisfy { $0.lengthOfBytes(using: .utf8) <= maximumBodyBytes }
    }
    private static func validID(_ value: String) -> Bool {
        !value.isEmpty && !value.hasPrefix("/") && !value.contains("\0")
            && value.split(separator: "/", omittingEmptySubsequences: false).allSatisfy { $0 != "." && $0 != ".." && !$0.isEmpty }
    }
}
