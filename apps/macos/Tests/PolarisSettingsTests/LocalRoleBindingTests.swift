import XCTest
@testable import PolarisSettings

final class LocalRoleBindingTests: XCTestCase {
  func testRoleIntentCanonicalizesBindingsAndRejectsCloudRoles() throws {
    let endpoint = "http://127.0.0.1:11434/"
    let local = try ExecutionBinding(provider: .ollama, model: "qwen:small", effort: nil,
                                     localEndpoint: endpoint)
    let b = try LocalRoleBinding(role: "reviewer", selection: local,
                                 observedToolSupport: .supported)
    let a = try LocalRoleBinding(role: "implementer", selection: local,
                                 observedToolSupport: .unknown)
    let intent = try RoleConfigurationIntent(
      requestID: "request", clientID: "client", projectID: "project", sessionID: "session",
      expectedRevision: 7, bindings: [b, a])
    XCTAssertEqual(intent.bindings.map(\.role), ["implementer", "reviewer"])
    XCTAssertThrowsError(try RoleConfigurationIntent(
      requestID: "request", clientID: "client", projectID: "project", sessionID: "session",
      expectedRevision: 7, bindings: [a, a]))
    let cloud = try ExecutionBinding(provider: .codex, model: "gpt-6-astra", effort: "medium")
    XCTAssertThrowsError(try LocalRoleBinding(role: "reviewer", selection: cloud,
                                              observedToolSupport: .supported))
  }
}
