import XCTest
import CryptoKit
import Foundation
@testable import PolarisDesktop

private func offerData() throws -> Data {
    try JSONSerialization.data(withJSONObject: [
        "schema_version": 1, "type": "plan", "consent_sha256": String(repeating: "a", count: 64),
        "plan": [
            "transaction_id": "synthetic", "profile": "fixture-v1", "generation": "fixture-generation",
            "model_repository": "synthetic/model", "model_revision": "dummy", "quantization": "synthetic",
            "runtime_revision": "synthetic-runtime", "platform": "macos", "architecture": "arm64", "device": "cpu",
            "context_tokens": 1024, "peak_bytes": 1, "reserve_bytes": 1,
            "destination": "/synthetic/generation", "installed_bytes": 10, "copy_reserve_bytes": 1,
            "worker": ["source": "/synthetic/worker", "bytes": 5, "sha256": String(repeating: "b", count: 64)],
            "model": ["source": "/synthetic/model", "bytes": 5, "sha256": String(repeating: "c", count: 64)],
            "delivery": ["purpose": "synthetic", "worker_source": "https://example.invalid/worker",
                "model_source": "https://example.invalid/model", "license": "synthetic", "license_source": "https://example.invalid/license",
                "notice_source": "https://example.invalid/notice", "components": []],
        ],
    ])
}

private actor FixtureDecisionTransport: DecisionInstallTransport {
    let profiles: [String]
    var confirmations: [Bool] = []
    var prepared = 0
    var cancelled = 0
    init(profiles: [String] = ["fixture-v1"]) { self.profiles = profiles }
    func catalog() -> DecisionInstallCatalog {
        DecisionInstallCatalog(schemaVersion: 1, registeredProfiles: profiles, capacity: ["synthetic"], note: "not readiness")
    }
    func prepare(profile: String, worker: URL, model: URL) throws -> DecisionInstallOffer {
        prepared += 1
        return try PackagedDecisionInstallClient.decode(DecisionInstallOffer.self, offerData())
    }
    func confirm(consent: String, approved: Bool) throws -> DecisionInstallOutcome {
        guard consent == String(repeating: "a", count: 64) else { throw ServiceError.correlation }
        confirmations.append(approved)
        return approved ? .installed : .cancelled
    }
    func cancel() { cancelled += 1 }
    func counts() -> (Int, [Bool], Int) { (prepared, confirmations, cancelled) }
}

final class DecisionInstallTests: XCTestCase {
    @MainActor private func settle(_ model: DecisionInstallModel) async throws {
        for _ in 0..<1000 {
            if !model.busy { return }
            try await Task.sleep(nanoseconds: 1_000_000)
        }
        XCTFail("model operation did not settle")
    }

    @MainActor func testEmptyCatalogCannotPrepareAndDoesNotEnableAnotherBackend() async throws {
        let transport = FixtureDecisionTransport(profiles: [])
        let model = DecisionInstallModel(transport: transport)
        model.refresh(); try await settle(model)
        model.worker = URL(fileURLWithPath: "/synthetic/worker"); model.model = URL(fileURLWithPath: "/synthetic/model")
        XCTAssertFalse(model.canPrepare)
        model.prepare(); try await settle(model)
        let counts = await transport.counts()
        XCTAssertEqual(counts.0, 0); XCTAssertEqual(counts.1, [])
        XCTAssertNil(model.offer); XCTAssertTrue(model.message.contains("0件"))
    }

    @MainActor func testOneOfferIsConfirmedOnceWithoutEnablingTheBackend() async throws {
        let transport = FixtureDecisionTransport()
        let model = DecisionInstallModel(transport: transport)
        model.refresh(); try await settle(model)
        model.worker = URL(fileURLWithPath: "/synthetic/worker"); model.model = URL(fileURLWithPath: "/synthetic/model")
        model.prepare(); try await settle(model)
        XCTAssertNotNil(model.offer)
        model.confirm(true); model.confirm(true); try await settle(model)
        let counts = await transport.counts()
        XCTAssertEqual(counts.0, 1); XCTAssertEqual(counts.1, [true])
        XCTAssertNil(model.offer); XCTAssertTrue(model.message.contains("有効化は変更していません"))
    }

