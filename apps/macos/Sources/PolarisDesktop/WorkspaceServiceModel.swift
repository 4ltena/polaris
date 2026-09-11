import Combine
import Foundation
import PolarisSettings

enum OwnerStartupFailure: Equatable {
  case providerUnsupported, configurationInvalid, resourcePath, runtimeBinary
  case manifestRevision, architecture, bounds, helperStorage, offlineReadiness, resourceChanged

  init?(orderlyExitStatus: Int32) {
    switch orderlyExitStatus {
    case 80: self = .providerUnsupported
    case 81: self = .configurationInvalid
    case 82: self = .resourcePath
    case 83: self = .runtimeBinary
    case 84: self = .manifestRevision
    case 85: self = .architecture
    case 86: self = .bounds
    case 87: self = .helperStorage
    case 88: self = .offlineReadiness
    case 89: self = .resourceChanged
    default: return nil
    }
  }

  var japaneseMessage: String {
    switch self {
    case .providerUnsupported: "strict10 は Codex または OpenAI の主接続でのみ利用できます。"
    case .configurationInvalid: "strict10 の [embedding] 設定が不足しているか無効です。~/.polaris/config.toml を確認してください。"
    case .resourcePath: "strict10 の埋め込み資源のパス、別名、または会話ソースのパスを確認してください。"
    case .runtimeBinary: "strict10 の埋め込み runtime 実行ファイルを確認してください。"
    case .manifestRevision: "strict10 の multilingual-e5-small の manifest、SHA-256、revision を確認してください。"
    case .architecture: "strict10 は 384 次元の埋め込み資源を必要とします。"
    case .bounds: "strict10 の埋め込み資源の上限設定を確認してください。"
    case .helperStorage: "strict10 の helper 保存領域を確認してください。"
    case .offlineReadiness: "strict10 のオフライン埋め込みパッケージまたは準備状態を確認してください。"
    case .resourceChanged: "strict10 の埋め込み資源が検証中に変更されました。確認後に再試行してください。"
    }
  }
}

/// A single explicitly supplied durable session. The application owns selection
/// and window integration; this adapter never discovers stores or providers.
protocol WorkspaceServiceTransport: Sendable {
  func start() async throws -> ServiceHello
  func send(requestID: String, request: ServiceRequest) async throws -> ServiceReply
  func send(requestID: String, request: ServiceReadRequest) async throws -> ServiceReply
  func takeEvents() async throws -> [ServiceValue]
  func close() async -> ServiceClient.Exit?
  func hasUnreapedChild() async -> Bool
  /// Only the owned child may report a startup exit code. Adapters without
  /// process evidence must not turn arbitrary transport failures into causes.
  func ownedOrderlyExitStatus() async -> Int32?
}
extension WorkspaceServiceTransport {
  // An adapter without lifecycle evidence must conservatively retain ownership.
  func hasUnreapedChild() async -> Bool { true }
  func ownedOrderlyExitStatus() async -> Int32? { nil }
}
protocol WorkspaceRecoveryTransport: WorkspaceServiceTransport {
  func recoveryHello(requestID: String) async throws -> SourceRecoveryClient.Status
  func retryResultSave(requestID: String, target: SourceRecoveryClient.Target) async throws -> SourceRecoveryClient.Status
  func recoveryUnknownRequestID() async -> String?
}
extension ServiceClient: WorkspaceRecoveryTransport {}

@MainActor
final class WorkspaceServiceModel: ObservableObject {
  enum Phase: Equatable { case idle, connecting, ready, unavailable, closing, closed }
  enum Failure: Error, Equatable {
    case transport(ServiceError), rejected(String), stale, capacity, unsavedDraft
  }
  @Published private(set) var configurationOutcomeUnknown = false
  @Published private(set) var pendingConfiguration: ConfigurationIntent?
  @Published private(set) var pendingRoleConfiguration: RoleConfigurationIntent?
  @Published private(set) var roleConfigurationNotice: String?
  @Published private(set) var executionNeedsRefresh = false
  private var connectedConfigurationRevision: UInt64?
  @Published private(set) var savedConfigurationSelection: ExecutionBinding?
  @Published private(set) var configurationSelectionCurrent = false
  @Published private(set) var memoryStatus: WorkspaceMemoryStatus?
  private let configurationStore: SettingsStore?
  @Published private(set) var recoveryStatus: SourceRecoveryClient.Status?
  @Published private(set) var recoveryFailure = false
  @Published private(set) var recoveryRequestID: String?
  @Published private(set) var recoveryOutcomeUnknown = false
  @Published private(set) var savedRecoveryResults: [SourceRecoveryClient.SavedResult] = []
  private var recoveryMode: ServiceClient.RecoveryMode
  private var recoveryEpoch: String?
  private var recoveryObservedExit: ServiceClient.Exit?
  private var missingRecoveryProofs: Set<Data> = []
  private enum RecoveryRequest {
    case hello(String)
    case retry(String, SourceRecoveryClient.Target)
    var id: String { switch self { case .hello(let id), .retry(let id, _): return id } }
  }
  private var pendingRecovery: RecoveryRequest?
  @Published private(set) var phase: Phase = .idle
  @Published private(set) var failure: Failure?
  @Published private(set) var snapshot: ServiceValue?
  @Published private(set) var messages: [ServiceHistoryMessage] = []
  @Published private(set) var draftText = ""
  @Published private(set) var attachmentIDs: [String] = []
  struct ImportedAttachment: Identifiable {
    let attachment: WorkspaceAttachment
    let text: String
    var reviewed = false
    var id: UUID { attachment.id }
  }
  @Published private(set) var importedAttachments: [ImportedAttachment] = []
  @Published private(set) var savedDraftRevision: UInt64?
  @Published private(set) var isDirty = false
  @Published private(set) var isBusy = false
  @Published private(set) var lastDraftRequestID: String?
  @Published private(set) var draftOutcomeUnknown = false
  @Published private(set) var draftConflict = false
  @Published private(set) var requestStatus: ServiceRequestStatus?
  @Published private(set) var shutdownReady = false
  @Published private(set) var lastExit: ServiceClient.Exit?
  @Published private(set) var ownerStartupFailure: OwnerStartupFailure?
  @Published private(set) var isRecovering = false
  @Published private(set) var runs: [WorkspaceRun] = []
  @Published private(set) var approvals: [WorkspaceApproval] = []
  @Published private(set) var sourceApplyCandidates: [SourceApplyCandidate] = []
  @Published private(set) var sourceApplyPages: [Data: SourceApplyPage] = [:]
  @Published private(set) var sourceApplyNextOffset: UInt64?
  @Published private(set) var pendingSourceApplyAnswers: Set<Data> = []
  @Published private(set) var sourceApplyOutcomeUnknown: Set<Data> = []
  @Published private(set) var sourceApplyNotice: String?
  @Published private(set) var sourceApplyResultTimedOut = false
  private var sourceApplyRevision: UInt64?
  // A source-apply resolve ACK records only an intent. The service currently
  // persists its result without a dedicated subscription event, so reconcile a
  // pending result a small, fixed number of times. This never repeats resolve.
  private var sourceApplyResultPoll: Task<Void, Never>?
  private var sourceApplyResultPollID: UUID?
  private var sourceApplyResultReadInFlight = false
  private let sourceApplyResultReconciliationDelays: [Duration]
  @Published private(set) var streamMessages: [WorkspaceStreamMessage] = []
  @Published private(set) var retainedHistory: [ServiceHistoryMessage] = []
  @Published private(set) var streamHistoryConflict = false
  private var displayEpoch: String?
  @Published private(set) var startOutcomeUnknown = false
  @Published private(set) var lastStartRequestID: String?
  @Published private(set) var startStatus: ServiceRequestStatus?
  @Published private(set) var cancelRequested: Set<Data> = []
  @Published private(set) var pendingApprovalAnswers: Set<Data> = []
  private var needsRefresh = false
  private var pollingGeneration: UUID?
  private var eventSequence: UInt64 = 0
  private var eventRevision: UInt64 = 0
  private var savedSessionRevision: UInt64 = 0
  private struct PendingStart {
    let requestID: String
    let draft: UInt64
    let edit: UUID
    var accepted = false
  }
  private var pendingStart: PendingStart?
  let store: ServiceClient.PersistentStore
  let clientID: String

  private var makeTransport: @Sendable () throws -> any WorkspaceServiceTransport
  private let storageTransport: @Sendable () throws -> any WorkspaceServiceTransport
  private let storageRecoveryMode: ServiceClient.RecoveryMode
  private var transport: (any WorkspaceServiceTransport)?
  private var configuredOwnerStartup = false
  private var hello: ServiceHello?
  private var context: ServiceReadContext?
  private var generation = UUID()
  private var localEdit = UUID()
  private var monitor: Task<Void, Never>?
  private var closeTask: Task<Bool, Never>?
  private var observedDraftIsCurrent = false
  private struct PendingDraft {
    let requestID: String
    let revision: UInt64
    let text: String
    let edit: UUID
  }
  private var pendingDraft: PendingDraft?
  @Published private(set) var nextHistoryCursor: String?
  private var historyCursors: Set<Data> = []
  private var followsLatestHistory = false

