import CryptoKit
import Darwin
import Foundation

enum ManagedDependencyID: String, CaseIterable, Sendable {
  case llamaCpp = "llama_cpp"
  case rust
  case embeddingRuntime = "embedding_runtime"
  case multilingualE5Small = "multilingual_e5_small"
  case multilingualE5Base = "multilingual_e5_base"
  case catTranslate3B = "cat_translate_3_3b"

  static let preset: [ManagedDependencyID] = [
    .embeddingRuntime, .multilingualE5Small, .multilingualE5Base, .llamaCpp, .rust,
  ]

  var title: String {
    switch self {
    case .llamaCpp: "llama.cpp 実行環境"
    case .rust: "Rust 1.96.0"
    case .embeddingRuntime: "e5 埋め込み実行環境"
    case .multilingualE5Small: "multilingual-e5-small（長期記憶）"
    case .multilingualE5Base: "multilingual-e5-base（事前選定）"
    case .catTranslate3B: "CAT-Translate 3.3B（翻訳・任意）"
    }
  }
}

enum ManagedDependencyStatus: Equatable, Sendable {
  case notInstalled
  case checking
  case downloading
  case installing
  case installed(version: String)
  case repairRequired
  case failed(String)

  var title: String {
    switch self {
    case .notInstalled: "未導入"
    case .checking: "確認中"
    case .downloading: "ダウンロード中"
    case .installing: "検証・導入中"
    case .installed(let version): "導入済み（\(version)）"
    case .repairRequired: "修復が必要"
    case .failed: "確認できませんでした"
    }
  }

  var isBusy: Bool {
    self == .checking || self == .downloading || self == .installing
  }
}

enum ManagedDependencyError: Error, LocalizedError, Equatable {
  case manifest
  case unsupportedPlatform
  case invalidResponse
  case insufficientSpace
  case integrity
  case archive
  case conflict
  case cancelled
  case install
  case runtimeUnavailable

  var errorDescription: String? {
    switch self {
    case .manifest: "依存関係の固定情報を確認できませんでした。"
    case .unsupportedPlatform: "このMac向けの依存関係がありません。"
    case .invalidResponse: "固定した取得先から正しい応答を受け取れませんでした。"
    case .insufficientSpace: "ダウンロードと展開に必要な空き容量がありません。"
    case .integrity: "取得したファイルのサイズまたはSHA-256が一致しません。"
    case .archive: "取得したアーカイブを安全に展開できません。"
    case .conflict: "同じ版の導入先に別の内容があります。"
    case .cancelled: "導入を取り消しました。"
    case .install: "依存関係を導入できませんでした。"
    case .runtimeUnavailable: "検証済みの読取専用runtime imageがありません。修復が必要です。"
    }
  }
}

struct ManagedGGUFPlan: Equatable, Sendable {
  let sourceURL: URL
  let downloadURL: URL
  let filename: String
  let size: Int64
}

struct ManagedGGUFModel: Codable, Equatable, Identifiable, Sendable {
  var id: String { filename }
  let filename: String
  let sourceURL: URL
  let size: Int64
  let sha256: String
  let installedAt: String

  enum CodingKeys: String, CodingKey {
    case filename, size, sha256
    case sourceURL = "source_url"
    case installedAt = "installed_at"
  }
}

enum ManagedGGUFStatus: Equatable, Sendable {
  case idle
  case inspecting
  case ready
  case downloading
  case installed
  case failed(String)

  var title: String {
    switch self {
    case .idle: "リンク未確認"
    case .inspecting: "リンクを確認中"
    case .ready: "ダウンロード準備完了"
    case .downloading: "モデルをダウンロード中"
    case .installed: "モデルを導入しました"
    case .failed: "モデルを導入できませんでした"
    }
  }
}

private struct ManagedGGUFCatalog: Codable {
  let schemaVersion: Int
  var models: [ManagedGGUFModel]

  enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", models }
}

private struct DependencyManifest: Decodable {
  let schemaVersion: Int
  let dependencies: [DependencyEntry]

  enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", dependencies }
}

private struct DependencyEntry: Decodable {
  let id: String
  let version: String
  let platform: String
  let architecture: String
  let url: URL?
  let sha256: String?
  let compressedSize: Int64?
  let requiredSpace: Int64?
  let archive: String?
  let archiveRoot: String?
  let installer: String?
  let installKind: String?
  let requiredExecutable: String?
  let destination: String?
  let files: [DependencyFile]?
  let pythonVersion: String?
  let pythonPackages: [String: String]?
  let excludeNewer: String?
  let legacyRuntime: Bool?

  enum CodingKeys: String, CodingKey {
    case id, version, platform, architecture, url, sha256, archive, installer, destination, files
    case compressedSize = "compressed_size"
    case requiredSpace = "required_space"
    case archiveRoot = "archive_root"
    case requiredExecutable = "required_executable"
    case installKind = "install_kind"
    case pythonVersion = "python_version"
    case pythonPackages = "python_packages"
    case excludeNewer = "exclude_newer"
    case legacyRuntime = "legacy_runtime"
  }
}

private struct DependencyFile: Decodable {
  let path: String
  let url: URL
  let sha256: String
  let size: Int64
}

private struct DependencyReceipt: Codable {
  let schemaVersion: Int
  let id: String
  let version: String
  let manifestSHA256: String
  let runtimeTreeSHA256: String?
  let installedAt: String

  enum CodingKeys: String, CodingKey {
    case schemaVersion = "schema_version"
    case id, version
    case manifestSHA256 = "manifest_sha256"
    case runtimeTreeSHA256 = "runtime_tree_sha256"
    case installedAt = "installed_at"
  }
}

private struct ManagedRuntimeTreeManifest: Codable {
  let schemaVersion: Int
  let files: [ManagedRuntimeTreeFile]

  enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", files }
}

private struct ManagedRuntimeTreeFile: Codable, Equatable {
  let path: String
  let size: Int64
  let sha256: String
}

private struct ReadonlyRuntimeImageLockV2: Decodable {
  let schemaVersion: Int
  let images: [ReadonlyRuntimeImageV2]

  enum CodingKeys: String, CodingKey { case schemaVersion = "schema_version", images }
}

private struct ReadonlyRuntimeImageV2: Decodable {
  let id: String
  let version: String
  let platform: String
  let architecture: String
  let imageFilename: String
  let imageSize: Int64
  let imageSHA256: String
  let treeManifestSHA256: String
  let sourceURL: URL?
  let bundledResource: String?

  enum CodingKeys: String, CodingKey {
    case id, version, platform, architecture
    case imageFilename = "image_filename"
    case imageSize = "image_size"
    case imageSHA256 = "image_sha256"
    case treeManifestSHA256 = "tree_manifest_sha256"
    case sourceURL = "source_url"
    case bundledResource = "bundled_resource"
  }
}

private struct ReadonlyRuntimeReceiptV2: Codable {
  let schemaVersion: Int
  let id: String
  let version: String
  let imageSHA256: String
  let treeManifestSHA256: String
  let installedAt: String

  enum CodingKeys: String, CodingKey {
    case schemaVersion = "schema_version"
    case id, version
    case imageSHA256 = "image_sha256"
    case treeManifestSHA256 = "tree_manifest_sha256"
    case installedAt = "installed_at"
  }
}

private struct RuntimeDirectoryIdentity {
  let device: dev_t
  let inode: ino_t

  init(_ info: stat) {
    device = info.st_dev
    inode = info.st_ino
  }

  func matches(_ info: stat) -> Bool {
    device == info.st_dev && inode == info.st_ino
  }
}

/// Keeps every verified ancestor open so later placement is relative to the
/// exact directories that were checked, even if their pathnames are swapped.
private final class RuntimeDirectoryChain {
  let home: URL
  let descriptors: [Int32]
  let names: [String]
  let identities: [RuntimeDirectoryIdentity]

  var polarisDescriptor: Int32 { descriptors[1] }
  var versionDescriptor: Int32 { descriptors.last! }

