import CryptoKit
import Foundation
import XCTest
@testable import PolarisDesktop

final class ManagedDependencyInstallerTests: XCTestCase {
  private func digest(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
  }

  func testReadonlyRuntimeV2InstallsOnlyVerifiedImageAtomically() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-runtime-v2-\(UUID())", isDirectory: true)
    let home = root.appendingPathComponent("home", isDirectory: true)
    let resources = root.appendingPathComponent("resources", isDirectory: true)
    try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: resources, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
#if arch(arm64)
    let architecture = "arm64"
#else
    let architecture = "x86_64"
#endif
    let image = Data("fixture dmg bytes".utf8)
    try image.write(to: resources.appendingPathComponent("runtime.dmg"))
    let lock: [String: Any] = [
      "schema_version": 2,
      "images": [[
        "id": "python-e5-cat", "version": "fixture", "platform": "macos",
        "architecture": architecture, "image_filename": "runtime.dmg",
        "image_size": image.count, "image_sha256": digest(image),
        "tree_manifest_sha256": String(repeating: "a", count: 64),
        "bundled_resource": "runtime.dmg",
        "tree_manifest": ["schema_version": 1, "files": []],
      ]],
    ]
    let lockURL = resources.appendingPathComponent("readonly-runtime-images-v2.json")
    try JSONSerialization.data(withJSONObject: lock).write(to: lockURL)
    let installer = ManagedDependencyInstaller(runtimeImageManifestURL: lockURL, home: home)
    let before = try await installer.status(.embeddingRuntime)
    XCTAssertEqual(before, .notInstalled)
    let installed = try await installer.install(.embeddingRuntime)
    XCTAssertEqual(installed, "fixture")
    let after = try await installer.status(.embeddingRuntime)
    XCTAssertEqual(after, .installed(version: "fixture"))
    let destination = home.appendingPathComponent(
      ".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg")
    XCTAssertEqual(try Data(contentsOf: destination), image)
    // Receipt is repair/UI metadata only. Replacing it cannot bless or revoke
    // image bytes that are independently pinned by the package lock.
    try Data("replaced receipt".utf8).write(
      to: destination.deletingLastPathComponent().appendingPathComponent("receipt.json"))
    let afterReceiptReplacement = try await installer.status(.embeddingRuntime)
    XCTAssertEqual(afterReceiptReplacement, .installed(version: "fixture"))
    XCTAssertFalse(try FileManager.default.contentsOfDirectory(
      at: destination.deletingLastPathComponent(),
      includingPropertiesForKeys: nil).contains { $0.lastPathComponent.contains("pending-") })
  }

  func testReadonlyRuntimeV2RejectsWrongArchitectureAndHashWithoutPublishing() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-runtime-v2-reject-\(UUID())", isDirectory: true)
    let resources = root.appendingPathComponent("resources", isDirectory: true)
    try FileManager.default.createDirectory(at: resources, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let image = Data("fixture".utf8)
    try image.write(to: resources.appendingPathComponent("runtime.dmg"))
    func writeLock(architecture: String, hash: String) throws -> URL {
      let url = resources.appendingPathComponent("lock-\(architecture).json")
      try JSONSerialization.data(withJSONObject: ["schema_version": 2, "images": [[
        "id": "python-e5-cat", "version": "fixture", "platform": "macos",
        "architecture": architecture, "image_filename": "runtime.dmg",
        "image_size": image.count, "image_sha256": hash,
        "tree_manifest_sha256": String(repeating: "b", count: 64),
        "bundled_resource": "runtime.dmg",
      ]]]).write(to: url)
      return url
    }
    let wrong = try writeLock(architecture: "wrong", hash: digest(image))
    let wrongInstaller = ManagedDependencyInstaller(runtimeImageManifestURL: wrong, home: root)
    let wrongStatus = try await wrongInstaller.status(.embeddingRuntime)
    XCTAssertEqual(wrongStatus, .repairRequired)
#if arch(arm64)
    let architecture = "arm64"
#else
    let architecture = "x86_64"
#endif
    let badHash = try writeLock(architecture: architecture, hash: String(repeating: "c", count: 64))
    let badInstaller = ManagedDependencyInstaller(runtimeImageManifestURL: badHash, home: root)
    do {
      _ = try await badInstaller.install(.embeddingRuntime)
      XCTFail("mismatched image hash was accepted")
    } catch let error as ManagedDependencyError {
      XCTAssertEqual(error, .integrity)
    }
    XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent(
      ".polaris/runtime-images/python-e5-cat/fixture/runtime.dmg").path))
  }

  func testReadonlyRuntimeV2RejectsSymlinkedDestinationAncestor() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-runtime-v2-symlink-\(UUID())", isDirectory: true)
    let home = root.appendingPathComponent("home", isDirectory: true)
    let resources = root.appendingPathComponent("resources", isDirectory: true)
    let outside = root.appendingPathComponent("outside", isDirectory: true)
    try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: resources, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
