import Foundation
import PolarisSettings

enum LocalInventoryAvailability: String, Sendable {
  case available
  case unreachable
  case invalidResponse = "invalid_response"
}

enum LocalObservedCapability: String, Sendable {
  case supported, unsupported, unknown
}

enum LocalModelLoadState: String, Sendable {
  case loaded, unloaded, unknown
}

struct LocalModelObservation: Identifiable, Equatable, Sendable {
  let id: String
  let digest: String?
  let variant: String?
  let maxContextLength: UInt64?
  let completion: LocalObservedCapability
  let tools: LocalObservedCapability
  let vision: LocalObservedCapability
  let reasoning: LocalObservedCapability
  let executionLocation: String
  let loadState: LocalModelLoadState
}

struct LocalInventory: Equatable, Sendable {
  let provider: ExecutionBinding.Provider
  let endpoint: String
  let availability: LocalInventoryAvailability
  let models: [LocalModelObservation]

  init(_ value: ServiceValue) throws {
    try ServiceSchema.localModels.check(value)
    guard let provider = ExecutionBinding.Provider(rawValue: value["provider"]!.string!),
          provider == .ollama || provider == .lmstudio,
          let availability = LocalInventoryAvailability(rawValue: value["availability"]!.string!),
          case .array(let models) = value["models"]
    else { throw ServiceError.schema }
    self.provider = provider
    endpoint = value["endpoint"]!.string!
    self.availability = availability
    self.models = models.map { model in
      LocalModelObservation(
        id: model["model_id"]!.string!, digest: model["digest"]?.string,
        variant: model["variant"]?.string, maxContextLength: model["max_context_length"]?.decimal,
        completion: LocalObservedCapability(rawValue: model["completion"]!.string!)!,
        tools: LocalObservedCapability(rawValue: model["tools"]!.string!)!,
        vision: LocalObservedCapability(rawValue: model["vision"]!.string!)!,
        reasoning: LocalObservedCapability(rawValue: model["reasoning"]!.string!)!,
        executionLocation: model["execution_location"]!.string!,
        // Pre-load-state services are compatible but cannot establish a
        // negative residency result.
        loadState: LocalModelLoadState(rawValue: model["load_state"]?.string ?? "unknown") ?? .unknown)
    }
  }
}

struct LocalRoleDescriptor: Identifiable, Equatable, Sendable {
  var id: String { role }
  let role: String
  let requiresTools: Bool

  init(_ value: ServiceValue) throws {
    try ServiceSchema.roleDescriptor.check(value)
    guard case .bool(let requiresTools) = value["requires_tools"] else { throw ServiceError.schema }
    role = value["role"]!.string!
    self.requiresTools = requiresTools
  }
}

extension LocalRoleBinding {
  init(service value: ServiceValue) throws {
    try ServiceSchema.roleBinding.check(value)
    guard let provider = ExecutionBinding.Provider(rawValue: value["provider"]!.string!),
          let support = ObservedToolSupport(rawValue: value["observed_tool_support"]!.string!)
    else { throw ServiceError.schema }
    let selection = try ExecutionBinding(
      provider: provider, model: value["model"]!.string!, effort: nil,
      localEndpoint: value["endpoint"]!.string!)
    try self.init(role: value["role"]!.string!, selection: selection,
                  observedToolSupport: support)
  }
}
