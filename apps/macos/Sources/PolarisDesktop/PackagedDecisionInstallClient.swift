import Foundation
import CryptoKit
import Darwin

/// One owned CLI process retains the opaque Rust plan until confirmation.
actor PackagedDecisionInstallClient: DecisionInstallTransport {
    private let helper: @Sendable () throws -> URL
    private var child: DecisionInstallChild?
    private var consent: String?
    private var completionTimeout: UInt64 = 300
    init(helper: @escaping @Sendable () throws -> URL = { try PackagedDecisionInstallClient.packagedCLI() }) { self.helper = helper }

    func catalog() async throws -> DecisionInstallCatalog {
        guard child == nil else { throw ServiceError.notReady }
        let process = try DecisionInstallChild(helper: helper(), arguments: ["decision", "list"])
        child = process
        defer { process.stop(); if child === process { child = nil } }
        let data = try await bounded(process, seconds: 15) { try process.readAll() }
        guard child === process else { throw ServiceError.notReady }
        let value = try Self.decode(DecisionInstallCatalog.self, data)
        guard value.schemaVersion == 1, value.registeredProfiles.count <= 64,
              Set(value.registeredProfiles).count == value.registeredProfiles.count,
              value.registeredProfiles.allSatisfy(Self.profileID), value.capacity.count <= 32,
              value.capacity.allSatisfy({ $0.utf8.count <= 4096 }), value.note.utf8.count <= 4096 else { throw ServiceError.schema }
        return value
    }

    func verify(profile: String) async throws -> DecisionGenerationVerification {
        guard child == nil, Self.profileID(profile) else { throw ServiceError.notReady }
        let process = try DecisionInstallChild(helper: helper(), arguments: ["decision", "verify", profile, "--stdio-result"])
        child = process
        defer { process.stop(); if child === process { child = nil } }
        let data = try await bounded(process, seconds: 300) { try process.readAll() }
        guard child === process else { throw ServiceError.notReady }
        return try Self.decodeVerification(data, profile: profile)
    }

    static func decodeVerification(_ data: Data, profile: String) throws -> DecisionGenerationVerification {
        let value = try decode(DecisionGenerationVerification.self, data)
        guard let raw = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              Set(raw.keys).isSubset(of: ["schema_version", "type", "profile", "status", "generation"]) else { throw ServiceError.schema }
        if let generation = raw["generation"] as? [String: Any] {
            guard Set(generation.keys) == ["profile", "generation", "model_repository", "model_revision", "quantization", "runtime_revision", "worker_sha256", "model_sha256", "verified_bytes"] else { throw ServiceError.schema }
        }
        guard value.schemaVersion == 1, value.type == "verification", value.profile == profile,
              profileID(profile) else { throw ServiceError.correlation }
        if value.status == "verified" {
            guard let g = value.generation, g.profile == profile, profileID(g.generation),
                  text(g.modelRepository), text(g.quantization), text(g.runtimeRevision),
                  g.modelRevision.utf8.count == 40, g.modelRevision.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
                  digest(g.workerSha256), digest(g.modelSha256), g.verifiedBytes > 0,
                  g.verifiedBytes <= 64 * 1024 * 1024 * 1024 + 128 * 1024 * 1024 else { throw ServiceError.schema }
        } else {
            guard value.generation == nil, ["not_installed", "unverified_profile", "integrity", "unsupported_platform", "unsupported_device", "cancelled", "unavailable"].contains(value.status) else { throw ServiceError.schema }
        }
        return value
    }
    private static func text(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 256 && value.unicodeScalars.allSatisfy { !CharacterSet.controlCharacters.contains($0) }
    }

    func prepare(profile: String, worker: URL, model: URL, repair: Bool = false) async throws -> DecisionInstallOffer {
        guard worker.isFileURL, model.isFileURL else { throw ServiceError.notReady }
        return try await prepareOperation(profile: profile, worker: worker, model: model, repair: repair)
    }

    func prepareDownload(profile: String, repair: Bool) async throws -> DecisionInstallOffer {
        try await prepareOperation(profile: profile, worker: nil, model: nil, repair: repair)
    }

    private func prepareOperation(profile: String, worker: URL?, model: URL?, repair: Bool) async throws -> DecisionInstallOffer {
        guard child == nil, Self.profileID(profile) else { throw ServiceError.notReady }
        let downloading = worker == nil && model == nil
        var arguments = ["decision", downloading ? "download" : (repair ? "repair" : "import"), profile]
        if downloading { if repair { arguments += ["--repair"] } }
        else {
            guard let worker, let model else { throw ServiceError.notReady }
            arguments += ["--worker", worker.path, "--model", model.path]
        }
        arguments += ["--stdio-confirm"]
        let process = try DecisionInstallChild(helper: helper(), arguments: arguments)
        child = process
        do {
            let data = try await bounded(process, seconds: 20) { try process.readFrame() }
            let offer = try Self.decode(DecisionInstallOffer.self, data)
            guard child === process else { throw ServiceError.notReady }
            guard offer.schemaVersion == 1, offer.type == "plan", Self.digest(offer.consentSha256),
                  offer.plan.profile == profile, offer.plan.isDownload == downloading,
                  downloading || (offer.plan.worker.source == worker?.path && offer.plan.model.source == model?.path),
                  offer.plan.delivery.components.count <= 64,
                  offer.plan.operation == (repair ? "repair" : "import") else { throw ServiceError.correlation }
            if downloading { try Self.validateDownloadOffer(offer) }
            if repair {
                guard let backup = offer.plan.replacement?.backupDestination,
                      backup.utf8.count <= 4096, backup.hasPrefix("/"),
                      !backup.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
                      URL(fileURLWithPath: backup).deletingLastPathComponent() == URL(fileURLWithPath: offer.plan.destination).deletingLastPathComponent(),
                      URL(fileURLWithPath: backup).lastPathComponent.hasPrefix(".recovery-") else { throw ServiceError.schema }
            } else if offer.plan.replacement != nil { throw ServiceError.schema }
            completionTimeout = downloading ? (offer.plan.transactionTimeoutSeconds ?? 0) + 300 : 300
            consent = offer.consentSha256
            return offer
        } catch { process.stop(); if child === process { child = nil; consent = nil }; throw error }
    }

    static func validateDownloadOffer(_ offer: DecisionInstallOffer) throws {
        let p = offer.plan
        let (size, overflow) = p.worker.bytes.addingReportingOverflow(p.model.bytes)
        let (copies, copiesOverflow) = size.multipliedReportingOverflow(by: 2)
        let (required, reserveOverflow) = copies.addingReportingOverflow(p.copyReserveBytes)
        guard !overflow, !copiesOverflow, !reserveOverflow, p.worker.bytes > 0, p.model.bytes > 0,
              p.worker.bytes <= 128 * 1024 * 1024, p.model.bytes <= 64 * 1024 * 1024 * 1024,
              p.transferBytes == size, p.installedBytes == size, p.diskRequiredBytes == required,
              p.transactionTimeoutSeconds == 7200,
              digest(p.worker.sha256), digest(p.model.sha256), let sources = p.delivery.downloads else { throw ServiceError.schema }
        for source in [sources.worker, sources.model] {
            guard downloadURL(source.url, limit: 2048), source.redirectOrigins.count <= 16,
                  source.redirectOrigins.allSatisfy({ downloadURL($0, limit: 512) && URLComponents(string: $0)?.path == "" }) else { throw ServiceError.schema }
        }
    }
    private static func downloadURL(_ value: String, limit: Int) -> Bool {
        guard value.utf8.count <= limit, !value.contains("\\"), !value.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.union(.controlCharacters).contains($0) }),
              let url = URLComponents(string: value), url.scheme == "https", url.host?.isEmpty == false,
              url.user == nil, url.password == nil, url.query == nil, url.fragment == nil else { return false }
        return true
    }

    func confirm(consent: String, approved: Bool) async throws -> DecisionInstallOutcome {
        guard let process = child, self.consent == consent else { throw ServiceError.correlation }
        self.consent = nil
        defer { process.stop(); if child === process { child = nil } }
        let data = try JSONSerialization.data(withJSONObject: ["schema_version": 1, "consent_sha256": consent,
            "decision": approved ? "approve" : "cancel"])
        try process.send(data + Data([10]))
        let response = try await bounded(process, seconds: completionTimeout) { try process.readCompletion() }
        guard child === process else { throw ServiceError.notReady }
        struct Completion: Decodable { let schemaVersion: Int; let type: String; let status: String; let consentSha256: String }
        let value = try Self.decode(Completion.self, response)
        guard value.schemaVersion == 1, value.type == "complete", value.consentSha256 == consent else { throw ServiceError.correlation }
        switch value.status {
        case "installed" where approved: return .installed
        case "cancelled" where !approved: return .cancelled
        default: throw ServiceError.schema
        }
    }

    func cancel() { child?.stop(); child = nil; consent = nil }

    private func bounded<T: Sendable>(_ process: DecisionInstallChild, seconds: UInt64,
                                     _ operation: @escaping @Sendable () throws -> T) async throws -> T {
        let timer = Task {
            do { try await Task.sleep(nanoseconds: seconds * 1_000_000_000) }
            catch { return }
            process.stop()
        }
        defer { timer.cancel() }
        return try await withTaskCancellationHandler {
            let result = try await Task.detached(priority: .userInitiated, operation: operation).value
            try Task.checkCancellation()
            return result
        } onCancel: { process.stop() }
    }

    static func decode<T: Decodable>(_ type: T.Type, _ data: Data) throws -> T {
        guard !data.isEmpty, data.count <= 256 * 1024 else { throw ServiceError.capacity }
        let decoder = JSONDecoder(); decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(type, from: data)
    }
    private static func digest(_ value: String) -> Bool { value.utf8.count == 64 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) } }
    private static func profileID(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 96 && value.utf8.first.map { (48...57).contains($0) || (97...122).contains($0) } == true
            && value.utf8.allSatisfy { (48...57).contains($0) || (97...122).contains($0) || [45, 46, 95].contains($0) }
    }

    /// Fixed bundled paths only; never PATH, user settings or project tools.
    nonisolated static func packagedCLI(bundle: URL = Bundle.main.bundleURL) throws -> URL {
        struct Manifest: Decodable { let schemaVersion: Int; let relativePath: String; let architectures: [String]; let sha256: String }
        let candidates = [bundle, bundle.appendingPathComponent("Contents/Resources/Polaris.app")]
        for app in candidates {
            let helper = app.appendingPathComponent("Contents/Helpers/polaris")
            guard FileManager.default.fileExists(atPath: helper.path) else { continue }
            let manifest = app.appendingPathComponent("Contents/Resources/polaris-cli.json")
            for file in [helper, manifest] {
                var info = stat()
                guard lstat(file.path, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
                      info.st_nlink == 1, info.st_mode & 0o22 == 0 else { throw ServiceError.invalidHelper }
            }
            let data = try Data(contentsOf: manifest)
            guard data.count <= 8192 else { throw ServiceError.capacity }
            let lock = try decode(Manifest.self, data)
            #if arch(arm64)
            let architecture = "arm64"
            #else
            let architecture = "x86_64"
            #endif
            guard lock.schemaVersion == 1, lock.relativePath == "Contents/Helpers/polaris",
                  lock.architectures.contains(architecture), digest(lock.sha256),
                  FileManager.default.isExecutableFile(atPath: helper.path) else { throw ServiceError.invalidHelper }
            let file = try FileHandle(forReadingFrom: helper); defer { try? file.close() }
            var hash = SHA256()
            while let bytes = try file.read(upToCount: 1024 * 1024), !bytes.isEmpty { hash.update(data: bytes) }
            guard hash.finalize().map({ String(format: "%02x", $0) }).joined() == lock.sha256 else { throw ServiceError.invalidHelper }
            return helper
        }
        throw ServiceError.invalidHelper
    }
}

