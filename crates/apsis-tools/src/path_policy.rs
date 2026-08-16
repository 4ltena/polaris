//! 読み取りを拒否するパスの判定。過検出より見逃しを避ける方向に倒す。
//!
//! 比較はすべて小文字化してから行う。macOS の既定ファイルシステムは
//! 大文字小文字を区別しないため、`.SSH` や `.ENV` のような表記でも
//! 実体は同じ秘密情報に届いてしまう。サンドボックスが入るまではこの
//! 関数が唯一の防壁なので、過小拒否になる方向の揺れは許容しない。

use std::path::Path;

/// パスの一部にこれらのディレクトリ名が現れたら拒否する（小文字で比較）。
const DENIED_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", "gcloud", "keychains"];

/// ファイル名がこれらと完全一致したら拒否する（小文字で比較）。
const DENIED_NAMES: &[&str] = &[".env", "credentials", "id_rsa", "id_ed25519", "id_ecdsa"];

/// 拡張子がこれらなら拒否する（小文字で比較）。
const DENIED_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "pub"];

pub fn is_denied(path: &Path) -> bool {
    for c in path.components() {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        if DENIED_DIRS.iter().any(|d| s == *d) {
            return true;
        }
    }

    let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase()) else {
        return false;
    };

    if DENIED_NAMES.iter().any(|n| name == *n) {
        return true;
    }
    // `.env.local` のような接尾辞付きも拒否する。`environment.rs` は巻き込まない。
    if name.starts_with(".env.") {
        return true;
    }
    // macOS のキーチェーンファイルだけを拒否する。`keychain_helpers.rs` のような
    // 「keychain」を含むだけの通常のソースファイルを部分一致で巻き込まない。
    if name.ends_with(".keychain") || name.ends_with(".keychain-db") {
        return true;
    }
    if let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_lowercase())
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

    #[test]
    fn allows_benign_keychain_named_source() {
        // `keychain` を含むだけの通常のソースファイルを部分一致で巻き込まない。
        assert!(!is_denied(Path::new(
            "/home/u/proj/crates/apsis-tools/src/keychain_helpers.rs"
        )));
    }

    #[test]
    fn denies_case_variants_on_case_insensitive_filesystems() {
        // macOS の既定ファイルシステムは大文字小文字を区別しない。
        for p in [
            "/home/u/.SSH/id_rsa",
            "/home/u/proj/.ENV",
            "/home/u/key.PEM",
        ] {
            assert!(
                is_denied(Path::new(p)),
                "{p} は大文字小文字を問わず拒否されるべき"
            );
        }
    }

    #[test]
    fn denies_gcloud_and_aws_directories_in_isolation() {
        // ファイル名側のルール（`credentials` 等）に頼らず、ディレクトリ名だけで
        // 拒否できることを確認する。どちらのファイル名も DENIED_NAMES に一致しない。
        for p in [
            "/home/u/.config/gcloud/credentials.db",
            "/home/u/.aws/config",
        ] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }
}
