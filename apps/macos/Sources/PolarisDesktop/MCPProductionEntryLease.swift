import Foundation
import Darwin
import CryptoKit
import MachO

/// Read-only admission before the app constructs a model or starts a service.
/// The shared lock remains alive until the app and its accepted children close.
final class MCPProductionEntryLease {
    enum Failure: Error { case refused }
    private static let gateName = "mcp-entry-gate-v2.json"
    private static let lockName = "mcp-entry-gate-v2.lock"
    private static let manifestName = "mcp-entry-identities-v2.json"
    private let home: String
    private var parent: Int32 = -1
    private var lock: Int32 = -1
    private var parentIdentity: stat?
    private var lockIdentity: stat?
    private var gateBytes: Data?
    private var manifestBytes: Data?
    private let executable: String

    static func enter() throws -> MCPProductionEntryLease {
        let home = try operatingSystemHome()
        var count: UInt32 = 4096
        var bytes = [CChar](repeating: 0, count: Int(count))
        guard _NSGetExecutablePath(&bytes, &count) == 0,
              let path = String(validatingCString: bytes) else { throw Failure.refused }
        return try MCPProductionEntryLease(home: home, executable: path)
    }

    init(home: String, executable: String) throws {
        self.home = home
        self.executable = executable
        let homeFD = try Self.physicalDirectory(home)
        defer { close(homeFD) }
        try Self.ownedDirectory(homeFD)
        parent = openat(homeFD, ".polaris", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        if parent < 0 {
            guard errno == ENOENT else { throw Failure.refused }
            return
        }
        do {
            try Self.ownedDirectory(parent)
            let gate = try Self.readOptional(parent, Self.gateName)
            let manifest = try Self.readOptional(parent, Self.manifestName)
            lock = openat(parent, Self.lockName, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
            if gate == nil && manifest == nil && lock < 0 && errno == ENOENT {
                close(parent); parent = -1
                return
            }
            guard lock >= 0 else { throw Failure.refused }
            let pinnedLock = try Self.privateFile(lock)
            guard flock(lock, LOCK_SH | LOCK_NB) == 0 else { throw Failure.refused }
            gateBytes = try Self.readOptional(parent, Self.gateName)
            manifestBytes = try Self.readOptional(parent, Self.manifestName)
            parentIdentity = try Self.metadata(parent)
            lockIdentity = pinnedLock
            try verify()
        } catch {
            if lock >= 0 { close(lock); lock = -1 }
            if parent >= 0 { close(parent); parent = -1 }
            throw Failure.refused
        }
    }

    deinit {
        if lock >= 0 { close(lock) }
        if parent >= 0 { close(parent) }
    }

    func verify() throws {
        let homeFD = try Self.physicalDirectory(home)
        defer { close(homeFD) }
        try Self.ownedDirectory(homeFD)
        let namedParent = openat(homeFD, ".polaris", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        if parent < 0 {
            if namedParent < 0 {
                guard errno == ENOENT else { throw Failure.refused }
                return
            }
            defer { close(namedParent) }
            for name in [Self.gateName, Self.lockName, Self.manifestName] {
                guard try Self.readOptional(namedParent, name) == nil else { throw Failure.refused }
            }
            return
        }
        guard namedParent >= 0 else { throw Failure.refused }
        defer { close(namedParent) }
        try Self.ownedDirectory(namedParent)
        guard let pinnedParent = parentIdentity, let pinnedLock = lockIdentity,
              Self.sameDirectory(pinnedParent, try Self.metadata(namedParent)),
              Self.sameDirectory(pinnedParent, try Self.metadata(parent)) else { throw Failure.refused }
        let namedLock = openat(namedParent, Self.lockName, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard namedLock >= 0 else { throw Failure.refused }
        defer { close(namedLock) }
        guard Self.sameFile(pinnedLock, try Self.privateFile(namedLock)),
              Self.sameFile(pinnedLock, try Self.privateFile(lock)),
              let gate = gateBytes, let manifest = manifestBytes,
              try Self.readOptional(namedParent, Self.gateName) == gate,
              try Self.readOptional(namedParent, Self.manifestName) == manifest else { throw Failure.refused }
        try Self.validateActive(gate: gate, manifest: manifest, executable: executable)
    }

    private struct Anchor: Decodable {
        let device: UInt64; let inode: UInt64; let uid: UInt32; let mode: UInt32; let links: UInt64
        var canonical: String { "{\"device\":\(device),\"inode\":\(inode),\"uid\":\(uid),\"mode\":\(mode),\"links\":\(links)}" }
        func matches(_ s: stat) -> Bool {
            device == UInt64(UInt32(bitPattern: s.st_dev)) && inode == s.st_ino
                && uid == s.st_uid && mode == UInt32(s.st_mode & 0o7777) && links == s.st_nlink
        }
    }
    private struct Binary: Decodable {
        let path: String; let anchor: Anchor; let sha256: String
        var canonical: String { get throws {
            "{\"path\":\(try quoted(path)),\"anchor\":\(anchor.canonical),\"sha256\":\(try quoted(sha256))}"
        } }
    }
    private struct Entry: Decodable {
        let kind: String; let binary: Binary
        var canonical: String { get throws {
            "{\"kind\":\(try quoted(kind)),\"binary\":\(try binary.canonical)}"
        } }
    }
    private struct Manifest: Decodable {
        let protocol_version: UInt32; let entries: [Entry]; let runtime_copy_source_hashes: [String]
        var canonical: Data { get throws {
            let entriesJSON = try entries.map { try $0.canonical }.joined(separator: ",")
            let sourcesJSON = try runtime_copy_source_hashes.map { try quoted($0) }.joined(separator: ",")
            return Data("{\"protocol_version\":\(protocol_version),\"entries\":[\(entriesJSON)],\"runtime_copy_source_hashes\":[\(sourcesJSON)]}".utf8)
        } }
    }
    private struct Gate: Decodable {
        let state: String; let protocol_version: UInt32; let migration_id: String
        let entry_manifest_hash: String; let active_intent_hash: String?
        var canonical: Data { get throws {
            guard let active_intent_hash else { throw Failure.refused }
            return Data("{\"state\":\(try quoted(state)),\"protocol_version\":\(protocol_version),\"migration_id\":\(try quoted(migration_id)),\"entry_manifest_hash\":\(try quoted(entry_manifest_hash)),\"active_intent_hash\":\(try quoted(active_intent_hash))}".utf8)
        } }
    }

    private static func validateActive(gate: Data, manifest: Data, executable: String) throws {
        let g = try JSONDecoder().decode(Gate.self, from: gate)
        let m = try JSONDecoder().decode(Manifest.self, from: manifest)
        guard g.state == "active_v2", g.protocol_version == 2, digest(g.migration_id),
              digest(g.entry_manifest_hash), g.active_intent_hash.map(digest) == true,
              m.protocol_version == 2, !m.entries.isEmpty, m.entries.count <= 8,
              m.runtime_copy_source_hashes.count <= 2,
              try g.canonical == gate, try m.canonical == manifest,
              hash(manifest) == g.entry_manifest_hash else { throw Failure.refused }
        var paths = Set<String>(); var kinds = Set<String>()
        for entry in m.entries {
            guard ["cli", "desktop_app", "desktop_service", "execution_helper"].contains(entry.kind),
                  paths.insert(entry.binary.path).inserted, kinds.insert(entry.kind).inserted else { throw Failure.refused }
            try verifyBinary(entry.binary)
        }
        var sources = Set<String>()
        for source in m.runtime_copy_source_hashes {
            guard digest(source), sources.insert(source).inserted,
                  m.entries.contains(where: {
                      ["cli", "execution_helper"].contains($0.kind) && $0.binary.sha256 == source
                  }) else { throw Failure.refused }
        }
        guard m.entries.contains(where: { $0.kind == "desktop_app" && $0.binary.path == executable }) else { throw Failure.refused }
    }

    private static func verifyBinary(_ binary: Binary) throws {
        guard digest(binary.sha256) else { throw Failure.refused }
        let fd = try physicalFile(binary.path)
        defer { close(fd) }
        let before = try metadata(fd)
        guard before.st_mode & S_IFMT == S_IFREG, before.st_nlink == 1,
              [UInt32(0), geteuid()].contains(before.st_uid),
              before.st_mode & 0o7022 == 0, before.st_mode & 0o111 != 0,
              before.st_size > 0, before.st_size <= 256 * 1024 * 1024,
              binary.anchor.matches(before) else { throw Failure.refused }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: false)
        var hasher = SHA256(); var total: Int64 = 0
        while let chunk = try handle.read(upToCount: 32768), !chunk.isEmpty {
            total += Int64(chunk.count)
            guard total <= 256 * 1024 * 1024 else { throw Failure.refused }
            hasher.update(data: chunk)
        }
        let named = try physicalFile(binary.path)
        defer { close(named) }
        guard total == before.st_size, sameFile(before, try metadata(fd)),
              sameFile(before, try metadata(named)),
              hasher.finalize().map({ String(format: "%02x", $0) }).joined() == binary.sha256 else { throw Failure.refused }
    }

    private static func quoted(_ value: String) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed, .withoutEscapingSlashes])
        guard let text = String(data: data, encoding: .utf8) else { throw Failure.refused }
        return text
    }
    private static func hash(_ value: Data) -> String {
        SHA256.hash(data: value).map { String(format: "%02x", $0) }.joined()
    }
    private static func digest(_ value: String) -> Bool {
        value.utf8.count == 64 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
    }
    private static func metadata(_ fd: Int32) throws -> stat {
        var info = stat()
        guard fstat(fd, &info) == 0 else { throw Failure.refused }
        return info
    }
    private static func sameDirectory(_ a: stat, _ b: stat) -> Bool {
        a.st_dev == b.st_dev && a.st_ino == b.st_ino && a.st_uid == b.st_uid && a.st_mode == b.st_mode
    }
    private static func sameFile(_ a: stat, _ b: stat) -> Bool {
        sameDirectory(a, b) && a.st_nlink == b.st_nlink && a.st_size == b.st_size
            && a.st_mtimespec.tv_sec == b.st_mtimespec.tv_sec && a.st_mtimespec.tv_nsec == b.st_mtimespec.tv_nsec
            && a.st_ctimespec.tv_sec == b.st_ctimespec.tv_sec && a.st_ctimespec.tv_nsec == b.st_ctimespec.tv_nsec
    }
    private static func ownedDirectory(_ fd: Int32) throws {
        let info = try metadata(fd)
        guard info.st_mode & S_IFMT == S_IFDIR, info.st_uid == geteuid(), info.st_mode & 0o7022 == 0 else { throw Failure.refused }
    }
    private static func privateFile(_ fd: Int32) throws -> stat {
        let info = try metadata(fd)
        guard info.st_mode & S_IFMT == S_IFREG, info.st_uid == geteuid(),
              info.st_mode & 0o7777 == 0o600, info.st_nlink == 1 else { throw Failure.refused }
        return info
    }
    private static func readOptional(_ directory: Int32, _ name: String) throws -> Data? {
        let fd = openat(directory, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        if fd < 0 {
            guard errno == ENOENT else { throw Failure.refused }
            return nil
        }
        defer { close(fd) }
        let before = try privateFile(fd)
        guard before.st_size >= 0, before.st_size <= 64 * 1024 else { throw Failure.refused }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: false)
        var data = Data()
        while let chunk = try handle.read(upToCount: min(32768, 65537 - data.count)), !chunk.isEmpty {
            data.append(chunk)
            guard data.count <= 65536 else { throw Failure.refused }
        }
        let named = openat(directory, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard named >= 0 else { throw Failure.refused }
        defer { close(named) }
        guard data.count == before.st_size, sameFile(before, try privateFile(fd)),
              sameFile(before, try privateFile(named)) else { throw Failure.refused }
        return data
    }
    private static func components(_ path: String) throws -> [Substring] {
        let values = path.dropFirst().split(separator: "/", omittingEmptySubsequences: false)
        guard path.hasPrefix("/"), path.utf8.count <= 4096, !path.contains("\0"),
              !values.isEmpty, values.allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." }) else { throw Failure.refused }
        return values
    }
    private static func physicalDirectory(_ path: String) throws -> Int32 {
        let names = try components(path)
        var fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard fd >= 0 else { throw Failure.refused }
        for name in names {
            let next = openat(fd, String(name), O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
            close(fd)
            guard next >= 0 else { throw Failure.refused }
            fd = next
        }
        return fd
    }
    private static func physicalFile(_ path: String) throws -> Int32 {
        let parts = try components(path)
        let parent = "/" + parts.dropLast().joined(separator: "/")
        let directory = parent == "/" ? open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC) : try physicalDirectory(parent)
        guard directory >= 0 else { throw Failure.refused }
        defer { close(directory) }
        let fd = openat(directory, String(parts.last!), O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
        guard fd >= 0 else { throw Failure.refused }
        return fd
    }
    private static func operatingSystemHome() throws -> String {
        var size = 1024
        while size <= 1024 * 1024 {
            var bytes = [CChar](repeating: 0, count: size)
            var record = passwd(); var result: UnsafeMutablePointer<passwd>?
            let status = getpwuid_r(geteuid(), &record, &bytes, bytes.count, &result)
            if status == ERANGE { size *= 2; continue }
            guard status == 0, result != nil, record.pw_uid == geteuid(),
                  let pointer = record.pw_dir, let home = String(validatingCString: pointer) else { throw Failure.refused }
            return home
        }
        throw Failure.refused
    }
}
