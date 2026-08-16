//! 読み取りを拒否するパスの判定。過検出より見逃しを避ける方向に倒す。

use std::path::Path;

/// パスの一部にこれらのディレクトリ名が現れたら拒否する。
const DENIED_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", "gcloud", "Keychains"];

/// ファイル名がこれらと完全一致したら拒否する。
const DENIED_NAMES: &[&str] = &[".env", "credentials", "id_rsa", "id_ed25519", "id_ecdsa"];

/// 拡張子がこれらなら拒否する。
const DENIED_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "pub"];

pub fn is_denied(path: &Path) -> bool {
    for c in path.components() {
        let s = c.as_os_str().to_string_lossy();
        if DENIED_DIRS.iter().any(|d| s == *d) {
            return true;
        }
    }

    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return false;
    };

    if DENIED_NAMES.iter().any(|n| name == *n) {
        return true;
    }
    // `.env.local` のような接尾辞付きも拒否する。`environment.rs` は巻き込まない。
    if name.starts_with(".env.") {
        return true;
    }
    if name.contains("keychain") {
        return true;
    }
    if let Some(ext) = path.extension().map(|e| e.to_string_lossy().into_owned())
        && DENIED_EXTS.iter().any(|e| ext == *e)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn denies_secret_bearing_paths() {
        for p in [
            "/home/u/proj/.env",
            "/home/u/proj/.env.local",
            "/home/u/.ssh/id_ed25519",
            "/home/u/.ssh/known_hosts",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.aws/credentials",
            "/home/u/key.pem",
            "/home/u/cert.pub",
            "/home/u/Library/Keychains/login.keychain-db",
        ] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn allows_ordinary_source_paths() {
        for p in [
            "/home/u/proj/src/main.rs",
            "/home/u/proj/Cargo.toml",
            "/home/u/proj/docs/env-setup.md",
            "/home/u/proj/environment.rs",
        ] {
            assert!(!is_denied(Path::new(p)), "{p} は許可されるべき");
        }
    }
}
