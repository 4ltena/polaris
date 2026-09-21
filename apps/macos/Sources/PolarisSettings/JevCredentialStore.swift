import Foundation
import Security

public enum JevCredentialError: Error, LocalizedError, Equatable {
    case invalidKey
    case keychain(OSStatus)

    public var errorDescription: String? {
        switch self {
        case .invalidKey:
            "Jev APIキーを入力してください。"
        case .keychain:
            "Jev APIキーをmacOSキーチェーンへ保存できませんでした。"
        }
    }
}

/// A narrow, injectable boundary for the Jev credential. The settings JSON
/// stores only whether Jev is selected; the secret remains in Keychain.
public struct JevCredentialAccess: Sendable {
    public let load: @Sendable () throws -> String?
    public let save: @Sendable (String) throws -> Void

    public init(load: @escaping @Sendable () throws -> String?,
                save: @escaping @Sendable (String) throws -> Void) {
        self.load = load
        self.save = save
    }

    public static let keychain = JevCredentialAccess(
        load: { try JevKeychain.load() },
        save: { try JevKeychain.save($0) }
    )
}

private enum JevKeychain {
    static let service = "com.polaris.desktop.local"
    static let account = "jev-api-key"

    static var identity: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    static func load() throws -> String? {
        var query = identity
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        query[kSecReturnData as String] = true
        var value: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &value)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = value as? Data,
              let key = String(data: data, encoding: .utf8), !key.isEmpty else {
            throw JevCredentialError.keychain(status)
        }
        return key
    }

    static func save(_ value: String) throws {
        let key = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty, key.utf8.count <= 8192, !key.contains("\0") else {
            throw JevCredentialError.invalidKey
        }
        let data = Data(key.utf8)
        let status = SecItemUpdate(identity as CFDictionary,
                                   [kSecValueData as String: data] as CFDictionary)
        if status == errSecSuccess { return }
        guard status == errSecItemNotFound else { throw JevCredentialError.keychain(status) }
        var item = identity
        item[kSecValueData as String] = data
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let added = SecItemAdd(item as CFDictionary, nil)
        guard added == errSecSuccess else { throw JevCredentialError.keychain(added) }
    }
}
