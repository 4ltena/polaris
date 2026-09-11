import Foundation
import Darwin
import CryptoKit

public struct PublicationProof: Codable, Equatable, Sendable {
    public let device: String
    public let inode: String
    public let sha256: String
    public let stagingName: String?
}
public enum BootstrapReconciliationError: Error { case absent }

public enum BootstrapPublication: Equatable, Sendable {
    case published(PublicationProof)
    // The renamed file may be visible. Never overwrite or launch automatically.
    case publicationUnknown(PublicationProof)
}

/// Owner metadata only; this document grants no execution authority.
public final class BootstrapPublisher {
    public static let filename = "owner-bootstrap.json"
    private let directory: MetadataDirectory
    private var retained: BootstrapPublication?
    public init(storeURL: URL) throws {
        directory = try MetadataDirectory(url: storeURL, create: false)
    }

    public func publish(binding: WorkspaceBinding, selection: ExecutionBinding,
                        permission: PermissionPreference, policyRevision: UInt64,
                        configurationRevision: UInt64, historyMode: HistoryMode = .legacy,
                        revalidate: () throws -> Void, recordProof: (PublicationProof) throws -> Void = { _ in }) throws -> BootstrapPublication {
        try publish(binding: binding, selection: selection, permission: permission,
                    policyRevision: policyRevision, configurationRevision: configurationRevision, historyMode: historyMode,
                    revalidate: revalidate, fault: { _ in }, recordProof: recordProof)
    }
    public func reconcile(expected: PublicationProof, revalidate: () throws -> Void) throws -> BootstrapPublication {
        try reconcile(expected: expected, revalidate: revalidate, beforeDirectorySync: {})
    }
    func reconcile(expected: PublicationProof, revalidate: () throws -> Void,
                   beforeDirectorySync: () throws -> Void) throws -> BootstrapPublication {
        try revalidate(); try directory.verify()
        var entry = stat()
        if fstatat(directory.descriptor, Self.filename, &entry, AT_SYMLINK_NOFOLLOW) != 0 {
            guard errno == ENOENT else { throw SettingsError.readFailed }
            try directory.verify(); try revalidate()
            // Absence alone is insufficient: require the exact retained file.
            let retired = retirementName(expected)
            let retainedName: String
            do { try directory.verifyBootstrap(expected, name: retired); retainedName = retired }
            catch {
                guard let staging = expected.stagingName, staging.hasPrefix(".owner-bootstrap-"),
                      !staging.contains("/"), staging.utf8.count <= 255 else { throw SettingsError.readFailed }
                try directory.verifyBootstrap(expected, name: staging)
                retainedName = staging
            }
            try beforeDirectorySync()
            guard fsync(directory.descriptor) == 0 else { throw SettingsError.saveFailed }
            try directory.verify(); try directory.verifyBootstrap(expected, name: retainedName)
            var current = stat()
            guard fstatat(directory.descriptor, Self.filename, &current, AT_SYMLINK_NOFOLLOW) != 0,
                  errno == ENOENT else { throw SettingsError.readFailed }
            try revalidate()
            throw BootstrapReconciliationError.absent
        }
        try directory.verifyBootstrap(expected)
        do {
            guard fsync(directory.descriptor) == 0 else { throw SettingsError.saveFailed }
            try directory.verify(); try directory.verifyBootstrap(expected); try revalidate()
            let result = BootstrapPublication.published(expected); retained = result; return result
        } catch {
            let result = BootstrapPublication.publicationUnknown(expected); retained = result; return result
        }
    }

    private func retirementName(_ proof: PublicationProof) -> String {
        ".owner-bootstrap-retired-\(proof.inode)-\(proof.sha256)"
    }

    /// Call only after the prior child is reaped and the exact receipt is reconciled.
    public func retire(expected: PublicationProof, revalidate: () throws -> Void) throws {
        guard retained == .published(expected) else { throw SettingsError.saveFailed }
        try revalidate(); try directory.verify(); try directory.verifyBootstrap(expected)
        let retired = retirementName(expected)
        guard renameatx_np(directory.descriptor, Self.filename, directory.descriptor, retired, UInt32(RENAME_EXCL)) == 0 else {
            throw SettingsError.saveFailed
        }
        // Validate the object actually moved; never unlink an unverified replacement.
        try directory.verifyBootstrap(expected, name: retired)
        guard fsync(directory.descriptor) == 0 else { throw SettingsError.saveFailed }
        try directory.verify(); try revalidate()
        // Keep the proven retired object if clearing the settings receipt fails.
        // Cleanup is separate from this handoff transaction.
        retained = nil
    }

