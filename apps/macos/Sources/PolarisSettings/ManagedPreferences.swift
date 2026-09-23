import Foundation

/// User-only preferences read by the service before project configuration.
/// This deliberately contains no project path, credentials, or benchmark lock.
public struct ManagedPreferences: Equatable, Sendable {
    public var language: PresentationLanguagePreference = .unspecified
    public var finalizer = AnswerFinalizerPreference()

    public init() {}

    public init(language: PresentationLanguagePreference, finalizer: AnswerFinalizerPreference) throws {
        self.language = language
        self.finalizer = finalizer
        try validate()
    }

    public func validate() throws {
        // The desktop UI is never an authority for enforce mode or a benchmark lock.
        guard finalizer.mode == .off || finalizer.mode == .shadow else {
            throw SettingsError.invalidDocument
        }
        switch finalizer.mode {
        case .off:
            guard finalizer.judgeProfile == nil,
                  !finalizer.externalDisclosure,
                  finalizer.translation.activation == .disabled,
                  finalizer.translation.profile == nil else { throw SettingsError.invalidDocument }
        case .shadow:
            guard finalizer.judgeProfile == .fulfillmentV1 else { throw SettingsError.invalidDocument }
            if finalizer.translation.activation == .requested {
                guard language == .ja, finalizer.translation.profile == .catTranslate3_3b else {
                    throw SettingsError.invalidDocument
                }
            } else {
                guard finalizer.translation.profile == nil else { throw SettingsError.invalidDocument }
            }
        case .enforce:
            throw SettingsError.invalidDocument
        }
    }

    public func applying(to preferences: inout Preferences) {
        preferences.presentationLanguage = language
        preferences.answerFinalizer = finalizer
    }
}

/// Private, atomic TOML persistence for service-owned presentation settings.
/// An explicit `~/.polaris/config.toml` is read by the service later and has
/// higher priority; this file does not attempt to merge or replace it.
public final class ManagedPreferencesStore: @unchecked Sendable {
    public static let fileName = "managed-preferences.toml"
    public let url: URL
    private let directory: MetadataDirectory

    public static var defaultURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".polaris", isDirectory: true)
            .appendingPathComponent(fileName)
    }

    public init(url: URL = ManagedPreferencesStore.defaultURL) throws {
        guard url.isFileURL, !url.lastPathComponent.isEmpty else { throw SettingsError.unavailableLocation }
        directory = try MetadataDirectory(url: url.deletingLastPathComponent(), create: true, privateMode: true)
        self.url = directory.url.appendingPathComponent(url.lastPathComponent)
    }

    public func load() throws -> ManagedPreferences {
        guard let data = try directory.read(url.lastPathComponent) else { return ManagedPreferences() }
        guard let text = String(data: data, encoding: .utf8) else { throw SettingsError.invalidDocument }
        return try Self.decode(text)
    }

    public func save(_ preferences: ManagedPreferences) throws {
        try preferences.validate()
        try directory.write(Data(Self.encode(preferences).utf8), name: url.lastPathComponent)
    }

    public static func encode(_ preferences: ManagedPreferences) -> String {
        var lines: [String] = []
        if preferences.language != .unspecified {
            lines += ["[presentation]", "language = \"\(preferences.language.rawValue)\"", ""]
        }
        lines += ["[answer_finalizer]", "mode = \"\(preferences.finalizer.mode.rawValue)\""]
        if preferences.finalizer.mode == .shadow {
            lines.append("judge_profile = \"fulfillment-v1\"")
            if preferences.finalizer.externalDisclosure {
                lines.append("external_disclosure = true")
            }
        }
        if preferences.finalizer.translation.activation == .requested {
            lines += ["", "[answer_finalizer.translation]", "enabled = true", "profile = \"cat-translate-3.3b\""]
        }
        return lines.joined(separator: "\n") + "\n"
    }

    private static func decode(_ text: String) throws -> ManagedPreferences {
        var section = ""
        var values = [String: String]()
        for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty { continue }
            if line == "[presentation]" || line == "[answer_finalizer]" || line == "[answer_finalizer.translation]" {
                section = String(line); continue
            }
            guard !section.isEmpty, let equals = line.firstIndex(of: "=") else { throw SettingsError.invalidDocument }
            let key = line[..<equals].trimmingCharacters(in: .whitespaces)
            let value = line[line.index(after: equals)...].trimmingCharacters(in: .whitespaces)
            let identifier = section + "." + key
            guard values[identifier] == nil else { throw SettingsError.invalidDocument }
            values[identifier] = String(value)
        }
        let allowed: Set<String> = [
            "[presentation].language", "[answer_finalizer].mode", "[answer_finalizer].judge_profile",
            "[answer_finalizer].external_disclosure",
            "[answer_finalizer.translation].enabled", "[answer_finalizer.translation].profile",
        ]
        guard Set(values.keys).isSubset(of: allowed),
              let rawMode = values["[answer_finalizer].mode"],
              let mode = AnswerFinalizerModePreference(rawValue: rawMode.trimmingCharacters(in: CharacterSet(charactersIn: "\"")))
        else { throw SettingsError.invalidDocument }
        let language: PresentationLanguagePreference
        if let raw = values["[presentation].language"] {
            guard let parsed = PresentationLanguagePreference(rawValue: raw.trimmingCharacters(in: CharacterSet(charactersIn: "\""))), parsed != .unspecified else {
                throw SettingsError.invalidDocument
            }
            language = parsed
        } else { language = .unspecified }
        var finalizer = AnswerFinalizerPreference()
        finalizer.mode = mode
        if let raw = values["[answer_finalizer].judge_profile"] {
            finalizer.judgeProfile = AnswerFinalizerJudgeProfilePreference(rawValue: raw.trimmingCharacters(in: CharacterSet(charactersIn: "\"")))
        }
        if let raw = values["[answer_finalizer].external_disclosure"] {
            guard raw == "true" else { throw SettingsError.invalidDocument }
            finalizer.externalDisclosure = true
        }
        let enabled = values["[answer_finalizer.translation].enabled"]
        let profile = values["[answer_finalizer.translation].profile"]
        switch (enabled, profile) {
        case (nil, nil): break
        case ("true", let raw?):
            finalizer.translation.activation = .requested
            finalizer.translation.profile = TranslationProfilePreference(rawValue: raw.trimmingCharacters(in: CharacterSet(charactersIn: "\"")))
        default: throw SettingsError.invalidDocument
        }
        return try ManagedPreferences(language: language, finalizer: finalizer)
    }
}
