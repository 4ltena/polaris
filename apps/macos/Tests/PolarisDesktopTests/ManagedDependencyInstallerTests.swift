import CryptoKit
import Foundation
import XCTest
@testable import PolarisDesktop

final class ManagedDependencyInstallerTests: XCTestCase {
  private func digest(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
  }

  func testBundledManifestContainsTheCompleteAutomaticPreset() async throws {
    let home = FileManager.default.temporaryDirectory
      .appendingPathComponent("polaris-preset-manifest-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: home) }
    let installer = ManagedDependencyInstaller(home: home)
    let statuses = try await installer.statuses()
    XCTAssertEqual(Set(statuses.keys), Set(ManagedDependencyID.preset))
    XCTAssertTrue(statuses.values.allSatisfy { $0 == .notInstalled })
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
    func receipt(_ relative: String, id: String, version: String) throws {
      let url = polaris.appendingPathComponent(relative)
      let data = try JSONSerialization.data(withJSONObject: [
        "schema_version": 1, "id": id, "version": version,
        "manifest_sha256": manifestDigest, "installed_at": "fixture",
      ])
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
    try receipt("runtimes/embedding/.embedding-fixed.receipt.json",
                id: "embedding_runtime", version: "embedding-fixed")

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
    try await installer.activatePreset()
    let managed = polaris.appendingPathComponent("managed-resources.toml")
    let body = try String(contentsOf: managed, encoding: .utf8)
    XCTAssertTrue(body.contains("runtimes/embedding/embedding-fixed/runtime/bin/python3.11"))
    XCTAssertTrue(body.contains("models/multilingual-e5-small/small-fixed"))
    XCTAssertTrue(body.contains("models/multilingual-e5-base/base-fixed"))
    XCTAssertTrue(body.contains("runtimes/llama.cpp/llama-fixed/llama-server"))
    let permissions = try FileManager.default.attributesOfItem(atPath: managed.path)[.posixPermissions] as? NSNumber
    XCTAssertEqual(permissions?.intValue, 0o600)
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
}