  init(home: URL, components: [String], create: Bool) throws {
    let flags = O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC
    var opened: [Int32] = []
    var identities: [RuntimeDirectoryIdentity] = []
    do {
      let homeFD = Darwin.open(home.path, flags)
      guard homeFD >= 0 else { throw ManagedDependencyError.conflict }
      opened.append(homeFD)
      var homeInfo = stat()
      guard fstat(homeFD, &homeInfo) == 0, homeInfo.st_mode & S_IFMT == S_IFDIR else {
        throw ManagedDependencyError.conflict
      }
      identities.append(RuntimeDirectoryIdentity(homeInfo))
      for component in components {
        var child = openat(opened.last!, component, flags)
        if child < 0, errno == ENOENT, create {
          guard mkdirat(opened.last!, component, mode_t(0o700)) == 0 || errno == EEXIST else {
            throw ManagedDependencyError.conflict
          }
          guard fsync(opened.last!) == 0 else { throw ManagedDependencyError.install }
          child = openat(opened.last!, component, flags)
        }
        guard child >= 0 else { throw ManagedDependencyError.conflict }
        var info = stat()
        guard fstat(child, &info) == 0, info.st_mode & S_IFMT == S_IFDIR,
              fchmod(child, mode_t(0o700)) == 0 else {
          Darwin.close(child)
          throw ManagedDependencyError.conflict
        }
        opened.append(child)
        identities.append(RuntimeDirectoryIdentity(info))
      }
      self.home = home
      self.descriptors = opened
      self.names = components
      self.identities = identities
    } catch {
      for descriptor in opened.reversed() { Darwin.close(descriptor) }
      throw error
    }
  }

  deinit {
    for descriptor in descriptors.reversed() { Darwin.close(descriptor) }
  }

  func verify() throws {
    var homeInfo = stat()
    guard lstat(home.path, &homeInfo) == 0, homeInfo.st_mode & S_IFMT == S_IFDIR,
          identities[0].matches(homeInfo) else {
      throw ManagedDependencyError.conflict
    }
    for index in names.indices {
      var pathInfo = stat()
      var descriptorInfo = stat()
      guard fstatat(descriptors[index], names[index], &pathInfo, AT_SYMLINK_NOFOLLOW) == 0,
            pathInfo.st_mode & S_IFMT == S_IFDIR,
            fstat(descriptors[index + 1], &descriptorInfo) == 0,
            identities[index + 1].matches(pathInfo),
            identities[index + 1].matches(descriptorInfo) else {
        throw ManagedDependencyError.conflict
      }
    }
  }
}

private struct InstalledModelManifest: Decodable {
  let revision: String
  let files: [String: String]
}

private final class HTTPSRedirectDelegate: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
  func urlSession(_ session: URLSession, task: URLSessionTask,
                  willPerformHTTPRedirection response: HTTPURLResponse,
                  newRequest request: URLRequest) async -> URLRequest? {
    request.url?.scheme == "https" && request.url?.user == nil && request.url?.password == nil
      ? request : nil
  }
}

private final class ManagedProcessControl: @unchecked Sendable {
  private let lock = NSLock()
  private var process: Process?
  private var cancelled = false

  func start(_ process: Process) throws {
    lock.lock()
    defer { lock.unlock() }
    guard !cancelled else { throw CancellationError() }
    self.process = process
    do { try process.run() }
    catch {
      self.process = nil
      throw error
    }
  }

  func finish() -> Bool {
    lock.lock()
    defer { lock.unlock() }
    process = nil
    return cancelled
  }

  func cancel() {
    lock.lock()
    cancelled = true
    let running = process?.isRunning == true ? process : nil
    lock.unlock()
    running?.terminate()
  }
}

protocol ManagedDependencyInstalling: Sendable {
  func statuses() async throws -> [ManagedDependencyID: ManagedDependencyStatus]
  func status(_ id: ManagedDependencyID) async throws -> ManagedDependencyStatus
  func install(_ id: ManagedDependencyID) async throws -> String
  func activatePreset() async throws
  func inspectGGUF(_ source: String) async throws -> ManagedGGUFPlan
  func installedGGUFModels() async throws -> [ManagedGGUFModel]
  func installGGUF(_ plan: ManagedGGUFPlan) async throws -> ManagedGGUFModel
}

