import Darwin
import Foundation

/// Directory-relative metadata I/O. Existing symlinks are never followed and
/// existing permissions are never repaired implicitly.
public final class MetadataDirectory: @unchecked Sendable {
    public let url: URL
    let descriptor: Int32
    private let identity: stat
    private let privateMode: Bool

    public init(url: URL, create: Bool, privateMode: Bool = true) throws {
        guard url.isFileURL, url.path.hasPrefix("/"), !url.path.contains("\0") else {
            throw SettingsError.unavailableLocation
        }
        // macOS public temporary paths are OS aliases; resolve only these fixed
        // prefixes. Arbitrary user-controlled symlink ancestors remain rejected.
        var path = url.path
        for alias in ["/tmp", "/var"] where path == alias || path.hasPrefix(alias + "/") {
            guard let resolved = realpath(alias, nil) else { throw SettingsError.unavailableLocation }
            let canonical = String(cString: resolved); free(resolved)
            path = canonical + path.dropFirst(alias.count)
            break
        }
        let components = path.split(separator: "/").map(String.init)
        guard !components.isEmpty, !components.contains(".."), !components.contains(".") else {
            throw SettingsError.unavailableLocation
        }
        var fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard fd >= 0 else { throw SettingsError.unavailableLocation }
        do {
            for component in components {
                var child = openat(fd, component, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                if child < 0, errno == ENOENT, create {
                    guard mkdirat(fd, component, mode_t(0o700)) == 0 || errno == EEXIST else {
                        throw SettingsError.unavailableLocation
                    }
                    guard fsync(fd) == 0 else { throw SettingsError.unavailableLocation }
                    child = openat(fd, component, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
                }
                guard child >= 0 else { throw SettingsError.unavailableLocation }
                Darwin.close(fd); fd = child
            }
            var info = stat()
            guard fstat(fd, &info) == 0, info.st_uid == geteuid(),
                  info.st_mode & (privateMode ? 0o077 : 0o022) == 0 else {
                throw SettingsError.unavailableLocation
            }
            descriptor = fd; identity = info; self.privateMode = privateMode
            self.url = URL(fileURLWithPath: "/" + components.joined(separator: "/"), isDirectory: true)
        } catch { Darwin.close(fd); throw error }
    }
    deinit { Darwin.close(descriptor) }
    public func verify() throws {
        let reopened = try MetadataDirectory(url: url, create: false, privateMode: privateMode)
        guard identity.st_dev == reopened.identity.st_dev, identity.st_ino == reopened.identity.st_ino else {
            throw SettingsError.unavailableLocation
        }
    }
    func read(_ name: String) throws -> Data? {
        try verify()
        let fd = openat(descriptor, name, O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC)
        if fd < 0, errno == ENOENT { return nil }
        guard fd >= 0 else { throw SettingsError.readFailed }
        defer { Darwin.close(fd) }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & S_IFMT == S_IFREG,
              info.st_uid == geteuid(), info.st_nlink == 1, info.st_size <= 1_048_576 else {
            throw SettingsError.readFailed
        }
        var data = Data(); var buffer = [UInt8](repeating: 0, count: 8192)
        while true {
            let count = Darwin.read(fd, &buffer, buffer.count)
            if count < 0, errno == EINTR { continue }
            guard count >= 0 else { throw SettingsError.readFailed }
            if count == 0 { break }
            guard data.count + count <= 1_048_576 else { throw SettingsError.readFailed }
            data.append(contentsOf: buffer.prefix(count))
        }
        try verify()
        return data
    }
    func write(_ data: Data, name: String) throws {
        try verify()
        var info = stat()
        if fstatat(descriptor, name, &info, AT_SYMLINK_NOFOLLOW) == 0 {
            guard info.st_mode & S_IFMT == S_IFREG, info.st_uid == geteuid(), info.st_nlink == 1 else {
                throw SettingsError.saveFailed
            }
        } else if errno != ENOENT { throw SettingsError.saveFailed }
        let temporary = ".desktop-settings-\(UUID().uuidString).tmp"
        let fd = openat(descriptor, temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, mode_t(0o600))
        guard fd >= 0 else { throw SettingsError.saveFailed }
        defer { Darwin.close(fd); _ = unlinkat(descriptor, temporary, 0) }
        try data.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                if count < 0, errno == EINTR { continue }
                guard count > 0 else { throw SettingsError.saveFailed }; offset += count
            }
        }
        guard fsync(fd) == 0 else { throw SettingsError.saveFailed }
        try verify()
        guard renameat(descriptor, temporary, descriptor, name) == 0, fsync(descriptor) == 0 else {
            throw SettingsError.saveFailed
        }
        try verify()
    }
}