#if arch(arm64)
    let architecture = "arm64"
#else
    let architecture = "x86_64"
#endif
    let image = Data("fixture dmg bytes".utf8)
    try image.write(to: resources.appendingPathComponent("runtime.dmg"))
    let lockURL = resources.appendingPathComponent("readonly-runtime-images-v2.json")
    try JSONSerialization.data(withJSONObject: ["schema_version": 2, "images": [[
      "id": "python-e5-cat", "version": "fixture", "platform": "macos",
      "architecture": architecture, "image_filename": "runtime.dmg",
      "image_size": image.count, "image_sha256": digest(image),
      "tree_manifest_sha256": String(repeating: "a", count: 64),
      "bundled_resource": "runtime.dmg",
    ]]]).write(to: lockURL)
    let runtimeImages = home.appendingPathComponent(".polaris/runtime-images", isDirectory: true)
    try FileManager.default.createDirectory(at: runtimeImages, withIntermediateDirectories: true)
    try FileManager.default.createSymbolicLink(
      at: runtimeImages.appendingPathComponent("python-e5-cat"),
      withDestinationURL: outside)

    let installer = ManagedDependencyInstaller(runtimeImageManifestURL: lockURL, home: home)
    func expectConflict(_ description: String) async throws {
      do {
        _ = try await installer.install(.embeddingRuntime)
        XCTFail("\(description) was accepted")
      } catch let error as ManagedDependencyError {
        XCTAssertEqual(error, .conflict)
      }
    }
    try await expectConflict("symlinked runtime ancestor")

    let runtime = runtimeImages.appendingPathComponent("python-e5-cat", isDirectory: true)
    try FileManager.default.removeItem(at: runtime)
    try FileManager.default.createDirectory(at: runtime, withIntermediateDirectories: false)
    let version = runtime.appendingPathComponent("fixture", isDirectory: true)
    try FileManager.default.createSymbolicLink(at: version, withDestinationURL: outside)
    try await expectConflict("symlinked runtime version")

    try FileManager.default.removeItem(at: version)
    try FileManager.default.createDirectory(at: version, withIntermediateDirectories: false)
    try FileManager.default.createSymbolicLink(
      at: version.appendingPathComponent("runtime.dmg"),
      withDestinationURL: outside.appendingPathComponent("runtime.dmg"))
    try await expectConflict("symlinked runtime image")
    XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: outside.path).isEmpty)
  }

  func testReadonlyRuntimeV2DirectorySwapCannotRedirectFinalWrites() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-runtime-v2-race-\(UUID())", isDirectory: true)
    let home = root.appendingPathComponent("home", isDirectory: true)
    let resources = root.appendingPathComponent("resources", isDirectory: true)
    let outside = root.appendingPathComponent("outside", isDirectory: true)
    try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: resources, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
#if arch(arm64)
    let architecture = "arm64"
#else
    let architecture = "x86_64"
#endif
    let image = Data("fixture dmg bytes".utf8)
    try image.write(to: resources.appendingPathComponent("runtime.dmg"))
    let lockURL = resources.appendingPathComponent("readonly-runtime-images-v2.json")
    try JSONSerialization.data(withJSONObject: ["schema_version": 2, "images": [[
      "id": "python-e5-cat", "version": "fixture", "platform": "macos",
      "architecture": architecture, "image_filename": "runtime.dmg",
      "image_size": image.count, "image_sha256": digest(image),
      "tree_manifest_sha256": String(repeating: "a", count: 64),
      "bundled_resource": "runtime.dmg",
    ]]]).write(to: lockURL)
    let version = home.appendingPathComponent(
      ".polaris/runtime-images/python-e5-cat/fixture", isDirectory: true)
    let displaced = version.deletingLastPathComponent()
      .appendingPathComponent("fixture-displaced", isDirectory: true)
    let versionPath = version.path
    let displacedPath = displaced.path
    let outsidePath = outside.path
    let installer = ManagedDependencyInstaller(
      runtimeImageManifestURL: lockURL, home: home,
      runtimePlacementHook: {
        guard rename(versionPath, displacedPath) == 0,
              symlink(outsidePath, versionPath) == 0 else {
          throw ManagedDependencyError.install
        }
      })
    do {
      _ = try await installer.install(.embeddingRuntime)
      XCTFail("directory swap was accepted")
    } catch let error as ManagedDependencyError {
      XCTAssertEqual(error, .conflict)
    }
    XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: outside.path).isEmpty)
    XCTAssertFalse(FileManager.default.fileExists(
      atPath: displaced.appendingPathComponent("runtime.dmg").path))
    XCTAssertFalse(FileManager.default.fileExists(
      atPath: displaced.appendingPathComponent("receipt.json").path))
  }

  func testBundledManifestContainsPresetAndOptionalManagedDependencies() async throws {
    let home = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-preset-manifest-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: home) }
    let installer = ManagedDependencyInstaller(home: home)
    let statuses = try await installer.statuses()
    XCTAssertEqual(Set(statuses.keys), Set(ManagedDependencyID.allCases))
    XCTAssertTrue(statuses.filter { $0.key != .embeddingRuntime }.values.allSatisfy { $0 == .notInstalled })
    XCTAssertEqual(statuses[.embeddingRuntime], .repairRequired)
    XCTAssertFalse(ManagedDependencyID.preset.contains(.catTranslate3B))
    XCTAssertEqual(statuses[.catTranslate3B], .notInstalled)
  }

  func testCompletePresetPublishesManagedRuntimeAndModelPins() async throws {
    let home = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-preset-activation-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: home) }
