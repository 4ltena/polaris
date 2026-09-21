import XCTest
@testable import PolarisDesktop

final class InstallerPayloadTests: XCTestCase {
    private func makePayload(in root: URL, bytes: String = "payload") throws -> URL {
        let app = root.appendingPathComponent("Polaris.app", isDirectory: true)
        let contents = app.appendingPathComponent("Contents", isDirectory: true)
        let macOS = contents.appendingPathComponent("MacOS", isDirectory: true)
        try FileManager.default.createDirectory(at: macOS, withIntermediateDirectories: true)
        let executable = macOS.appendingPathComponent("PolarisDesktop")
        try Data(bytes.utf8).write(to: executable)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)
        let info: [String: Any] = ["CFBundlePackageType": "APPL",
                                   "CFBundleIdentifier": "com.polaris.desktop.local",
                                   "CFBundleExecutable": "PolarisDesktop"]
        let data = try PropertyListSerialization.data(fromPropertyList: info, format: .xml, options: 0)
        try data.write(to: contents.appendingPathComponent("Info.plist"))
        return app
    }

    func testInstallsValidatedPayloadIntoChosenFolder() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-payload-\(UUID())")
        let source = root.appendingPathComponent("source", isDirectory: true)
        let destination = root.appendingPathComponent("destination", isDirectory: true)
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: destination, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let payload = try makePayload(in: source)
        let manager = InstallerPayloadManager(payload: payload, availableCapacity: { _ in .max })

        let preflight = try await manager.preflight(destination: destination, additionalBytes: 0)
        XCTAssertFalse(preflight.replacesExistingApplication)
        let installed = try await manager.install(destination: destination, replaceExisting: false)
        XCTAssertEqual(installed, destination.appendingPathComponent("Polaris.app", isDirectory: true))
        XCTAssertEqual(try String(contentsOf: installed.appendingPathComponent("Contents/MacOS/PolarisDesktop")), "payload")
    }

    func testRequiresExplicitReplacementAndReplacesAtomically() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("polaris-replace-\(UUID())")
        let source = root.appendingPathComponent("source", isDirectory: true)
        let destination = root.appendingPathComponent("destination", isDirectory: true)
        try FileManager.default.createDirectory(at: source, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: destination, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let payload = try makePayload(in: source, bytes: "new")
        _ = try makePayload(in: destination, bytes: "old")
        let manager = InstallerPayloadManager(payload: payload, availableCapacity: { _ in .max })

        do {
            _ = try await manager.install(destination: destination, replaceExisting: false)
            XCTFail("replacement should require confirmation")
        } catch {
            XCTAssertEqual(error as? InstallerPayloadError, .existingApplication)
        }
        let installed = try await manager.install(destination: destination, replaceExisting: true)
        XCTAssertEqual(try String(contentsOf: installed.appendingPathComponent("Contents/MacOS/PolarisDesktop")), "new")
        XCTAssertFalse(try FileManager.default.contentsOfDirectory(atPath: destination.path).contains { $0.contains("backup") })
    }
}
