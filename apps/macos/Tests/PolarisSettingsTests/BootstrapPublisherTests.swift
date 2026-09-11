import XCTest
import Foundation
import CryptoKit
@testable import PolarisSettings

final class BootstrapPublisherTests: XCTestCase {
    private func fixture(_ body: (URL, WorkspaceBinding, ExecutionBinding) throws -> Void) throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let source = root.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: false)
        let store = root.appendingPathComponent("store")
        _ = try MetadataDirectory(url: store, create: true)
        let binding = WorkspaceBinding(source: try WorkspaceBinding.sourceIdentity(path: source.path))
        try body(store, binding, ExecutionBinding(provider: .codex, model: "gpt-6-astra", effort: "medium"))
    }
    func testPublishedExactSchemaAndProof() throws {
        try fixture { store, binding, selection in
            let publisher = try BootstrapPublisher(storeURL: store)
            var checks = 0
            let result = try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                                               policyRevision: 2, configurationRevision: 4, revalidate: { checks += 1 })
            guard case .published(let proof) = result else { return XCTFail("not published") }
            let file = store.appendingPathComponent(BootstrapPublisher.filename)
            let bytes = try Data(contentsOf: file)
            let json = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
            XCTAssertEqual(json["schema_version"] as? Int, 1)
            XCTAssertEqual(json["configuration_revision"] as? String, "4")
            XCTAssertEqual(json["model"] as? String, selection.model)
            XCTAssertNil(json["local_endpoint"])
            XCTAssertNil(json["history_mode"])
            XCTAssertEqual(proof.sha256, SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined())
            let attrs = try FileManager.default.attributesOfItem(atPath: file.path)
            XCTAssertEqual((attrs[.systemFileNumber] as? NSNumber)?.stringValue, proof.inode)
            XCTAssertEqual((attrs[.posixPermissions] as? NSNumber)?.intValue, 0o600)
            XCTAssertGreaterThanOrEqual(checks, 3)
            XCTAssertThrowsError(try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                policyRevision: 2, configurationRevision: 4, revalidate: {}))
        }
    }
    func testStrict10BootstrapCarriesExactMode() throws {
        try fixture { store, binding, selection in
            let publisher = try BootstrapPublisher(storeURL: store)
            _ = try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                                      policyRevision: 2, configurationRevision: 4, historyMode: .strict10, revalidate: {})
            let bytes = try Data(contentsOf: store.appendingPathComponent(BootstrapPublisher.filename))
            let json = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes) as? [String: Any])
            XCTAssertEqual(json["history_mode"] as? String, "strict10")
        }
    }
    func testDirectorySyncFailureRetainsProofAndNeverOverwrites() throws {
        try fixture { store, binding, selection in
            let publisher = try BootstrapPublisher(storeURL: store)
            let result = try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                policyRevision: 2, configurationRevision: 4, revalidate: {}, fault: {
                    if $0 == .directorySync { throw SettingsError.saveFailed }
                })
            guard case .publicationUnknown(let proof) = result else { return XCTFail("missing uncertainty") }
            let file = store.appendingPathComponent(BootstrapPublisher.filename)
            let before = try Data(contentsOf: file)
            XCTAssertEqual(proof.sha256, SHA256.hash(data: before).map { String(format: "%02x", $0) }.joined())
            let second = try BootstrapPublisher(storeURL: store)
            XCTAssertThrowsError(try second.publish(binding: binding, selection: selection, permission: .readOnly,
                policyRevision: 2, configurationRevision: 5, revalidate: {}))
            XCTAssertEqual(try Data(contentsOf: file), before)
        }
    }
    func testSelectionGenerationChangesBeforeAndAfterRename() throws {
        for stage in [BootstrapPublisher.Stage.beforeRename, .afterRename] {
            try fixture { store, binding, selection in
                var current = true
                let publisher = try BootstrapPublisher(storeURL: store)
                let operation = {
                    try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                        policyRevision: 2, configurationRevision: 4,
                        revalidate: { if !current { throw SettingsError.sourceChanged } },
                        fault: { if $0 == stage { current = false } })
                }
                if stage == .beforeRename {
                    XCTAssertThrowsError(try operation())
                    XCTAssertFalse(FileManager.default.fileExists(atPath: store.appendingPathComponent(BootstrapPublisher.filename).path))
                } else {
                    guard case .publicationUnknown = try operation() else { return XCTFail("lost post-rename proof") }
                }
            }
        }
    }
    func testSourceAndRootReplacementRefusedBeforePublication() throws {
        for replaceRoot in [false, true] {
            try fixture { store, binding, selection in
                let publisher = try BootstrapPublisher(storeURL: store)
                XCTAssertThrowsError(try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                    policyRevision: 2, configurationRevision: 4, revalidate: {}, fault: { stage in
                        guard stage == .beforeRename else { return }
                        let target = replaceRoot ? store : URL(fileURLWithPath: binding.sourcePath)
                        try FileManager.default.moveItem(at: target, to: target.appendingPathExtension("old"))
                        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: false,
                                                             attributes: [.posixPermissions: 0o700])
                    }))
                XCTAssertFalse(FileManager.default.fileExists(atPath: store.appendingPathComponent(BootstrapPublisher.filename).path))
            }
        }
    }
    func testRootAndSourceChangeAfterRenameRetainsUnknownProof() throws {
        for replaceRoot in [false, true] {
            try fixture { store, binding, selection in
                let publisher = try BootstrapPublisher(storeURL: store)
                let result = try publisher.publish(binding: binding, selection: selection, permission: .readOnly,
                    policyRevision: 2, configurationRevision: 4, revalidate: {}, fault: { stage in
                        guard stage == .afterRename else { return }
                        let target = replaceRoot ? store : URL(fileURLWithPath: binding.sourcePath)
                        try FileManager.default.moveItem(at: target, to: target.appendingPathExtension("old"))
                        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: false,
                                                             attributes: [.posixPermissions: 0o700])
                    })
                guard case .publicationUnknown(let proof) = result else { return XCTFail("missing proof") }
                let location = replaceRoot ? store.appendingPathExtension("old") : store
                let bytes = try Data(contentsOf: location.appendingPathComponent(BootstrapPublisher.filename))
                XCTAssertEqual(proof.sha256, SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined())
            }
        }
    }

    func testExactProofReconcileAndRetireAllowsNextConfiguration() throws {
        try fixture { store, binding, selection in
            let first = try BootstrapPublisher(storeURL: store)
            var beforeRename: PublicationProof?
            let result = try first.publish(binding: binding, selection: selection, permission: .readOnly,
                policyRevision: 2, configurationRevision: 4, revalidate: {}, recordProof: { beforeRename = $0 })
            guard case .published(let proof) = result else { return XCTFail() }
            XCTAssertEqual(beforeRename, proof)
            let next = try BootstrapPublisher(storeURL: store)
            XCTAssertEqual(try next.reconcile(expected: proof, revalidate: {}), .published(proof))
            try next.retire(expected: proof, revalidate: {})
            guard case .published(let second) = try next.publish(binding: binding, selection: selection,
                permission: .readOnly, policyRevision: 2, configurationRevision: 5, revalidate: {}) else { return XCTFail() }
            XCTAssertNotEqual(second.sha256, proof.sha256)
            XCTAssertThrowsError(try next.reconcile(expected: proof, revalidate: {}))
        }
    }

    func testPreRenameReceiptRequiresPositiveStagingProofForExplicitRecovery() throws {
        try fixture { store, binding, selection in
            var receipt: PublicationProof?
            XCTAssertThrowsError(try BootstrapPublisher(storeURL: store).publish(binding: binding, selection: selection,
                permission: .readOnly, policyRevision: 2, configurationRevision: 4, revalidate: {},
                fault: { if $0 == .beforeRename { throw SettingsError.saveFailed } }, recordProof: { receipt = $0 }))
            let proof = try XCTUnwrap(receipt)
            let publisher = try BootstrapPublisher(storeURL: store)
            XCTAssertThrowsError(try publisher.reconcile(expected: proof, revalidate: {})) {
                guard case BootstrapReconciliationError.absent = $0 else { return XCTFail("not positively reconciled") }
            }
            // Missing fixed filename alone must never clear an uncertain receipt.
            try FileManager.default.removeItem(at: store.appendingPathComponent(try XCTUnwrap(proof.stagingName)))
            XCTAssertThrowsError(try publisher.reconcile(expected: proof, revalidate: {})) {
                XCTAssertFalse($0 is BootstrapReconciliationError)
            }
        }
    }
    func testRetirementKeepsExactProofAcrossSettingsReceiptClearFailure() throws {
        try fixture { store, binding, selection in
            let publisher = try BootstrapPublisher(storeURL: store)
            guard case .published(let receipt) = try publisher.publish(binding: binding, selection: selection,
                permission: .readOnly, policyRevision: 2, configurationRevision: 4, revalidate: {}) else { return XCTFail() }
            try publisher.retire(expected: receipt, revalidate: {})
            // Simulate a failed settings write: the old receipt remains durable.
            let resumed = try BootstrapPublisher(storeURL: store)
            XCTAssertThrowsError(try resumed.reconcile(expected: receipt, revalidate: {})) {
                guard case BootstrapReconciliationError.absent = $0 else { return XCTFail("lost retired proof") }
            }
            guard case .published = try resumed.publish(binding: binding, selection: selection,
                permission: .readOnly, policyRevision: 2, configurationRevision: 5, revalidate: {}) else { return XCTFail() }
        }
    }

    func testStagingDirectorySyncPrecedesReceiptAndAbsentRecoveryRequiresSync() throws {
        try fixture { store, binding, selection in
            var receipt: PublicationProof?
            XCTAssertThrowsError(try BootstrapPublisher(storeURL: store).publish(binding: binding, selection: selection,
                permission: .readOnly, policyRevision: 2, configurationRevision: 4, revalidate: {},
                fault: { if $0 == .stagingSync { throw SettingsError.saveFailed } }, recordProof: { receipt = $0 }))
            XCTAssertNil(receipt)
            XCTAssertThrowsError(try BootstrapPublisher(storeURL: store).publish(binding: binding, selection: selection,
                permission: .readOnly, policyRevision: 2, configurationRevision: 4, revalidate: {},
                fault: { if $0 == .beforeRename { throw SettingsError.saveFailed } }, recordProof: { receipt = $0 }))
            let proof = try XCTUnwrap(receipt)
            let resumed = try BootstrapPublisher(storeURL: store)
            XCTAssertThrowsError(try resumed.reconcile(expected: proof, revalidate: {},
                                                       beforeDirectorySync: { throw SettingsError.saveFailed })) {
                XCTAssertFalse($0 is BootstrapReconciliationError)
            }
            XCTAssertThrowsError(try resumed.reconcile(expected: proof, revalidate: {})) {
                guard case BootstrapReconciliationError.absent = $0 else { return XCTFail() }
            }
        }
    }

}
