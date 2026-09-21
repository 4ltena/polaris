import Foundation

enum InstallerPayloadError: Error, LocalizedError, Equatable {
    case missingPayload
    case invalidPayload
    case invalidDestination
    case insufficientSpace
    case existingApplication
    case installFailed

    var errorDescription: String? {
        switch self {
        case .missingPayload: "Polaris本体がインストーラに含まれていません。"
        case .invalidPayload: "インストーラに含まれるPolaris本体を検証できませんでした。"
        case .invalidDestination: "選択したフォルダへPolarisをインストールできません。"
        case .insufficientSpace: "インストール先の空き容量が不足しています。"
        case .existingApplication: "インストール先にPolaris.appがあります。置き換えを確認してください。"
        case .installFailed: "Polaris本体をインストールできませんでした。"
        }
    }
}

struct InstallerPreflight: Equatable, Sendable {
    let target: URL
    let payloadBytes: Int64
    let availableBytes: Int64?
    let replacesExistingApplication: Bool
}

/// The installer owns only the application bundle copy. Managed runtimes and
/// settings keep their existing owners and are coordinated by InstallerView.
actor InstallerPayloadManager {
    private let fileManager: FileManager
    private let payload: URL
    private let availableCapacity: @Sendable (URL) -> Int64?

    init(payload: URL? = nil, fileManager: FileManager = .default,
         availableCapacity: @escaping @Sendable (URL) -> Int64? = { destination in
             let values = try? destination.resourceValues(
                 forKeys: [.volumeAvailableCapacityForImportantUsageKey])
             return values?.volumeAvailableCapacityForImportantUsage
         }) {
        self.fileManager = fileManager
        self.payload = payload
            ?? Bundle.main.resourceURL?.appendingPathComponent("Polaris.app", isDirectory: true)
            ?? URL(fileURLWithPath: "/__missing_polaris_payload__")
        self.availableCapacity = availableCapacity
    }

    func preflight(destination: URL, additionalBytes: Int64 = 4_100_000_000) throws -> InstallerPreflight {
        let payload = payload.standardizedFileURL
        let destination = destination.standardizedFileURL
        guard payload.isFileURL, destination.isFileURL,
              payload.lastPathComponent == "Polaris.app",
              payload.pathExtension == "app",
              destination.path != payload.path,
              validApplication(at: payload) else { throw InstallerPayloadError.invalidPayload }

        var isDirectory: ObjCBool = false
        guard fileManager.fileExists(atPath: destination.path, isDirectory: &isDirectory),
              isDirectory.boolValue,
              !destination.pathComponents.starts(with: payload.pathComponents),
              fileManager.isWritableFile(atPath: destination.path) else {
            throw InstallerPayloadError.invalidDestination
        }

        let payloadBytes = try allocatedSize(of: payload)
        let available = availableCapacity(destination)
        if let available, available < payloadBytes + additionalBytes {
            throw InstallerPayloadError.insufficientSpace
        }
        let target = destination.appendingPathComponent("Polaris.app", isDirectory: true)
        return InstallerPreflight(target: target, payloadBytes: payloadBytes,
                                  availableBytes: available,
                                  replacesExistingApplication: fileManager.fileExists(atPath: target.path))
    }

    func install(destination: URL, replaceExisting: Bool) throws -> URL {
        let preflight = try preflight(destination: destination, additionalBytes: 0)
        if preflight.replacesExistingApplication && !replaceExisting {
            throw InstallerPayloadError.existingApplication
        }

        let staging = destination.appendingPathComponent(".Polaris.installing.\(UUID().uuidString).app")
        let backup = destination.appendingPathComponent(".Polaris.backup.\(UUID().uuidString).app")
        var movedExisting = false
        do {
            try fileManager.copyItem(at: payload, to: staging)
            guard validApplication(at: staging) else { throw InstallerPayloadError.invalidPayload }
            if preflight.replacesExistingApplication {
                try fileManager.moveItem(at: preflight.target, to: backup)
                movedExisting = true
            }
            try fileManager.moveItem(at: staging, to: preflight.target)
            if movedExisting { try fileManager.removeItem(at: backup) }
            return preflight.target
        } catch {
            try? fileManager.removeItem(at: staging)
            if movedExisting, !fileManager.fileExists(atPath: preflight.target.path) {
                try? fileManager.moveItem(at: backup, to: preflight.target)
            }
            if let known = error as? InstallerPayloadError { throw known }
            throw InstallerPayloadError.installFailed
        }
    }

    private func validApplication(at url: URL) -> Bool {
        guard !url.hasDirectoryPath || fileManager.fileExists(atPath: url.path) else { return false }
        let info = url.appendingPathComponent("Contents/Info.plist")
        guard let data = fileManager.contents(atPath: info.path),
              let plist = try? PropertyListSerialization.propertyList(from: data, format: nil),
              let dictionary = plist as? [String: Any],
              dictionary["CFBundlePackageType"] as? String == "APPL",
              dictionary["CFBundleIdentifier"] as? String == "com.polaris.desktop.local",
              let executable = dictionary["CFBundleExecutable"] as? String,
              !executable.isEmpty else { return false }
        return fileManager.isExecutableFile(
            atPath: url.appendingPathComponent("Contents/MacOS/\(executable)").path)
    }

    private func allocatedSize(of root: URL) throws -> Int64 {
        guard let enumerator = fileManager.enumerator(
            at: root, includingPropertiesForKeys: [.isRegularFileKey, .fileAllocatedSizeKey],
            options: [.skipsHiddenFiles]) else { throw InstallerPayloadError.invalidPayload }
        var result: Int64 = 0
        for case let url as URL in enumerator {
            let values = try url.resourceValues(forKeys: [.isRegularFileKey, .fileAllocatedSizeKey])
            if values.isRegularFile == true { result += Int64(values.fileAllocatedSize ?? 0) }
        }
        return result
    }
}
