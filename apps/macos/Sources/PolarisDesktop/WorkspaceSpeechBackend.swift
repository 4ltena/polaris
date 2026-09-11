import AppKit
import AVFoundation
import Speech

/// 全Window共通のslot。初期化だけでは権限要求も録音もしない。
@MainActor
final class WorkspaceNativeSpeechBackend: WorkspaceSpeechBackend {
    private static var owner: UUID?
    private let identity = UUID()
    private var epoch: UUID?
    private var engine: AVAudioEngine?
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var recognition: SFSpeechRecognitionTask?
    private var recognizer: SFSpeechRecognizer?
    private var hasTap = false
    private var receive: (@MainActor (WorkspaceSpeechEvent) -> Void)?
    private let locale: Locale

    init(locale: Locale = .current) { self.locale = locale }
    func start(_ receive: @escaping @MainActor (WorkspaceSpeechEvent) -> Void) {
        guard Self.owner == nil else { receive(.failure("別のウィンドウで音声入力を使用中です。")); return }
        guard Bundle.main.object(forInfoDictionaryKey: "NSSpeechRecognitionUsageDescription") != nil,
              Bundle.main.object(forInfoDictionaryKey: "NSMicrophoneUsageDescription") != nil else {
            receive(.failure("音声入力のApp権限設定が未統合です。")); return
        }
        guard let recognizer = SFSpeechRecognizer(locale: locale), recognizer.supportsOnDeviceRecognition else {
            receive(.failure("この端末または言語は端末内音声認識に対応していません。")); return
        }
        Self.owner = identity
        let token = UUID(); epoch = token; self.receive = receive
        SFSpeechRecognizer.requestAuthorization { [weak self] status in
            Task { @MainActor in
                guard let self, self.epoch == token else { return }
                guard status == .authorized else { self.fail("音声認識が許可されていません。システム設定で確認してください。"); return }
                AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                    Task { @MainActor in
                        guard let self, self.epoch == token else { return }
                        guard granted else { self.fail("マイクが許可されていません。システム設定で確認してください。"); return }
                        self.begin(token: token)
                    }
                }
            }
        }
    }
    private func begin(token: UUID) {
        guard let recognizer = SFSpeechRecognizer(locale: locale),
              recognizer.supportsOnDeviceRecognition, recognizer.isAvailable else {
            fail("端末内音声認識を利用できません。クラウドには接続しません。"); return
        }
        self.recognizer = recognizer
        let audio = AVAudioEngine()
        let input = audio.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.channelCount > 0, format.sampleRate > 0 else { fail("マイク入力を利用できません。"); return }
        let bufferRequest = SFSpeechAudioBufferRecognitionRequest()
        bufferRequest.requiresOnDeviceRecognition = true
        bufferRequest.shouldReportPartialResults = false
        engine = audio; request = bufferRequest
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in
            bufferRequest.append(buffer)
        }
        hasTap = true
        recognition = recognizer.recognitionTask(with: bufferRequest) { [weak self] result, error in
            let finalText = result?.isFinal == true ? result?.bestTranscription.formattedString : nil
            let failed = error != nil
            Task { @MainActor in
                guard let self, self.epoch == token else { return }
                if let finalText {
                    let callback = self.receive; self.cancel(); callback?(.final(finalText))
                } else if failed { self.fail("音声認識に失敗しました。下書きは保持しています。") }
            }
        }
        do { audio.prepare(); try audio.start(); receive?(.recording) }
        catch { fail("マイクを開始できませんでした。下書きは保持しています。") }
    }
    func stop() {
        engine?.stop()
        if hasTap { engine?.inputNode.removeTap(onBus: 0); hasTap = false }
        request?.endAudio()
    }
    func cancel() {
        epoch = nil; receive = nil
        stop(); recognition?.cancel(); recognition = nil
        request = nil; engine = nil; recognizer = nil
        if Self.owner == identity { Self.owner = nil }
    }
    private func fail(_ reason: String) {
        let callback = receive; cancel(); callback?(.failure(reason))
    }
}
