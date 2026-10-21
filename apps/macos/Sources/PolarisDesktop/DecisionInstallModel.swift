import Foundation
import Combine

struct DecisionInstallCatalog: Decodable, Sendable {
    let schemaVersion: Int
    let registeredProfiles: [String]
    let capacity: [String]
    let note: String
}

struct DecisionInstallOffer: Decodable, Sendable {
    struct Plan: Decodable, Sendable {
        struct Artifact: Decodable, Sendable { let source: String; let bytes: UInt64; let sha256: String }
        struct Delivery: Decodable, Sendable {
            struct Component: Decodable, Sendable { let name: String; let version: String; let license: String; let source: String }
            let purpose: String; let workerSource: String; let modelSource: String
            let license: String; let licenseSource: String; let noticeSource: String
            let components: [Component]
        }
        let transactionId: String; let profile: String; let generation: String
        let modelRepository: String; let modelRevision: String; let quantization: String
        let runtimeRevision: String; let platform: String; let architecture: String; let device: String
        let contextTokens: UInt32; let peakBytes: UInt64; let reserveBytes: UInt64
        let destination: String; let installedBytes: UInt64; let copyReserveBytes: UInt64
        let worker: Artifact; let model: Artifact; let delivery: Delivery
    }
    let schemaVersion: Int
    let type: String
    let consentSha256: String
    let plan: Plan
}

struct DecisionGenerationVerification: Decodable, Sendable {
    struct Generation: Decodable, Sendable {
        let profile: String; let generation: String
        let modelRepository: String; let modelRevision: String; let quantization: String
        let runtimeRevision: String; let workerSha256: String; let modelSha256: String
        let verifiedBytes: UInt64
    }
    let schemaVersion: Int; let type: String; let profile: String; let status: String
    let generation: Generation?
}

enum DecisionInstallOutcome: Sendable { case installed, cancelled }

protocol DecisionInstallTransport: Sendable {
    func verify(profile: String) async throws -> DecisionGenerationVerification
    func catalog() async throws -> DecisionInstallCatalog
    func prepare(profile: String, worker: URL, model: URL) async throws -> DecisionInstallOffer
    func confirm(consent: String, approved: Bool) async throws -> DecisionInstallOutcome
    func cancel() async
}

@MainActor
final class DecisionInstallModel: ObservableObject {
    @Published private(set) var catalog: DecisionInstallCatalog?
    @Published private(set) var verification: DecisionGenerationVerification?
    @Published private(set) var offer: DecisionInstallOffer?
    @Published private(set) var busy = false
    @Published private(set) var message = "登録済みprofileはまだ確認していません。"
    @Published var profile = "" {
        didSet {
            if profile != oldValue {
                verification = nil
                message = "対象profileを変更しました。固定generationはまだ検証していません。"
            }
        }
    }
    @Published var worker: URL?
    @Published var model: URL?
    private let transport: any DecisionInstallTransport
    private var task: Task<Void, Never>?
    private var operationID = UUID()

    init(transport: any DecisionInstallTransport = PackagedDecisionInstallClient()) { self.transport = transport }
    deinit { task?.cancel() }
    var canPrepare: Bool {
        !busy && offer == nil && worker != nil && model != nil
            && catalog?.registeredProfiles.contains(profile) == true
    }

    var canVerify: Bool { !busy && offer == nil && catalog?.registeredProfiles.contains(profile) == true }

    func verify() {
        guard canVerify else { return }
        verification = nil
        let selected = profile
        start(failureMessage: "固定generationを検証できませんでした。既存設定は維持しています。対象と導入状態を確認してください。") {
            let value = try await self.transport.verify(profile: selected)
            guard !Task.isCancelled else { return }
            guard self.profile == selected, value.profile == selected,
                  value.status != "verified" || value.generation?.profile == selected else { throw ServiceError.correlation }
            self.verification = value
            switch value.status {
            case "verified": self.message = "固定SHAを照合しました（読取り時点）。判断器の選択・有効化・推論・容量判定は行っていません。"
            case "not_installed": self.message = "固定generationは未導入です。対象ファイルを選んで導入計画を確認してください。"
            case "unverified_profile": self.message = "このprofileの受入登録は未完了です。実行可能とは扱いません。"
            case "integrity": self.message = "固定generationの整合性を確認できませんでした。既存資源は保持しています。"
            case "unsupported_platform", "unsupported_device": self.message = "このplatformまたはdeviceの検証には対応していません。"
            case "cancelled": self.message = "検証を取り消しました。検証済みとは扱いません。"
            default: self.message = "検証結果を確定できませんでした。既存資源は保持しています。"
            }
        }
    }

    func refresh() {
        guard !busy, offer == nil else { return }
        verification = nil
        start {
            let catalog = try await self.transport.catalog()
            guard !Task.isCancelled else { return }
            self.catalog = catalog
            if !catalog.registeredProfiles.contains(self.profile) { self.profile = catalog.registeredProfiles.first ?? "" }
            self.message = catalog.registeredProfiles.isEmpty
                ? "受入済みprofileは0件です。Clef/Clef-flashはまだ導入できません。"
                : "登録は導入・実行可能という確認ではありません。対象ファイルを選んで計画を確認してください。"
        }
    }

    func prepare() {
        guard canPrepare, let worker, let model else { return }
        let selected = profile
        verification = nil
        start {
            let offer = try await self.transport.prepare(profile: selected, worker: worker, model: model)
            guard !Task.isCancelled else { return }
            guard offer.plan.profile == selected, offer.plan.worker.source == worker.path,
                  offer.plan.model.source == model.path else { throw ServiceError.correlation }
            self.offer = offer
            self.message = "この計画で導入するか確認してください。確認は120秒以内に行います。"
        }
    }

    func confirm(_ approved: Bool) {
        guard !busy, let offer else { return }
        if approved && (profile != offer.plan.profile || worker?.path != offer.plan.worker.source || model?.path != offer.plan.model.source) {
            cancel()
            message = "対象が変わりました。新しい計画を確認してください。"
            return
        }
        self.offer = nil
        verification = nil
        start {
            let result = try await self.transport.confirm(consent: offer.consentSha256, approved: approved)
            guard !Task.isCancelled else { return }
            switch result {
            case .installed: self.message = "generationを導入しました。判断器の選択・有効化は変更していません。"
            case .cancelled: self.message = "導入を取り消しました。既存generationは保持しています。"
            }
        }
    }

    func cancel() {
        task?.cancel(); offer = nil; verification = nil; busy = true
        let id = UUID(); operationID = id
        message = "停止を要求しました。導入済みと確定していない内容は、計画を作り直して確認してください。"
        task = Task {
            await transport.cancel()
            guard self.operationID == id else { return }
            self.busy = false; self.task = nil
        }
    }

    private func start(failureMessage: String = "導入を確定できませんでした。既存設定は維持しています。計画を作り直してください。", _ operation: @escaping @MainActor () async throws -> Void) {
        busy = true
        let id = UUID(); operationID = id
        task = Task {
            defer { if self.operationID == id { self.busy = false; self.task = nil } }
            do { try await operation() }
            catch {
                guard self.operationID == id else { return }
                await self.transport.cancel()
                guard self.operationID == id else { return }
                self.offer = nil
                self.verification = nil
                self.message = failureMessage
            }
        }
    }
}
