import XCTest
import PolarisSettings
@testable import PolarisDesktop

final class LocalModelSettingsTests: XCTestCase {
    private func inventory(_ model: ServiceValue) throws -> LocalInventory {
        try LocalInventory(.object([
            "provider": .string("ollama"),
            "endpoint": .string("http://127.0.0.1:11434/"),
            "availability": .string("available"),
            "models": .array([model]),
        ]))
    }

    private func model(_ state: String? = nil) -> ServiceValue {
        var fields: [String: ServiceValue] = [
            "model_id": .string("qwen"),
            "completion": .string("supported"), "tools": .string("unknown"),
            "vision": .string("unknown"), "reasoning": .string("unknown"),
            "execution_location": .string("unknown"),
        ]
        if let state { fields["load_state"] = .string(state) }
        return .object(fields)
    }

    func testMissingLoadStateIsExplicitlyUnknownForOlderService() throws {
        XCTAssertEqual(try inventory(model()).models.single?.loadState, .unknown)
    }

    func testLoadedAndUnloadedStatesDecodeForPresentation() throws {
        XCTAssertEqual(try inventory(model("loaded")).models.single?.loadState, .loaded)
        XCTAssertEqual(try inventory(model("unloaded")).models.single?.loadState, .unloaded)
    }
}

private extension Array {
    var single: Element? { count == 1 ? self[0] : nil }
}