actor ManagedDependencyInstaller: ManagedDependencyInstalling {
  /// This is also the path assembled by `LocalCatTranslate::default_model_path`.
  /// Keep the two pins in lockstep: the runtime must never discover a different
  /// CAT snapshot from the one the installer verified.
  static let catTranslateRevision = "47e382331d005acd54a42cdf088a76aa88788e0c"
  static let catTranslateDestination = "models/cat-translate-3.3b"
  static let catTranslatePayloadBytes: Int64 = 6_713_117_979
  static let catTranslateRequiredSpaceBytes: Int64 = catTranslatePayloadBytes * 3 + 256 * 1024 * 1024
  static let runtimeTreeManifestName = ".polaris-runtime-tree.json"
  static let runtimeTreeMaxFiles = 100_000
  static let runtimeTreeMaxBytes: Int64 = 16 * 1024 * 1024 * 1024

  private let manifestURL: URL
  private let runtimeImageManifestURL: URL
  private let home: URL
  private let fileManager: FileManager
  private let runtimePlacementHook: (@Sendable () throws -> Void)?

  init(manifestURL: URL? = Bundle.module.url(forResource: "runtime-dependencies", withExtension: "json"),
       runtimeImageManifestURL: URL? = Bundle.module.url(
         forResource: "readonly-runtime-images-v2", withExtension: "json"),
       home: URL = FileManager.default.homeDirectoryForCurrentUser,
       fileManager: FileManager = .default,
       runtimePlacementHook: (@Sendable () throws -> Void)? = nil) {
    self.manifestURL = manifestURL ?? URL(fileURLWithPath: "/invalid-runtime-dependencies.json")
    self.runtimeImageManifestURL = runtimeImageManifestURL
      ?? URL(fileURLWithPath: "/invalid-readonly-runtime-images-v2.json")
    self.home = home
    self.fileManager = fileManager
    self.runtimePlacementHook = runtimePlacementHook
  }

  func statuses() throws -> [ManagedDependencyID: ManagedDependencyStatus] {
    let (manifest, digest) = try loadManifest()
    return try Dictionary(uniqueKeysWithValues: ManagedDependencyID.allCases.map { id in
      if id == .embeddingRuntime { return (id, try runtimeImageStatus()) }
      let entry = try selectedEntry(id, manifest: manifest)
      return (id, try status(entry: entry, id: id, manifestDigest: digest))
    })
  }

  func status(_ id: ManagedDependencyID) throws -> ManagedDependencyStatus {
    if id == .embeddingRuntime { return try runtimeImageStatus() }
    let (manifest, digest) = try loadManifest()
    let entry = try selectedEntry(id, manifest: manifest)
    return try status(entry: entry, id: id, manifestDigest: digest)
  }

  func install(_ id: ManagedDependencyID) async throws -> String {
    do {
      if id == .embeddingRuntime { return try await installRuntimeImage() }
      try Task.checkCancellation()
      let (manifest, digest) = try loadManifest()
      let entry = try selectedEntry(id, manifest: manifest)
      if case .installed(let version) = try status(entry: entry, id: id, manifestDigest: digest) {
        return version
      }
      let destination = try destinationURL(entry: entry, id: id)
      if fileManager.fileExists(atPath: destination.path) { throw ManagedDependencyError.conflict }
      try ensurePrivateDirectory(destination.deletingLastPathComponent())
      let polaris = home.appendingPathComponent(".polaris", isDirectory: true)
      try ensurePrivateDirectory(polaris)
      let staging = polaris.appendingPathComponent(".install-\(UUID().uuidString)", isDirectory: true)
      try ensurePrivateDirectory(staging)
      defer { try? fileManager.removeItem(at: staging) }
      try checkFreeSpace(required: requiredSpace(entry), at: polaris)

      let payload: URL
      if let files = entry.files {
        payload = try await installFiles(files, entry: entry, staging: staging)
      } else {
        payload = try await installArchive(entry, id: id, staging: staging)
      }
      let runtimeTreeDigest = id == .embeddingRuntime
        ? try Self.writeRuntimeTreeManifest(at: payload, fileManager: fileManager)
        : nil
      try Task.checkCancellation()
      try validatePayload(payload, entry: entry, id: id)
      try fileManager.moveItem(at: payload, to: destination)
      do {
        try writeReceipt(
          entry: entry, id: id, manifestDigest: digest,
          runtimeTreeDigest: runtimeTreeDigest, destination: destination)
      } catch {
        try? fileManager.removeItem(at: destination)
        throw error
      }
      return entry.version
    } catch is CancellationError {
      throw ManagedDependencyError.cancelled
    }
  }

  func activatePreset() throws {
    let (manifest, digest) = try loadManifest()
    var selected: [ManagedDependencyID: DependencyEntry] = [:]
    for id in ManagedDependencyID.preset {
      if id == .embeddingRuntime {
        guard case .installed = try runtimeImageStatus() else {
          throw ManagedDependencyError.runtimeUnavailable
        }
        continue
      }
      let entry = try selectedEntry(id, manifest: manifest)
      guard case .installed = try status(entry: entry, id: id, manifestDigest: digest) else {
        throw ManagedDependencyError.install
      }
      selected[id] = entry
    }
    guard let small = selected[.multilingualE5Small],
          let base = selected[.multilingualE5Base],
          let llama = selected[.llamaCpp],
          let llamaExecutable = llama.requiredExecutable else {
      throw ManagedDependencyError.manifest
    }
    let smallRoot = try destinationURL(entry: small, id: .multilingualE5Small)
    let baseRoot = try destinationURL(entry: base, id: .multilingualE5Base)
    let llamaRoot = try destinationURL(entry: llama, id: .llamaCpp)
    let llamaServer = llamaRoot.appendingPathComponent(llamaExecutable)
    let values = [
      "model_path": smallRoot.path,
      "revision": small.version,
      "preselect_model_path": baseRoot.path,
      "preselect_revision": base.version,
      "llama_server": llamaServer.path,
      "llama_models": ggufRoot().path,
      "llama_build": llama.version,
    ]
    let body = try """
    # Generated by Polaris. Explicit config.toml values take precedence.
    [embedding]
    model_path = \(tomlString(values["model_path"]!))
    revision = \(tomlString(values["revision"]!))
    preselect_model_path = \(tomlString(values["preselect_model_path"]!))
    preselect_revision = \(tomlString(values["preselect_revision"]!))

    [llama_cpp]
    server = \(tomlString(values["llama_server"]!))
    models_dir = \(tomlString(values["llama_models"]!))
    build = \(tomlString(values["llama_build"]!))
    """
    try writeManagedResources(Data(body.utf8))
  }

  private func loadRuntimeImageLock() throws -> ReadonlyRuntimeImageLockV2 {
    let values = try runtimeImageManifestURL.resourceValues(
      forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
    guard values.isRegularFile == true, values.isSymbolicLink != true,
          let size = values.fileSize, size <= 16 * 1024 * 1024 else {
      throw ManagedDependencyError.manifest
    }
    let lock = try JSONDecoder().decode(
      ReadonlyRuntimeImageLockV2.self,
      from: Data(contentsOf: runtimeImageManifestURL, options: [.mappedIfSafe]))
    guard lock.schemaVersion == 2, lock.images.count <= 8 else {
      throw ManagedDependencyError.manifest
    }
    return lock
  }

  private func selectedRuntimeImage() throws -> ReadonlyRuntimeImageV2 {
#if arch(arm64)
    let architecture = "arm64"
#elseif arch(x86_64)
    let architecture = "x86_64"
#else
    throw ManagedDependencyError.unsupportedPlatform
#endif
    let matches = try loadRuntimeImageLock().images.filter {
      $0.id == "python-e5-cat" && $0.platform == "macos" && $0.architecture == architecture
    }
    guard matches.count == 1, let image = matches.first else {
      throw ManagedDependencyError.runtimeUnavailable
    }
    guard validRuntimeComponent(image.version), validRuntimeComponent(image.imageFilename),
          image.imageFilename.hasSuffix(".dmg"), image.imageSize > 0,
          image.imageSize <= Self.runtimeTreeMaxBytes, validDigest(image.imageSHA256),
          validDigest(image.treeManifestSHA256),
          (image.sourceURL == nil) != (image.bundledResource == nil),
          image.sourceURL.map(validDownloadURL) ?? true,
          image.bundledResource.map(Self.validRuntimeRelativePath) ?? true
    else { throw ManagedDependencyError.manifest }
    return image
  }

  private func runtimeImageDestination(_ image: ReadonlyRuntimeImageV2) -> URL {
    home.appendingPathComponent(".polaris/runtime-images/python-e5-cat")
      .appendingPathComponent(image.version, isDirectory: true)
      .appendingPathComponent(image.imageFilename)
  }

  private func runtimeImageStatus() throws -> ManagedDependencyStatus {
    let image: ReadonlyRuntimeImageV2
    do { image = try selectedRuntimeImage() }
    catch ManagedDependencyError.runtimeUnavailable { return .repairRequired }
    let destination = runtimeImageDestination(image)
    do {
      guard try runtimeImageAncestryExistsAndIsPhysical(image) else { return .notInstalled }
      var info = stat()
      if lstat(destination.path, &info) != 0 {
        return errno == ENOENT ? .notInstalled : .repairRequired
      }
      guard (info.st_mode & S_IFMT) == S_IFREG,
            info.st_size == image.imageSize,
            try fileDigest(destination) == image.imageSHA256 else {
        return .repairRequired
      }
      return .installed(version: image.version)
    } catch { return .repairRequired }
  }

  private func installRuntimeImage() async throws -> String {
    let image = try selectedRuntimeImage()
    if case .installed = try runtimeImageStatus() { return image.version }
    let chain = try RuntimeDirectoryChain(
      home: home, components: [".polaris", "runtime-images", "python-e5-cat", image.version],
      create: true)
    try chain.verify()
    let polaris = home.appendingPathComponent(".polaris", isDirectory: true)
    try checkFreeSpace(required: image.imageSize * 2 + 256 * 1024 * 1024, at: polaris)
    let stagingName = ".runtime-image-install-\(UUID())"
    guard mkdirat(chain.polarisDescriptor, stagingName, mode_t(0o700)) == 0,
          fsync(chain.polarisDescriptor) == 0 else {
      throw ManagedDependencyError.install
    }
    let stagingFD = openat(
      chain.polarisDescriptor, stagingName,
      O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
    guard stagingFD >= 0 else { throw ManagedDependencyError.install }
    defer {
      Darwin.close(stagingFD)
      _ = unlinkat(chain.polarisDescriptor, stagingName, AT_REMOVEDIR)
    }
    let staging = polaris.appendingPathComponent(stagingName, isDirectory: true)
    let candidate: URL
    if let url = image.sourceURL {
      candidate = try await download(url, expectedSize: image.imageSize, staging: staging)
    } else if let resource = image.bundledResource {
      let source = runtimeImageManifestURL.deletingLastPathComponent().appendingPathComponent(resource)
      let values = try source.resourceValues(
        forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
      guard values.isRegularFile == true, values.isSymbolicLink != true,
            values.fileSize == Int(image.imageSize) else { throw ManagedDependencyError.integrity }
      candidate = staging.appendingPathComponent("bundled-image")
      try fileManager.copyItem(at: source, to: candidate)
    } else { throw ManagedDependencyError.manifest }
    guard candidate.deletingLastPathComponent().standardizedFileURL == staging.standardizedFileURL,
          validRuntimeComponent(candidate.lastPathComponent) else {
      throw ManagedDependencyError.integrity
    }
    let candidateName = candidate.lastPathComponent
    defer { _ = unlinkat(stagingFD, candidateName, 0) }
    let candidateFD = openat(
      stagingFD, candidateName, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
    guard candidateFD >= 0 else { throw ManagedDependencyError.integrity }
    defer { Darwin.close(candidateFD) }
    var candidateInfo = stat()
    guard fstat(candidateFD, &candidateInfo) == 0,
          candidateInfo.st_mode & S_IFMT == S_IFREG,
          candidateInfo.st_nlink == 1,
          candidateInfo.st_size == image.imageSize,
          try fileDigest(descriptor: candidateFD) == image.imageSHA256,
          fchmod(candidateFD, mode_t(0o400)) == 0,
          fsync(candidateFD) == 0 else {
      throw ManagedDependencyError.integrity
    }
    let pendingName = ".\(image.imageFilename).pending-\(UUID())"
    var imagePublished = false
    var receiptPublished = false
    do {
      try chain.verify()
      // Test-only fault injection occurs after the last pathname check. All
      // writes below remain pinned to the already-open version directory.
      try runtimePlacementHook?()
      guard renameatx_np(
        stagingFD, candidateName, chain.versionDescriptor, pendingName,
        UInt32(RENAME_EXCL)) == 0 else {
        throw ManagedDependencyError.conflict
      }
      try requireSameRuntimeFile(
        descriptor: candidateFD, directory: chain.versionDescriptor, name: pendingName)
      guard renameatx_np(
        chain.versionDescriptor, pendingName,
        chain.versionDescriptor, image.imageFilename,
        UInt32(RENAME_EXCL)) == 0 else {
        throw ManagedDependencyError.conflict
      }
      imagePublished = true
      try requireSameRuntimeFile(
        descriptor: candidateFD, directory: chain.versionDescriptor, name: image.imageFilename)
      guard fsync(chain.versionDescriptor) == 0 else { throw ManagedDependencyError.install }
      try chain.verify()
      let receipt = ReadonlyRuntimeReceiptV2(
        schemaVersion: 2, id: image.id, version: image.version,
        imageSHA256: image.imageSHA256, treeManifestSHA256: image.treeManifestSHA256,
        installedAt: ISO8601DateFormatter().string(from: Date()))
      try writeRuntimeReceipt(JSONEncoder().encode(receipt), directory: chain.versionDescriptor)
      receiptPublished = true
      guard fsync(chain.versionDescriptor) == 0 else { throw ManagedDependencyError.install }
      try chain.verify()
    } catch {
      _ = unlinkat(chain.versionDescriptor, pendingName, 0)
      if receiptPublished { _ = unlinkat(chain.versionDescriptor, "receipt.json", 0) }
      if imagePublished { _ = unlinkat(chain.versionDescriptor, image.imageFilename, 0) }
      _ = fsync(chain.versionDescriptor)
      throw error
    }
    return image.version
  }

  private func runtimeImageDirectories(_ image: ReadonlyRuntimeImageV2) -> [URL] {
    let polaris = home.appendingPathComponent(".polaris", isDirectory: true)
    let runtimeImages = polaris.appendingPathComponent("runtime-images", isDirectory: true)
    let runtime = runtimeImages.appendingPathComponent("python-e5-cat", isDirectory: true)
    return [
      polaris,
      runtimeImages,
      runtime,
      runtime.appendingPathComponent(image.version, isDirectory: true),
    ]
  }

  private func runtimeImageAncestryExistsAndIsPhysical(
    _ image: ReadonlyRuntimeImageV2
  ) throws -> Bool {
    for directory in runtimeImageDirectories(image) {
      var info = stat()
      if lstat(directory.path, &info) != 0 {
        if errno == ENOENT { return false }
        throw ManagedDependencyError.conflict
      }
      guard (info.st_mode & S_IFMT) == S_IFDIR else {
        throw ManagedDependencyError.conflict
      }
    }
    return true
  }


  private func validRuntimeComponent(_ value: String) -> Bool {
    !value.isEmpty && value.utf8.count <= 128 && value != "." && value != ".."
      && !value.contains("/") && !value.contains("\0")
  }

  func inspectGGUF(_ source: String) async throws -> ManagedGGUFPlan {
    let trimmed = source.trimmingCharacters(in: .whitespacesAndNewlines)
    guard trimmed == source, let sourceURL = URL(string: source), validDownloadURL(sourceURL) else {
      throw ManagedDependencyError.manifest
    }
    let downloadURL = try resolvedGGUFURL(sourceURL)
    let filename = try ggufFilename(downloadURL)
    let (size, finalURL) = try await metadata(downloadURL)
    guard size > 0, size <= 64 * 1024 * 1024 * 1024, finalURL.scheme == "https" else {
      throw ManagedDependencyError.invalidResponse
    }
    return ManagedGGUFPlan(sourceURL: sourceURL, downloadURL: downloadURL,
                           filename: filename, size: size)
  }

  func installedGGUFModels() throws -> [ManagedGGUFModel] {
    let root = ggufRoot()
    let catalogURL = root.appendingPathComponent("catalog.json")
    guard fileManager.fileExists(atPath: catalogURL.path) else { return [] }
    let metadata = try fileManager.attributesOfItem(atPath: catalogURL.path)
    guard (metadata[.type] as? FileAttributeType) == .typeRegular,
          (metadata[.size] as? NSNumber)?.intValue ?? Int.max <= 1024 * 1024
    else { throw ManagedDependencyError.integrity }
    let catalog = try JSONDecoder().decode(
      ManagedGGUFCatalog.self, from: Data(contentsOf: catalogURL, options: [.mappedIfSafe]))
    guard catalog.schemaVersion == 1, catalog.models.count <= 128,
          Set(catalog.models.map(\.filename)).count == catalog.models.count
    else { throw ManagedDependencyError.integrity }
    return try catalog.models.sorted { $0.filename < $1.filename }.map { model in
      guard try ggufFilename(URL(fileURLWithPath: model.filename)) == model.filename,
            validDownloadURL(model.sourceURL), validDigest(model.sha256), model.size > 0
      else { throw ManagedDependencyError.integrity }
      let file = root.appendingPathComponent(model.filename)
      let values = try fileManager.attributesOfItem(atPath: file.path)
      guard (values[.type] as? FileAttributeType) == .typeRegular,
            (values[.size] as? NSNumber)?.int64Value == model.size
      else { throw ManagedDependencyError.integrity }
      return model
    }
  }

  func installGGUF(_ plan: ManagedGGUFPlan) async throws -> ManagedGGUFModel {
    do {
      guard try resolvedGGUFURL(plan.sourceURL) == plan.downloadURL,
            try ggufFilename(plan.downloadURL) == plan.filename, plan.size > 0
      else { throw ManagedDependencyError.manifest }
      let root = ggufRoot()
      try ensurePrivateDirectory(root)
      let destination = root.appendingPathComponent(plan.filename)
      guard !fileManager.fileExists(atPath: destination.path) else {
        throw ManagedDependencyError.conflict
      }
      try checkFreeSpace(required: plan.size * 2 + 256 * 1024 * 1024, at: root)
      let polaris = home.appendingPathComponent(".polaris", isDirectory: true)
      let staging = polaris.appendingPathComponent(".model-install-\(UUID().uuidString)", isDirectory: true)
      try ensurePrivateDirectory(staging)
      defer { try? fileManager.removeItem(at: staging) }
      let temporary = try await download(plan.downloadURL, expectedSize: plan.size, staging: staging)
      let digest = try fileDigest(temporary)
      try Task.checkCancellation()
      try fileManager.moveItem(at: temporary, to: destination)
      try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
      let installed = ManagedGGUFModel(
        filename: plan.filename, sourceURL: plan.sourceURL, size: plan.size,
        sha256: digest, installedAt: ISO8601DateFormatter().string(from: Date()))
      do {
        var models = (try? installedGGUFModels()) ?? []
        models.append(installed)
        let catalog = ManagedGGUFCatalog(schemaVersion: 1, models: models.sorted { $0.filename < $1.filename })
        let data = try JSONEncoder().encode(catalog)
        try data.write(to: root.appendingPathComponent("catalog.json"), options: [.atomic])
      } catch {
        try? fileManager.removeItem(at: destination)
        throw error
      }
      return installed
    } catch is CancellationError {
      throw ManagedDependencyError.cancelled
    }
  }

  private func loadManifest() throws -> (DependencyManifest, String) {
    let data = try Data(contentsOf: manifestURL, options: [.mappedIfSafe])
    guard data.count <= 512 * 1024 else { throw ManagedDependencyError.manifest }
    let manifest = try JSONDecoder().decode(DependencyManifest.self, from: data)
    guard manifest.schemaVersion == 1, !manifest.dependencies.isEmpty else {
      throw ManagedDependencyError.manifest
    }
    try manifest.dependencies.forEach(validateEntry)
    return (manifest, hexDigest(data))
  }

  private func selectedEntry(_ id: ManagedDependencyID,
                             manifest: DependencyManifest) throws -> DependencyEntry {
    let architecture: String
#if arch(arm64)
    architecture = "arm64"
#elseif arch(x86_64)
    architecture = "x86_64"
#else
    throw ManagedDependencyError.unsupportedPlatform
#endif
    let matches = manifest.dependencies.filter {
      $0.id == id.rawValue && $0.platform == "macos"
        && ($0.architecture == architecture || $0.architecture == "universal")
    }
    guard matches.count == 1, let entry = matches.first else {
      throw ManagedDependencyError.unsupportedPlatform
    }
    return entry
  }

  private func validateEntry(_ entry: DependencyEntry) throws {
    guard !entry.version.isEmpty, entry.version.utf8.count <= 128,
          !entry.version.contains("/"), !entry.version.contains(".."),
          entry.requiredSpace.map({ $0 > 0 && $0 <= 32 * 1024 * 1024 * 1024 }) ?? true
    else { throw ManagedDependencyError.manifest }
    if let files = entry.files {
      guard entry.url == nil, entry.archive == nil, entry.installKind == nil,
            entry.pythonVersion == nil, entry.pythonPackages == nil,
            !files.isEmpty, files.count <= 16 else {
        throw ManagedDependencyError.manifest
      }
      var paths = Set<String>()
      for file in files {
        guard validRelative(file.path), paths.insert(file.path).inserted,
              validDownloadURL(file.url), validDigest(file.sha256), file.size > 0
        else { throw ManagedDependencyError.manifest }
      }
      if entry.id == ManagedDependencyID.catTranslate3B.rawValue {
        guard entry.version == Self.catTranslateRevision,
              entry.destination == Self.catTranslateDestination,
              entry.requiredSpace == nil
        else { throw ManagedDependencyError.manifest }
      }
    } else {
      guard let url = entry.url, validDownloadURL(url),
            let digest = entry.sha256, validDigest(digest),
            let size = entry.compressedSize, size > 0,
            entry.archive == "tar_gzip" || entry.archive == "tar_xz",
            entry.archiveRoot.map(validRelative) == true,
            entry.requiredExecutable.map(validRelative) == true
      else { throw ManagedDependencyError.manifest }
      if let installer = entry.installer, !validRelative(installer) {
        throw ManagedDependencyError.manifest
      }
      if entry.installKind == "uv_embedding" {
        guard entry.id == ManagedDependencyID.embeddingRuntime.rawValue,
              entry.legacyRuntime == true,
              entry.installer == nil,
              entry.pythonVersion == "3.11.9",
              let packages = entry.pythonPackages,
              Set(packages.keys) == Set([
                "torch", "transformers", "tokenizers", "safetensors", "numpy", "sentencepiece",
              ]),
              packages.values.allSatisfy({ validVersion($0) }),
              entry.excludeNewer == "2026-09-19T00:00:00Z"
        else { throw ManagedDependencyError.manifest }
      } else if entry.installKind != nil || entry.pythonVersion != nil
                  || entry.pythonPackages != nil || entry.excludeNewer != nil
                  || entry.legacyRuntime != nil {
        throw ManagedDependencyError.manifest
      }
    }
  }

  private func status(entry: DependencyEntry, id: ManagedDependencyID,
                      manifestDigest: String) throws -> ManagedDependencyStatus {
    let destination = try destinationURL(entry: entry, id: id)
    var directory: ObjCBool = false
    guard fileManager.fileExists(atPath: destination.path, isDirectory: &directory) else {
      return .notInstalled
    }
    guard directory.boolValue else { return .failed("導入先がフォルダではありません。") }
    let receiptMatches: Bool = {
      let receiptURL = receiptURL(for: destination)
      guard let attributes = try? fileManager.attributesOfItem(atPath: receiptURL.path),
            (attributes[.type] as? FileAttributeType) == .typeRegular,
            ((attributes[.size] as? NSNumber)?.intValue ?? Int.max) <= 16 * 1024,
            let values = try? receiptURL.resourceValues(forKeys: [.isSymbolicLinkKey]),
            values.isSymbolicLink != true,
            let data = try? Data(contentsOf: receiptURL),
            let receipt = try? JSONDecoder().decode(DependencyReceipt.self, from: data)
      else { return false }
      let runtimeDigestMatches: Bool
      if id == .embeddingRuntime {
        let tree = destination.appendingPathComponent(Self.runtimeTreeManifestName)
        runtimeDigestMatches = receipt.runtimeTreeSHA256 != nil
          && (try? fileDigest(tree)) == receipt.runtimeTreeSHA256
      } else {
        runtimeDigestMatches = receipt.runtimeTreeSHA256 == nil
      }
      return receipt.schemaVersion == 1 && receipt.id == id.rawValue
        && receipt.version == entry.version && receipt.manifestSHA256 == manifestDigest
        && runtimeDigestMatches
    }()
    let existingPinnedModel = (id == .multilingualE5Small || id == .multilingualE5Base
      || id == .catTranslate3B)
      && modelManifestMatches(destination, entry: entry)
    guard receiptMatches || existingPinnedModel else {
      return .failed("導入情報が固定manifestと一致しません。")
    }
    do {
      // Status runs during normal startup. Full CAT hashing is retained when
      // bytes are installed and by the runtime's use-time preflight; here keep
      // the check bounded to receipt, manifest identity, sizes, and symlinks.
      try validatePayload(destination, entry: entry, id: id, verifyHashes: false)
      return .installed(version: entry.version)
    } catch {
      return .failed("導入済みファイルを確認できませんでした。")
    }
  }

  private func modelManifestMatches(_ destination: URL, entry: DependencyEntry) -> Bool {
    guard let expected = entry.files,
          let data = try? Data(contentsOf: destination.appendingPathComponent("manifest.json")),
          data.count <= 1024 * 1024,
          let manifest = try? JSONDecoder().decode(InstalledModelManifest.self, from: data),
          manifest.revision == entry.version,
          manifest.files == Dictionary(uniqueKeysWithValues: expected.map { ($0.path, $0.sha256) })
    else { return false }
    return expected.allSatisfy { file in
      let path = destination.appendingPathComponent(file.path).path
      guard let attributes = try? fileManager.attributesOfItem(atPath: path) else { return false }
      return (attributes[.type] as? FileAttributeType) == .typeRegular
        && (attributes[.size] as? NSNumber)?.int64Value == file.size
    }
  }

  private func destinationURL(entry: DependencyEntry, id: ManagedDependencyID) throws -> URL {
    let root = home.appendingPathComponent(".polaris", isDirectory: true)
    switch id {
    case .llamaCpp:
      return root.appendingPathComponent("runtimes/llama.cpp/\(entry.version)", isDirectory: true)
    case .rust:
      return root.appendingPathComponent("toolchains/rust/\(entry.version)", isDirectory: true)
    case .embeddingRuntime:
      return root.appendingPathComponent("runtimes/embedding/\(entry.version)", isDirectory: true)
    case .multilingualE5Small:
      guard entry.destination == "models/multilingual-e5-small" else {
        throw ManagedDependencyError.manifest
      }
      return root.appendingPathComponent("models/multilingual-e5-small/\(entry.version)", isDirectory: true)
    case .multilingualE5Base:
      guard entry.destination == "models/multilingual-e5-base" else {
        throw ManagedDependencyError.manifest
      }
      return root.appendingPathComponent("models/multilingual-e5-base/\(entry.version)", isDirectory: true)
    case .catTranslate3B:
      guard entry.version == Self.catTranslateRevision,
            entry.destination == Self.catTranslateDestination else {
        throw ManagedDependencyError.manifest
      }
      return root.appendingPathComponent("\(Self.catTranslateDestination)/\(Self.catTranslateRevision)", isDirectory: true)
    }
  }

  private func installFiles(_ files: [DependencyFile], entry: DependencyEntry,
                            staging: URL) async throws -> URL {
    let payload = staging.appendingPathComponent("payload", isDirectory: true)
    try ensurePrivateDirectory(payload)
    for file in files {
      try Task.checkCancellation()
      let temporary = try await download(file.url, expectedSize: file.size, staging: staging)
      guard try fileDigest(temporary) == file.sha256 else { throw ManagedDependencyError.integrity }
      let destination = payload.appendingPathComponent(file.path)
      try fileManager.moveItem(at: temporary, to: destination)
    }
    let fileMap = Dictionary(uniqueKeysWithValues: files.map { ($0.path, $0.sha256) })
    let modelManifest = try JSONSerialization.data(
      withJSONObject: ["revision": entry.version, "files": fileMap], options: [.prettyPrinted, .sortedKeys])
    try modelManifest.write(to: payload.appendingPathComponent("manifest.json"), options: .atomic)
    return payload
  }

  private func installArchive(_ entry: DependencyEntry, id: ManagedDependencyID,
                              staging: URL) async throws -> URL {
    guard let url = entry.url, let expectedSize = entry.compressedSize,
          let expectedDigest = entry.sha256, let archive = entry.archive,
          let archiveRoot = entry.archiveRoot else { throw ManagedDependencyError.manifest }
    let downloaded = try await download(url, expectedSize: expectedSize, staging: staging)
    guard try fileDigest(downloaded) == expectedDigest else { throw ManagedDependencyError.integrity }
    let names = try await runProcess("/usr/bin/tar", ["-tf", downloaded.path], capture: true)
    let entries = String(decoding: names, as: UTF8.self).split(separator: "\n", omittingEmptySubsequences: true)
    guard !entries.isEmpty, entries.count <= 4096,
          entries.allSatisfy({ raw in
            let name = String(raw).hasSuffix("/") ? String(raw.dropLast()) : String(raw)
            return validRelative(name) && (name == archiveRoot || name.hasPrefix(archiveRoot + "/"))
          })
    else { throw ManagedDependencyError.archive }
    let extracted = staging.appendingPathComponent("extracted", isDirectory: true)
    try ensurePrivateDirectory(extracted)
    let mode = archive == "tar_gzip" ? "-xzf" : "-xJf"
    try await runProcess("/usr/bin/tar", [mode, downloaded.path, "-C", extracted.path], capture: false)
    let root = extracted.appendingPathComponent(archiveRoot, isDirectory: true)
    if id == .rust {
      guard let installer = entry.installer else { throw ManagedDependencyError.manifest }
      let payload = staging.appendingPathComponent("payload", isDirectory: true)
      try ensurePrivateDirectory(payload)
      try await runProcess(
        "/bin/sh", [root.appendingPathComponent(installer).path,
                    "--prefix=\(payload.path)", "--disable-ldconfig"],
        capture: false, environment: ["PATH": "/usr/bin:/bin", "LANG": "C"])
      return payload
    }
    if id == .embeddingRuntime {
      return try await installEmbeddingRuntime(uvRoot: root, entry: entry, staging: staging)
    }
    return root
  }

  private func installEmbeddingRuntime(uvRoot: URL, entry: DependencyEntry,
                                       staging: URL) async throws -> URL {
    guard entry.installKind == "uv_embedding", let pythonVersion = entry.pythonVersion,
          let packages = entry.pythonPackages, let excludeNewer = entry.excludeNewer else {
      throw ManagedDependencyError.manifest
    }
    let uv = uvRoot.appendingPathComponent("uv")
    guard fileManager.isExecutableFile(atPath: uv.path) else { throw ManagedDependencyError.install }
    let payload = staging.appendingPathComponent("payload", isDirectory: true)
    let pythonStore = payload.appendingPathComponent("python", isDirectory: true)
    let runtime = payload.appendingPathComponent("runtime", isDirectory: true)
    let isolatedHome = staging.appendingPathComponent("home", isDirectory: true)
    try ensurePrivateDirectory(payload)
    try ensurePrivateDirectory(isolatedHome)
    let environment = ["HOME": isolatedHome.path, "PATH": "/usr/bin:/bin", "LANG": "C",
                       "LC_ALL": "C", "UV_NO_CONFIG": "1"]
    try await runProcess(
      uv.path,
      ["python", "install", pythonVersion, "--install-dir", pythonStore.path, "--no-bin",
       "--no-registry", "--managed-python", "--no-cache", "--no-progress", "--no-config"],
      capture: false, environment: environment)
    let children = try fileManager.contentsOfDirectory(
      at: pythonStore, includingPropertiesForKeys: [.isDirectoryKey], options: [.skipsHiddenFiles])
    guard children.count == 1,
          (try children[0].resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true else {
      throw ManagedDependencyError.install
    }
    try fileManager.moveItem(at: children[0], to: runtime)
    try fileManager.removeItem(at: pythonStore)
    guard let required = entry.requiredExecutable else { throw ManagedDependencyError.manifest }
    let python = payload.appendingPathComponent(required)
    guard fileManager.isExecutableFile(atPath: python.path) else { throw ManagedDependencyError.install }
    let requirements = packages.keys.sorted().map { "\($0)==\(packages[$0]!)" }
    var arguments = ["pip", "install", "--python", python.path, "--exact", "--no-cache",
                     "--no-progress", "--no-config", "--no-python-downloads",
                     "--only-binary", ":all:", "--exclude-newer", excludeNewer]
    arguments.append(contentsOf: requirements)
    try await runProcess(uv.path, arguments, capture: false, environment: environment)
    let packageExpression = packages.keys.sorted().map { "\"\($0)\"" }.joined(separator: ",")
    let receipt = try await runProcess(
      python.path,
      ["-I", "-c", "import importlib.metadata,json; n=[\(packageExpression)]; print(json.dumps({k:importlib.metadata.version(k) for k in n},sort_keys=True,separators=(',',':')))"],
      capture: true, environment: ["HOME": isolatedHome.path, "PATH": "/usr/bin:/bin",
                                   "LANG": "C", "LC_ALL": "C", "PYTHONNOUSERSITE": "1"])
    let observed = try JSONDecoder().decode([String: String].self, from: receipt)
    guard observed == packages else { throw ManagedDependencyError.integrity }
    let encoded = try JSONEncoder().encode(observed)
    try encoded.write(to: payload.appendingPathComponent("embedding-packages.json"), options: [.atomic])
    return payload
  }

  private func download(_ url: URL, expectedSize: Int64, staging: URL) async throws -> URL {
    try Task.checkCancellation()
    let configuration = URLSessionConfiguration.ephemeral
    configuration.urlCache = nil
    configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
    configuration.timeoutIntervalForRequest = 60
    configuration.timeoutIntervalForResource = 60 * 60
    let session = URLSession(configuration: configuration, delegate: HTTPSRedirectDelegate(), delegateQueue: nil)
    defer { session.invalidateAndCancel() }
    var request = URLRequest(url: url)
    request.httpMethod = "HEAD"
    let (_, metadataResponse) = try await session.data(for: request)
    guard let metadata = metadataResponse as? HTTPURLResponse, metadata.statusCode == 200,
          metadata.url?.scheme == "https", metadata.expectedContentLength == expectedSize
    else { throw ManagedDependencyError.invalidResponse }
    try Task.checkCancellation()
    let (temporary, response) = try await session.download(from: url)
    guard let response = response as? HTTPURLResponse, response.statusCode == 200,
          response.url?.scheme == "https", response.expectedContentLength == expectedSize
    else { throw ManagedDependencyError.invalidResponse }
    let attributes = try fileManager.attributesOfItem(atPath: temporary.path)
    guard (attributes[.type] as? FileAttributeType) == .typeRegular,
          (attributes[.size] as? NSNumber)?.int64Value == expectedSize
    else { throw ManagedDependencyError.integrity }
    let owned = staging.appendingPathComponent("download-\(UUID().uuidString)")
    try fileManager.moveItem(at: temporary, to: owned)
    return owned
  }

  private func metadata(_ url: URL) async throws -> (Int64, URL) {
    let configuration = URLSessionConfiguration.ephemeral
    configuration.urlCache = nil
    configuration.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
    configuration.timeoutIntervalForRequest = 60
    let session = URLSession(configuration: configuration, delegate: HTTPSRedirectDelegate(), delegateQueue: nil)
    defer { session.invalidateAndCancel() }
    var request = URLRequest(url: url)
    request.httpMethod = "HEAD"
    let (_, response) = try await session.data(for: request)
    guard let http = response as? HTTPURLResponse, http.statusCode == 200,
          let finalURL = http.url, finalURL.scheme == "https",
          http.expectedContentLength > 0
    else { throw ManagedDependencyError.invalidResponse }
    return (http.expectedContentLength, finalURL)
  }

  private func resolvedGGUFURL(_ source: URL) throws -> URL {
    guard validDownloadURL(source) else { throw ManagedDependencyError.manifest }
    if source.host?.lowercased() != "huggingface.co" {
      _ = try ggufFilename(source)
      return source
    }
    var parts = source.path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
    guard parts.count >= 5, parts[2] == "blob" || parts[2] == "resolve" else {
      throw ManagedDependencyError.manifest
    }
    parts[2] = "resolve"
    var components = URLComponents(url: source, resolvingAgainstBaseURL: false)
    components?.path = "/" + parts.joined(separator: "/")
    components?.fragment = nil
    guard let result = components?.url else { throw ManagedDependencyError.manifest }
    _ = try ggufFilename(result)
    return result
  }

  private func ggufFilename(_ url: URL) throws -> String {
    let filename = url.lastPathComponent.removingPercentEncoding ?? url.lastPathComponent
    guard !filename.isEmpty, filename.utf8.count <= 255,
          filename.lowercased().hasSuffix(".gguf"), filename != ".", filename != "..",
          !filename.contains("/"), !filename.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
    else { throw ManagedDependencyError.manifest }
    return filename
  }

  private func ggufRoot() -> URL {
    home.appendingPathComponent(".polaris/models/llama.cpp", isDirectory: true)
  }

  private func validatePayload(_ payload: URL, entry: DependencyEntry, id: ManagedDependencyID,
                               verifyHashes: Bool = true) throws {
    var directory: ObjCBool = false
    guard fileManager.fileExists(atPath: payload.path, isDirectory: &directory), directory.boolValue else {
      throw ManagedDependencyError.install
    }
    if let files = entry.files {
      for file in files {
        let url = payload.appendingPathComponent(file.path)
        let attributes = try fileManager.attributesOfItem(atPath: url.path)
        let values = try url.resourceValues(forKeys: [.isSymbolicLinkKey])
        let hashMatches: Bool
        if verifyHashes { hashMatches = try fileDigest(url) == file.sha256 }
        else { hashMatches = true }
        guard (attributes[.type] as? FileAttributeType) == .typeRegular,
              values.isSymbolicLink != true,
              (attributes[.size] as? NSNumber)?.int64Value == file.size,
              hashMatches
        else { throw ManagedDependencyError.integrity }
      }
      guard fileManager.fileExists(atPath: payload.appendingPathComponent("manifest.json").path) else {
        throw ManagedDependencyError.integrity
      }
      return
    }
    guard let required = entry.requiredExecutable else { throw ManagedDependencyError.manifest }
    let executable = payload.appendingPathComponent(required)
    guard fileManager.isExecutableFile(atPath: executable.path),
          (try fileManager.attributesOfItem(atPath: executable.path)[.type] as? FileAttributeType) == .typeRegular
    else { throw ManagedDependencyError.install }
    if id == .llamaCpp {
      let resolved = executable.resolvingSymlinksInPath().path
      guard resolved == executable.path else { throw ManagedDependencyError.install }
    }
    if id == .embeddingRuntime {
      guard let expected = entry.pythonPackages,
            let data = try? Data(contentsOf: payload.appendingPathComponent("embedding-packages.json")),
            data.count <= 16 * 1024,
            (try? JSONDecoder().decode([String: String].self, from: data)) == expected
      else { throw ManagedDependencyError.integrity }
      _ = try Self.readRuntimeTreeManifest(at: payload, fileManager: fileManager)
    }
  }

  private func writeReceipt(entry: DependencyEntry, id: ManagedDependencyID,
                            manifestDigest: String, runtimeTreeDigest: String?,
                            destination: URL) throws {
    let receipt = DependencyReceipt(
      schemaVersion: 1, id: id.rawValue, version: entry.version,
      manifestSHA256: manifestDigest, runtimeTreeSHA256: runtimeTreeDigest,
      installedAt: ISO8601DateFormatter().string(from: Date()))
    let data = try JSONEncoder().encode(receipt)
    try data.write(to: receiptURL(for: destination), options: [.atomic])
  }

  private func receiptURL(for destination: URL) -> URL {
    destination.deletingLastPathComponent().appendingPathComponent(".\(destination.lastPathComponent).receipt.json")
  }

  @discardableResult
  static func writeRuntimeTreeManifest(
    at root: URL, fileManager: FileManager = .default
  ) throws -> String {
    let metadata = try root.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey])
    guard metadata.isDirectory == true, metadata.isSymbolicLink != true else {
      throw ManagedDependencyError.integrity
    }
    let manifestURL = root.appendingPathComponent(runtimeTreeManifestName)
    if fileManager.fileExists(atPath: manifestURL.path) {
      throw ManagedDependencyError.integrity
    }
    var files: [ManagedRuntimeTreeFile] = []
    var total: Int64 = 0
    var entries = 0

    func visit(_ directory: URL, relativePrefix: String, depth: Int) throws {
      guard depth <= 64 else { throw ManagedDependencyError.integrity }
      let children = try fileManager.contentsOfDirectory(
        at: directory,
        includingPropertiesForKeys: [.isDirectoryKey, .isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey],
        options: [])
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
      for child in children {
        entries += 1
        guard entries <= runtimeTreeMaxFiles else { throw ManagedDependencyError.integrity }
        let name = child.lastPathComponent
        guard name != ".", name != "..", !name.contains("/"), !name.contains("\0") else {
          throw ManagedDependencyError.integrity
        }
        let relative = relativePrefix.isEmpty ? name : "\(relativePrefix)/\(name)"
        guard validRuntimeRelativePath(relative) else { throw ManagedDependencyError.integrity }
        let values = try child.resourceValues(
          forKeys: [.isDirectoryKey, .isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
        guard values.isSymbolicLink != true else { throw ManagedDependencyError.integrity }
        if values.isDirectory == true {
          try visit(child, relativePrefix: relative, depth: depth + 1)
        } else {
          guard values.isRegularFile == true,
                let unsignedSize = values.fileSize,
                unsignedSize >= 0 else { throw ManagedDependencyError.integrity }
          let size = Int64(unsignedSize)
          let sum = total.addingReportingOverflow(size)
          guard !sum.overflow, files.count < runtimeTreeMaxFiles,
                sum.partialValue <= runtimeTreeMaxBytes else {
            throw ManagedDependencyError.integrity
          }
          total = sum.partialValue
          files.append(ManagedRuntimeTreeFile(
            path: relative, size: size, sha256: try fileDigestStatic(child)))
        }
      }
    }
    try visit(root, relativePrefix: "", depth: 0)
    let document = ManagedRuntimeTreeManifest(schemaVersion: 1, files: files)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    let data = try encoder.encode(document)
    guard data.count <= 16 * 1024 * 1024 else { throw ManagedDependencyError.integrity }
    try data.write(to: manifestURL, options: [.atomic])
    return hexDigestStatic(data)
  }

  private static func readRuntimeTreeManifest(
    at root: URL, fileManager: FileManager
  ) throws -> ManagedRuntimeTreeManifest {
    let url = root.appendingPathComponent(runtimeTreeManifestName)
    let values = try url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey, .fileSizeKey])
    guard values.isRegularFile == true, values.isSymbolicLink != true,
          let size = values.fileSize, size <= 16 * 1024 * 1024 else {
      throw ManagedDependencyError.integrity
    }
    let data = try Data(contentsOf: url)
    let manifest = try JSONDecoder().decode(ManagedRuntimeTreeManifest.self, from: data)
    var paths = Set<String>()
    var total: Int64 = 0
    guard manifest.schemaVersion == 1, manifest.files.count <= runtimeTreeMaxFiles else {
      throw ManagedDependencyError.integrity
    }
    for file in manifest.files {
      let sum = total.addingReportingOverflow(file.size)
      guard validRuntimeRelativePath(file.path), file.path != runtimeTreeManifestName,
            paths.insert(file.path).inserted, file.size >= 0, !sum.overflow,
            sum.partialValue <= runtimeTreeMaxBytes,
            file.sha256.count == 64,
            file.sha256.allSatisfy({ $0.isHexDigit && !$0.isUppercase }) else {
        throw ManagedDependencyError.integrity
      }
      total = sum.partialValue
    }
    return manifest
  }

  private static func validRuntimeRelativePath(_ path: String) -> Bool {
    let parts = path.split(separator: "/", omittingEmptySubsequences: false)
    return !parts.isEmpty && parts.count <= 64 && path.utf8.count <= 4096 && parts.allSatisfy { part in
      !part.isEmpty && part != "." && part != ".." && !part.contains("\0")
    }
  }

  private static func fileDigestStatic(_ url: URL) throws -> String {
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var hasher = SHA256()
    while true {
      let data = try handle.read(upToCount: 1024 * 1024) ?? Data()
      if data.isEmpty { break }
      hasher.update(data: data)
    }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
  }

  private static func hexDigestStatic(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
  }

  private func requiredSpace(_ entry: DependencyEntry) -> Int64 {
    if entry.id == ManagedDependencyID.catTranslate3B.rawValue {
      return Self.catTranslateRequiredSpaceBytes
    }
    if let required = entry.requiredSpace { return required }
    let bytes = entry.files?.reduce(0, { $0 + $1.size }) ?? entry.compressedSize ?? 0
    return max(512 * 1024 * 1024, bytes * 3 + 256 * 1024 * 1024)
  }

  private func checkFreeSpace(required: Int64, at url: URL) throws {
    let attributes = try fileManager.attributesOfFileSystem(forPath: url.path)
    guard let free = (attributes[.systemFreeSize] as? NSNumber)?.int64Value, free >= required else {
      throw ManagedDependencyError.insufficientSpace
    }
  }

  private func ensurePrivateDirectory(_ url: URL) throws {
    try fileManager.createDirectory(at: url, withIntermediateDirectories: true,
                                    attributes: [.posixPermissions: 0o700])
    try fileManager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
  }

  private func tomlString(_ value: String) throws -> String {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.withoutEscapingSlashes]
    let data = try encoder.encode(value)
    guard let encoded = String(data: data, encoding: .utf8) else {
      throw ManagedDependencyError.install
    }
    return encoded
  }

  private func writeManagedResources(_ data: Data) throws {
    let directory = home.appendingPathComponent(".polaris", isDirectory: true)
    try ensurePrivateDirectory(directory)
    let destination = directory.appendingPathComponent("managed-resources.toml")
    if fileManager.fileExists(atPath: destination.path) {
      let values = try destination.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey])
      guard values.isRegularFile == true, values.isSymbolicLink != true else {
        throw ManagedDependencyError.conflict
      }
    }
    let temporary = directory.appendingPathComponent(".managed-resources-\(UUID().uuidString).tmp")
    defer { try? fileManager.removeItem(at: temporary) }
    try data.write(to: temporary, options: [.withoutOverwriting])
    try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: temporary.path)
    if fileManager.fileExists(atPath: destination.path) {
      _ = try fileManager.replaceItemAt(destination, withItemAt: temporary)
    } else {
      try fileManager.moveItem(at: temporary, to: destination)
    }
    try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
  }

  private func requireSameRuntimeFile(descriptor: Int32, directory: Int32, name: String) throws {
    var held = stat()
    var entry = stat()
    guard fstat(descriptor, &held) == 0,
          fstatat(directory, name, &entry, AT_SYMLINK_NOFOLLOW) == 0,
          held.st_mode & S_IFMT == S_IFREG,
          entry.st_mode & S_IFMT == S_IFREG,
          held.st_dev == entry.st_dev, held.st_ino == entry.st_ino,
          held.st_nlink == 1, entry.st_nlink == 1 else {
      throw ManagedDependencyError.integrity
    }
  }

  private func writeRuntimeReceipt(_ data: Data, directory: Int32) throws {
    let descriptor = openat(
      directory, "receipt.json",
      O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
      mode_t(0o600))
    guard descriptor >= 0 else { throw ManagedDependencyError.conflict }
    var keep = false
    defer {
      Darwin.close(descriptor)
      if !keep { _ = unlinkat(directory, "receipt.json", 0) }
    }
    try data.withUnsafeBytes { bytes in
      var offset = 0
      while offset < bytes.count {
        let count = Darwin.write(
          descriptor, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
        if count < 0, errno == EINTR { continue }
        guard count > 0 else { throw ManagedDependencyError.install }
        offset += count
      }
    }
    guard fsync(descriptor) == 0 else { throw ManagedDependencyError.install }
    var info = stat()
    guard fstat(descriptor, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
          info.st_nlink == 1 else {
      throw ManagedDependencyError.integrity
    }
    keep = true
  }

  private func fileDigest(descriptor: Int32) throws -> String {
    var hasher = SHA256()
    var buffer = [UInt8](repeating: 0, count: 1024 * 1024)
    var offset: off_t = 0
    while true {
      let count = pread(descriptor, &buffer, buffer.count, offset)
      if count < 0, errno == EINTR { continue }
      guard count >= 0 else { throw ManagedDependencyError.integrity }
      if count == 0 { break }
      hasher.update(data: Data(buffer.prefix(count)))
      offset += off_t(count)
    }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
  }

  private func fileDigest(_ url: URL) throws -> String {
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var digest = SHA256()
    while let chunk = try handle.read(upToCount: 1024 * 1024), !chunk.isEmpty {
      try Task.checkCancellation()
      digest.update(data: chunk)
    }
    return digest.finalize().map { String(format: "%02x", $0) }.joined()
  }

  private func hexDigest(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
  }

  private func validDigest(_ value: String) -> Bool {
    value.count == 64 && value.utf8.allSatisfy { ($0 >= 48 && $0 <= 57) || ($0 >= 97 && $0 <= 102) }
  }

  private func validVersion(_ value: String) -> Bool {
    !value.isEmpty && value.utf8.count <= 64
      && value.utf8.allSatisfy { byte in
        (byte >= 48 && byte <= 57) || (byte >= 65 && byte <= 90) || (byte >= 97 && byte <= 122)
          || byte == 45 || byte == 46 || byte == 43
      }
  }

  private func validDownloadURL(_ url: URL) -> Bool {
    url.scheme == "https" && url.user == nil && url.password == nil && url.fragment == nil
      && url.host != nil
  }

  private func validRelative(_ value: String) -> Bool {
    !value.isEmpty && value.utf8.count <= 1024 && !value.hasPrefix("/")
      && value.split(separator: "/", omittingEmptySubsequences: false).allSatisfy {
        !$0.isEmpty && $0 != "." && $0 != ".."
      }
  }

  @discardableResult
  private func runProcess(_ executable: String, _ arguments: [String], capture: Bool,
                          environment: [String: String]? = nil) async throws -> Data {
    let control = ManagedProcessControl()
    return try await withTaskCancellationHandler {
      try await Task.detached(priority: .utility) {
        let process = Process()
        let output = Pipe()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.environment = environment ?? [:]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = capture ? output : FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try control.start(process)
        let data = capture ? output.fileHandleForReading.readDataToEndOfFile() : Data()
        process.waitUntilExit()
        if control.finish() { throw CancellationError() }
        guard process.terminationReason == .exit, process.terminationStatus == 0,
              data.count <= 2 * 1024 * 1024 else { throw ManagedDependencyError.archive }
        return data
      }.value
    } onCancel: {
      control.cancel()
    }
  }
}