  init(helperURL: URL, store: ServiceClient.PersistentStore, clientID: String,
       makeTransport: (@Sendable () throws -> any WorkspaceServiceTransport)? = nil,
       recoveryMode: ServiceClient.RecoveryMode = .disabled, configurationStore: SettingsStore? = nil,
       sourceApplyResultReconciliationDelays: [Duration] = [
         .milliseconds(250), .milliseconds(500), .seconds(1), .seconds(2),
         .seconds(4), .seconds(8), .seconds(14)
       ]) throws {
    try ServiceSchema.id.check(.string(clientID))
    // Validate the supplied launch coordinates even with an injected test transport.
    _ = try ServiceClient(helperURL: helperURL, clientID: clientID, persistentStore: store, recoveryMode: recoveryMode)
    self.configurationStore = configurationStore
    if let pending = try configurationStore?.load().pendingRoleConfiguration {
      guard pending.projectID == store.projectID, pending.sessionID == store.sessionID,
            pending.clientID == clientID else { throw ServiceError.correlation }
      pendingRoleConfiguration = pending
    }
    if let pending = try configurationStore?.load().pendingConfiguration {
      guard Data(pending.projectID.utf8) == Data(store.projectID.utf8), Data(pending.sessionID.utf8) == Data(store.sessionID.utf8),
            Data(pending.clientID.utf8) == Data(clientID.utf8) else { throw ServiceError.correlation }
      pendingConfiguration = pending; configurationOutcomeUnknown = true
    }
    self.recoveryMode = recoveryMode
    self.storageRecoveryMode = recoveryMode
    self.sourceApplyResultReconciliationDelays = sourceApplyResultReconciliationDelays
    self.store = store
    self.clientID = clientID
    let storage = makeTransport ?? {
      try ServiceClient(helperURL: helperURL, clientID: clientID, persistentStore: store, recoveryMode: recoveryMode)
    }
    self.storageTransport = storage
    self.makeTransport = storage
  }

  var canSaveDraft: Bool {
    phase == .ready && !isBusy && !draftOutcomeUnknown && !startOutcomeUnknown && !draftConflict && isDirty
      && savedDraftRevision != nil && attachmentIDs.isEmpty
      && importedAttachments.allSatisfy(\.reviewed)
      && hello?.capabilities.contains("draft_update") == true
  }
  var executionAvailable: Bool { supports("run_start") }
  var workspaceReadAvailable: Bool { supports("workspace_read") }
  var sourceApplyReadAvailable: Bool { supports("source_apply_read") }
  var canSend: Bool {
    pendingConfiguration == nil && pendingRoleConfiguration == nil && !executionNeedsRefresh && phase == .ready && !isBusy && !draftOutcomeUnknown && !draftConflict && !startOutcomeUnknown
      && !runs.contains(where: \.blocksStart) && attachmentIDs.isEmpty && savedDraftRevision != nil
      && (!draftText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !importedAttachments.isEmpty)
      && importedAttachments.allSatisfy(\.reviewed)
      && supports("run_start") && supports("request_status") && (!isDirty || supports("draft_update"))
      && snapshot?["configuration"]?["configuration_revision"]?.decimal != nil
      && snapshot?["policy_revision"]?.decimal != nil
  }
  private func supports(_ capability: String) -> Bool { hello?.capabilities.contains(capability) == true }
  func canCancel(_ run: WorkspaceRun) -> Bool {
    phase == .ready && !isBusy && supports("run_cancel") && runs.contains(run)
      && !run.isTerminal && run.state != "cancelling" && !cancelRequested.contains(run.key)
  }
  func canResolve(_ approval: WorkspaceApproval) -> Bool {
    phase == .ready && !isBusy && supports("approval_resolve") && approvals.contains(approval)
      && !pendingApprovalAnswers.contains(approval.key)
      && approval.policy == snapshot?["policy_revision"]?.decimal
      && Double(approval.expires) > Date().timeIntervalSince1970 * 1000
      && runs.contains { $0.id.utf8.elementsEqual(approval.runID.utf8) && $0.attemptID.utf8.elementsEqual(approval.attemptID.utf8) && !$0.isTerminal }
  }

  func canResolveSourceApply(_ candidate: SourceApplyCandidate,
                             decision: ServiceApprovalDecision = .allow) -> Bool {
    guard phase == .ready, !isBusy, supports("source_apply_resolve"),
      sourceApplyRevision == snapshot?["session_revision"]?.decimal,
      sourceApplyRevision == eventRevision,
      !candidate.invalidated, !candidate.intentCommitted, candidate.decision == nil,
      !pendingSourceApplyAnswers.contains(candidate.key), !sourceApplyOutcomeUnknown.contains(candidate.key),
      candidate.policyRevision == snapshot?["policy_revision"]?.decimal,
      !candidate.isExpired()
    else { return false }
    guard sourceApplyCandidates.contains(where: { $0.key == candidate.key
      && $0.payloadHash.utf8.elementsEqual(candidate.payloadHash.utf8)
      && $0.runID.utf8.elementsEqual(candidate.runID.utf8)
      && $0.attemptID.utf8.elementsEqual(candidate.attemptID.utf8) }) else { return false }
    guard runs.contains(where: { $0.id.utf8.elementsEqual(candidate.runID.utf8)
      && $0.attemptID.utf8.elementsEqual(candidate.attemptID.utf8) && $0.isTerminal }) else { return false }
    guard decision == .allow else { return true }
    guard let page = sourceApplyPages[candidate.key],
      page.approvalID.utf8.elementsEqual(candidate.id.utf8),
      page.payloadHash.utf8.elementsEqual(candidate.payloadHash.utf8),
      page.sessionRevision == sourceApplyRevision, page.nextOffset == nil,
      UInt64(page.entries.count) == candidate.entryCount
    else { return false }
    return true
  }
  var canReconnect: Bool { !isBusy && !isRecovering && (phase == .unavailable || phase == .closed) }
  var canRestoreStorageOnly: Bool {
    phase == .unavailable && !isBusy && !isRecovering && pendingConfiguration == nil
      && pendingRoleConfiguration == nil && !draftOutcomeUnknown && !startOutcomeUnknown
      && !isDirty && !recoveryBlocksReplacement && transport != nil
  }

  var recoveryHasExited: Bool { recoveryObservedExit != nil }
  var recoveryAwaitingProof: Bool { !missingRecoveryProofs.isEmpty }
  var recoveryAvailable: Bool { recoveryMode == .resultOnly && transport is any WorkspaceRecoveryTransport && (phase == .unavailable || phase == .closed) }
  var canCheckRecovery: Bool {
    recoveryMode == .resultOnly && transport is any WorkspaceRecoveryTransport
      && phase == .unavailable && !isBusy && !isRecovering && recoveryObservedExit == nil
  }
  var canRetryRecovery: Bool { canCheckRecovery && pendingRecovery == nil && recoveryStatus?.retryTarget != nil }
  var canReconcileRecovery: Bool { canCheckRecovery && pendingRecovery != nil }
  private var recoveryBlocksReplacement: Bool { pendingRecovery != nil || !missingRecoveryProofs.isEmpty }
  private var recoveryExitProven: Bool {
    recoveryStatus?.state == .readyToExit && recoveryStatus?.error == nil
      && !recoveryBlocksReplacement && !recoveryOutcomeUnknown && !recoveryFailure
      && recoveryObservedExit?.status == 0 && recoveryObservedExit?.forced == false
  }

  func checkRecoveryStatus() async {
    guard canCheckRecovery, pendingRecovery == nil, let connection = transport as? any WorkspaceRecoveryTransport else { return }
    let token = generation
    isBusy = true
    let unknown = await connection.recoveryUnknownRequestID()
    guard generation == token else { return }
    isBusy = false
    // Only startup hello can predate this model's pending request.
    await performRecovery(.hello(unknown ?? UUID().uuidString), connection: connection, token: token)
  }

  func reconcileRecoveryRequest() async {
    guard canReconcileRecovery, let pendingRecovery, let connection = transport as? any WorkspaceRecoveryTransport else { return }
    await performRecovery(pendingRecovery, connection: connection, token: generation)
  }

  func retryRecoveryResult() async {
    guard canRetryRecovery, let status = recoveryStatus, let target = status.retryTarget,
          let connection = transport as? any WorkspaceRecoveryTransport else { return }
    let exact = SourceRecoveryClient.Target(engineEpoch: status.engineEpoch, projectID: store.projectID,
      sessionID: store.sessionID, runID: target.runID, attemptID: target.attemptID,
      approvalID: target.approvalID, operationID: target.operationID, payloadHash: target.payloadHash)
    await performRecovery(.retry(UUID().uuidString, exact), connection: connection, token: generation)
  }

