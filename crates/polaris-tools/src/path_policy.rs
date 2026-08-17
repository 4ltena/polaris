//! 読み取りを拒否するパスの判定。過検出より見逃しを避ける方向に倒す。
//!
//! 比較はすべて小文字化してから行う。macOS の既定ファイルシステムは
//! 大文字小文字を区別しないため、`.SSH` や `.ENV` のような表記でも
//! 実体は同じ秘密情報に届いてしまう。サンドボックスが入るまではこの
//! 関数が唯一の防壁なので、過小拒否になる方向の揺れは許容しない。
//!
//! `polaris_core::secret_screen::is_excluded_path` は監査ログへ書く前の
//! パス除外判定であり、このモジュールとは別の目的を持つ独立した関数
//! （`polaris-tools` は `polaris-core` に依存できないため、そもそも直接
//! 呼べない）。過去のレビューで、そちらのほうがここより広い秘密パスの
//! 一覧を持っていたことが指摘された。以下の一覧はその一覧が拒否する
//! 範囲を最低限含むように拡張してある。今後どちらか一方だけを広げると
//! 同じズレが再発するため、両方に手を入れる。

use std::path::Path;

/// パスの一部にこれらのディレクトリ名が現れたら拒否する（小文字で比較）。
const DENIED_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    "gcloud",
    "keychains",
    ".docker",
    ".kube",
];

/// ファイル名がこれらと完全一致したら拒否する（小文字で比較）。
const DENIED_NAMES: &[&str] = &[
    ".env",
    "credentials",
    "id_rsa",
    "id_dsa",
    "id_ed25519",
    "id_ecdsa",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".envrc",
    ".git-credentials",
    ".pgpass",
    ".my.cnf",
    "credentials.json",
    "service-account.json",
];

/// 拡張子がこれらなら拒否する（小文字で比較）。
const DENIED_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "pub", "p8", "jks", "keystore"];

pub fn is_denied(path: &Path) -> bool {
    for c in path.components() {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        if DENIED_DIRS.iter().any(|d| s == *d) {
            return true;
        }
    }

    if is_proc_environ(path) || is_gh_hosts_file(path) {
        return true;
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

/// `/proc/<pid>/environ`（Linux）はプロセスの環境変数をそのまま含み、
/// harness 自身の `POLARIS_API_KEY` もそこに載る。ただし `proc` は
/// ソースツリーにも普通に現れうるディレクトリ名なので、ディレクトリ名
/// 単体では拒否せず、ファイル名が厳密に `environ` であることと組み合わせる。
fn is_proc_environ(path: &Path) -> bool {
    let is_environ = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase() == "environ")
        .unwrap_or(false);
    if !is_environ {
        return false;
    }
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().to_lowercase() == "proc")
}

/// GitHub CLI の `hosts.yml` は保存済みトークンを平文で含む。`hosts.yml`
/// という名前単体は他の用途（Ansible インベントリ等）でも使われるため、
/// `gh` ディレクトリ区間との組み合わせでのみ拒否する。
fn is_gh_hosts_file(path: &Path) -> bool {
    let is_hosts_yml = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase() == "hosts.yml")
        .unwrap_or(false);
    if !is_hosts_yml {
        return false;
    }
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().to_lowercase() == "gh")
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
            "/home/u/proj/crates/polaris-tools/src/keychain_helpers.rs"
        )));
    }

    #[test]
    fn denies_case_variants_on_case_insensitive_filesystems() {
        // macOS の既定ファイルシステムは大文字小文字を区別しない。
        // `known_hosts` はどの完全一致ルールにも掛からないため、ここが
        // 通るのはディレクトリ区間の大文字小文字畳み込みが効いている
        // 場合に限られる（`.SSH/id_rsa` だと `id_rsa` の完全一致ルール
        // 単体でも通ってしまい、ディレクトリ側の畳み込みを検証できない）。
        for p in [
            "/home/u/.SSH/known_hosts",
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

    // --- Important 4: secret_screen::is_excluded_path と同等以上の被覆 -------
    // レビューが `is_denied == false` と実測した9パスのうち、`/proc/self/
    // environ` を除く8つ（`.zsh_history` はどちらの一覧にも無いため対象外）を
    // 1ルールずつ切り分けて検証する。

    #[test]
    fn denies_netrc_npmrc_and_pypirc_by_exact_name() {
        for p in ["/home/u/.netrc", "/home/u/.npmrc", "/home/u/.pypirc"] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn denies_envrc_and_git_credentials_by_exact_name() {
        for p in ["/home/u/.envrc", "/home/u/proj/.git-credentials"] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn denies_pgpass_and_my_cnf_by_exact_name() {
        for p in ["/home/u/.pgpass", "/home/u/.my.cnf"] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn denies_cloud_credential_json_files_by_exact_name() {
        for p in [
            "/home/u/creds/credentials.json",
            "/home/u/gcp/service-account.json",
        ] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn denies_docker_and_kube_directories_in_isolation() {
        // ファイル名側のルールに頼らず、ディレクトリ名だけで拒否できることを
        // 確認する。`config.json` / `config` はどちらも DENIED_NAMES に無い。
        for p in ["/home/u/.docker/config.json", "/home/u/.kube/config"] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn denies_gh_hosts_file_under_gh_directory() {
        assert!(is_denied(Path::new("/home/u/.config/gh/hosts.yml")));
    }

    #[test]
    fn denies_proc_self_environ() {
        assert!(is_denied(Path::new("/proc/self/environ")));
        // 他プロセスの pid でも同様に拒否する。
        assert!(is_denied(Path::new("/proc/1234/environ")));
    }

    #[test]
    fn denies_additional_key_extensions() {
        for p in ["/home/u/key.p8", "/home/u/app.jks", "/home/u/app.keystore"] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn denies_id_dsa_by_exact_name() {
        // `.ssh` ディレクトリの外でも、ファイル名単体のルールで拒否できる
        // ことを確認する（ディレクトリ側のルールに頼らない）。
        assert!(is_denied(Path::new("/home/u/backup/id_dsa")));
    }

    #[test]
    fn allows_benign_files_resembling_the_new_rules() {
        // 新しいルールが厳密一致・接頭辞・接尾辞・ディレクトリ区間のみで
        // 判定されており、部分一致に倒れていないことを確認する。
        for p in [
            // ".netrc" 等はファイル名の完全一致であり、それを含むだけの
            // 通常のファイルは巻き込まない。
            "/home/u/docs/netrc-setup.md",
            "/home/u/src/npmrc_loader.rs",
            // ".docker" / ".kube" はディレクトリ区間の完全一致であり、
            // 紛らわしい別名のディレクトリは巻き込まない。
            "/home/u/proj/docker-compose/README.md",
            "/home/u/proj/src/kubeconfig_loader.rs",
            // "hosts.yml" は `gh` ディレクトリ配下でのみ拒否する。
            "/home/u/proj/ansible/hosts.yml",
            // "environ" は `proc` ディレクトリ配下でのみ拒否する。
            "/home/u/proj/environ.rs",
            "/home/u/proj/proc/build.rs",
            // 拡張子ルールはドット区切りの拡張子一致であり、ファイル名の
            // 途中に文字列として現れるだけでは拒否しない。
            "/home/u/proj/src/jks_parser.rs",
            "/home/u/proj/notes/keystore-migration.md",
        ] {
            assert!(!is_denied(Path::new(p)), "{p} は許可されるべき");
        }
    }
}
