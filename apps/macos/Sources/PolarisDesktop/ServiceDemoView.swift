import AppKit
import SwiftUI

/// Parent entry: ServiceDemoView(model: aParentOwnedServiceDemoModel).
/// Parent must defer window/application termination until await model.close()
/// returns true. The disappearance fallback does not replace that contract.
struct ServiceDemoView: View {
  @ObservedObject var model: ServiceDemoModel

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      VStack(alignment: .leading, spacing: 6) {
        Label("P5 ローカル通信の検証", systemImage: "testtube.2")
          .font(.title2.weight(.semibold))
        Text("検証用polaris-fake-serviceだけを起動する画面です。合成データを使い、実モデル・認証・製品の会話には接続しません。")
          .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
      }
      Divider()
      HStack(alignment: .top, spacing: 12) {
        VStack(alignment: .leading, spacing: 4) {
          Text("検証用helper").font(.headline)
          Text(model.helperURL?.path ?? "未選択")
            .font(.callout).textSelection(.enabled).lineLimit(2)
            .foregroundStyle(model.helperURL == nil ? .secondary : .primary)
        }
        Spacer()
        Button("ファイルを選択…", action: selectHelper).disabled(!model.canSelectHelper)
        Button("起動してHello確認") { Task { await model.start() } }
          .disabled(!model.canStart)
      }
      HStack(spacing: 8) {
        if model.phase == .starting || model.phase == .closing {
          ProgressView().controlSize(.small)
        }
        Label(
          model.phase.label,
          systemImage: model.phase == .unknown ? "questionmark.circle" : "circle.dotted"
        )
        .font(.headline)
        Spacer()
        if let exit = model.lastExit {
          Text("終了status \(exit.status)" + (exit.forced ? "・停止処理あり" : ""))
            .font(.caption).foregroundStyle(.secondary)
        }
      }
      Text(model.message).font(.callout).fixedSize(horizontal: false, vertical: true)
        .accessibilityIdentifier("service-demo-status")
      if let hello = model.hello {
        HStack {
          Text("Hello v1 確認済み").font(.callout)
          Text("epoch: \(hello.epoch)").font(.caption).foregroundStyle(.secondary).textSelection(
            .enabled)
        }
      }
      Divider()
      HStack {
        Button("snapshot取得") { Task { await model.fetchSnapshot() } }.disabled(!model.canSnapshot)
        if let snapshot = model.snapshot {
          Text("保存世代 \(snapshot["session_revision"]?.string ?? "不明")")
            .font(.caption).foregroundStyle(.secondary)
        } else {
          Text("開始前にsnapshotを取得").font(.caption).foregroundStyle(.secondary)
        }
        Spacer()
      }
      VStack(alignment: .leading, spacing: 6) {
        Text("合成runの入力").font(.headline)
        TextEditor(text: $model.draftText)
          .font(.body).frame(minHeight: 64, maxHeight: 100)
          .overlay(RoundedRectangle(cornerRadius: 4).stroke(.separator))
          .disabled(model.isBusy || model.phase != .connected)
          .accessibilityLabel("合成runの入力")
        HStack {
          Text("入力上限16 KiB・応答表示上限32 KiB。実データを入力しないでください。")
            .font(.caption).foregroundStyle(.secondary)
          Spacer()
          Button("合成runを開始") { Task { await model.startRun() } }.disabled(!model.canRun)
          Button("取消を要求") { Task { await model.cancelRun() } }.disabled(!model.canCancel)
        }
      }
      HStack {
        Text("run: \(model.runLabel)")
        if model.cancelAccepted { Text("取消ACK受信済み").foregroundStyle(.secondary) }
      }.font(.callout)
      ScrollView {
        Text(model.responseText.isEmpty ? "応答イベントはまだありません。" : model.responseText)
          .font(.body).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
          .padding(10)
      }.frame(minHeight: 68, maxHeight: 120).background(.quaternary.opacity(0.3))
      HStack {
        Text(model.shutdownReady ? "shutdown readyを受信済み" : "shutdown readyは未確認")
          .font(.caption).foregroundStyle(.secondary)
        Spacer()
        Button("helperを終了して回収") { Task { _ = await model.close() } }
          .disabled(!model.needsClose && model.phase != .unknown)
      }
    }
    .padding(24)
    .frame(minWidth: 760, minHeight: 740)
    .onDisappear {
      Task { _ = await model.close() }
    }
  }

  private func selectHelper() {
    let panel = NSOpenPanel()
    panel.title = "検証用polaris-fake-serviceを選択"
    panel.message = "選択した実行ファイルを、起動ボタンから引数なし・環境非継承で実行します。"
    panel.prompt = "選択"
    panel.canChooseFiles = true
    panel.canChooseDirectories = false
    panel.allowsMultipleSelection = false
    panel.resolvesAliases = true
    // No initial directory, defaults, environment variables or helper search.
    if panel.runModal() == .OK { model.selectHelper(panel.url) }
  }
}