  private func performRecovery(_ request: RecoveryRequest, connection: any WorkspaceRecoveryTransport, token: UUID) async {
    pendingRecovery = request; recoveryRequestID = request.id
    recoveryOutcomeUnknown = true; recoveryFailure = false; isBusy = true
    defer { if generation == token { isBusy = false } }
    do {
      let status: SourceRecoveryClient.Status
      switch request {
      case .hello(let id): status = try await connection.recoveryHello(requestID: id)
      case .retry(let id, let target): status = try await connection.retryResultSave(requestID: id, target: target)
      }
      try requireCurrent(token)
      guard Data(status.requestID.utf8) == Data(request.id.utf8),
            Data(status.projectID.utf8) == Data(store.projectID.utf8),
            Data(status.sessionID.utf8) == Data(store.sessionID.utf8),
            (hello?.epoch ?? recoveryEpoch).map({ Data($0.utf8) == Data(status.engineEpoch.utf8) }) ?? true
      else { throw Failure.stale }
      if case .retry(_, let target) = request, status.error == nil {
        guard let result = status.result, Data(result.operationID.utf8) == Data(target.operationID.utf8) else { throw Failure.stale }
      }
      if case .hello = request, status.result != nil { throw Failure.stale }
      if let result = status.result {
        let key = Data(result.operationID.utf8)
        if let old = savedRecoveryResults.first(where: { Data($0.operationID.utf8) == key }) {
          guard Data(old.resultID.utf8) == Data(result.resultID.utf8), old.savedRevision == result.savedRevision else { throw Failure.stale }
        } else {
          guard savedRecoveryResults.count < 128 else { throw Failure.capacity }
          savedRecoveryResults.append(result)
        }
        missingRecoveryProofs.remove(key)
      }
      if let next = status.retryTarget {
        guard missingRecoveryProofs.count < 128 else { throw Failure.capacity }
        let key = Data(next.operationID.utf8)
        if !savedRecoveryResults.contains(where: { Data($0.operationID.utf8) == key }) { missingRecoveryProofs.insert(key) }
      }
      recoveryEpoch = status.engineEpoch; recoveryStatus = status; recoveryOutcomeUnknown = false
      recoveryFailure = status.error != nil
      if status.error == nil { pendingRecovery = nil }
    } catch {
      guard generation == token else { return }
      recoveryFailure = true // Keep exact request and draft; never auto-retry.
    }
  }

  /// Explicit observation only. No recovery request, replacement or readiness inference.
  func observeRecoveryExit() async {
    guard recoveryAvailable, !isBusy, !isRecovering, let connection = transport else { return }
    let token = generation
    isBusy = true
    let exit = await connection.close()
    guard generation == token else { return }
    lastExit = exit; recoveryObservedExit = exit; isBusy = false
    if recoveryExitProven && !isDirty && !draftOutcomeUnknown && !draftConflict && !startOutcomeUnknown {
      phase = .closed
    }
  }

  /// Explicit same-store recovery. Never replace a child whose exit is unknown,
  /// and never replace this model's draft or original publication coordinates.
  /// Rebind transport only after shutdown and actual child exit; keep editing state.
  func startConfigured(revalidate: () throws -> Void = {}, recoveryMode: ServiceClient.RecoveryMode = .resultOnly,
                       makeTransport: @escaping @Sendable () throws -> any WorkspaceServiceTransport) async throws {
    guard phase == .closed, shutdownReady, lastExit?.status == 0, lastExit?.forced == false,
          pendingConfiguration == nil, !draftOutcomeUnknown, !startOutcomeUnknown, !isDirty,
          !recoveryBlocksReplacement else { throw ServiceError.notReady }
    let token = generation
    if let transport, await transport.hasUnreapedChild() { throw ServiceError.notReady }
    guard generation == token, phase == .closed, shutdownReady,
          pendingConfiguration == nil, !draftOutcomeUnknown, !startOutcomeUnknown, !isDirty,
          !recoveryBlocksReplacement else { throw ServiceError.notReady }
    try revalidate()
    self.transport = nil
    self.makeTransport = makeTransport; self.recoveryMode = recoveryMode
    configuredOwnerStartup = true; ownerStartupFailure = nil
    connectedConfigurationRevision = nil; executionNeedsRefresh = false
    phase = .idle
    await start()
  }

  func reconnect() async {
    guard canReconnect, !recoveryBlocksReplacement else { return }
    isRecovering = true
    defer { isRecovering = false }
    let token = UUID(); generation = token
    monitor?.cancel(); monitor = nil
    phase = .connecting; isBusy = true; observedDraftIsCurrent = false
    if let connection = transport {
      let exit = await connection.close()
      let mayOwnChild = await connection.hasUnreapedChild()
      guard generation == token else { return }
      lastExit = exit
      guard exit != nil || !mayOwnChild else { failed(ServiceError.notReady, token: token); return }
      transport = nil
    }
    phase = .idle; isBusy = false
    await start()
    if phase == .ready, draftOutcomeUnknown { await reconcileCurrentDraft() }
  }

  /// Restore the original storage-only owner only after a failed configured
  /// owner has exited. This neither repeats configuration nor starts a provider.
  func restoreStorageOnlyAfterOwnerFailure() async -> Bool {
    guard canRestoreStorageOnly, let connection = transport else { return false }
    let token = generation
    isRecovering = true; isBusy = true
    defer { isRecovering = false; isBusy = false }
    let exit = await connection.close()
    let ownsChild = await connection.hasUnreapedChild()
    let knownStartupFailure = ownerStartupFailure != nil
    guard generation == token, exit?.forced == false, !ownsChild,
          (exit?.status == 0 || knownStartupFailure) else { return false }
    lastExit = exit; recoveryObservedExit = exit; transport = nil
    makeTransport = storageTransport; recoveryMode = storageRecoveryMode
    connectedConfigurationRevision = nil; executionNeedsRefresh = false
    phase = .idle
    await start()
    return phase == .ready
  }

  func startStorageOnlyAfterClose(revalidate: () throws -> Void = {}) async throws {
    guard phase == .closed, shutdownReady, lastExit?.status == 0, lastExit?.forced == false,
          pendingConfiguration == nil, !draftOutcomeUnknown, !startOutcomeUnknown, !isDirty,
          !recoveryBlocksReplacement else { throw ServiceError.notReady }
    try revalidate()
    transport = nil; makeTransport = storageTransport; recoveryMode = storageRecoveryMode
    connectedConfigurationRevision = nil; executionNeedsRefresh = false
    phase = .idle
    await start()
    guard phase == .ready else { throw ServiceError.notReady }
  }

  /// A completed original ledger request proves publication, but adopting the
  /// current remote text still requires an explicit conflict choice if it differs.
  func reconcileDraft() async {
    guard phase == .ready, !isBusy, draftOutcomeUnknown else { return }
    let token = generation
    await refresh()
    guard generation == token else { return }
    await reconcileCurrentDraft()
  }

  private func reconcileCurrentDraft() async {
    guard phase == .ready, !isBusy, draftOutcomeUnknown, let pendingDraft else { return }
    let token = generation
    await lookupRequest(clientID: clientID, requestID: pendingDraft.requestID)
    guard generation == token, phase == .ready,
          case .completed(let revision, _) = requestStatus,
          let context, revision <= context.sessionRevision,
          let draft = snapshot?["draft"], let remoteRevision = draft["draft_revision"]?.decimal,
          pendingDraft.revision < UInt64.max, remoteRevision >= pendingDraft.revision + 1 else { return }
    draftOutcomeUnknown = false
    if remoteRevision == pendingDraft.revision + 1, draft["text"]?.string == pendingDraft.text,
       attachmentIDs.isEmpty {
      savedDraftRevision = remoteRevision
      draftConflict = false
      if localEdit == pendingDraft.edit { isDirty = false }
    } else {
      draftConflict = true
    }
  }

  func editDraft(_ text: String) {
    guard phase != .closing else { return }
    draftText = text
    localEdit = UUID()
    isDirty = true
  }

  func addImportedAttachment(_ attachment: WorkspaceAttachment, text: String) throws {
    guard phase == .ready, !isBusy, !draftOutcomeUnknown, !startOutcomeUnknown,
          !importedAttachments.contains(where: { $0.attachment.url.standardizedFileURL == attachment.url.standardizedFileURL }),
          importedAttachments.count < 4, text.utf8.count <= 65_536,
          importedAttachments.reduce(0, { $0 + $1.text.utf8.count }) + text.utf8.count <= 131_072 else { throw ServiceError.capacity }
    importedAttachments.append(ImportedAttachment(attachment: attachment, text: text))
    editDraft(draftText)
  }

  func reviewImportedAttachment(_ id: UUID) {
    guard let index = importedAttachments.firstIndex(where: { $0.id == id }) else { return }
    importedAttachments[index].reviewed = true
  }

  func removeImportedAttachment(_ id: UUID) {
    importedAttachments.removeAll { $0.id == id }
    editDraft(draftText)
  }

  /// Persist reviewed bytes as visible message text, never reopen a mutable path at send time.
  private func materializeAttachments() {
    guard !importedAttachments.isEmpty else { return }
    let additions = importedAttachments.map { "\n\n--- 添付: " + $0.attachment.name + " ---\n" + $0.text + "\n--- 添付終端 ---" }.joined()
    editDraft(draftText + additions)
    importedAttachments = []
  }

