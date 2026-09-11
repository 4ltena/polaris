import Foundation
import Darwin
import CryptoKit
import PolarisSettings

/// Typed native-owner arguments. Bootstrap JSON is not a source permission grant.
struct OwnerServiceLaunch: Sendable {
    let store: ServiceClient.PersistentStore
    let binding: WorkspaceBinding
    let proof: PublicationProof
    let permission: PermissionPreference
    let policyRevision: UInt64
    let storeDevice: String
    let storeInode: String
    init(store: ServiceClient.PersistentStore, binding: WorkspaceBinding, proof: PublicationProof,
         permission: PermissionPreference, policyRevision: UInt64) throws {
        guard Data(store.projectID.utf8) == Data(binding.projectID.utf8),
              Data(store.sessionID.utf8) == Data(binding.sessionID.utf8) else { throw ServiceError.correlation }
        let root = try MetadataDirectory(url: store.root, create: false)
        try root.verify()
        var info = stat()
        guard lstat(root.url.path, &info) == 0, info.st_mode & S_IFMT == S_IFDIR else { throw ServiceError.schema }
        self.store = store; self.binding = binding; self.proof = proof
        self.permission = permission; self.policyRevision = policyRevision
        storeDevice = String(UInt64(UInt32(bitPattern: info.st_dev))); storeInode = String(info.st_ino)
    }
    func verify() throws {
        let directory = try MetadataDirectory(url: store.root, create: false)
        try directory.verify()
        var root = stat(); var source = stat()
        guard lstat(directory.url.path, &root) == 0,
              String(UInt64(UInt32(bitPattern: root.st_dev))) == storeDevice, String(root.st_ino) == storeInode,
              lstat(binding.sourcePath, &source) == 0, source.st_mode & S_IFMT == S_IFDIR,
              String(UInt64(UInt32(bitPattern: source.st_dev))) == binding.sourceDevice,
              String(source.st_ino) == binding.sourceInode else { throw ServiceError.correlation }
        let publisher = try BootstrapPublisher(storeURL: store.root)
        guard case .published = try publisher.reconcile(expected: proof, revalidate: {}) else { throw ServiceError.notReady }
    }
    var arguments: [String] {
        let tier: String = switch permission {
        case .readOnly: "read_only"
        case .readCreate: "read_create"
        case .readCreateBuild: "read_create_build"
        case .readCreateBuildExternal: "read_create_build_external"
        }
        return ["--owner-launch-version", "1", "--store-root", store.root.path,
            "--project-id", binding.projectID, "--session-id", binding.sessionID,
            "--store-device", storeDevice, "--store-inode", storeInode,
            "--bootstrap-name", BootstrapPublisher.filename, "--bootstrap-device", proof.device,
            "--bootstrap-inode", proof.inode, "--bootstrap-sha256", proof.sha256,
            "--confirmed-source-path", binding.sourcePath, "--confirmed-source-device", binding.sourceDevice,
            "--confirmed-source-inode", binding.sourceInode, "--confirmed-tier", tier,
            "--confirmed-policy-revision", String(policyRevision), "--source-recovery-fd2"]
    }
    static func verifyPackagedExecutionHelper(bundle: URL) throws {
        let helper = bundle.appendingPathComponent("Contents/Helpers/polaris-execution-helper")
        let manifest = bundle.appendingPathComponent("Contents/Resources/execution-helper.json")
        struct Manifest: Decodable { let schema_version: Int; let sha256: String }
        for file in [helper, manifest] {
            let values = try file.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
            guard values.isRegularFile == true, values.isSymbolicLink != true else { throw ServiceError.invalidHelper }
        }
        let data = try Data(contentsOf: manifest)
        guard data.count <= 8192 else { throw ServiceError.capacity }
        let value = try JSONDecoder().decode(Manifest.self, from: data)
        guard value.schema_version == 1, FileManager.default.isExecutableFile(atPath: helper.path),
              SHA256.hash(data: try Data(contentsOf: helper)).map({ String(format: "%02x", $0) }).joined() == value.sha256 else { throw ServiceError.invalidHelper }
    }
}