    @MainActor func testChangedSelectionCannotApproveTheOldOfferAndCancellationIsSeparate() async throws {
        let transport = FixtureDecisionTransport()
        let model = DecisionInstallModel(transport: transport)
        model.refresh(); try await settle(model)
        model.worker = URL(fileURLWithPath: "/synthetic/worker"); model.model = URL(fileURLWithPath: "/synthetic/model")
        model.prepare(); try await settle(model)
        model.model = URL(fileURLWithPath: "/synthetic/different")
        model.confirm(true); try await settle(model)
        var counts = await transport.counts()
        XCTAssertEqual(counts.1, []); XCTAssertEqual(counts.2, 1)
        model.model = URL(fileURLWithPath: "/synthetic/model")
        model.prepare(); try await settle(model)
        model.confirm(false); try await settle(model)
        counts = await transport.counts()
        XCTAssertEqual(counts.1, [false]); XCTAssertTrue(model.message.contains("取り消しました"))
    }

    func testPackagedCLIRequiresTheFixedPathHashAndArchitecture() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("decision-package-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let helper = root.appendingPathComponent("Contents/Helpers/polaris")
        let resources = root.appendingPathComponent("Contents/Resources")
        try FileManager.default.createDirectory(at: helper.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: resources, withIntermediateDirectories: true)
        let payload = Data("synthetic bytes, never executed".utf8)
        try payload.write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: helper.path)
        #if arch(arm64)
        let architecture = "arm64"
        #else
        let architecture = "x86_64"
        #endif
        let digest = SHA256.hash(data: payload).map { String(format: "%02x", $0) }.joined()
        func manifest(_ hash: String) throws {
            try JSONSerialization.data(withJSONObject: ["schema_version": 1, "relative_path": "Contents/Helpers/polaris",
                "architectures": [architecture], "sha256": hash]).write(to: resources.appendingPathComponent("polaris-cli.json"))
        }
        try manifest(digest)
        XCTAssertEqual(try PackagedDecisionInstallClient.packagedCLI(bundle: root), helper)
        try manifest(String(repeating: "0", count: 64))
        XCTAssertThrowsError(try PackagedDecisionInstallClient.packagedCLI(bundle: root))
        try manifest(digest)
        try FileManager.default.moveItem(at: helper, to: root.appendingPathComponent("other"))
        try FileManager.default.createSymbolicLink(at: helper, withDestinationURL: root.appendingPathComponent("other"))
        XCTAssertThrowsError(try PackagedDecisionInstallClient.packagedCLI(bundle: root))
    }

    func testOwnedProcessKeepsOneOfferUntilTheExactConfirmationIsDelivered() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("decision-pipe-\(UUID())")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let helper = root.appendingPathComponent("fixture-helper")
        let frame = String(data: try offerData(), encoding: .utf8)!
        let script = """
        #!/bin/sh
        printf '%s\\n' '\(frame)'
        IFS= read -r answer || exit 2
        case "$answer" in
          *'"decision":"cancel"'*) status=cancelled ;;
          *'"decision":"approve"'*) status=installed ;;
          *) exit 3 ;;
        esac
        printf '{"schema_version":1,"type":"complete","consent_sha256":"\(String(repeating: "a", count: 64))","status":"%s"}\\n' "$status"
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: helper.path)
        let client = PackagedDecisionInstallClient(helper: { helper })
        let offer = try await client.prepare(profile: "fixture-v1", worker: URL(fileURLWithPath: "/synthetic/worker"), model: URL(fileURLWithPath: "/synthetic/model"))
        do {
            _ = try await client.confirm(consent: String(repeating: "b", count: 64), approved: true)
            XCTFail("another plan was confirmed")
        } catch {}
        let result = try await client.confirm(consent: offer.consentSha256, approved: false)
        if case .cancelled = result {} else { XCTFail("cancel was not preserved") }
        let next = try await client.prepare(profile: "fixture-v1", worker: URL(fileURLWithPath: "/synthetic/worker"), model: URL(fileURLWithPath: "/synthetic/model"))
        let installed = try await client.confirm(consent: next.consentSha256, approved: true)
        if case .installed = installed {} else { XCTFail("explicit approval was not delivered") }
    }
}