  func start() async {
    guard transport == nil, phase != .connecting, phase != .closing else { return }
    let token = UUID(); generation = token
    let ownerStartup = configuredOwnerStartup
    configuredOwnerStartup = false
    phase = .connecting; isBusy = true; failure = nil; shutdownReady = false
    context = nil; hello = nil; needsRefresh = false; pollingGeneration = nil
    await stopSourceApplyResultReconciliation()
    recoveryStatus = nil; recoveryFailure = false; recoveryRequestID = nil; recoveryOutcomeUnknown = false
    recoveryEpoch = nil; pendingRecovery = nil; missingRecoveryProofs = []; savedRecoveryResults = []; recoveryObservedExit = nil
    do {
      let connection = try makeTransport()
      transport = connection
      let accepted = try await connection.start()
      try requireCurrent(token)
      hello = accepted
      guard accepted.capabilities.contains("session_read"), accepted.capabilities.contains("history_read") else {
        throw ServiceError.notReady
      }
      try await load(connection, token: token)
      try requireCurrent(token)
      if ownerStartup { ownerStartupFailure = nil }
      phase = .ready; isBusy = false
      monitor = Task { [weak self] in
        while !Task.isCancelled {
          do { try await Task.sleep(for: .milliseconds(100)) } catch { return }
          guard let self, self.generation == token else { return }
          await self.poll()
        }
      }
    } catch {
      if ownerStartup, let status = await transport?.ownedOrderlyExitStatus(),
         let mapped = OwnerStartupFailure(orderlyExitStatus: status) {
        ownerStartupFailure = mapped
      }
      if recoveryMode == .resultOnly, let connection = transport as? any WorkspaceRecoveryTransport {
        let unknown = await connection.recoveryUnknownRequestID()
        guard generation == token else { return }
        if let unknown {
          pendingRecovery = .hello(unknown); recoveryRequestID = unknown; recoveryOutcomeUnknown = true
        }
      }
      if generation == token, (error as? ServiceError) == .launch { transport = nil }
      failed(error, token: token)
    }
  }

  func refresh() async {
    guard phase == .ready, !isBusy, let transport else { return }
    let token = generation
    isBusy = true; failure = nil
    // Consume only when the read actually starts. Busy guards leave it pending;
    // failed reads surface an error rather than retrying in a background loop.
    needsRefresh = false
    do {
      try await load(transport, token: token)
      try requireCurrent(token)
      isBusy = false
    } catch { failed(error, token: token) }
  }

  private func load(_ connection: any WorkspaceServiceTransport, token: UUID) async throws {
    guard let hello else { throw ServiceError.notReady }
    observedDraftIsCurrent = false
    let editAtStart = localEdit
    let payload = try await send(.subscribe(session: store.sessionID), connection: connection, token: token)
    let incoming = try ServiceReadContext(snapshot: payload, epoch: hello.epoch, sessionID: store.sessionID)
    if let context, context.epoch.utf8.elementsEqual(incoming.epoch.utf8),
       incoming.sessionRevision < max(context.sessionRevision, eventRevision) { throw Failure.stale }
    // A confirmed mutation is a durable lower bound even across transport restart.
    guard incoming.sessionRevision >= savedSessionRevision else { throw Failure.stale }
    // Preserve the already visible window when refreshing the same conversation.
    let visibleHistoryCount = max(1, messages.count + retainedHistory.count)
    var cursor: String? = payload["history_start_cursor"]!.string!
    var seenCursors: Set<Data> = []
    var seenMessages: Set<Data> = []
    var loaded: [ServiceHistoryMessage] = []
    var bytes = 0
    while let current = cursor {
      guard seenCursors.insert(Data(current.utf8)).inserted else { throw Failure.stale }
      let request = ServiceReadRequest.history(context: incoming, cursor: current, limit: 4)
      let page = try ServiceHistoryPage(try await send(request, connection: connection, token: token))
      guard page.snapshotID.utf8.elementsEqual(incoming.snapshotID.utf8),
        page.sessionRevision == incoming.sessionRevision, page.messages.count <= 4,
        page.nextCursor == nil || !page.messages.isEmpty
      else { throw Failure.stale }
      for message in page.messages {
        guard seenMessages.insert(Data(message.id.utf8)).inserted else { throw Failure.stale }
        bytes += message.text.utf8.count
        guard bytes <= 16_777_216, loaded.count < 65_536 else { throw Failure.capacity }
        loaded.append(message)
      }
      cursor = page.nextCursor
      if let cursor, seenCursors.contains(Data(cursor.utf8)) { throw Failure.stale }
      if !followsLatestHistory || loaded.count >= visibleHistoryCount { break }
    }
    try requireCurrent(token)
    let draft = payload["draft"]!
    let remoteRevision = draft["draft_revision"]!.decimal!
    if let savedDraftRevision, remoteRevision < savedDraftRevision { throw Failure.stale }
    let restoredRuns = try array(payload["runs"]).map { try WorkspaceRun($0, generation: token) }
    let restoredApprovals = try array(payload["unresolved_approvals"]).map { try WorkspaceApproval($0, generation: token) }
    guard Set(restoredRuns.map(\.key)).count == restoredRuns.count,
          Set(restoredApprovals.map(\.key)).count == restoredApprovals.count else { throw Failure.stale }
    // run.start consumes exactly the submitted draft. Its ACK never consumes a later edit.
    if let pending = pendingStart, pending.accepted, pending.draft < UInt64.max,
       remoteRevision == pending.draft + 1, draft["text"] == .string("") {
      savedDraftRevision = remoteRevision
      if localEdit == pending.edit { isDirty = false }
      draftConflict = false
      pendingStart = nil
    }
    let display = try reconciledDisplay(history: loaded, epoch: incoming.epoch)
    let restoredMemory = try payload["memory"].map(WorkspaceMemoryStatus.init)
    snapshot = payload; context = incoming; messages = loaded; memoryStatus = restoredMemory
    if executionAvailable, let revision = payload["configuration"]?["configuration_revision"]?.decimal {
      if let connectedConfigurationRevision { executionNeedsRefresh = revision != connectedConfigurationRevision }
      else { connectedConfigurationRevision = revision }
    }
    retainedHistory = display.history; streamMessages = display.streams
    displayEpoch = incoming.epoch; streamHistoryConflict = false
    runs = restoredRuns; approvals = restoredApprovals
    eventSequence = payload["position"]!["event_seq"]!.decimal!
    eventRevision = incoming.sessionRevision
    pendingApprovalAnswers.formIntersection(Set(approvals.map(\.key)))
    cancelRequested.formIntersection(Set(runs.filter { !$0.isTerminal }.map(\.key)))
    nextHistoryCursor = cursor; historyCursors = seenCursors
    if isDirty, let savedDraftRevision, remoteRevision != savedDraftRevision {
      draftConflict = true
    } else {
      if isDirty, savedDraftRevision == nil, draft["text"]!.string! != "" {
        draftConflict = true
      }
      savedDraftRevision = remoteRevision
    }
    observedDraftIsCurrent = true
    if !isDirty, !startOutcomeUnknown, localEdit == editAtStart {
      draftText = draft["text"]!.string!
    }
    if case .array(let ids) = draft["attachment_ids"] { attachmentIDs = ids.compactMap(\.string) }
  }

  /// Follow validated cursors to the tail; page reads enforce byte and row limits.
  func catchUpHistory() async {
    followsLatestHistory = true
    let token = generation
    while !Task.isCancelled {
      guard generation == token, phase == .ready, !isBusy, let cursor = nextHistoryCursor else { return }
      await loadNextHistoryPage()
      guard failure == nil, nextHistoryCursor != cursor else { return }
    }
  }

  func loadNextHistoryPage() async {
    guard phase == .ready, !isBusy, let transport, let context, let cursor = nextHistoryCursor else { return }
    let token = generation
    isBusy = true
    do {
      guard !historyCursors.contains(Data(cursor.utf8)) else { throw Failure.stale }
      let page = try ServiceHistoryPage(try await send(.history(context: context, cursor: cursor, limit: 4),
                                                      connection: transport, token: token))
      guard page.snapshotID.utf8.elementsEqual(context.snapshotID.utf8),
            page.sessionRevision == context.sessionRevision, page.messages.count <= 4,
            page.nextCursor == nil || !page.messages.isEmpty else { throw Failure.stale }
      var cursors = historyCursors; cursors.insert(Data(cursor.utf8))
      if let next = page.nextCursor, cursors.contains(Data(next.utf8)) { throw Failure.stale }
      var ids = Set(messages.map { Data($0.id.utf8) })
      for message in page.messages {
        guard ids.insert(Data(message.id.utf8)).inserted else { throw Failure.stale }
      }
      let combined = messages + page.messages
      guard combined.count <= 65_536, combined.reduce(0, { $0 + $1.text.utf8.count }) <= 16_777_216 else {
        throw Failure.capacity
      }
      let display = try reconciledDisplay(history: combined, epoch: context.epoch)
      messages = combined; retainedHistory = display.history; streamMessages = display.streams
      streamHistoryConflict = false
      historyCursors = cursors; nextHistoryCursor = page.nextCursor
      isBusy = false
    } catch { failed(error, token: token) }
  }

