import SwiftUI
import PolarisSettings

struct LocalModelSettingsView: View {
  let inventories: [LocalInventory]
  let roles: [LocalRoleDescriptor]
  let bindings: [LocalRoleBinding]
  let isRefreshing: Bool
  let isSaving: Bool
  let refresh: (ExecutionBinding.Provider, String) -> Void
  let selectMain: (LocalInventory, LocalModelObservation) -> Void
  let assign: (String, LocalInventory, LocalModelObservation) -> Void
  let clear: (String) -> Void
  let save: () -> Void

  private let defaults: [(ExecutionBinding.Provider, String)] = [
    (.ollama, "http://127.0.0.1:11434/"),
    (.lmstudio, "http://127.0.0.1:1234/"),
  ]

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Text("ローカルモデル").font(.headline)
        Spacer()
        Button("再検出") {
          for (provider, endpoint) in defaults { refresh(provider, endpoint) }
        }.disabled(isRefreshing)
      }
      ForEach(Array(defaults.enumerated()), id: \.offset) { item in
        let (provider, endpoint) = item.element
        let inventory = inventories.first { $0.provider == provider && $0.endpoint == endpoint }
        VStack(alignment: .leading, spacing: 6) {
          Text(provider == .ollama ? "Ollama" : "LM Studio").font(.subheadline.bold())
          if let inventory {
            switch inventory.availability {
            case .available:
              Text(inventory.models.isEmpty ? "接続済み・モデルなし" : "接続済み")
                .foregroundStyle(.secondary)
              ForEach(inventory.models.filter {
                $0.executionLocation != "remote" && $0.completion != .unsupported
              }) { model in
                VStack(alignment: .leading, spacing: 3) {
                  Text("\(model.id) · \(loadStateTitle(model.loadState))")
                    .font(.caption).foregroundStyle(loadStateColor(model.loadState))
                  Button("主モデルに \(model.id) を使用") { selectMain(inventory, model) }
                }
              }
            case .unreachable:
              Text("到達できません。導入状態は判定していません。").foregroundStyle(.secondary)
            case .invalidResponse:
              Text("応答を確認できません").foregroundStyle(.secondary)
            }
          } else {
            Text("未確認").foregroundStyle(.secondary)
          }
        }
      }
      Divider()
      ForEach(roles) { role in
        VStack(alignment: .leading, spacing: 5) {
          HStack {
            Text(role.role)
            if role.requiresTools { Text("ツール必須").font(.caption).foregroundStyle(.secondary) }
            Spacer()
            if bindings.contains(where: { $0.role == role.role }) {
              Button("解除") { clear(role.role) }
            }
          }
          if let binding = bindings.first(where: { $0.role == role.role }) {
            Text("選択中：\(binding.selection.model) · \(binding.selection.provider.rawValue)").font(.caption)
          } else {
            Text("未割当").font(.caption).foregroundStyle(.secondary)
          }
          ForEach(inventories.filter { $0.availability == .available }, id: \.endpoint) { inventory in
            ForEach(inventory.models) { model in
              let incompatible = role.requiresTools && model.tools != .supported
              Button("\(model.id) · \(inventory.provider.rawValue) · \(loadStateTitle(model.loadState))") {
                assign(role.role, inventory, model)
              }
              .disabled(incompatible || model.executionLocation == "remote")
            }
          }
        }
      }
      Button("役割設定を保存", action: save)
        .disabled(isSaving)
    }
  }

  private func loadStateTitle(_ state: LocalModelLoadState) -> String {
    switch state {
    case .loaded: "メモリにロード済み"
    case .unloaded: "未ロード"
    case .unknown: "ロード状態は未確認"
    }
  }

  private func loadStateColor(_ state: LocalModelLoadState) -> Color {
    switch state {
    case .loaded: .green
    case .unloaded, .unknown: .secondary
    }
  }
}