/// I/O is used by one read operation at a time; stop/write are synchronized.
private final class DecisionInstallChild: @unchecked Sendable {
    private let process = Process()
    private let input = Pipe()
    private let output = Pipe()
    private let lock = NSLock()
    private var stopping = false
    init(helper: URL, arguments: [String]) throws {
        process.executableURL = helper; process.arguments = arguments
        process.environment = [:]; process.currentDirectoryURL = helper.deletingLastPathComponent()
        process.standardInput = input; process.standardOutput = output; process.standardError = FileHandle.nullDevice
        try process.run()
    }
    deinit { stop() }
    func send(_ data: Data) throws {
        lock.lock(); defer { lock.unlock() }
        guard !stopping, process.isRunning else { throw ServiceError.launch }
        try input.fileHandleForWriting.write(contentsOf: data)
    }
    func readFrame() throws -> Data {
        var data = Data()
        while data.count <= 256 * 1024 {
            guard let byte = try output.fileHandleForReading.read(upToCount: 1), !byte.isEmpty else { throw ServiceError.launch }
            if byte == Data([10]) { return data }
            data.append(byte)
        }
        throw ServiceError.capacity
    }
    func readCompletion() throws -> Data {
        let data = try readFrame()
        process.waitUntilExit()
        guard process.terminationReason == .exit, process.terminationStatus == 0 else { throw ServiceError.launch }
        return data
    }
    func readAll() throws -> Data {
        var data = Data()
        while let bytes = try output.fileHandleForReading.read(upToCount: 4096), !bytes.isEmpty {
            guard data.count + bytes.count <= 256 * 1024 else { throw ServiceError.capacity }
            data.append(bytes)
        }
        process.waitUntilExit()
        guard process.terminationReason == .exit, process.terminationStatus == 0 else { throw ServiceError.launch }
        return data
    }
    func stop() {
        lock.lock(); defer { lock.unlock() }
        guard !stopping else { return }; stopping = true
        try? input.fileHandleForWriting.close()
        if process.isRunning {
            process.interrupt()
            let process = process
            DispatchQueue.global().asyncAfter(deadline: .now() + 2) { if process.isRunning { process.terminate() } }
        }
    }
}