  /// Save configuration only; no bootstrap, provider opening or run starts here.
  func configureSelection() async {
    guard phase == .ready, !isBusy, pendingConfiguration == nil, supports("session_configure"),
          let configurationStore, let transport,
          let revision = snapshot?["configuration"]?["configuration_revision"]?.decimal else { return }
    let token = generation
    do {
      let settings = try configurationStore.load()
      if let pending = settings.pendingConfiguration {
        pendingConfiguration = pending; configurationOutcomeUnknown = true; return
      }
      guard let selection = settings.preferences?.executionBinding, let historyMode = settings.preferences?.historyMode else { return }
      guard historyMode != .strict10 || selection.provider == .codex || selection.provider == .openai else {
        throw Failure.rejected("strict10_requires_cloud")
      }
      let intent = ConfigurationIntent(requestID: UUID().uuidString, clientID: clientID,
        projectID: store.projectID, sessionID: store.sessionID, expectedRevision: revision, selection: selection, historyMode: historyMode)
      // Persist before any send; later settings edits merge and retain this record.
      try configurationStore.recordConfigurationIntent(intent)
      pendingConfiguration = intent; configurationOutcomeUnknown = true
      configurationSelectionCurrent = false; isBusy = true; failure = nil
      let reply = try await transport.send(requestID: intent.requestID,
        request: .configure(session: store.sessionID, revision: revision, selection: selection, historyMode: historyMode))
      try requireCurrent(token)
      let payload = try checked(reply, id: intent.requestID, method: "session.configure")
      guard revision < UInt64.max, let config = payload["configuration"],
            config["configuration_revision"]?.decimal == revision + 1,
            matchesConfiguration(config, selection, historyMode: historyMode), let saved = payload["session_revision"]?.decimal,
            saved > (context?.sessionRevision ?? 0) else { throw Failure.stale }
      savedSessionRevision = saved
      // A late selection change cannot undo this proven durable CAS.
      savedConfigurationSelection = selection; configurationOutcomeUnknown = false
      isBusy = false
      await refresh()
      try requireCurrent(token)
      try finishConfiguration(intent)
    } catch {
      guard generation == token else { return }
      if case Failure.rejected(let code) = error, !Self.ambiguousPublication(code), let pending = pendingConfiguration {
        configurationOutcomeUnknown = false
        do { try configurationStore.clearConfigurationIntent(requestID: pending.requestID); pendingConfiguration = nil }
        catch { /* Keep durable intent if cleanup could not be confirmed. */ }
      }
      failed(error, token: token)
    }
  }

  /// Only query the original ledger ID. NotFound never causes a new configure.
  func reconcileConfiguration() async {
    guard phase == .ready, !isBusy, let pending = pendingConfiguration else { return }
    let token = generation
    await refresh()
    guard generation == token, phase == .ready else { return }
    await lookupRequest(clientID: pending.clientID, requestID: pending.requestID)
    guard generation == token, phase == .ready,
          case .completed(let revision, _) = requestStatus,
          revision <= (context?.sessionRevision ?? 0) else { return }
    configurationOutcomeUnknown = false
    savedConfigurationSelection = pending.selection
    do {
      try finishConfiguration(pending)
    } catch { failed(error, token: token) }
  }

  private func matchesConfiguration(_ value: ServiceValue, _ selection: ExecutionBinding, historyMode: HistoryMode) -> Bool {
    value["provider"]?.matchesID(selection.provider.rawValue) == true
      && value["model"]?.matchesID(selection.model) == true
      && value["effort"]?.matchesID(selection.storedEffort) == true
      && (value["history_mode"]?.string ?? HistoryMode.legacy.rawValue) == historyMode.rawValue
  }
  private func finishConfiguration(_ intent: ConfigurationIntent) throws {
    guard phase == .ready, let config = snapshot?["configuration"],
          let expected = UInt64(intent.expectedRevision), expected < UInt64.max,
          let observed = config["configuration_revision"]?.decimal, observed >= expected + 1,
          matchesConfiguration(config, intent.selection, historyMode: intent.historyMode), let configurationStore else { throw Failure.stale }
    try configurationStore.clearConfigurationIntent(requestID: intent.requestID)
    pendingConfiguration = nil
    configurationSelectionCurrent = try configurationStore.load().preferences?.executionBinding == intent.selection
  }

  /// Saving must succeed before shutdown invalidates the connected editing state.
  func saveAndClose() async -> Bool {
    guard pendingConfiguration == nil, importedAttachments.allSatisfy(\.reviewed) else { return false }
    materializeAttachments()
    if recoveryMode == .resultOnly, phase == .unavailable || phase == .closed {
      await observeRecoveryExit()
      return recoveryExitProven && !isDirty && !draftOutcomeUnknown && !draftConflict && !startOutcomeUnknown
    }
    guard !isBusy, !draftOutcomeUnknown, !draftConflict, !startOutcomeUnknown else { return false }
    if isDirty { await saveDraft() }
    guard !isDirty, !draftOutcomeUnknown, !isBusy else { return false }
    return await close()
  }

  func saveDraft() async {
    guard canSaveDraft else { return }
    materializeAttachments()
    guard canSaveDraft, let transport, let revision = savedDraftRevision else { return }
    let token = generation, edit = localEdit
    let text = draftText, requestID = UUID().uuidString
    pendingDraft = PendingDraft(requestID: requestID, revision: revision, text: text, edit: edit)
    lastDraftRequestID = requestID; requestStatus = nil
    draftOutcomeUnknown = true; observedDraftIsCurrent = false
    isBusy = true; failure = nil
    do {
      let reply = try await transport.send(requestID: requestID,
        request: .draft(session: store.sessionID, revision: revision, text: text))
      try requireCurrent(token)
      let payload = try checked(reply, id: requestID, method: "draft.update")
      guard revision < UInt64.max, payload["draft_revision"]?.decimal == revision + 1,
        let sessionRevision = payload["session_revision"]?.decimal,
        sessionRevision > (context?.sessionRevision ?? 0)
      else { throw Failure.stale }
      savedSessionRevision = sessionRevision
      savedDraftRevision = revision + 1
      draftOutcomeUnknown = false
      // ACK records what was saved; it never clears or rewrites the current text.
      if edit == localEdit { isDirty = false }
      isBusy = false
    } catch {
      guard generation == token else { return }
      if case Failure.rejected(let code) = error {
        draftOutcomeUnknown = Self.ambiguousPublication(code)
        if code == "revision_conflict" { draftConflict = true }
      }
      failed(error, token: token)
    }
  }

  /// The caller supplies the original ledger coordinates. No automatic resend,
  /// including for NotFound or after the helper has been reaped.
  func lookupRequest(clientID owner: String, requestID target: String) async {
    guard phase == .ready, !isBusy, let transport, let context else { return }
    let token = generation
    isBusy = true; failure = nil; requestStatus = nil
    do {
      let value = try await send(.status(context: context, clientID: owner, requestID: target),
                                 connection: transport, token: token)
      let status = try ServiceRequestStatus(value)
      try requireCurrent(token)
      requestStatus = status; isBusy = false
    } catch { failed(error, token: token) }
  }

  /// Explicit conflict resolution by the owner, never an automatic refresh side effect.
  func useObservedDraft() {
    guard phase == .ready, !isBusy, !draftOutcomeUnknown, !startOutcomeUnknown, observedDraftIsCurrent, let draft = snapshot?["draft"] else { return }
    draftText = draft["text"]!.string!
    savedDraftRevision = draft["draft_revision"]!.decimal!
    localEdit = UUID(); isDirty = false; draftOutcomeUnknown = false; draftConflict = false; failure = nil
  }

  func poll() async {
    guard phase == .ready, !isBusy, pollingGeneration == nil, let transport else { return }
    let token = generation
    pollingGeneration = token
    defer { if pollingGeneration == token { pollingGeneration = nil } }
    do {
      let events = try await transport.takeEvents()
      try requireCurrent(token)
      try applyEvents(events)
      // Terminal transcript and draft/config updates are restored from durable state.
      if events.contains(where: { event in
        ["draft.updated", "configuration.updated", "role_bindings.updated", "task.updated", "child.updated"].contains(event["type"]?.string ?? "")
          || (event["type"] == .string("run.state")
              && ["succeeded", "failed", "cancelled", "interrupted", "outcome_unknown"].contains(event["payload"]?["state"]?.string ?? ""))
      }) { needsRefresh = true }
      if needsRefresh { await refresh() }
      // Source-apply result persistence has no dedicated event. A newer
      // ordinary event still provides a safe occasion to reconcile the saved
      // result, while the bounded post-ACK reconciliation covers no-event jobs.
      if hasPendingSourceApplyResult { scheduleSourceApplyResultReconciliation() }
    } catch { failed(error, token: token) }
  }

