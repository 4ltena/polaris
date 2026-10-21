import XCTest
import CryptoKit
import Foundation
import PolarisSettings
@testable import PolarisDesktop

private func offerData(repair: Bool = false, download: Bool = false) throws -> Data {
    let data = try JSONSerialization.data(withJSONObject: [
        "schema_version": 1, "type": "plan", "consent_sha256": String(repeating: "a", count: 64),
        "plan": [
            "operation": "import",
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
    var frame = try JSONSerialization.jsonObject(with: data) as! [String: Any]
    var plan = frame["plan"] as! [String: Any]
    if repair {
        plan["operation"] = "repair"
        plan["replacement"] = ["backup_destination": "/synthetic/.recovery-" + String(repeating: "a", count: 32)]
    }
    if download {
        plan["transfer_bytes"] = 10; plan["disk_required_bytes"] = 21; plan["transaction_timeout_seconds"] = 7200
        var delivery = plan["delivery"] as! [String: Any]
        delivery["downloads"] = ["worker": ["url": "https://example.invalid/worker", "redirect_origins": []],
            "model": ["url": "https://example.invalid/model", "redirect_origins": ["https://cdn.example.invalid"]]]
        plan["delivery"] = delivery
    }
    frame["plan"] = plan
    return try JSONSerialization.data(withJSONObject: frame)
}

private func verificationData(profile: String = "fixture-v1", status: String = "verified") throws -> Data {
    var frame: [String: Any] = ["schema_version": 1, "type": "verification", "profile": profile, "status": status]
    if status == "verified" {
        frame["generation"] = ["profile": profile, "generation": "fixture-generation", "model_repository": "synthetic/model",
            "model_revision": String(repeating: "a", count: 40), "quantization": "synthetic-q4", "runtime_revision": "synthetic-runtime",
            "worker_sha256": String(repeating: "b", count: 64), "model_sha256": String(repeating: "c", count: 64), "verified_bytes": 10]
    }
    return try JSONSerialization.data(withJSONObject: frame)
}

private actor FixtureDecisionTransport: DecisionInstallTransport {
    let profiles: [String]
    var confirmations: [Bool] = []
    var prepared = 0
    var cancelled = 0
    var verified = 0
    let verificationStatus: String
    let verificationDelay: UInt64
    init(profiles: [String] = ["fixture-v1"], verificationStatus: String = "verified", verificationDelay: UInt64 = 0) {
        self.profiles = profiles; self.verificationStatus = verificationStatus; self.verificationDelay = verificationDelay
    }
    func verify(profile: String) async throws -> DecisionGenerationVerification {
        verified += 1
        if verificationDelay > 0 { try await Task.sleep(nanoseconds: verificationDelay) }
        return try PackagedDecisionInstallClient.decodeVerification(verificationData(profile: profile, status: verificationStatus), profile: profile)
    }
    func verificationCount() -> Int { verified }
    func catalog() -> DecisionInstallCatalog {
        DecisionInstallCatalog(schemaVersion: 1, registeredProfiles: profiles, capacity: ["synthetic"], note: "not readiness")
    }
    func prepare(profile: String, worker: URL, model: URL, repair: Bool) throws -> DecisionInstallOffer {
        prepared += 1
        return try PackagedDecisionInstallClient.decode(DecisionInstallOffer.self, offerData(repair: repair))
    }
    func prepareDownload(profile: String, repair: Bool) throws -> DecisionInstallOffer {
        prepared += 1
        return try PackagedDecisionInstallClient.decode(DecisionInstallOffer.self, offerData(repair: repair, download: true))
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

    @MainActor func testNativeSelectionIsDisabledUntilExplicitFreshVerification() async throws {
        let transport = FixtureDecisionTransport()
        let model = DecisionInstallModel(transport: transport)
        var selection: ClefDecisionSelection?
        model.refresh(); try await settle(model)
        model.selectDisabled { selection = $0 }
        XCTAssertEqual(selection, try ClefDecisionSelection(profile: "fixture-v1"))
        var count = await transport.verificationCount(); XCTAssertEqual(count, 0)
        model.verify(); try await settle(model)
        XCTAssertFalse(selection!.enabled)
        model.activate { selection = $0 }; try await settle(model)
        XCTAssertEqual(selection, try ClefDecisionSelection(profile: "fixture-v1", enabled: true))
        count = await transport.verificationCount(); XCTAssertEqual(count, 2)
        XCTAssertTrue(model.message.contains("下書き"))
        model.selectionEdited()
        XCTAssertNil(model.verification)
        XCTAssertFalse(model.message.contains("有効化"))
    }

    @MainActor func testFailedCancelledChangedOrStaleActivationNeverWritesSelection() async throws {
        for mode in ["integrity", "cancelled", "changed", "stale"] {
            let model = DecisionInstallModel(transport: FixtureDecisionTransport(profiles: ["fixture-v1", "other-v1"],
                verificationStatus: mode == "integrity" ? "integrity" : "verified", verificationDelay: 20_000_000))
            model.refresh(); try await settle(model)
            var written = false
            model.activate { _ in
                if mode == "stale" { throw ServiceError.correlation }
                written = true
            }
            if mode == "cancelled" { model.cancel() }
            if mode == "changed" { model.profile = "other-v1" }
            try await settle(model); try await Task.sleep(nanoseconds: 30_000_000)
            XCTAssertFalse(written, mode)
            XCTAssertNil(model.verification, mode)
        }
        let model = DecisionInstallModel(transport: FixtureDecisionTransport(profiles: []))
        model.refresh(); try await settle(model)
        var written = false
        model.selectDisabled { _ in written = true }; model.activate { _ in written = true }
        XCTAssertFalse(written)
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

    @MainActor func testRepairRequiresItsOwnPlanAndKeepsTheBackupLocationVisible() async throws {
        let transport = FixtureDecisionTransport()
        let model = DecisionInstallModel(transport: transport)
        model.refresh(); try await settle(model)
        model.worker = URL(fileURLWithPath: "/synthetic/worker"); model.model = URL(fileURLWithPath: "/synthetic/model")
        model.repair = true
        model.prepare(); try await settle(model)
        XCTAssertEqual(model.offer?.plan.operation, "repair")
        XCTAssertTrue(model.recoveryBackup!.contains(".recovery-"))
        model.confirm(false); try await settle(model)
        model.prepare(); try await settle(model)
        model.repair = false
        model.confirm(true); try await settle(model)
        var counts = await transport.counts(); XCTAssertEqual(counts.1, [false])
        model.repair = true
        model.prepare(); try await settle(model); model.confirm(true); try await settle(model)
        counts = await transport.counts(); XCTAssertEqual(counts.1, [false, true])
        XCTAssertTrue(model.message.contains("旧generation"))
        XCTAssertNotNil(model.recoveryBackup)
    }

    @MainActor func testExplicitDownloadNeedsNoLocalFilesAndCannotApproveAnotherMode() async throws {
        let transport = FixtureDecisionTransport()
        let model = DecisionInstallModel(transport: transport)
        model.refresh(); try await settle(model)
        XCTAssertFalse(model.canPrepare)
        model.download = true
        XCTAssertTrue(model.canPrepare)
        model.prepare(); try await settle(model)
        XCTAssertEqual(model.offer?.plan.transferBytes, 10)
        XCTAssertEqual(model.offer?.plan.diskRequiredBytes, 21)
        var counts = await transport.counts(); XCTAssertEqual(counts.1, [])
        model.confirm(false); try await settle(model)
        model.prepare(); try await settle(model)
        model.download = false; model.confirm(true); try await settle(model)
        counts = await transport.counts(); XCTAssertEqual(counts.1, [false]); XCTAssertEqual(counts.2, 1)
        model.download = true; model.repair = true
        model.prepare(); try await settle(model); model.confirm(true); try await settle(model)
        counts = await transport.counts(); XCTAssertEqual(counts.1, [false, true])
        XCTAssertNotNil(model.recoveryBackup)
        XCTAssertTrue(model.message.contains("有効化は変更していません"))
    }

    func testDownloadPlanValidatesFixedURLsHashesAndDiskRequirements() throws {
        let valid = try offerData(download: true)
        let decode: (Data) throws -> DecisionInstallOffer = { try PackagedDecisionInstallClient.decode(DecisionInstallOffer.self, $0) }
        try PackagedDecisionInstallClient.validateDownloadOffer(decode(valid))
        for key in ["transfer_bytes", "disk_required_bytes", "transaction_timeout_seconds"] {
            var frame = try JSONSerialization.jsonObject(with: valid) as! [String: Any]
            var plan = frame["plan"] as! [String: Any]; plan[key] = 0; frame["plan"] = plan
            XCTAssertThrowsError(try PackagedDecisionInstallClient.validateDownloadOffer(decode(JSONSerialization.data(withJSONObject: frame))))
        }
        for url in ["http://example.invalid/model", "https://dummy@example.invalid/model", "https://example.invalid/model?token=dummy"] {
            var frame = try JSONSerialization.jsonObject(with: valid) as! [String: Any]
            var plan = frame["plan"] as! [String: Any]; var delivery = plan["delivery"] as! [String: Any]
            var downloads = delivery["downloads"] as! [String: Any]
            downloads["model"] = ["url": url, "redirect_origins": []]
            delivery["downloads"] = downloads; plan["delivery"] = delivery; frame["plan"] = plan
            XCTAssertThrowsError(try PackagedDecisionInstallClient.validateDownloadOffer(decode(JSONSerialization.data(withJSONObject: frame))))
        }
    }

    func testOwnedDownloadProcessWaitsForTheBoundConfirmationWithoutLocalSourceArguments() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("decision-download-\(UUID())")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let helper = root.appendingPathComponent("fixture-helper")
        let frame = String(data: try offerData(repair: true, download: true), encoding: .utf8)!
        let script = """
        #!/bin/sh
        test "$#" = 5 && test "$1" = decision && test "$2" = download && test "$3" = fixture-v1 && test "$4" = --repair && test "$5" = --stdio-confirm || exit 2
        printf '%s\\n' '\(frame)'
        IFS= read -r answer || exit 3
        printf '{"schema_version":1,"type":"complete","consent_sha256":"\(String(repeating: "a", count: 64))","status":"cancelled"}\\n'
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: helper.path)
        let client = PackagedDecisionInstallClient(helper: { helper })
        let offer = try await client.prepareDownload(profile: "fixture-v1", repair: true)
        XCTAssertTrue(offer.plan.isDownload)
        let result = try await client.confirm(consent: offer.consentSha256, approved: false)
        if case .cancelled = result {} else { XCTFail("download cancel was not preserved") }
    }

    func testOwnedRepairProcessReceivesRepairArgumentsAndPreservesTheBoundPlan() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("decision-repair-\(UUID())")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let helper = root.appendingPathComponent("fixture-helper")
        let frame = String(data: try offerData(repair: true), encoding: .utf8)!
        let script = """
        #!/bin/sh
        test "$1" = decision && test "$2" = repair && test "$3" = fixture-v1 || exit 2
        printf '%s\\n' '\(frame)'
        IFS= read -r answer || exit 3
        printf '{"schema_version":1,"type":"complete","consent_sha256":"\(String(repeating: "a", count: 64))","status":"cancelled"}\\n'
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: helper.path)
        let client = PackagedDecisionInstallClient(helper: { helper })
        let offer = try await client.prepare(profile: "fixture-v1", worker: URL(fileURLWithPath: "/synthetic/worker"), model: URL(fileURLWithPath: "/synthetic/model"), repair: true)
        XCTAssertEqual(offer.plan.operation, "repair")
        let result = try await client.confirm(consent: offer.consentSha256, approved: false)
        if case .cancelled = result {} else { XCTFail("repair cancel was not preserved") }
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

    @MainActor func testVerificationNeedsOnlyARegisteredProfileAndClearsOnSelectionChange() async throws {
        let transport = FixtureDecisionTransport(profiles: ["fixture-v1", "other-v1"])
        let model = DecisionInstallModel(transport: transport)
        model.refresh(); try await settle(model)
        XCTAssertNil(model.worker); XCTAssertTrue(model.canVerify)
        model.verify(); try await settle(model)
        XCTAssertEqual(model.verification?.generation?.profile, "fixture-v1")
        XCTAssertTrue(model.message.contains("有効化・推論・容量判定は行っていません"))
        let counts = await transport.counts(); XCTAssertEqual(counts.0, 0); XCTAssertEqual(counts.1, [])
        model.profile = "other-v1"; XCTAssertNil(model.verification)
        XCTAssertTrue(model.message.contains("まだ検証していません")); XCTAssertFalse(model.message.contains("照合しました"))
    }

    @MainActor func testVerificationFailureDoesNotBecomeAnInstalledOrVerifiedResult() async throws {
        for status in ["not_installed", "integrity", "unverified_profile", "unsupported_platform", "cancelled"] {
            let model = DecisionInstallModel(transport: FixtureDecisionTransport(verificationStatus: status))
            model.refresh(); try await settle(model); model.verify(); try await settle(model)
            XCTAssertEqual(model.verification?.status, status); XCTAssertNil(model.verification?.generation)
            XCTAssertNil(model.offer)
        }
        let empty = FixtureDecisionTransport(profiles: [])
        let model = DecisionInstallModel(transport: empty)
        model.refresh(); try await settle(model); XCTAssertFalse(model.canVerify)
        model.verify(); let count = await empty.verificationCount(); XCTAssertEqual(count, 0)
    }

    @MainActor func testCancelledOrChangedVerificationCannotPublishTheOldResult() async throws {
        for cancel in [false, true] {
            let model = DecisionInstallModel(transport: FixtureDecisionTransport(profiles: ["fixture-v1", "other-v1"], verificationDelay: 20_000_000))
            model.refresh(); try await settle(model); model.verify()
            if cancel { model.cancel() } else { model.profile = "other-v1" }
            try await settle(model); try await Task.sleep(nanoseconds: 30_000_000)
            XCTAssertNil(model.verification); XCTAssertFalse(model.busy)
        }
    }

    func testVerificationDecoderRequiresTheSelectedProfileAndBoundedCompleteEvidence() throws {
        let valid = try verificationData()
        XCTAssertEqual(try PackagedDecisionInstallClient.decodeVerification(valid, profile: "fixture-v1").generation?.verifiedBytes, 10)
        XCTAssertThrowsError(try PackagedDecisionInstallClient.decodeVerification(valid, profile: "other-v1"))
        for (key, value) in [("status", "other"), ("type", "plan"), ("unexpected", "other")] {
            var frame = try JSONSerialization.jsonObject(with: valid) as! [String: Any]; frame[key] = value
            XCTAssertThrowsError(try PackagedDecisionInstallClient.decodeVerification(JSONSerialization.data(withJSONObject: frame), profile: "fixture-v1"))
        }
        for key in ["worker_sha256", "model_revision", "profile"] {
            var frame = try JSONSerialization.jsonObject(with: valid) as! [String: Any]
            var generation = frame["generation"] as! [String: Any]; generation[key] = "wrong"; frame["generation"] = generation
            XCTAssertThrowsError(try PackagedDecisionInstallClient.decodeVerification(JSONSerialization.data(withJSONObject: frame), profile: "fixture-v1"))
        }
        var frame = try JSONSerialization.jsonObject(with: valid) as! [String: Any]; frame["status"] = "not_installed"
        XCTAssertThrowsError(try PackagedDecisionInstallClient.decodeVerification(JSONSerialization.data(withJSONObject: frame), profile: "fixture-v1"))
        for status in ["not_installed", "unverified_profile", "integrity", "unsupported_platform", "unsupported_device", "cancelled", "unavailable"] {
            XCTAssertNil(try PackagedDecisionInstallClient.decodeVerification(verificationData(status: status), profile: "fixture-v1").generation)
        }
    }

    func testOwnedVerificationProcessUsesExplicitReadOnlyArgumentsAndNoImport() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("decision-verify-\(UUID())")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let helper = root.appendingPathComponent("fixture-helper")
        let frame = String(data: try verificationData(), encoding: .utf8)!
        let script = """
        #!/bin/sh
        test "$1" = decision && test "$2" = verify && test "$3" = fixture-v1 && test "$4" = --stdio-result || exit 2
        printf '%s\\n' '\(frame)'
        """
        try Data(script.utf8).write(to: helper)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: helper.path)
        let client = PackagedDecisionInstallClient(helper: { helper })
        let value = try await client.verify(profile: "fixture-v1")
        XCTAssertEqual(value.status, "verified")
        XCTAssertEqual(value.generation?.modelRepository, "synthetic/model")
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
