import AppKit
import SwiftUI

struct DecisionInstallPanel: View {
    @StateObject private var model = DecisionInstallModel()
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Clef / Clef-flashの固定artifact導入。Jev・SemIfは維持し、この操作では判断器を有効にしません。")
                .font(.caption).foregroundStyle(.secondary)
            HStack {
                Button("登録profileを確認") { model.refresh() }.disabled(model.busy || model.offer != nil)
                if model.busy { ProgressView().controlSize(.small) }
            }
            Text(model.message).font(.caption).textSelection(.enabled)
            if let catalog = model.catalog {
                DisclosureGroup("容量の推薦（実行可否は未確認）") {
                    ForEach(Array(catalog.capacity.enumerated()), id: \.offset) { _, line in Text(line).font(.caption) }
                }
                if !catalog.registeredProfiles.isEmpty {
                    Picker("profile", selection: $model.profile) {
                        ForEach(catalog.registeredProfiles, id: \.self) { Text($0).tag($0) }
                    }.disabled(model.busy || model.offer != nil)
                    Button("導入済みgenerationを検証") { model.verify() }.disabled(!model.canVerify)
                    if let generation = model.verification?.generation {
                        DisclosureGroup("照合した固定内容（有効化・容量判定なし）") {
                            Text("\(generation.profile) / \(generation.generation)\n\(generation.modelRepository) @ \(generation.modelRevision)\n\(generation.quantization) / \(generation.runtimeRevision)")
                            Text("照合容量: \(bytes(generation.verifiedBytes))\nworker SHA-256: \(generation.workerSha256)\nmodel SHA-256: \(generation.modelSha256)").textSelection(.enabled)
                        }.font(.caption)
                    }
                    sourceRow("worker", url: model.worker) { model.worker = $0 }
                    sourceRow("model", url: model.model) { model.model = $0 }
                    Button("導入計画を確認") { model.prepare() }.disabled(!model.canPrepare)
                }
            }
            if let offer = model.offer {
                let plan = offer.plan
                GroupBox("今回の導入計画") {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("\(plan.profile) / \(plan.quantization) / \(plan.device)").font(.headline)
                        Text(plan.delivery.purpose)
                        Text("配置容量: \(bytes(plan.installedBytes))、copy予備: \(bytes(plan.copyReserveBytes))")
                        Text("導入先: \(plan.destination)").textSelection(.enabled)
                        Text("model取得元: \(plan.delivery.modelSource)").textSelection(.enabled)
                        Text("worker取得元: \(plan.delivery.workerSource)").textSelection(.enabled)
                        Text("license: \(plan.delivery.license)\n\(plan.delivery.licenseSource)\nNOTICE: \(plan.delivery.noticeSource)").textSelection(.enabled)
                        DisclosureGroup("固定内容・component一覧") {
                            Text("model: \(plan.modelRepository) @ \(plan.modelRevision)\nruntime: \(plan.runtimeRevision)\n\(plan.platform) / \(plan.architecture)\ncontext: \(plan.contextTokens)\npeak: \(bytes(plan.peakBytes)) / reserve: \(bytes(plan.reserveBytes))")
                            Text("worker: \(plan.worker.source)\nSHA-256: \(plan.worker.sha256)\nmodel: \(plan.model.source)\nSHA-256: \(plan.model.sha256)").textSelection(.enabled)
                            ForEach(Array(plan.delivery.components.enumerated()), id: \.offset) { _, c in Text("\(c.name) \(c.version) / \(c.license)\n\(c.source)") }
                        }
                        HStack {
                            Button("この計画で導入") { model.confirm(true) }
                            Button("取り消す", role: .cancel) { model.confirm(false) }
                        }
                    }.font(.caption).frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            if model.busy { Button("停止を要求", role: .cancel) { model.cancel() } }
        }.onDisappear { model.cancel() }
    }

    private func sourceRow(_ title: String, url: URL?, selected: @escaping (URL) -> Void) -> some View {
        HStack {
            Text(url?.path ?? "\(title): 未選択").font(.caption).lineLimit(2).textSelection(.enabled)
            Button("\(title)を選択…") {
                let panel = NSOpenPanel(); panel.canChooseFiles = true; panel.canChooseDirectories = false
                panel.allowsMultipleSelection = false
                if panel.runModal() == .OK, let url = panel.url { selected(url) }
            }.disabled(model.busy || model.offer != nil)
        }
    }
    private func bytes(_ value: UInt64) -> String { ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .binary) }
}