  /// Shared close owns cleanup even when its caller is cancelled. Ready alone
  /// does not prove helper exit; dirty/unknown drafts remain visible afterward.
  func close() async -> Bool {
    if let closeTask { return await closeTask.value }
    guard let connection = transport else { return phase == .closed && !isDirty && !draftOutcomeUnknown }
    let epoch = hello?.epoch
    let task = Task { @MainActor in
      // Publish this shared task before draining. Concurrent close callers
      // must join this owner instead of sending a second shutdown request.
      await self.stopSourceApplyResultReconciliation()
      self.generation = UUID(); self.monitor?.cancel(); self.monitor = nil
      self.phase = .closing; self.isBusy = true
      if let epoch {
        let deadline = ContinuousClock.now + .seconds(5)
        repeat {
          do {
            let id = UUID().uuidString
            let reply = try await connection.send(requestID: id, request: .shutdown(epoch: epoch))
            let payload = try self.checked(reply, id: id, method: "shutdown.request")
            guard payload["engine_epoch"]!.matchesID(epoch) else { throw Failure.stale }
            self.shutdownReady = payload["state"] == .string("ready")
          } catch { self.shutdownReady = false; break }
          if self.shutdownReady { break }
          try? await Task.sleep(for: .milliseconds(20))
        } while ContinuousClock.now < deadline
      }
      var exit = await connection.close()
      if self.recoveryMode == .resultOnly && self.shutdownReady && !self.recoveryBlocksReplacement {
        // Result-only close initiates EOF and intentionally returns before exit.
        // After a verified shutdown-ready response, allow the owned pump to
        // observe natural exit; do not send recovery operations or signals.
        let deadline = ContinuousClock.now + .seconds(5)
        while exit == nil && ContinuousClock.now < deadline {
          try? await Task.sleep(for: .milliseconds(20))
          exit = await connection.close()
        }
      }
      self.lastExit = exit
      if self.recoveryMode == .resultOnly { self.recoveryObservedExit = exit }
      if exit != nil && self.recoveryMode == .disabled { self.transport = nil }
      self.isBusy = false
      let clean = (self.recoveryMode == .disabled || !self.recoveryBlocksReplacement) && self.shutdownReady && exit?.status == 0 && exit?.forced == false
        && !self.isDirty && !self.draftOutcomeUnknown
      self.phase = clean ? .closed : .unavailable
      if self.isDirty || self.draftOutcomeUnknown { self.failure = .unsavedDraft }
      return clean
    }
    closeTask = task
    let result = await task.value
    closeTask = nil
    return result
  }

  private func send(_ request: ServiceRequest, connection: any WorkspaceServiceTransport,
                    token: UUID) async throws -> ServiceValue {
    let id = UUID().uuidString
    let reply = try await connection.send(requestID: id, request: request)
    try requireCurrent(token)
    return try checked(reply, id: id, method: request.method)
  }

  func readWorkspace(selectedPath: String?) async throws -> ServiceValue {
    try await metadataRequest(.workspaceRead(session: store.sessionID, selectedPath: selectedPath ?? ""))
  }
  func readAttachment(path: String) async throws -> ServiceValue {
    try await metadataRequest(.attachmentRead(session: store.sessionID, path: path))
  }
  func localInventory(provider: ExecutionBinding.Provider, endpoint: String) async throws -> LocalInventory {
    let value = try await metadataRequest(.localModels(session: store.sessionID, provider: provider, endpoint: endpoint))
    let observed = try LocalInventory(value)
    guard observed.provider == provider, observed.endpoint == endpoint else { throw ServiceError.correlation }
    return observed
  }

  var observedRoleBindings: [LocalRoleBinding] {
    ((try? array(snapshot?["role_bindings"])) ?? []).compactMap { try? LocalRoleBinding(service: $0) }
  }
  var observedRoleCatalog: [LocalRoleDescriptor] {
    ((try? array(snapshot?["role_catalog"])) ?? []).compactMap { try? LocalRoleDescriptor($0) }
  }
  func saveRoleBindings(_ bindings: [LocalRoleBinding], revision: UInt64) async {
    guard snapshot?["configuration"]?["configuration_revision"]?.decimal == revision else {
      roleConfigurationNotice = "役割設定が更新されています。最新の設定を確認してから保存してください。"
      return
    }
    guard phase == .ready, !isBusy, pendingRoleConfiguration == nil, pendingConfiguration == nil,
          !runs.contains(where: \.blocksStart), supports("role_configure"),
          let connection = transport, let configurationStore else { return }
    let token = generation
    do {
      let intent = try RoleConfigurationIntent(requestID: UUID().uuidString, clientID: clientID,
          projectID: store.projectID, sessionID: store.sessionID, expectedRevision: revision, bindings: bindings)
      try configurationStore.recordRoleConfigurationIntent(intent)
      pendingRoleConfiguration = intent; isBusy = true; roleConfigurationNotice = nil
      let response = try await connection.send(requestID: intent.requestID,
          request: .configureRoles(session: store.sessionID, revision: revision, bindings: intent.bindings))
      try requireCurrent(token)
      _ = try checked(response, id: intent.requestID, method: "session.roles.configure")
      isBusy = false
      await reconcileRoleConfiguration()
    } catch {
      guard generation == token else { return }
      isBusy = false
      roleConfigurationNotice = "役割設定の保存は未確認です。再送せず保存状態を照合してください。"
    }
  }
  func reconcileRoleConfiguration() async {
    guard phase == .ready, !isBusy, let pending = pendingRoleConfiguration, let configurationStore,
          let expected = UInt64(pending.expectedRevision) else { return }
    await refresh()
    guard phase == .ready, !isBusy, failure == nil,
          let revision = snapshot?["configuration"]?["configuration_revision"]?.decimal else { return }
    let observed = observedRoleBindings.sorted { Data($0.role.utf8).lexicographicallyPrecedes(Data($1.role.utf8)) }
    if (expected < UInt64.max && revision == expected + 1 && observed == pending.bindings) || revision == expected {
      do {
        try configurationStore.clearRoleConfigurationIntent(requestID: pending.requestID)
        pendingRoleConfiguration = nil
        roleConfigurationNotice = revision == expected ? "設定は未保存です。必要なら再度保存してください。" : "役割設定を保存しました。"
      } catch { roleConfigurationNotice = "保存確認の記録を更新できませんでした。" }
    } else { roleConfigurationNotice = "役割設定が他の更新と競合しています。保存済み設定を確認してください。" }
  }
  func adoptObservedRoleConfiguration() async {
    guard phase == .ready, !isBusy, let pending = pendingRoleConfiguration, let configurationStore,
          let expected = UInt64(pending.expectedRevision) else { return }
    await refresh()
    guard phase == .ready, !isBusy, failure == nil,
          let revision = snapshot?["configuration"]?["configuration_revision"]?.decimal,
          revision > expected else {
      roleConfigurationNotice = "現在の保存済み役割設定を確認できませんでした。未確認の保存記録を保持します。"
      return
    }
    do {
      try configurationStore.clearRoleConfigurationIntent(requestID: pending.requestID)
      pendingRoleConfiguration = nil
      executionNeedsRefresh = true
      roleConfigurationNotice = "現在の保存済み役割設定を採用しました。元の保存結果は確認できません。"
    } catch {
      roleConfigurationNotice = "保存確認の記録を更新できませんでした。"
    }
  }
  private func metadataRequest(_ request: ServiceRequest) async throws -> ServiceValue {
    guard phase == .ready, !isBusy, supports(request.capability), let transport else { throw ServiceError.notReady }
    let token = generation
    isBusy = true
    defer { if generation == token { isBusy = false } }
    return try await send(request, connection: transport, token: token)
  }
  private func send(_ request: ServiceReadRequest, connection: any WorkspaceServiceTransport,
                    token: UUID) async throws -> ServiceValue {
    let id = UUID().uuidString
    let reply = try await connection.send(requestID: id, request: request)
    try requireCurrent(token)
    return try checked(reply, id: id, method: request.method)
  }
  private func checked(_ reply: ServiceReply, id: String, method: String) throws -> ServiceValue {
    guard reply.requestID.utf8.elementsEqual(id.utf8) else { throw Failure.stale }
    if let rejection = reply.rejection {
      guard reply.payload == nil, reply.method == nil, let code = rejection["code"]?.string else {
        throw ServiceError.schema
      }
      throw Failure.rejected(code)
    }
    guard reply.method == method, let payload = reply.payload else { throw Failure.stale }
    try ServiceSchema.result(method).check(payload)
    return payload
  }
  private func requireCurrent(_ token: UUID) throws {
    guard generation == token, phase != .closing else { throw Failure.stale }
  }
  private func failed(_ error: Error, token: UUID) {
    guard generation == token else { return }
    isBusy = false
    failure = (error as? Failure) ?? .transport((error as? ServiceError) ?? (error is CancellationError ? .cancelled : .io))
    // A known CAS/cursor rejection permits explicit refresh; transport failure
    // retains ownership until close and requires a new connection.
    if case .rejected(let code) = failure, !Self.ambiguousPublication(code) { phase = .ready }
    else { phase = .unavailable; monitor?.cancel(); monitor = nil }
  }

  private static func ambiguousPublication(_ code: String) -> Bool {
    code == "storage_failed" || code == "recovery_required"
  }
}

extension WorkspaceServiceModel {
  /// One explicit submission; the saved revision identifies the immutable text.
  func startRun() async {
    guard canSend, let connection = transport,
          let configuration = snapshot?["configuration"]?["configuration_revision"]?.decimal,
          let policy = snapshot?["policy_revision"]?.decimal else { return }
    materializeAttachments()
    let token = generation, edit = localEdit
    let neededSave = isDirty, before = savedDraftRevision
    if neededSave { await saveDraft() }
    if neededSave {
      guard let before, before < UInt64.max, savedDraftRevision == before + 1, failure == nil else { return }
    }
    guard generation == token, phase == .ready, !isBusy, !draftOutcomeUnknown, !draftConflict,
          let draft = savedDraftRevision, !startOutcomeUnknown else { return }
    let id = UUID().uuidString
    pendingStart = PendingStart(requestID: id, draft: draft, edit: edit)
    lastStartRequestID = id; startStatus = nil; startOutcomeUnknown = true
    isBusy = true; failure = nil
    do {
      let reply = try await connection.send(requestID: id,
        request: .run(session: store.sessionID, draft: draft, configuration: configuration, policy: policy))
      try requireCurrent(token)
      let payload = try checked(reply, id: id, method: "run.start")
      guard let revision = payload["session_revision"]?.decimal, revision > max(context?.sessionRevision ?? 0, savedSessionRevision) else { throw Failure.stale }
      let accepted = try WorkspaceRun(.object([
        "run_id": payload["run_id"]!, "attempt_id": payload["attempt_id"]!,
        "state": payload["state"]!, "task_ids": .array([])]), generation: token)
      guard !runs.contains(where: { $0.key == accepted.key }) else { throw Failure.stale }
      savedSessionRevision = revision
      runs.append(accepted)
      pendingStart?.accepted = true; startOutcomeUnknown = false; isBusy = false
      await refresh()
    } catch {
      guard generation == token else { return }
      if case Failure.rejected(let code) = error, !Self.ambiguousPublication(code) {
        startOutcomeUnknown = false; pendingStart = nil
      }
      failed(error, token: token)
    }
  }

