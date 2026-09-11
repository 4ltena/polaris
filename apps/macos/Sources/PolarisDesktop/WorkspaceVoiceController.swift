import Foundation
import Combine

@MainActor
protocol WorkspaceSpeechBackend: AnyObject {
    func start(_ receive: @escaping @MainActor (WorkspaceSpeechEvent) -> Void)
    func stop()
    func cancel()
}
enum WorkspaceSpeechEvent: Sendable {
    case recording, final(String), failure(String)
}

/// 音声は下書きへの入力だけ。送信callbackは所有しない。
@MainActor
final class WorkspaceVoiceController {
    private weak var state: WorkspaceState?
    private let backend: any WorkspaceSpeechBackend
    private var generation: UUID?
    private var conversationID: String?
    private var timer: Task<Void, Never>?
    private let recordingLimit: Duration
    private let finalLimit: Duration

    init(state: WorkspaceState, backend: any WorkspaceSpeechBackend,
         recordingLimit: Duration = .seconds(60), finalLimit: Duration = .seconds(10)) {
        self.state = state; self.backend = backend
        self.recordingLimit = recordingLimit; self.finalLimit = finalLimit
    }
    func handle(_ id: String, _ action: WorkspaceVoiceAction) {
        switch action {
        case .start:
            guard generation == nil, let state, state.selectedConversationID == id,
                  state.pending[id] == nil, state.voicePhases[id] == nil, state.voiceDeferred[id] == nil else { return }
            let token = UUID()
            generation = token; conversationID = id
            state.voicePhases[id] = .transcribing
            state.voiceNotes[id] = "音声入力の許可を確認しています"
            arm(.seconds(30), token: token)
            backend.start { [weak self] event in self?.receive(event, token: token) }
        case .stop:
            guard conversationID == id, let token = generation,
                  state?.voicePhases[id] == .recording else { return }
            state?.voicePhases[id] = .transcribing
            state?.voiceNotes[id] = "端末内で文字起こししています"
            arm(finalLimit, token: token)
            backend.stop()
        case .cancel:
            guard conversationID == id else { return }
            cancel()
        }
    }
    func cancel() {
        let id = conversationID
        generation = nil; conversationID = nil
        timer?.cancel(); timer = nil
        backend.cancel()
        if let id { state?.voicePhases[id] = nil; state?.voiceNotes[id] = nil }
    }
    private func arm(_ duration: Duration, token: UUID) {
        timer?.cancel()
        // 期限までは所有を保ち、View破棄で回収処理だけが消えないようにする。
        timer = Task { [self] in
            do { try await Task.sleep(for: duration) } catch { return }
            self.receive(.failure("音声入力が時間切れになりました。下書きは保持しています。"), token: token)
        }
    }
    private func receive(_ event: WorkspaceSpeechEvent, token: UUID) {
        guard generation == token, let id = conversationID else { return }
        guard let state else { cancel(); return }
        guard state.selectedConversationID == id, state.pending[id] == nil else { cancel(); return }
        switch event {
        case .recording:
            state.voicePhases[id] = .recording
            state.voiceNotes[id] = "端末内音声入力中 · 停止しても送信しません"
            arm(recordingLimit, token: token)
        case .failure(let reason):
            cancel(); state.voiceNotes[id] = reason
        case .final(let text):
            cancel()
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
            guard text.utf8.count <= 32_768 else {
                state.voiceNotes[id] = "認識文が長すぎるため追加しませんでした。"; return
            }
            if state.drafts[id]?.isComposing == true {
                state.voiceDeferred[id] = text
                state.voiceNotes[id] = "文字変換を確定してから「認識文を追加」を押してください。"
            } else { state.appendVoiceText(text, to: id) }
        }
    }
}