    enum Stage { case stagingSync, beforeRename, directorySync, afterRename }
    // Fault seam is internal and used only by synthetic filesystem tests.
    func publish(binding: WorkspaceBinding, selection: ExecutionBinding,
                 permission: PermissionPreference, policyRevision: UInt64,
                 configurationRevision: UInt64, historyMode: HistoryMode = .legacy, revalidate: () throws -> Void,
                 fault: (Stage) throws -> Void, recordProof: (PublicationProof) throws -> Void = { _ in }) throws -> BootstrapPublication {
        guard retained == nil else { throw SettingsError.saveFailed }
        try selection.validate(); try binding.validate()
        func validate() throws {
            try directory.verify()
            let source = try WorkspaceBinding.sourceIdentity(path: binding.sourcePath)
            guard Data(source.path.utf8) == Data(binding.sourcePath.utf8),
                  source.device == binding.sourceDevice, source.inode == binding.sourceInode else {
                throw SettingsError.sourceChanged
            }
            try revalidate()
        }
        try validate()
        var root = stat()
        guard fstat(directory.descriptor, &root) == 0 else { throw SettingsError.saveFailed }
        let tiers: [PermissionPreference: String] = [.readOnly: "read_only", .readCreate: "read_create",
            .readCreateBuild: "read_create_build", .readCreateBuildExternal: "read_create_build_external"]
        var body: [String: Any] = ["schema_version": 1, "project_id": binding.projectID,
            "session_id": binding.sessionID,
            "store_identity": ["device": String(UInt64(UInt32(bitPattern: root.st_dev))), "inode": String(root.st_ino)],
            "source_path": binding.sourcePath,
            "source_identity": ["device": binding.sourceDevice, "inode": binding.sourceInode],
            "tier": tiers[permission]!, "policy_revision": String(policyRevision),
            "configuration_revision": String(configurationRevision), "provider": selection.provider.rawValue,
            "model": selection.model]
        if let effort = selection.effort { body["effort"] = effort }
        if let endpoint = selection.localEndpoint { body["local_endpoint"] = endpoint }
        if historyMode != .legacy { body["history_mode"] = historyMode.rawValue }
        let data = try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys])
        guard data.count <= 32 * 1024 else { throw SettingsError.invalidDocument }
        let result = try directory.publishBootstrap(data, validate: validate, fault: fault, recordProof: recordProof)
        retained = result
        return result
    }
}

extension MetadataDirectory {
    func verifyBootstrap(_ expected: PublicationProof, name: String = BootstrapPublisher.filename) throws {
        guard !name.contains("/"), name != ".", name != "..", name.utf8.count <= 255 else { throw SettingsError.readFailed }
        let fd = openat(descriptor, name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
        guard fd >= 0 else { throw SettingsError.readFailed }
        defer { Darwin.close(fd) }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & S_IFMT == S_IFREG, info.st_mode & 0o077 == 0,
              info.st_uid == geteuid(), info.st_nlink == 1, info.st_size <= 32768,
              String(UInt64(UInt32(bitPattern: info.st_dev))) == expected.device,
              String(info.st_ino) == expected.inode else { throw SettingsError.readFailed }
        var bytes = Data(); var buffer = [UInt8](repeating: 0, count: 4096)
        while true {
            let count = Darwin.read(fd, &buffer, buffer.count)
            if count < 0 && errno == EINTR { continue }
            guard count >= 0, bytes.count + count <= 32768 else { throw SettingsError.readFailed }
            if count == 0 { break }
            bytes.append(contentsOf: buffer.prefix(count))
        }
        guard SHA256.hash(data: bytes).map({ String(format: "%02x", $0) }).joined() == expected.sha256 else { throw SettingsError.readFailed }
        var current = stat()
        guard fstatat(descriptor, name, &current, AT_SYMLINK_NOFOLLOW) == 0,
              current.st_dev == info.st_dev, current.st_ino == info.st_ino else { throw SettingsError.readFailed }
    }
    /// Separate from settings writes: retain proof across every post-rename failure.
    func publishBootstrap(_ data: Data, validate: () throws -> Void,
                          fault: (BootstrapPublisher.Stage) throws -> Void, recordProof: (PublicationProof) throws -> Void) throws -> BootstrapPublication {
        try validate()
        let name = BootstrapPublisher.filename
        var old = stat()
        if fstatat(descriptor, name, &old, AT_SYMLINK_NOFOLLOW) == 0 {
            // Publication is one-shot per handoff; existing evidence needs reconciliation.
            throw SettingsError.saveFailed
        }
        guard errno == ENOENT else { throw SettingsError.saveFailed }
        let temporary = ".owner-bootstrap-\(UUID().uuidString).tmp"
        let fd = openat(descriptor, temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, mode_t(0o600))
        guard fd >= 0 else { throw SettingsError.saveFailed }
        var preserveStaging = false
        defer { Darwin.close(fd); if !preserveStaging { _ = unlinkat(descriptor, temporary, 0) } }
        try data.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw SettingsError.saveFailed }
                offset += count
            }
        }
        var info = stat()
        guard fsync(fd) == 0, fstat(fd, &info) == 0 else { throw SettingsError.saveFailed }
        let proof = PublicationProof(device: String(UInt64(UInt32(bitPattern: info.st_dev))),
                                     inode: String(info.st_ino),
                                     sha256: SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined(), stagingName: temporary)
        // Make the retained name durable before the settings receipt can refer to it.
        try fault(.stagingSync)
        guard fsync(descriptor) == 0 else { throw SettingsError.saveFailed }
        preserveStaging = true
        try recordProof(proof)
        try fault(.beforeRename)
        try validate()
        // Exclusive rename prevents another same-user publisher from being overwritten.
        guard renameatx_np(descriptor, temporary, descriptor, name, UInt32(RENAME_EXCL)) == 0 else {
            throw SettingsError.saveFailed
        }
        do {
            try fault(.directorySync)
            guard fsync(descriptor) == 0 else { throw SettingsError.saveFailed }
            try fault(.afterRename)
            try validate()
            return .published(proof)
        } catch { return .publicationUnknown(proof) }
    }
}