  /// Neither reconnect nor NotFound permits resending an ambiguous start.
  func reconcileStart() async {
    guard phase == .ready, !isBusy, startOutcomeUnknown, supports("request_status"), let pending = pendingStart else { return }
    let token = generation
    await refresh()
    guard generation == token, phase == .ready else { return }
    await lookupRequest(clientID: clientID, requestID: pending.requestID)
    guard generation == token, phase == .ready, let status = requestStatus, let context else { return }
    startStatus = status
    switch status {
    case .accepted(let revision, let target):
      guard revision <= context.sessionRevision, let target,
            runs.contains(where: { $0.id.utf8.elementsEqual(target.id.utf8) && $0.attemptID.utf8.elementsEqual(target.attemptID.utf8) }) else { return }
    case .completed(let revision, _):
      guard revision <= context.sessionRevision else { return }
    case .notFound, .outcomeUnknown: return
    }
    pendingStart?.accepted = true; startOutcomeUnknown = false
    // Reuse the same validated snapshot's consumed-draft rule, retaining late input.
    await refresh()
  }

  func cancelRun(_ run: WorkspaceRun) async {
    guard canCancel(run), let connection = transport else { return }
    let token = generation
    isBusy = true; failure = nil
    // Unknown ACK cannot silently become a fresh cancel; state comes from events/snapshot.
    cancelRequested.insert(run.key)
    do {
      _ = try await send(.cancel(session: store.sessionID, run: run.id, attempt: run.attemptID), connection: connection, token: token)
      isBusy = false
    } catch {
      if generation == token, case Failure.rejected(let code) = error, !Self.ambiguousPublication(code) {
        cancelRequested.remove(run.key)
      }
      failed(error, token: token)
    }
  }

  func resolveApproval(_ approval: WorkspaceApproval, decision: ServiceApprovalDecision) async {
    guard canResolve(approval), approval.choices.contains(decision), let connection = transport else { return }
    let token = generation
    isBusy = true; failure = nil; pendingApprovalAnswers.insert(approval.key)
    do {
      let payload = try await send(.approval(session: store.sessionID, id: approval.id, run: approval.runID,
        attempt: approval.attemptID, policy: approval.policy, decision: decision), connection: connection, token: token)
      guard payload["approval_id"]?.matchesID(approval.id) == true,
            payload["decision"] == .string(decision.rawValue) else { throw Failure.stale }
      approvals.removeAll { $0.key == approval.key }
      pendingApprovalAnswers.remove(approval.key); isBusy = false
    } catch {
      if generation == token, case Failure.rejected(let code) = error, !Self.ambiguousPublication(code) {
        pendingApprovalAnswers.remove(approval.key)
      }
      failed(error, token: token)
    }
  }

  private var hasPendingSourceApplyResult: Bool {
    sourceApplyCandidates.contains { $0.intentCommitted && !$0.resultSaved && $0.result == nil }
  }

  private func stopSourceApplyResultReconciliation() async {
    guard let task = sourceApplyResultPoll else { return }
    // Invalidating first prevents a completed snapshot read from starting a
    // second list exchange. Cancelling a sleep is safe; cancelling an active
    // ServiceClient exchange would close the client before graceful shutdown.
    sourceApplyResultPollID = nil
    if !sourceApplyResultReadInFlight { task.cancel() }
    await task.value
    sourceApplyResultPoll = nil
    sourceApplyResultPollID = nil
    sourceApplyResultReadInFlight = false
  }

  private func scheduleSourceApplyResultReconciliation() {
    // Empty subscription polls must not restart a completed bounded window.
    // Only an explicit foreground candidate refresh or a new resolve clears
    // this state and starts a fresh reconciliation window.
    guard sourceApplyResultPoll == nil, !sourceApplyResultTimedOut,
          hasPendingSourceApplyResult else { return }
    let id = UUID(), token = generation
    sourceApplyResultTimedOut = false
    sourceApplyResultPollID = id
    sourceApplyResultPoll = Task { @MainActor [weak self] in
      guard let self else { return }
      defer {
        if self.sourceApplyResultPollID == id {
          self.sourceApplyResultPoll = nil
          self.sourceApplyResultPollID = nil
        }
      }
      // Source jobs are asynchronous. This fixed ~30-second backoff bounds
      // traffic and recovery time, and failed reads remain a local notice
      // rather than changing the entire workspace into a generic failure.
      for delay in self.sourceApplyResultReconciliationDelays {
        do { try await Task.sleep(for: delay) } catch { return }
        guard self.generation == token, self.sourceApplyResultPollID == id,
              self.phase == .ready, self.hasPendingSourceApplyResult else { return }
        // A foreground action owns the transport. Skip this slot and retain
        // the fixed deadline instead of abandoning the pending result forever.
        guard !self.isBusy else { continue }
        self.sourceApplyResultReadInFlight = true
        defer { self.sourceApplyResultReadInFlight = false }
        guard await self.refreshSourceApplyResult(token: token) else { return }
        guard self.generation == token, self.sourceApplyResultPollID == id,
              self.phase == .ready, self.hasPendingSourceApplyResult else { return }
        await self.loadSourceApplyCandidates(reconcileStaleRead: true, background: true,
                                             schedulePendingResultReconciliation: false)
      }
      if self.generation == token, self.sourceApplyResultPollID == id,
         self.phase == .ready, self.hasPendingSourceApplyResult {
        // This is separate from sourceApplyNotice so a foreground failure is
        // never overwritten merely because the background deadline elapsed.
        self.sourceApplyResultTimedOut = true
      }
    }
  }

  private func refreshSourceApplyResult(token: UUID) async -> Bool {
    guard generation == token, phase == .ready, !isBusy, let transport else { return false }
    isBusy = true
    do {
      try await load(transport, token: token)
      try requireCurrent(token)
      isBusy = false
      return true
    } catch {
      guard generation == token else { return false }
      isBusy = false
      sourceApplyNotice = "反映結果を自動照合できませんでした。候補を更新して確認してください。"
      return false
    }
  }

  /// Fetch saved review metadata only. A later snapshot or mixed page revision
  /// invalidates the display rather than combining records from two ledgers.
  func loadSourceApplyCandidates(reconcileStaleRead: Bool = false, background: Bool = false,
                                 schedulePendingResultReconciliation: Bool = true) async {
    // Source-apply mutations advance durable state but do not have their own
    // subscription event. Every foreground review therefore starts from a
    // fresh snapshot; background reconciliation uses its separate no-failure
    // reader and remains bounded by its owning task.
    if background {
      if let revision = snapshot?["session_revision"]?.decimal, eventRevision > revision {
        guard await refreshSourceApplyResult(token: generation) else { return }
      }
    } else {
      await refresh()
      guard phase == .ready, !isBusy, failure == nil else { return }
    }
    guard phase == .ready, !isBusy, supports("source_apply_read"),
      let revision = snapshot?["session_revision"]?.decimal, let connection = transport else { return }
    let token = generation
    isBusy = true
    if !background { failure = nil; sourceApplyNotice = nil }
    defer { if generation == token { isBusy = false } }
    do {
      var offset: UInt64 = 0, seenOffsets: Set<UInt64> = [], seen: Set<Data> = [], values: [SourceApplyCandidate] = []
      var next: UInt64?
      repeat {
        guard seenOffsets.insert(offset).inserted, values.count < 256 else { throw Failure.capacity }
        let payload = try await send(.sourceApplyList(session: store.sessionID, revision: revision,
          offset: offset, limit: 32), connection: connection, token: token)
        try ServiceSchema.sourceApplyList.check(payload)
        guard payload["session_revision"]?.decimal == revision,
          case .array(let items) = payload["items"], items.count <= 32 else { throw Failure.stale }
        let decoded = try items.map(SourceApplyCandidate.init)
        guard decoded.allSatisfy({ seen.insert($0.key).inserted }) else { throw Failure.stale }
        values.append(contentsOf: decoded)
        next = payload["next_offset"]?.decimal
        if let next { guard next == offset + UInt64(decoded.count), !decoded.isEmpty else { throw Failure.stale }; offset = next }
      } while next != nil
      try requireCurrent(token)
      guard snapshot?["session_revision"]?.decimal == revision else { throw Failure.stale }
      sourceApplyCandidates = values; sourceApplyPages = [:]; sourceApplyRevision = revision
      sourceApplyNextOffset = nil
      if hasPendingSourceApplyResult {
        if !background { sourceApplyResultTimedOut = false }
        if schedulePendingResultReconciliation { scheduleSourceApplyResultReconciliation() }
      } else {
        sourceApplyResultTimedOut = false
      }
    } catch {
      // A decision ACK can precede its next snapshot/event publication. Retry
      // only this read path once after a fresh snapshot; never repeat resolve.
      if reconcileStaleRead, generation == token, case Failure.rejected("revision_conflict") = error {
        isBusy = false; failure = nil
        if background {
          guard await refreshSourceApplyResult(token: token) else { return }
        } else {
          await refresh()
        }
        guard phase == .ready, !isBusy, failure == nil else { return }
        await loadSourceApplyCandidates(background: background,
                                        schedulePendingResultReconciliation: schedulePendingResultReconciliation)
        return
      }
      if background {
        sourceApplyNotice = "反映結果を自動照合できませんでした。候補を更新して確認してください。"
        return
      }
      sourceApplyNotice = "変更候補を現在の保存状態と照合できませんでした。再送せず、状態を更新してください。"; failed(error, token: token)
    }
  }

