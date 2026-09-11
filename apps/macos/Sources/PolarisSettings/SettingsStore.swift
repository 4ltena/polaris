import Foundation
import Darwin

public final class SettingsStore: @unchecked Sendable {
    public let url: URL
    private let directory: MetadataDirectory
    private var selectionEpoch = UUID()
    public var selectionGeneration: UUID {
        mutationLock.lock(); defer { mutationLock.unlock() }; return selectionEpoch
    }
    private let mutationLock = NSRecursiveLock()
    private let lockDescriptor: Int32
    public init(url: URL) throws {
        guard url.isFileURL, !url.lastPathComponent.isEmpty else { throw SettingsError.unavailableLocation }
        directory = try MetadataDirectory(url: url.deletingLastPathComponent(), create: true, privateMode: false)
        self.url = directory.url.appendingPathComponent(url.lastPathComponent)
        let fd = openat(directory.descriptor, url.lastPathComponent + ".lock",
                        O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, mode_t(0o600))
        guard fd >= 0 else { throw SettingsError.unavailableLocation }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_uid == geteuid(), info.st_mode & S_IFMT == S_IFREG,
              info.st_nlink == 1, info.st_mode & 0o077 == 0 else {
            Darwin.close(fd); throw SettingsError.unavailableLocation
        }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else { Darwin.close(fd); throw SettingsError.inUse }
        lockDescriptor = fd
    }
    deinit { Darwin.close(lockDescriptor) }

    public func load() throws -> SettingsDocument {
        mutationLock.lock(); defer { mutationLock.unlock() }
        let data: Data?
        do { data = try directory.read(url.lastPathComponent) }
        catch { throw SettingsError.readFailed }
        guard let data else { return SettingsDocument() }
        do {
            let document = try JSONDecoder().decode(SettingsDocument.self, from: data)
            try document.validate(); return document
        } catch { throw SettingsError.invalidDocument }
    }
    public func save(_ document: SettingsDocument) throws {
        mutationLock.lock(); defer { mutationLock.unlock() }
        try document.validate()
        var candidate = document
        // Ordinary settings edits cannot erase or resurrect a request ledger intent.
        do {
            let current = try load()
            if current.preferences != document.preferences { selectionEpoch = UUID() }
            candidate.pendingConfiguration = current.pendingConfiguration
            candidate.pendingRoleConfiguration = current.pendingRoleConfiguration
            candidate.bootstrapProof = current.bootstrapProof
        }
        catch { throw SettingsError.saveFailed }
        try write(candidate)
    }
    public func recordBootstrapProof(_ proof: PublicationProof?) throws {
        mutationLock.lock(); defer { mutationLock.unlock() }
        var document = try load(); document.bootstrapProof = proof
        try write(document)
    }
    private func write(_ document: SettingsDocument) throws {
        try document.validate()
        do { try directory.write(JSONEncoder().encode(document), name: url.lastPathComponent) }
        catch { throw SettingsError.saveFailed }
    }
    public func recordConfigurationIntent(_ intent: ConfigurationIntent) throws {
        mutationLock.lock(); defer { mutationLock.unlock() }
        var document = try load()
        guard document.preferences?.executionBinding == intent.selection,
              document.pendingConfiguration == nil || document.pendingConfiguration == intent else { throw SettingsError.saveFailed }
        document.pendingConfiguration = intent
        try write(document)
    }
    public func clearConfigurationIntent(requestID: String) throws {
        mutationLock.lock(); defer { mutationLock.unlock() }
        var document = try load()
        guard let pending = document.pendingConfiguration, Data(pending.requestID.utf8) == Data(requestID.utf8) else { throw SettingsError.saveFailed }
        document.pendingConfiguration = nil
        try write(document)
    }
    public func recordRoleConfigurationIntent(_ intent: RoleConfigurationIntent) throws {
        mutationLock.lock(); defer { mutationLock.unlock() }
        var document = try load()
        guard document.pendingRoleConfiguration == nil || document.pendingRoleConfiguration == intent else {
            throw SettingsError.saveFailed
        }
        document.pendingRoleConfiguration = intent
        try write(document)
    }
    public func clearRoleConfigurationIntent(requestID: String) throws {
        mutationLock.lock(); defer { mutationLock.unlock() }
        var document = try load()
        guard let pending = document.pendingRoleConfiguration,
              Data(pending.requestID.utf8) == Data(requestID.utf8) else {
            throw SettingsError.saveFailed
        }
        document.pendingRoleConfiguration = nil
        try write(document)
    }

    /// Persist the selected source and stable IDs before any service is launched.
    /// A missing binding is the only creation case. Never replace a broken one.
    public func prepareWorkspace(_ document: SettingsDocument) throws -> (SettingsDocument, URL?) {
        mutationLock.lock(); defer { mutationLock.unlock() }
        var candidate = document
        guard let preferences = document.preferences, let selected = preferences.projectPath else {
            guard document.workspace == nil else { throw SettingsError.sourceChanged }
            return (candidate, nil)
        }
        let source = try WorkspaceBinding.sourceIdentity(path: selected)
        let storeURL = directory.url.appendingPathComponent(url.lastPathComponent + ".workspace", isDirectory: true)
        let metadata = storeURL.pathComponents, sourceParts = URL(fileURLWithPath: source.path).pathComponents
        guard !metadata.starts(with: sourceParts),
              !url.pathComponents.starts(with: sourceParts), !sourceParts.starts(with: metadata) else {
            throw SettingsError.metadataInSource
        }
        if let binding = candidate.workspace {
            guard binding.sourcePath == source.path, binding.sourceDevice == source.device,
                  binding.sourceInode == source.inode else { throw SettingsError.sourceChanged }
        } else {
            candidate.workspace = WorkspaceBinding(source: source)
        }
        // Only this private metadata directory is created; never source contents.
        let root = try MetadataDirectory(url: storeURL, create: candidate.workspace != document.workspace)
        try save(candidate)
        try root.verify()
        return (candidate, root.url)
    }
}