#if arch(arm64)
    let architecture = "arm64"
#else
    let architecture = "x86_64"
#endif
    let packageVersions = [
      "torch": "1", "transformers": "2", "tokenizers": "3", "safetensors": "4", "numpy": "5",
      "sentencepiece": "6",
    ]
    let smallData = Data("small".utf8), baseData = Data("base".utf8)
    let entries: [[String: Any]] = [
      ["id": "llama_cpp", "version": "llama-fixed", "platform": "macos",
       "architecture": architecture, "url": "https://example.com/llama.tar.gz",
       "sha256": String(repeating: "a", count: 64), "compressed_size": 1,
       "archive": "tar_gzip", "archive_root": "llama", "required_executable": "llama-server"],
      ["id": "rust", "version": "rust-fixed", "platform": "macos",
       "architecture": architecture, "url": "https://example.com/rust.tar.xz",
       "sha256": String(repeating: "b", count: 64), "compressed_size": 1,
       "archive": "tar_xz", "archive_root": "rust", "installer": "install.sh",
       "required_executable": "bin/rustc"],
      ["id": "embedding_runtime", "version": "embedding-fixed", "platform": "macos",
       "architecture": architecture, "url": "https://example.com/uv.tar.gz",
       "sha256": String(repeating: "c", count: 64), "compressed_size": 1,
       "archive": "tar_gzip", "archive_root": "uv", "install_kind": "uv_embedding",
       "legacy_runtime": true,
       "required_executable": "runtime/bin/python3.11", "python_version": "3.11.9",
       "exclude_newer": "2026-09-19T00:00:00Z", "python_packages": packageVersions],
      ["id": "multilingual_e5_small", "version": "small-fixed", "platform": "macos",
       "architecture": "universal", "destination": "models/multilingual-e5-small",
       "files": [["path": "config.json", "url": "https://example.com/small.json",
                   "sha256": digest(smallData), "size": smallData.count]]],
      ["id": "multilingual_e5_base", "version": "base-fixed", "platform": "macos",
       "architecture": "universal", "destination": "models/multilingual-e5-base",
       "files": [["path": "config.json", "url": "https://example.com/base.json",
                   "sha256": digest(baseData), "size": baseData.count]]],
    ]
    let manifest = try JSONSerialization.data(
      withJSONObject: ["schema_version": 1, "dependencies": entries], options: [.sortedKeys])
    let manifestURL = home.appendingPathComponent("runtime-dependencies.json")
    try manifest.write(to: manifestURL)
    let manifestDigest = digest(manifest)
    let polaris = home.appendingPathComponent(".polaris", isDirectory: true)

    func executable(_ relative: String) throws {
      let url = polaris.appendingPathComponent(relative)
      try FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
      try Data("binary".utf8).write(to: url)
      try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: url.path)
    }
    func receipt(_ relative: String, id: String, version: String,
                 runtimeTreeSHA256: String? = nil) throws {
      let url = polaris.appendingPathComponent(relative)
      var object: [String: Any] = [
        "schema_version": 1, "id": id, "version": version,
        "manifest_sha256": manifestDigest, "installed_at": "fixture",
      ]
      if let runtimeTreeSHA256 { object["runtime_tree_sha256"] = runtimeTreeSHA256 }
      let data = try JSONSerialization.data(withJSONObject: object)
      try data.write(to: url)
    }
    try executable("runtimes/llama.cpp/llama-fixed/llama-server")
    try receipt("runtimes/llama.cpp/.llama-fixed.receipt.json", id: "llama_cpp", version: "llama-fixed")
    try executable("toolchains/rust/rust-fixed/bin/rustc")
    try receipt("toolchains/rust/.rust-fixed.receipt.json", id: "rust", version: "rust-fixed")
    try executable("runtimes/embedding/embedding-fixed/runtime/bin/python3.11")
    let embeddingRoot = polaris.appendingPathComponent("runtimes/embedding/embedding-fixed")
    try JSONEncoder().encode(packageVersions)
      .write(to: embeddingRoot.appendingPathComponent("embedding-packages.json"))
    let runtimeTreeSHA256 = try ManagedDependencyInstaller.writeRuntimeTreeManifest(at: embeddingRoot)
    try receipt("runtimes/embedding/.embedding-fixed.receipt.json",
                id: "embedding_runtime", version: "embedding-fixed",
                runtimeTreeSHA256: runtimeTreeSHA256)

    func model(_ relative: String, version: String, data: Data) throws {
      let root = polaris.appendingPathComponent(relative)
      try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
      try data.write(to: root.appendingPathComponent("config.json"))
      let body = try JSONSerialization.data(withJSONObject: [
        "revision": version, "files": ["config.json": digest(data)],
      ])
      try body.write(to: root.appendingPathComponent("manifest.json"))
    }
    try model("models/multilingual-e5-small/small-fixed", version: "small-fixed", data: smallData)
    try model("models/multilingual-e5-base/base-fixed", version: "base-fixed", data: baseData)

    let installer = ManagedDependencyInstaller(manifestURL: manifestURL, home: home)
    do {
      try await installer.activatePreset()
      XCTFail("legacy writable runtime must not activate the preset")
    } catch let error as ManagedDependencyError {
      XCTAssertEqual(error, .runtimeUnavailable)
    }
    XCTAssertFalse(polaris.appendingPathComponent("managed-resources.toml").isFileURL
      && FileManager.default.fileExists(atPath: polaris.appendingPathComponent("managed-resources.toml").path))
  }

  func testExistingPinnedModelIsRecognizedWithoutInstallerReceipt() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-dependency-test-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let revision = "fixed-small"
    let bytes = Data("fixed model configuration".utf8)
    let hash = digest(bytes)
    let source = URL(string: "https://huggingface.co/example/model/resolve/\(revision)/config.json")!
    let manifestObject: [String: Any] = [
      "schema_version": 1,
      "dependencies": [[
        "id": "multilingual_e5_small", "version": revision,
        "platform": "macos", "architecture": "universal",
        "destination": "models/multilingual-e5-small",
        "files": [["path": "config.json", "url": source.absoluteString,
                   "sha256": hash, "size": bytes.count]],
      ]],
    ]
    let manifest = try JSONSerialization.data(withJSONObject: manifestObject)
    let manifestURL = root.appendingPathComponent("runtime-dependencies.json")
    try manifest.write(to: manifestURL)
    let model = root.appendingPathComponent(
      ".polaris/models/multilingual-e5-small/\(revision)", isDirectory: true)
    try FileManager.default.createDirectory(at: model, withIntermediateDirectories: true)
    try bytes.write(to: model.appendingPathComponent("config.json"))
    let installedManifest = try JSONSerialization.data(
      withJSONObject: ["revision": revision, "files": ["config.json": hash]])
    try installedManifest.write(to: model.appendingPathComponent("manifest.json"))

    let installer = ManagedDependencyInstaller(manifestURL: manifestURL, home: root)
    let initial = try await installer.status(.multilingualE5Small)
    XCTAssertEqual(initial, .installed(version: revision))
    try Data("changed".utf8).write(to: model.appendingPathComponent("config.json"))
    guard case .failed = try await installer.status(.multilingualE5Small) else {
      return XCTFail("size mismatch must fail")
    }
  }

  func testCATTranslateUsesTheCanonicalManagedModelLocation() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-cat-translate-test-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let revision = ManagedDependencyInstaller.catTranslateRevision
    XCTAssertEqual(revision, "47e382331d005acd54a42cdf088a76aa88788e0c")
    XCTAssertEqual(ManagedDependencyInstaller.catTranslateDestination, "models/cat-translate-3.3b")
    XCTAssertEqual(ManagedDependencyInstaller.catTranslateRequiredSpaceBytes, 20_407_789_393)
    let bytes = Data("fixed CAT configuration".utf8)
    let hash = digest(bytes)
    let manifestObject: [String: Any] = [
      "schema_version": 1,
      "dependencies": [[
        "id": "cat_translate_3_3b", "version": revision,
        "platform": "macos", "architecture": "universal",
        "destination": ManagedDependencyInstaller.catTranslateDestination,
        "files": [[
          "path": "config.json",
          "url": "https://huggingface.co/cyberagent/CAT-Translate-3.3b/resolve/\(revision)/config.json",
          "sha256": hash, "size": bytes.count,
        ]],
      ]],
    ]
    let manifest = try JSONSerialization.data(withJSONObject: manifestObject)
    let manifestURL = root.appendingPathComponent("runtime-dependencies.json")
    try manifest.write(to: manifestURL)
    let model = root.appendingPathComponent(
      ".polaris/models/cat-translate-3.3b/\(revision)", isDirectory: true)
    try FileManager.default.createDirectory(at: model, withIntermediateDirectories: true)
    try bytes.write(to: model.appendingPathComponent("config.json"))
    let installedManifest = try JSONSerialization.data(
      withJSONObject: ["revision": revision, "files": ["config.json": hash]])
    try installedManifest.write(to: model.appendingPathComponent("manifest.json"))

    let installer = ManagedDependencyInstaller(manifestURL: manifestURL, home: root)
    let status = try await installer.status(.catTranslate3B)
    XCTAssertEqual(status, .installed(version: revision))
    let outside = root.appendingPathComponent("outside-config.json")
    try bytes.write(to: outside)
    try FileManager.default.removeItem(at: model.appendingPathComponent("config.json"))
    try FileManager.default.createSymbolicLink(
      at: model.appendingPathComponent("config.json"), withDestinationURL: outside)
    guard case .failed = try await installer.status(.catTranslate3B) else {
      return XCTFail("CAT status must reject a symbolic-link artifact")
    }
    XCTAssertEqual(
      model,
      root.appendingPathComponent(
        ".polaris/\(ManagedDependencyInstaller.catTranslateDestination)/\(ManagedDependencyInstaller.catTranslateRevision)",
        isDirectory: true))
    XCTAssertFalse(ManagedDependencyID.preset.contains(.catTranslate3B))
  }

  func testManifestRejectsNonHTTPSDependency() async throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-dependency-manifest-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let manifest = Data(#"{"schema_version":1,"dependencies":[{"id":"multilingual_e5_base","version":"fixed","platform":"macos","architecture":"universal","destination":"models/multilingual-e5-base","files":[{"path":"config.json","url":"http://example.com/config.json","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1}]}]}"#.utf8)
    let manifestURL = root.appendingPathComponent("runtime-dependencies.json")
    try manifest.write(to: manifestURL)
    let installer = ManagedDependencyInstaller(manifestURL: manifestURL, home: root)
    do {
      _ = try await installer.status(.multilingualE5Base)
      XCTFail("HTTP dependency must be refused")
    } catch {
      XCTAssertEqual(error as? ManagedDependencyError, .manifest)
    }
  }

  func testEmbeddingRuntimeManifestIsDeterministicAndRejectsAliases() throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-runtime-tree-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(
      at: root.appendingPathComponent("runtime/lib", isDirectory: true),
      withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    try Data("python".utf8).write(to: root.appendingPathComponent("runtime/python"))
    try Data("module".utf8).write(to: root.appendingPathComponent("runtime/lib/module.py"))

    let digest = try ManagedDependencyInstaller.writeRuntimeTreeManifest(at: root)
    let manifest = root.appendingPathComponent(ManagedDependencyInstaller.runtimeTreeManifestName)
    XCTAssertEqual(digest, self.digest(try Data(contentsOf: manifest)))
    let object = try XCTUnwrap(
      JSONSerialization.jsonObject(with: Data(contentsOf: manifest)) as? [String: Any])
    let files = try XCTUnwrap(object["files"] as? [[String: Any]])
    XCTAssertEqual(files.compactMap { $0["path"] as? String }, [
      "runtime/lib/module.py", "runtime/python",
    ])

    let aliasRoot = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-runtime-alias-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: aliasRoot, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: aliasRoot) }
    let outside = aliasRoot.deletingLastPathComponent().appendingPathComponent("outside-\(UUID())")
    try Data("outside".utf8).write(to: outside)
    defer { try? FileManager.default.removeItem(at: outside) }
    try FileManager.default.createSymbolicLink(
      at: aliasRoot.appendingPathComponent("module.py"), withDestinationURL: outside)
    XCTAssertThrowsError(try ManagedDependencyInstaller.writeRuntimeTreeManifest(at: aliasRoot))
  }
}