  func loadSourceApplyPage(_ candidate: SourceApplyCandidate, offset: UInt64 = 0) async {
    guard phase == .ready, !isBusy, supports("source_apply_read"),
      sourceApplyRevision == snapshot?["session_revision"]?.decimal, sourceApplyRevision == eventRevision,
      let connection = transport else { return }
    let token = generation, revision = sourceApplyRevision!
    isBusy = true; failure = nil; sourceApplyNotice = nil
    defer { if generation == token { isBusy = false } }
    do {
      let payload = try await send(.sourceApplyPage(session: store.sessionID, approval: candidate.id,
        hash: candidate.payloadHash, revision: revision, offset: offset, limit: 32), connection: connection, token: token)
      let page = try SourceApplyPage(payload)
      guard page.approvalID.utf8.elementsEqual(candidate.id.utf8),
        page.payloadHash.utf8.elementsEqual(candidate.payloadHash.utf8), page.sessionRevision == revision,
        page.entries.count <= 32, (page.nextOffset == nil || (page.nextOffset! == offset + UInt64(page.entries.count) && !page.entries.isEmpty)),
        snapshot?["session_revision"]?.decimal == revision else { throw Failure.stale }
      let key = candidate.key
      if let prior = sourceApplyPages[key] {
        guard offset == UInt64(prior.entries.count), prior.nextOffset == offset else { throw Failure.stale }
        var all = prior.entries; let ids = Set(all.map(\.id))
        guard page.entries.allSatisfy({ !ids.contains($0.id) }) else { throw Failure.stale }
        all.append(contentsOf: page.entries)
        sourceApplyPages[key] = .init(approvalID: page.approvalID, payloadHash: page.payloadHash,
          sessionRevision: page.sessionRevision, entries: all, nextOffset: page.nextOffset)
      } else {
        guard offset == 0 else { throw Failure.stale }
        sourceApplyPages[key] = page
      }
    } catch { sourceApplyNotice = "変更内容を現在の候補と照合できませんでした。再送せず、状態を更新してください。"; failed(error, token: token) }
  }

  func resolveSourceApply(_ candidate: SourceApplyCandidate, decision: ServiceApprovalDecision) async {
    guard canResolveSourceApply(candidate, decision: decision), let revision = sourceApplyRevision,
      let connection = transport else { return }
    let token = generation
    isBusy = true; failure = nil; sourceApplyNotice = nil; sourceApplyResultTimedOut = false
    pendingSourceApplyAnswers.insert(candidate.key)
    do {
      let payload = try await send(.sourceApplyResolve(session: store.sessionID, run: candidate.runID,
        attempt: candidate.attemptID, approval: candidate.id, hash: candidate.payloadHash, revision: revision,
        policy: candidate.policyRevision, decision: decision), connection: connection, token: token)
      guard payload["approval_id"]?.matchesID(candidate.id) == true,
        payload["decision"] == .string(decision.rawValue) else { throw Failure.stale }
      pendingSourceApplyAnswers.remove(candidate.key); sourceApplyOutcomeUnknown.remove(candidate.key)
      isBusy = false
      // The response only records a decision. The saved result remains separate.
      await refresh()
      guard phase == .ready, !isBusy, failure == nil else { return }
      await loadSourceApplyCandidates(reconcileStaleRead: true)
    } catch {
      guard generation == token else { return }
      if case Failure.rejected(let code) = error, !Self.ambiguousPublication(code) {
        pendingSourceApplyAnswers.remove(candidate.key)
        sourceApplyNotice = "変更の回答は受け付けられませんでした。候補を更新して確認してください。"
      } else {
        sourceApplyOutcomeUnknown.insert(candidate.key)
        sourceApplyNotice = "変更の回答の成否が不明です。再送せず、候補を更新して確認してください。"
      }
      failed(error, token: token)
    }
  }

  /// Old cursors are invalidated, but their already displayed history remains
  /// explicitly labelled as a previous snapshot until the new pages confirm it.
  private func reconciledDisplay(history: [ServiceHistoryMessage], epoch: String) throws
    -> (history: [ServiceHistoryMessage], streams: [WorkspaceStreamMessage]) {
    guard displayEpoch?.utf8.elementsEqual(epoch.utf8) == true else { return ([], []) }
    let current = Dictionary(uniqueKeysWithValues: history.map { (Data($0.id.utf8), $0) })
    var retained: [ServiceHistoryMessage] = []
    var seen: [Data: ServiceHistoryMessage] = [:]
    for previous in messages + retainedHistory {
      let key = Data(previous.id.utf8)
      if let confirmed = current[key] ?? seen[key] {
        guard confirmed.text.utf8.elementsEqual(previous.text.utf8) else {
          streamHistoryConflict = true; throw Failure.stale
        }
      } else { retained.append(previous); seen[key] = previous }
    }
    var streams: [WorkspaceStreamMessage] = []
    for stream in streamMessages {
      if let confirmed = current[stream.key] {
        guard confirmed.text.utf8.elementsEqual(stream.text.utf8) else {
          streamHistoryConflict = true; throw Failure.stale
        }
        // Exact byte identity and text prove handoff, never ID equality alone.
      } else { streams.append(stream) }
    }
    guard history.count + retained.count <= 65_536,
          (history + retained).reduce(0, { $0 + $1.text.utf8.count }) <= 16_777_216 else { throw Failure.capacity }
    return (retained, streams)
  }

  private func array(_ value: ServiceValue?) throws -> [ServiceValue] {
    guard case .array(let values) = value else { throw ServiceError.schema }
    return values
  }

  private func applyEvents(_ events: [ServiceValue]) throws {
    guard let hello, context != nil else { throw ServiceError.notReady }
    for event in events {
      try ServiceSchema.validateEvent(event, epoch: hello.epoch)
      guard event["session_id"]?.matchesID(store.sessionID) == true,
            event["subscription_id"]?.matchesID(snapshot?["position"]?["subscription_id"]?.string ?? "") == true,
            eventSequence < UInt64.max, event["event_seq"]?.decimal == eventSequence + 1,
            let revision = event["session_revision"]?.decimal, revision >= eventRevision else { throw Failure.stale }
      let payload = event["payload"]!
      switch event["type"]!.string! {
      case "role_bindings.updated":
        if executionAvailable { executionNeedsRefresh = true }
      case "memory.updated":
        memoryStatus = try WorkspaceMemoryStatus(payload)
      case "run.state":
        let run = try WorkspaceRun(payload, generation: generation)
        if let index = runs.firstIndex(where: { $0.key == run.key }) {
          guard runs[index].attemptID.utf8.elementsEqual(run.attemptID.utf8), !runs[index].isTerminal || runs[index] == run else { throw Failure.stale }
          runs[index] = run
        } else {
          guard runs.count < 4096 else { throw Failure.capacity }
          runs.append(run)
        }
        if run.isTerminal { cancelRequested.remove(run.key) }
      case "message.delta":
        let id = payload["message_id"]!.string!
        if let index = streamMessages.firstIndex(where: { $0.id.utf8.elementsEqual(id.utf8) }) {
          try streamMessages[index].apply(payload)
        } else {
          guard streamMessages.count < 256 else { throw Failure.capacity }
          var message = WorkspaceStreamMessage(id: id)
          try message.apply(payload); streamMessages.append(message)
        }
        guard streamMessages.reduce(0, { $0 + $1.text.utf8.count }) <= 4_194_304 else { throw Failure.capacity }
      case "approval.requested":
        let approval = try WorkspaceApproval(payload, generation: generation)
        if let existing = approvals.first(where: { $0.key == approval.key }) {
          guard existing == approval else { throw Failure.stale }
        } else {
          guard approvals.count < 256 else { throw Failure.capacity }
          approvals.append(approval)
        }
      case "approval.resolved", "approval.expired":
        let id = payload["approval_id"]!.string!
        approvals.removeAll { $0.id.utf8.elementsEqual(id.utf8) }; pendingApprovalAnswers.remove(Data(id.utf8))
      default: break // task/child presentation belongs to its independent adapter.
      }
      eventSequence += 1; eventRevision = revision
    }
  }
}
