import SwiftUI

/// Review-only projection of the engine's saved source-apply ledger.
struct SourceApplyReviewView: View {
  @ObservedObject var service: WorkspaceServiceModel

  var body: some View {
    if service.sourceApplyReadAvailable || !service.sourceApplyCandidates.isEmpty {
      VStack(alignment: .leading, spacing: 8) {
        HStack {
          Text("保留中の変更").font(.headline)
          Spacer()
          Button("変更候補を更新") { Task { await service.loadSourceApplyCandidates() } }
            .disabled(service.phase != .ready || service.isBusy || !service.sourceApplyReadAvailable)
        }
        if let notice = service.sourceApplyNotice {
          Text(notice).font(.caption).foregroundStyle(.red)
        }
        if service.sourceApplyCandidates.isEmpty {
          Text("保存済みの変更候補はありません。").font(.caption).foregroundStyle(.secondary)
        }
        TimelineView(.periodic(from: .now, by: 1)) { timeline in
          ForEach(service.sourceApplyCandidates) { candidate in
            candidateView(candidate, now: timeline.date)
          }
        }
      }
      .onAppear {
        guard service.sourceApplyCandidates.isEmpty, service.sourceApplyReadAvailable else { return }
        Task { await service.loadSourceApplyCandidates() }
      }
    }
  }

  @ViewBuilder private func candidateView(_ candidate: SourceApplyCandidate, now: Date) -> some View {
    VStack(alignment: .leading, spacing: 5) {
      Text(candidate.sourcePath).font(.subheadline).textSelection(.enabled)
      Text("候補 " + String(candidate.entryCount) + " 件・ハッシュ " + candidate.payloadHash)
        .font(.caption2).textSelection(.enabled)
      Text("run " + candidate.runID + " / operation " + candidate.operationID)
        .font(.caption2).foregroundStyle(.secondary).textSelection(.enabled)
      if candidate.shouldShowExpiry(at: now) {
        Text("この候補の期限が切れました。状態を更新して確認してください。").font(.caption).foregroundStyle(.red)
      } else if candidate.invalidated {
        Text("この候補は無効化されています。新たな反映はできません。").font(.caption).foregroundStyle(.red)
      } else if candidate.intentCommitted {
        Text("反映操作は開始済みです。結果を確認してください。").font(.caption).foregroundStyle(.secondary)
        if service.sourceApplyResultTimedOut, !candidate.resultSaved, candidate.result == nil {
          Text("自動照合の期限を過ぎました。候補を更新して結果を確認してください。")
            .font(.caption).foregroundStyle(.red)
        }
      } else if let decision = candidate.decision {
        Text(decision == .allow ? "許可の記録があります。実際の反映結果を確認してください。" : "拒否の記録があります。")
          .font(.caption).foregroundStyle(.secondary)
      }
      resultText(candidate)
      if let page = service.sourceApplyPages[candidate.key] {
        entries(page)
        if let next = page.nextOffset {
          Button("変更内容の続きを表示") { Task { await service.loadSourceApplyPage(candidate, offset: next) } }
            .disabled(service.isBusy)
        }
      } else {
        Button("変更内容を確認") { Task { await service.loadSourceApplyPage(candidate) } }
          .disabled(service.isBusy || !service.sourceApplyReadAvailable)
      }
      HStack {
        Button("許可") { Task { await service.resolveSourceApply(candidate, decision: .allow) } }
          .disabled(candidate.isExpired(at: now) || !service.canResolveSourceApply(candidate, decision: .allow))
        Button("拒否") { Task { await service.resolveSourceApply(candidate, decision: .deny) } }
          .disabled(candidate.isExpired(at: now) || !service.canResolveSourceApply(candidate, decision: .deny))
      }
      if service.pendingSourceApplyAnswers.contains(candidate.key) {
        Text("回答の確認待ちです。再送は行いません。").font(.caption)
      }
      if service.sourceApplyOutcomeUnknown.contains(candidate.key) {
        Text("回答の成否が不明です。元の候補を更新して確認してください。").font(.caption).foregroundStyle(.red)
      }
    }
    .padding(.vertical, 6)
  }

  @ViewBuilder private func resultText(_ candidate: SourceApplyCandidate) -> some View {
    if let result = candidate.result {
      let counts = "書込み " + String(result.installedCount) + " / 削除 " + String(result.deletedCount) + " / 復旧 " + String(result.restoredCount)
      Text("反映結果：" + resultStatus(result.status) + "（" + counts + "）")
        .font(.caption).foregroundStyle(result.status == "applied" ? Color.secondary : Color.red)
      if let kind = result.failureKind { Text("詳細：" + failureKind(kind)).font(.caption2).foregroundStyle(.secondary) }
    } else if candidate.resultSaved {
      Text("結果記録はありますが、反映成功を示す詳細は取得できません。").font(.caption).foregroundStyle(.secondary)
    } else {
      Text("反映結果は未記録です。実行の成功とは別に確認します。").font(.caption).foregroundStyle(.secondary)
    }
  }

  private func entries(_ page: SourceApplyPage) -> some View {
    VStack(alignment: .leading, spacing: 3) {
      ForEach(page.entries) { entry in
        let before = entry.before?.hash ?? "なし", after = entry.after?.hash ?? "なし"
        Text("\(entry.relativePath)  \(before) → \(after)").font(.caption2).textSelection(.enabled)
      }
    }
  }

  private func resultStatus(_ value: String) -> String {
    switch value { case "applied": "反映済み"; case "failed": "失敗"; case "partial": "一部反映"; default: "結果不明" }
  }
  private func failureKind(_ value: String) -> String {
    switch value {
    case "protection_unavailable": "保護情報を確認できません"
    case "invalid_change_set": "変更集合が不正です"
    case "unsupported_entry": "対応していない変更です"
    case "conflict": "外部変更と競合しました"
    case "secret": "秘密情報を含むため拒否されました"
    case "ancestor_changed": "親フォルダが変更されています"
    case "cross_device": "別のファイルシステムへは反映できません"
    case "restore_conflict": "復旧先と競合しました"
    default: "入出力エラー"
    }
  }
}
