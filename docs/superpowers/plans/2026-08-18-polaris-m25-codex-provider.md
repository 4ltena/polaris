# polaris M2.5 Codex プロバイダ 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ChatGPT のサブスクリプション認証で polaris を実モデルへ繋ぐ。API キーを持たない利用者が `polaris login` の後に `POLARIS_PROVIDER=codex polaris -p "…"` を実行でき、M1 の受け入れ基準のうち唯一未確認の「実キーでの一発実行」をキー無しで満たす。

**Architecture:** 新クレート `polaris-auth` が OAuth の寿命と資格情報の保管だけを持ち、`polaris-provider/src/codex.rs` が Responses API へのワイヤ変換と SSE の畳み込みだけを持つ。両者は `TokenSource` トレイトで繋がり、その実装は `polaris-cli` に置く。依存の向きは `cli → {auth, provider}`、`provider → tools` の一方向で、`auth` は polaris の他クレートに依存しない。

**Tech Stack:** Rust 1.96 / edition 2024、`reqwest`（rustls、stream）、`tokio`（net、time を追加）、`sha2`（PKCE S256）、`base64`、`url`、乱数は `/dev/urandom` を直接読む。テストは `wiremock` と `tempfile`。

**Spec:** `docs/superpowers/specs/2026-08-18-polaris-codex-provider-design.md`

## Global Constraints

以下は仕様から逐語で写した値である。全タスクの要件に暗黙に含まれる。

- 認証発行者は `https://auth.openai.com`。認可 `/oauth/authorize`、トークン `/oauth/token`、失効 `/oauth/revoke`
- client_id は `app_EMoamEEZ73f0CkXaXp7hrann`
- PKCE は S256。scope は `openid profile email offline_access`
- コールバックは `http://localhost:1455/auth/callback`。ポート 1455 は client_id に登録済みの redirect_uri であり、変更できない
- API は `https://chatgpt.com/backend-api/codex/responses`
- ヘッダは `Authorization: Bearer <access_token>` と `chatgpt-account-id: <account_id>`
- モデルは `gpt-5.1-codex-max`、`gpt-5.2-codex`、`gpt-5.3-codex`。`codex` プロバイダの既定は `gpt-5.3-codex`
- 保管先は `~/.polaris/auth.json`、パーミッションは 0600、書き込みは一時ファイルと rename による原子的書き込み
- **`~/.codex/` には読み書きとも一切触れない**
- `store` は偽。毎ターン全文を送る。`previous_response_id` は使わない
- `Provider` トレイトは非ストリーミングのまま変えない。SSE は内部で畳む
- 認証の失敗はモデルの失敗と別事象にする（`ProviderError::Auth`）
- `response.completed` を見ないまま終わった応答を正常終了として返さない
- タイムアウトは応答全体ではなく無通信で測る
- **実 API 呼び出しを行うテストと、実資格情報を読むテストは 1 本も作らない**
- 否定を確かめるテストには、同じテストの中に肯定の対照を置く
- 検証は `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` の 3 つ。`docs/filemap.md` が動いたら `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` で再生成し、`Cargo.lock` とともに同じコミットへ含める

## ファイル構成

| ファイル | 責務 |
| --- | --- |
| `crates/polaris-auth/Cargo.toml` | 新クレートの依存 |
| `crates/polaris-auth/src/lib.rs` | `Credentials`、`AuthError`、`ensure_fresh`、`logout`。各モジュールの束ね |
| `crates/polaris-auth/src/pkce.rs` | verifier と challenge の生成（S256）。乱数の取得 |
| `crates/polaris-auth/src/store.rs` | `~/.polaris/auth.json` の読み書きと削除。0600 と原子的書き込み |
| `crates/polaris-auth/src/token.rs` | `/oauth/token` への交換と更新。期限の算出 |
| `crates/polaris-auth/src/login.rs` | 1455 のコールバック受け、ブラウザ起動、login の統合 |
| `crates/polaris-provider/src/lib.rs` | `ProviderError::Auth`、`Token`、`TokenSource` を追加 |
| `crates/polaris-provider/src/codex.rs` | Responses API への変換、SSE の畳み込み、HTTP 経路 |
| `crates/polaris-cli/src/main.rs` | `login` / `logout` サブコマンド、`POLARIS_PROVIDER`、`TokenSource` の実装 |
| `README.md` | 環境変数と login 手順 |

`polaris-auth` を 5 ファイルに割るのは、壊れ方が別だからである。PKCE は計算、store はファイルシステム、token はネットワーク、login は待ち合わせと競合。1 ファイルに混ぜると、落ちたテストがどの層の欠陥かを指さない。

---

### Task 1: `polaris-auth` クレートと PKCE

**Files:**
- Create: `crates/polaris-auth/Cargo.toml`
- Create: `crates/polaris-auth/src/lib.rs`
- Create: `crates/polaris-auth/src/pkce.rs`
- Modify: `Cargo.toml`（ワークスペースの `[workspace.dependencies]` に `sha2` と `base64` と `url` を追加、`tokio` に `net` と `time` を追加）

**Interfaces:**
- Consumes: なし
- Produces: `polaris_auth::pkce::Pkce { verifier: String, challenge: String }`、`polaris_auth::pkce::generate() -> Result<Pkce, AuthError>`、`polaris_auth::pkce::random_urlsafe(bytes: usize) -> Result<String, AuthError>`、`polaris_auth::AuthError`

- [ ] **Step 1: ワークスペースの依存を足す**

`Cargo.toml` の `[workspace.dependencies]` へ 3 行を足し、`tokio` の features を差し替える。

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "fs", "io-util", "net", "time"] }
sha2 = "0.10"
base64 = "0.22"
url = "2"
```

`net` はコールバックの `TcpListener`、`time` は待ち合わせのタイムアウトに要る。`base64` と `url` は既に推移依存として lock に入っている（0.22.1 と 2.5.8）ので、直接依存にしても新しい木は増えない。`sha2` だけが新規である。PKCE の S256 に SHA-256 が要り、手書きするより標準の実装を使う。

- [ ] **Step 2: クレートの `Cargo.toml` を書く**

`crates/polaris-auth/Cargo.toml`:

```toml
[package]
name = "polaris-auth"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
reqwest = { workspace = true }
tokio = { workspace = true }
sha2 = { workspace = true }
base64 = { workspace = true }
url = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
wiremock = { workspace = true }
```

polaris の他クレートへは依存しない。`polaris-auth` は polaris の型を知らない。

- [ ] **Step 3: 失敗するテストを書く**

`crates/polaris-auth/src/pkce.rs` を新規作成し、次を書く。実装はまだ書かない。

```rust
//! PKCE (RFC 7636) の verifier と challenge。
//!
//! 乱数は `/dev/urandom` を直接読む。polaris は既に Unix 前提であり
//! （`st_nlink` の検査、0600 のパーミッション）、乱数のためだけに依存を
//! 増やす理由が無い。

use crate::AuthError;

/// verifier と、それから導いた challenge の対。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B の既知ベクタ。verifier をこの値に固定したとき、
    /// challenge がこの値にならなければ S256 の計算が間違っている。
    #[test]
    fn s256_matches_the_rfc_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_for(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// verifier は RFC が定める長さ（43〜128 文字）に収まり、
    /// unreserved 文字だけで構成される。
    #[test]
    fn a_generated_verifier_is_within_the_rfc_length_and_charset() {
        let p = generate().expect("生成できない");
        assert!(
            (43..=128).contains(&p.verifier.chars().count()),
            "verifier の長さが RFC の範囲外: {}",
            p.verifier.chars().count()
        );
        assert!(
            p.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
            "verifier に unreserved 以外の文字がある: {}",
            p.verifier
        );
    }

    /// 生成のたびに違う値が出る。固定値を返す実装がこのテストで落ちる。
    /// 対になる肯定側は上の 2 本（形が正しいこと）が見ている。
    #[test]
    fn two_generations_differ() {
        let a = generate().expect("生成できない");
        let b = generate().expect("生成できない");
        assert_ne!(a.verifier, b.verifier, "verifier が毎回同じ");
        assert_ne!(a.challenge, b.challenge, "challenge が毎回同じ");
    }

    /// 生成した対は整合している。challenge が verifier と無関係でないこと。
    #[test]
    fn a_generated_pair_is_self_consistent() {
        let p = generate().expect("生成できない");
        assert_eq!(p.challenge, challenge_for(&p.verifier));
    }

    #[test]
    fn random_urlsafe_has_no_padding_and_no_unsafe_characters() {
        let s = random_urlsafe(32).expect("生成できない");
        assert!(!s.contains('='), "パディングが残っている: {s}");
        assert!(!s.contains('+') && !s.contains('/'), "URL 安全でない文字がある: {s}");
        assert!(!s.is_empty());
    }
}
```

- [ ] **Step 4: テストが落ちることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: コンパイルエラー。`challenge_for`、`generate`、`random_urlsafe`、`AuthError` が無い。

- [ ] **Step 5: `AuthError` と `lib.rs` を書く**

`crates/polaris-auth/src/lib.rs`:

```rust
//! ChatGPT のサブスクリプション認証（OAuth）の寿命と資格情報の保管。
//!
//! このクレートは polaris の他のクレートに依存しない。Responses API も
//! `Provider` トレイトも知らない。認証は時計とファイルシステムと
//! ネットワークの競合で壊れ、ワイヤ変換は形式の解釈で壊れる。同じ場所に
//! 置くと、落ちたテストがどちらの欠陥かを指さない。
//!
//! `~/.codex/` には読み書きとも一切触れない。サーバが refresh token を
//! ローテーションさせる場合、こちらが更新した瞬間に codex 側の控えが
//! 失効しうる。独立した store を持てば、この事故は原理的に起きない。

pub mod pkce;

/// 認可の発行者。
pub const ISSUER: &str = "https://auth.openai.com";
/// codex CLI に登録された client_id。redirect_uri もこれに紐づく。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 登録済みの redirect_uri。ポートは選び直せない。
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// コールバックを待ち受けるポート。`REDIRECT_URI` と必ず一致させる。
pub const CALLBACK_PORT: u16 = 1455;
/// 要求する scope。`offline_access` が無いと refresh token が返らない。
pub const SCOPE: &str = "openid profile email offline_access";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("入出力エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP エラー: {0}")]
    Http(String),
    #[error("応答を解釈できない: {0}")]
    Decode(String),
    #[error("認可が拒否された: {0}")]
    Denied(String),
    // 書式に `{CALLBACK_PORT}` と書いてはいけない。thiserror は `{NAME}` を
    // variant のフィールド名として解釈するので、定数を書くとコンパイルが
    // 通らない。ポート番号は literal で書き、`CALLBACK_PORT` と一致させる。
    #[error(
        "ポート 1455 を使えない。redirect_uri がこのポートに登録されているため選び直せない。\
         `codex login` が同時に走っていないか確かめること: {0}"
    )]
    PortInUse(String),
    #[error("ログインしていない")]
    NotLoggedIn,
}
```

- [ ] **Step 6: `pkce.rs` の実装を書く**

Step 3 で作った `pkce.rs` の、`Pkce` 構造体と `#[cfg(test)]` の間に次を挿入する。

```rust
/// `/dev/urandom` から読んだ乱数を base64url（パディング無し）にする。
pub fn random_urlsafe(bytes: usize) -> Result<String, AuthError> {
    use base64::Engine;
    use std::io::Read;

    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf))
}

/// verifier から challenge を導く。S256 は「SHA-256 して base64url」である。
pub fn challenge_for(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// 新しい対を作る。32 バイトの乱数は base64url で 43 文字になり、
/// RFC が定める下限とちょうど一致する。
pub fn generate() -> Result<Pkce, AuthError> {
    let verifier = random_urlsafe(32)?;
    let challenge = challenge_for(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}
```

`lib.rs` には既に `pub mod pkce;` がある。

- [ ] **Step 7: テストが通ることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: 5 passed, 0 failed。

Run: `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all -- --check`
Expected: どちらも clean。

- [ ] **Step 8: 変異で確かめる**

`challenge_for` の `Sha256::digest(verifier.as_bytes())` を
`Sha256::digest(b"fixed")` に置き換える。ディスクに乗ったことを `grep` で確かめてから
`cargo test -p polaris-auth` を走らせる。

Expected: `s256_matches_the_rfc_test_vector` と `a_generated_pair_is_self_consistent` と
`two_generations_differ` が落ちる。戻して `git status` が空であることを確かめる。

- [ ] **Step 9: コミット**

```bash
git add Cargo.toml Cargo.lock crates/polaris-auth
git commit -m "feat(auth): add the polaris-auth crate with PKCE S256"
```

`docs/filemap.md` が動く場合は `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` を走らせ、同じコミットへ含める。

---

### Task 2: 資格情報の保管

**Files:**
- Create: `crates/polaris-auth/src/store.rs`
- Modify: `crates/polaris-auth/src/lib.rs`（`pub mod store;` と `Credentials` を追加）

**Interfaces:**
- Consumes: `AuthError`
- Produces: `polaris_auth::Credentials { access_token: String, refresh_token: String, account_id: String, expires_at: Option<u64> }`、`polaris_auth::store::default_path() -> Result<PathBuf, AuthError>`、`polaris_auth::store::save_to(path: &Path, c: &Credentials) -> Result<(), AuthError>`、`polaris_auth::store::load_from(path: &Path) -> Result<Option<Credentials>, AuthError>`、`polaris_auth::store::delete_at(path: &Path) -> Result<bool, AuthError>`

`expires_at` が `Option` なのは、トークン応答が `expires_in` を持たない場合を「期限不明」として表すためである。ここを 0 や現在時刻で埋めると、不明と失効の区別が消える。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-auth/src/store.rs` を新規作成する。

```rust
//! 資格情報の保管。`~/.polaris/auth.json`、0600、原子的書き込み。
//!
//! `~/.codex/auth.json` は読まない。コピーもしない。

use std::path::{Path, PathBuf};

use crate::{AuthError, Credentials};

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acct".into(),
            expires_at: Some(1_800_000_000),
        }
    }

    #[test]
    fn a_saved_credential_round_trips() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");
        let got = load_from(&p).expect("読めない").expect("無い");
        assert_eq!(got, sample());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let got = load_from(&dir.path().join("nope.json")).expect("存在しないことは失敗ではない");
        assert!(got.is_none());
    }

    /// 資格情報のファイルは所有者だけが読める。他のプロセスから読めては
    /// ならない。
    #[test]
    fn the_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");
        let mode = std::fs::metadata(&p).expect("メタデータ").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "パーミッションが 0600 でない: {:o}", mode & 0o777);
    }

    /// 上書き保存でもパーミッションが緩まない。1 回目で 0600 になっても、
    /// 2 回目が既定の 0644 で作り直せば穴が開く。
    #[test]
    fn overwriting_keeps_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");
        let mut second = sample();
        second.access_token = "at2".into();
        save_to(&p, &second).expect("保存できない");
        let mode = std::fs::metadata(&p).expect("メタデータ").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "上書きでパーミッションが緩んだ: {:o}", mode & 0o777);
        assert_eq!(load_from(&p).expect("読めない").expect("無い").access_token, "at2");
    }

    /// 書き込みは一時ファイルへ書いてから rename する。rename の前に
    /// 落ちても旧ファイルは無傷である。ここでは「一時ファイルが残っていても
    /// 本体は旧内容のまま読める」ことで、書き込み先が本体でないことを見る。
    #[test]
    fn a_leftover_temp_file_does_not_disturb_the_stored_credentials() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("保存できない");

        // 中断された書き込みの痕跡を模す。
        std::fs::write(dir.path().join("auth.json.tmp"), b"half-written").expect("書けない");

        let got = load_from(&p).expect("読めない").expect("無い");
        assert_eq!(got, sample(), "本体が一時ファイルに汚染されている");
    }

    #[test]
    fn delete_reports_whether_a_file_was_there() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        assert!(!delete_at(&p).expect("削除で失敗しない"), "無いのに消したと言った");
        save_to(&p, &sample()).expect("保存できない");
        assert!(delete_at(&p).expect("削除できない"), "あったのに消していないと言った");
        assert!(!p.exists());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_absence() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        std::fs::write(&p, b"{ not json").expect("書けない");
        let err = load_from(&p).expect_err("壊れたファイルは失敗であるべき");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "壊れたファイルが Decode 以外になっている: {err:?}"
        );
    }
}
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-auth store`
Expected: コンパイルエラー。`Credentials`、`save_to`、`load_from`、`delete_at` が無い。

- [ ] **Step 3: `Credentials` を `lib.rs` へ足す**

`lib.rs` の `pub mod pkce;` の下へ `pub mod store;` を足し、`AuthError` の定義の下へ次を書く。

```rust
/// 保管する資格情報。`expires_at` は Unix 秒。応答が `expires_in` を
/// 持たない場合は `None` にする。0 や現在時刻で埋めると、期限が不明で
/// あることと失効していることの区別が消える。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(default)]
    pub expires_at: Option<u64>,
}
```

- [ ] **Step 4: `store.rs` の実装を書く**

`store.rs` の `use` の下、`#[cfg(test)]` の上へ次を挿入する。

```rust
/// 既定の保管先。`~/.polaris/auth.json`。
pub fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        AuthError::Io(std::io::Error::other("HOME が設定されていない"))
    })?;
    Ok(Path::new(&home).join(".polaris").join("auth.json"))
}

/// 保存する。一時ファイルへ 0600 で書いてから rename する。rename は
/// 同一ディレクトリ内で原子的なので、途中で落ちても本体が半端な内容に
/// 置き換わることがない。
pub fn save_to(path: &Path, c: &Credentials) -> Result<(), AuthError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(c)
        .map_err(|e| AuthError::Decode(format!("資格情報を直列化できない: {e}")))?;

    // 既存の一時ファイルが残っている場合に備えて truncate する。mode は
    // 作成時にしか効かないので、既存ファイルを開いた場合に備えて
    // set_permissions でも明示する。
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 読む。存在しないことは失敗ではない。壊れていることは失敗である。
pub fn load_from(path: &Path) -> Result<Option<Credentials>, AuthError> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AuthError::Io(e)),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| AuthError::Decode(format!("{} を解釈できない: {e}", path.display())))
}

/// 削除する。戻り値は「実際にファイルがあったか」。
pub fn delete_at(path: &Path) -> Result<bool, AuthError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AuthError::Io(e)),
    }
}
```

`std::os::unix::fs::PermissionsExt` を `set_permissions` のために `use` へ足す必要がある。ファイル冒頭の `use` を次にする。

```rust
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::{AuthError, Credentials};
```

- [ ] **Step 5: テストが通ることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: 12 passed, 0 failed（Task 1 の 5 本 + このタスクの 7 本）。

- [ ] **Step 6: 変異で確かめる**

3 つ試す。それぞれディスクに乗ったことを `grep` で確かめ、ビルドが走り直したことを確かめ、戻したあと `git status` が空であることを確かめる。

1. `.mode(0o600)` を `.mode(0o644)` にする → `the_saved_file_is_owner_only` と `overwriting_keeps_the_file_owner_only` が落ちる
2. `set_permissions` の行を消す → 上書き時のみ緩む経路が残る。両方通ってしまう場合はテストが弱いので、`overwriting_keeps_the_file_owner_only` が `.mode()` ではなく `set_permissions` を見ていることを確かめる（`.mode(0o600)` を `.mode(0o644)` に**戻さずに** `set_permissions` だけ消す）
3. 一時ファイルを経ずに `std::fs::write(path, &body)` で直接書く → `a_leftover_temp_file_does_not_disturb_the_stored_credentials` は落ちない。**これは想定どおりである。** このテストは書き込み先が本体でないことを間接的にしか見ていない。落ちないことを確認したうえで、報告に「原子性そのものはテストで固定できていない」と書く。プロセスを途中で殺す試験は単体テストの範囲を超える

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-auth
git commit -m "feat(auth): store credentials at ~/.polaris/auth.json, 0600, atomically"
```

---

### Task 3: トークンの交換と更新

**Files:**
- Create: `crates/polaris-auth/src/token.rs`
- Modify: `crates/polaris-auth/src/lib.rs`（`pub mod token;`）

**Interfaces:**
- Consumes: `AuthError`、`Credentials`、`CLIENT_ID`、`REDIRECT_URI`
- Produces: `polaris_auth::token::exchange_code(issuer: &str, code: &str, verifier: &str) -> Result<Credentials, AuthError>`、`polaris_auth::token::refresh(issuer: &str, refresh_token: &str) -> Result<Credentials, AuthError>`

`issuer` を引数で受けるのは、テストが `wiremock` の URL を渡せるようにするためである。本番の呼び出し側は `polaris_auth::ISSUER` を渡す。定数を関数の内側に埋めると、実 API を叩かずに試験する方法が無くなる。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-auth/src/token.rs` を新規作成する。

```rust
//! `/oauth/token` への交換と更新。
//!
//! `issuer` を引数で受けるのは、テストが偽のサーバを指せるようにするため
//! である。実 API を叩くテストは 1 本も作らない。

use serde::Deserialize;

use crate::{AuthError, CLIENT_ID, Credentials, REDIRECT_URI};

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// account_id は id_token の payload に入る。テスト用に
    /// `{"chatgpt_account_id":"acct-1"}` を base64url で包んだ JWT 風の
    /// 3 分割文字列を作る。署名は検証しない（サーバから TLS で受け取った
    /// ものであり、こちらが発行者を検証する立場に無い）。
    fn id_token_with_account(account: &str) -> String {
        use base64::Engine;
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account }
        });
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{b}.signature")
    }

    async fn server_returning(body: serde_json::Value) -> MockServer {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&s)
            .await;
        s
    }

    #[tokio::test]
    async fn exchange_returns_credentials_with_an_expiry() {
        let s = server_returning(serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id_token_with_account("acct-1"),
            "expires_in": 3600
        }))
        .await;

        let c = exchange_code(&s.uri(), "the-code", "the-verifier")
            .await
            .expect("交換できるべき");
        assert_eq!(c.access_token, "at");
        assert_eq!(c.refresh_token, "rt");
        assert_eq!(c.account_id, "acct-1");
        assert!(c.expires_at.is_some(), "expires_in があるのに期限が無い");
    }

    /// `expires_in` が無い応答は「期限不明」であり、`None` になる。
    /// ここを現在時刻や 0 で埋めると、不明と失効の区別が消える。
    #[tokio::test]
    async fn a_response_without_expires_in_has_an_unknown_expiry() {
        let s = server_returning(serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id_token_with_account("acct-1")
        }))
        .await;

        let c = exchange_code(&s.uri(), "c", "v").await.expect("交換できるべき");
        assert_eq!(c.expires_at, None, "期限不明が None になっていない");
    }

    /// refresh token を返さない更新応答では、渡した既存の値を保つ。
    /// ここを空にすると、次回の更新ができなくなる。
    #[tokio::test]
    async fn refresh_keeps_the_old_refresh_token_when_the_response_omits_it() {
        let s = server_returning(serde_json::json!({
            "access_token": "new-at",
            "id_token": id_token_with_account("acct-1"),
            "expires_in": 3600
        }))
        .await;

        let c = refresh(&s.uri(), "old-rt").await.expect("更新できるべき");
        assert_eq!(c.access_token, "new-at");
        assert_eq!(c.refresh_token, "old-rt", "既存の refresh token が捨てられた");
    }

    /// 応答が新しい refresh token を返したら、そちらを使う。上の対。
    /// 片方だけでは「常に古い方を返す」実装が通ってしまう。
    #[tokio::test]
    async fn refresh_adopts_a_rotated_refresh_token() {
        let s = server_returning(serde_json::json!({
            "access_token": "new-at",
            "refresh_token": "rotated-rt",
            "id_token": id_token_with_account("acct-1"),
            "expires_in": 3600
        }))
        .await;

        let c = refresh(&s.uri(), "old-rt").await.expect("更新できるべき");
        assert_eq!(c.refresh_token, "rotated-rt", "回転した refresh token を採っていない");
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_http_error() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad_grant"))
            .mount(&s)
            .await;

        let err = refresh(&s.uri(), "rt").await.expect_err("失敗すべき");
        assert!(matches!(err, AuthError::Http(_)), "Http 以外になっている: {err:?}");
    }

    /// access_token が無い応答は成功ではない。空の資格情報を返すと、
    /// 次の API 呼び出しが 401 になり、原因が認証まで遡れなくなる。
    #[tokio::test]
    async fn a_response_without_an_access_token_is_a_decode_error() {
        let s = server_returning(serde_json::json!({ "expires_in": 3600 })).await;
        let err = refresh(&s.uri(), "rt").await.expect_err("失敗すべき");
        assert!(matches!(err, AuthError::Decode(_)), "Decode 以外になっている: {err:?}");
    }

    #[test]
    fn the_account_id_comes_out_of_the_id_token_payload() {
        let got = account_id_from_id_token(&id_token_with_account("acct-xyz"))
            .expect("取り出せるべき");
        assert_eq!(got, "acct-xyz");
    }

    #[test]
    fn a_malformed_id_token_does_not_panic() {
        assert!(account_id_from_id_token("not-a-jwt").is_none());
        assert!(account_id_from_id_token("a.!!!.c").is_none());
        assert!(account_id_from_id_token("").is_none());
    }
}
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-auth token`
Expected: コンパイルエラー。`exchange_code`、`refresh`、`account_id_from_id_token` が無い。

- [ ] **Step 3: 実装を書く**

`token.rs` の `use` の下、`#[cfg(test)]` の上へ挿入する。

```rust
/// トークン応答。`refresh_token` と `expires_in` は返らないことがある。
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

/// 現在の Unix 秒。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// id_token（JWT）の payload から chatgpt_account_id を取り出す。
///
/// 署名は検証しない。この値は TLS の下でサーバから受け取ったものであり、
/// こちらは発行者を検証する立場に無い。壊れていたら `None` を返す。
/// panic しないことをテストで固定する。
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = id_token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// フォーム POST を投げて `Credentials` を組み立てる共通部分。
/// `fallback_refresh` は、応答が refresh_token を省いたときに保つ値。
async fn post_token(
    issuer: &str,
    form: &[(&str, &str)],
    fallback_refresh: Option<&str>,
) -> Result<Credentials, AuthError> {
    let resp = reqwest::Client::new()
        .post(format!("{issuer}/oauth/token"))
        .form(form)
        .send()
        .await
        .map_err(|e| AuthError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AuthError::Http(format!("status {status}: {body}")));
    }

    let t: TokenResponse = resp
        .json()
        .await
        .map_err(|e| AuthError::Decode(e.to_string()))?;

    let access_token = t
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::Decode("応答に access_token が無い".into()))?;

    let refresh_token = t
        .refresh_token
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_refresh.map(|s| s.to_string()))
        .ok_or_else(|| AuthError::Decode("応答にも手元にも refresh_token が無い".into()))?;

    let account_id = t
        .id_token
        .as_deref()
        .and_then(account_id_from_id_token)
        .unwrap_or_default();

    Ok(Credentials {
        access_token,
        refresh_token,
        account_id,
        expires_at: t.expires_in.map(|s| now_secs().saturating_add(s)),
    })
}

/// 認可コードを資格情報へ交換する。
pub async fn exchange_code(
    issuer: &str,
    code: &str,
    verifier: &str,
) -> Result<Credentials, AuthError> {
    post_token(
        issuer,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier),
        ],
        None,
    )
    .await
}

/// refresh token で更新する。
pub async fn refresh(issuer: &str, refresh_token: &str) -> Result<Credentials, AuthError> {
    post_token(
        issuer,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ],
        Some(refresh_token),
    )
    .await
}
```

`lib.rs` へ `pub mod token;` を足す。`Cargo.toml` の `[dependencies]` には既に `reqwest` があるが、`form` を使うため features に変更は要らない（`reqwest` の既定で有効）。

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: 20 passed, 0 failed。

- [ ] **Step 5: 変異で確かめる**

1. `fallback_refresh` を使う `.or_else(...)` を消す → `refresh_keeps_the_old_refresh_token_when_the_response_omits_it` が落ちる
2. `t.refresh_token` を無視して常に `fallback_refresh` を使う → `refresh_adopts_a_rotated_refresh_token` が落ちる
3. `expires_at: t.expires_in.map(...)` を `expires_at: Some(now_secs())` にする → `a_response_without_expires_in_has_an_unknown_expiry` が落ちる
4. `access_token` の `ok_or_else` を `unwrap_or_default()` にする → `a_response_without_an_access_token_is_a_decode_error` が落ちる

それぞれ適用をディスクで確かめ、戻して `git status` を空にする。

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-auth
git commit -m "feat(auth): exchange and refresh tokens against the OAuth endpoint"
```

---

### Task 4: コールバックの受け取りと login

**Files:**
- Create: `crates/polaris-auth/src/login.rs`
- Modify: `crates/polaris-auth/src/lib.rs`（`pub mod login;`）

**Interfaces:**
- Consumes: `pkce::generate`、`token::exchange_code`、`store::save_to`、`store::default_path`、`AuthError`、`CALLBACK_PORT`、`CLIENT_ID`、`ISSUER`、`REDIRECT_URI`、`SCOPE`
- Produces: `polaris_auth::login::authorize_url(challenge: &str, state: &str) -> String`、`polaris_auth::login::parse_callback(request_line: &str) -> Result<(String, String), AuthError>`、`polaris_auth::login::wait_for_callback(listener: tokio::net::TcpListener, expected_state: &str, timeout: Duration) -> Result<String, AuthError>`、`polaris_auth::login::bind_callback() -> Result<tokio::net::TcpListener, AuthError>`、`polaris_auth::login::run(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError>`

`wait_for_callback` が `TcpListener` を引数で受けるのは、テストが 0 番ポートで bind した listener を渡せるようにするためである。1455 を握ってテストすると、`codex login` や他のテストと衝突する。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-auth/src/login.rs` を新規作成する。

```rust
//! 認可 URL の組み立てと、コールバックの一度きりの受け取り。
//!
//! ポート 1455 は client_id に登録された redirect_uri のポートであり、
//! 選び直せない。したがって `codex login` とは同時に走らない。塞がって
//! いるときは、汎用の bind エラーではなくその衝突を名指しする。原因を
//! 掴めない拒否メッセージは、同じ失敗の反復を招く。

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{
    AuthError, CALLBACK_PORT, CLIENT_ID, Credentials, REDIRECT_URI, SCOPE, pkce, store, token,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_every_parameter_the_flow_needs() {
        let u = authorize_url("the-challenge", "the-state");
        let parsed = url::Url::parse(&u).expect("URL として壊れている");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.host_str(), Some("auth.openai.com"));
        assert_eq!(parsed.path(), "/oauth/authorize");
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("redirect_uri").map(String::as_str), Some(REDIRECT_URI));
        assert_eq!(q.get("scope").map(String::as_str), Some(SCOPE));
        assert_eq!(q.get("code_challenge").map(String::as_str), Some("the-challenge"));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(q.get("state").map(String::as_str), Some("the-state"));
    }

    #[test]
    fn the_callback_query_is_parsed_into_code_and_state() {
        let (code, state) =
            parse_callback("GET /auth/callback?code=abc&state=xyz HTTP/1.1").expect("解釈できるべき");
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    /// パーセント符号化された値が復号される。生のまま交換へ渡すと、
    /// サーバ側で invalid_grant になり、原因が符号化だと分からない。
    #[test]
    fn percent_encoded_values_are_decoded() {
        let (code, _) =
            parse_callback("GET /auth/callback?code=a%2Fb%2Bc&state=s HTTP/1.1").expect("解釈できるべき");
        assert_eq!(code, "a/b+c");
    }

    /// 認可が拒否されたときは `error` が返る。これを「code が無い」で
    /// 片付けると、利用者に何が起きたか伝わらない。
    #[test]
    fn an_error_response_is_surfaced_with_its_reason() {
        let err = parse_callback("GET /auth/callback?error=access_denied HTTP/1.1")
            .expect_err("失敗すべき");
        let AuthError::Denied(msg) = err else {
            panic!("Denied 以外になっている: {err:?}");
        };
        assert!(msg.contains("access_denied"), "理由が文面に無い: {msg}");
    }

    #[test]
    fn a_request_line_without_a_code_is_an_error() {
        assert!(parse_callback("GET /auth/callback HTTP/1.1").is_err());
        assert!(parse_callback("garbage").is_err());
    }

    /// state が一致しないコールバックは受け取らない。CSRF の対策であり、
    /// ここを外すと第三者が仕込んだ code を掴まされうる。
    #[tokio::test]
    async fn a_state_mismatch_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
            s.write_all(b"GET /auth/callback?code=c&state=WRONG HTTP/1.1\r\n\r\n")
                .await
                .expect("write");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
        });

        let err = wait_for_callback(listener, "EXPECTED", Duration::from_secs(5))
            .await
            .expect_err("state 不一致は失敗すべき");
        assert!(matches!(err, AuthError::Denied(_)), "Denied 以外: {err:?}");
        client.await.expect("client");
    }

    /// 対になる肯定側。state が一致すれば code を返す。これが無いと、
    /// 「常に拒否する」実装が上のテストを通ってしまう。
    #[tokio::test]
    async fn a_matching_state_yields_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
            s.write_all(b"GET /auth/callback?code=the-code&state=EXPECTED HTTP/1.1\r\n\r\n")
                .await
                .expect("write");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            // ブラウザに何か表示されること。空の応答だと利用者は
            // 成功したのか分からない。
            let body = String::from_utf8_lossy(&buf).to_string();
            body
        });

        let code = wait_for_callback(listener, "EXPECTED", Duration::from_secs(5))
            .await
            .expect("受け取れるべき");
        assert_eq!(code, "the-code");

        let body = client.await.expect("client");
        assert!(body.starts_with("HTTP/1.1 200"), "200 を返していない: {body}");
        assert!(!body.trim().is_empty(), "ブラウザへ何も返していない");
    }

    #[tokio::test]
    async fn waiting_times_out_instead_of_hanging_forever() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let err = wait_for_callback(listener, "S", Duration::from_millis(50))
            .await
            .expect_err("タイムアウトすべき");
        assert!(
            matches!(err, AuthError::Denied(_) | AuthError::Io(_)),
            "予期しない種類: {err:?}"
        );
    }

    /// 1455 が塞がっているとき、汎用の bind エラーではなく衝突を名指しする。
    #[tokio::test]
    async fn a_busy_callback_port_names_the_collision() {
        let Ok(_held) = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await else {
            // 何か他のプロセスが既に握っている環境では、この試験の前提が
            // 成立しない。握れないこと自体は異常ではないので飛ばす。
            return;
        };
        let err = bind_callback().await.expect_err("塞がっているので失敗すべき");
        let AuthError::PortInUse(_) = err else {
            panic!("PortInUse 以外になっている: {err:?}");
        };
        // 文面に codex login への言及があること。`Display` は
        // `AuthError` の属性で組み立てられる。
        let shown = err.to_string();
        assert!(shown.contains("codex login"), "衝突の相手を名指ししていない: {shown}");
        assert!(shown.contains(&CALLBACK_PORT.to_string()), "ポート番号が文面に無い: {shown}");
    }
}
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-auth login`
Expected: コンパイルエラー。`authorize_url`、`parse_callback`、`wait_for_callback`、`bind_callback` が無い。

- [ ] **Step 3: 実装を書く**

`login.rs` の `use` の下、`#[cfg(test)]` の上へ挿入する。

```rust
/// ブラウザで開く認可 URL を組み立てる。
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let mut u = url::Url::parse(crate::ISSUER).expect("ISSUER が URL として壊れている");
    u.set_path("/oauth/authorize");
    u.query_pairs_mut()
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    u.to_string()
}

/// HTTP のリクエスト行から `code` と `state` を取り出す。
///
/// `GET /auth/callback?code=…&state=… HTTP/1.1` の 2 つ目の空白区切りが
/// 対象。相対パスのままでは `Url::parse` が使えないので、任意のベースへ
/// 接いでから query を読む。ベースは解釈のためだけに使い、外へは出ない。
pub fn parse_callback(request_line: &str) -> Result<(String, String), AuthError> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| AuthError::Decode(format!("リクエスト行を読めない: {request_line}")))?;

    let parsed = url::Url::parse("http://localhost")
        .expect("ベースが壊れている")
        .join(target)
        .map_err(|e| AuthError::Decode(format!("コールバックの URL を読めない: {e}")))?;

    let q: std::collections::HashMap<String, String> =
        parsed.query_pairs().into_owned().collect();

    if let Some(e) = q.get("error") {
        let desc = q.get("error_description").map(String::as_str).unwrap_or("");
        return Err(AuthError::Denied(format!("{e} {desc}").trim().to_string()));
    }

    let code = q
        .get("code")
        .cloned()
        .ok_or_else(|| AuthError::Decode("コールバックに code が無い".into()))?;
    let state = q.get("state").cloned().unwrap_or_default();
    Ok((code, state))
}

/// 登録済みのポートを掴む。塞がっていたら衝突を名指しする。
pub async fn bind_callback() -> Result<TcpListener, AuthError> {
    TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| AuthError::PortInUse(e.to_string()))
}

/// コールバックを一度だけ受け、`code` を返す。
///
/// 待ち続けないよう `timeout` で切る。ブラウザには短い本文を返す。空の
/// 応答だと、利用者は成功したのか分からないまま端末へ戻ることになる。
pub async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    let accepted = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| AuthError::Denied("コールバックを待ち切れなかった".into()))?;
    let (mut stream, _) = accepted?;

    // リクエスト行だけ読めれば足りる。ヘッダの終端まで待つ必要は無い。
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).await?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let request_line = head.lines().next().unwrap_or_default().to_string();

    let result = parse_callback(&request_line).and_then(|(code, state)| {
        if state != expected_state {
            Err(AuthError::Denied(
                "state が一致しない。別の認可の応答を受け取った可能性がある".into(),
            ))
        } else {
            Ok(code)
        }
    });

    let (status, body) = match &result {
        Ok(_) => ("200 OK", "polaris のログインが完了しました。端末に戻ってください。"),
        Err(_) => ("400 Bad Request", "polaris のログインに失敗しました。端末を確認してください。"),
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;

    result
}

/// ブラウザを開く。開けなくても致命ではない。URL を出して続ける。
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    let _ = std::process::Command::new(program).arg(url).spawn();
}

/// login 一式。bind してからブラウザを開く。順序が逆だと、利用者が
/// 認可を終えた時点でこちらがまだ待ち受けておらず、取りこぼす。
pub async fn run(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let p = pkce::generate()?;
    let state = pkce::random_urlsafe(16)?;

    let listener = bind_callback().await?;
    let url = authorize_url(&p.challenge, &state);
    eprintln!("ブラウザで認可してください: {url}");
    open_browser(&url);

    let code = wait_for_callback(listener, &state, Duration::from_secs(300)).await?;
    let creds = token::exchange_code(issuer, &code, &p.verifier).await?;
    store::save_to(store_path, &creds)?;
    Ok(creds)
}
```

`lib.rs` へ `pub mod login;` を足す。

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: 30 passed, 0 failed。

- [ ] **Step 5: 変異で確かめる**

1. `state != expected_state` の分岐を消す → `a_state_mismatch_is_rejected` が落ち、`a_matching_state_yields_the_code` は通ったまま
2. `wait_for_callback` を常に `Err(Denied)` にする → `a_matching_state_yields_the_code` が落ちる（対の肯定側が効いていることの確認）
3. `bind_callback` の `map_err` を消して `?` にする → `a_busy_callback_port_names_the_collision` が落ちる
4. `run` の中で `bind_callback` を `open_browser` の後に移す → どのテストも落ちない。**想定どおりである。** 順序は単体テストで固定できていないので、報告にそう書く

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-auth
git commit -m "feat(auth): receive the OAuth callback on the registered port"
```

---

### Task 5: `ensure_fresh` と logout

**Files:**
- Modify: `crates/polaris-auth/src/lib.rs`

**Interfaces:**
- Consumes: `store`、`token`、`Credentials`、`AuthError`
- Produces: `polaris_auth::EXPIRY_MARGIN_SECS: u64`、`polaris_auth::needs_refresh(c: &Credentials, now: u64) -> bool`、`polaris_auth::ensure_fresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError>`、`polaris_auth::force_refresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError>`、`polaris_auth::logout(store_path: &Path) -> Result<bool, AuthError>`

- [ ] **Step 1: 失敗するテストを書く**

`lib.rs` の末尾へ追加する。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn creds(expires_at: Option<u64>) -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acct".into(),
            expires_at,
        }
    }

    /// 期限が不明なら毎回更新する。楽観的に使い回すと、失効した
    /// トークンでの 401 が通常経路になり、認証の問題がモデルの問題に
    /// 見える。
    #[test]
    fn an_unknown_expiry_always_needs_refresh() {
        assert!(needs_refresh(&creds(None), 1_000));
    }

    /// 余裕の外側では更新しない。ここが常に真だと、毎ターン更新が走る。
    #[test]
    fn a_token_well_before_expiry_is_left_alone() {
        let now = 1_000_000;
        let c = creds(Some(now + EXPIRY_MARGIN_SECS + 10));
        assert!(!needs_refresh(&c, now), "余裕があるのに更新しようとしている");
    }

    /// 余裕の内側では更新する。上の対。
    #[test]
    fn a_token_inside_the_margin_needs_refresh() {
        let now = 1_000_000;
        let c = creds(Some(now + EXPIRY_MARGIN_SECS - 10));
        assert!(needs_refresh(&c, now), "期限が近いのに更新しない");
    }

    #[test]
    fn an_expired_token_needs_refresh() {
        let now = 1_000_000;
        assert!(needs_refresh(&creds(Some(now - 1)), now));
    }

    #[tokio::test]
    async fn ensure_fresh_on_an_empty_store_says_not_logged_in() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = ensure_fresh("http://unused.invalid", &dir.path().join("auth.json"))
            .await
            .expect_err("ログインしていないので失敗すべき");
        assert!(
            matches!(err, AuthError::NotLoggedIn),
            "NotLoggedIn 以外になっている: {err:?}"
        );
    }

    /// 余裕のあるトークンは、ネットワークへ出ずにそのまま返る。issuer に
    /// 到達不能な URL を渡しているので、更新しようとすれば失敗する。
    #[tokio::test]
    async fn ensure_fresh_returns_a_valid_token_without_touching_the_network() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        store::save_to(&p, &creds(Some(now + EXPIRY_MARGIN_SECS + 3600))).expect("保存");

        let got = ensure_fresh("http://127.0.0.1:1/unreachable", &p)
            .await
            .expect("更新せずに返るべき");
        assert_eq!(got.access_token, "at");
    }

    #[tokio::test]
    async fn logout_removes_the_store_and_reports_it() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = dir.path().join("auth.json");
        store::save_to(&p, &creds(None)).expect("保存");
        assert!(logout(&p).expect("削除できる"));
        assert!(!logout(&p).expect("2 回目も失敗しない"));
    }
}
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: コンパイルエラー。`EXPIRY_MARGIN_SECS`、`needs_refresh`、`ensure_fresh`、`logout` が無い。

- [ ] **Step 3: 実装を書く**

`lib.rs` の `Credentials` の下、`#[cfg(test)]` の上へ挿入する。

```rust
/// 期限までこの秒数を切ったら更新する。1 ターンの往復が数十秒に及ぶ
/// ことがあるため、要求の途中で失効しない程度の余裕を取る。
pub const EXPIRY_MARGIN_SECS: u64 = 300;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 更新が要るか。期限が不明なら要る。
pub fn needs_refresh(c: &Credentials, now: u64) -> bool {
    match c.expires_at {
        None => true,
        Some(exp) => exp <= now.saturating_add(EXPIRY_MARGIN_SECS),
    }
}

/// 保管された資格情報を読み、必要なら更新して返す。
pub async fn ensure_fresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let c = store::load_from(store_path)?.ok_or(AuthError::NotLoggedIn)?;
    if !needs_refresh(&c, now_secs()) {
        return Ok(c);
    }
    let fresh = token::refresh(issuer, &c.refresh_token).await?;
    store::save_to(store_path, &fresh)?;
    Ok(fresh)
}

/// 期限に関わらず更新する。401 を受けたあとの再試行で使う。
pub async fn force_refresh(issuer: &str, store_path: &Path) -> Result<Credentials, AuthError> {
    let c = store::load_from(store_path)?.ok_or(AuthError::NotLoggedIn)?;
    let fresh = token::refresh(issuer, &c.refresh_token).await?;
    store::save_to(store_path, &fresh)?;
    Ok(fresh)
}

/// 保管を消す。戻り値は「実際にあったか」。
pub fn logout(store_path: &Path) -> Result<bool, AuthError> {
    store::delete_at(store_path)
}
```

冒頭の `use` に `use std::path::Path;` を足す。

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-auth`
Expected: 37 passed, 0 failed。

Run: `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all -- --check`
Expected: どちらも clean。

- [ ] **Step 5: 変異で確かめる**

1. `needs_refresh` の `None => true` を `None => false` にする → `an_unknown_expiry_always_needs_refresh` が落ちる
2. `needs_refresh` を常に `true` にする → `a_token_well_before_expiry_is_left_alone` と `ensure_fresh_returns_a_valid_token_without_touching_the_network` が落ちる
3. `ensure_fresh` の `ok_or(AuthError::NotLoggedIn)` を `unwrap_or_default()` 相当にする（`Credentials::default()` は無いので、`ok_or(AuthError::Http("x".into()))` に変えて種類だけ壊す）→ `ensure_fresh_on_an_empty_store_says_not_logged_in` が落ちる

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-auth
git commit -m "feat(auth): refresh within a margin and report a missing login as such"
```

---

### Task 6: `ProviderError::Auth` と `TokenSource`

**Files:**
- Modify: `crates/polaris-provider/src/lib.rs`

**Interfaces:**
- Consumes: なし
- Produces: `polaris_provider::ProviderError::Auth(String)`、`polaris_provider::Token { access_token: String, account_id: String }`、`polaris_provider::TokenSource`（`async fn token()` と `async fn refreshed()`）

追加のみで、既存の型は変えない。既存の `OpenAiProvider` は `Auth` を返さない。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-provider/src/lib.rs` の `mod tests` へ追加する。

```rust
    struct CannedTokens {
        first: String,
        second: String,
    }

    #[async_trait::async_trait]
    impl TokenSource for CannedTokens {
        async fn token(&self) -> Result<Token, ProviderError> {
            Ok(Token {
                access_token: self.first.clone(),
                account_id: "acct".into(),
            })
        }
        async fn refreshed(&self) -> Result<Token, ProviderError> {
            Ok(Token {
                access_token: self.second.clone(),
                account_id: "acct".into(),
            })
        }
    }

    #[tokio::test]
    async fn token_source_is_object_safe_and_distinguishes_refresh() {
        let s: Box<dyn TokenSource> = Box::new(CannedTokens {
            first: "a".into(),
            second: "b".into(),
        });
        assert_eq!(s.token().await.expect("取れる").access_token, "a");
        assert_eq!(s.refreshed().await.expect("取れる").access_token, "b");
    }

    /// 認証の失敗はモデルの失敗と別の種類である。文面ではなく型で
    /// 区別できること。文面での判別は、メッセージを直した瞬間に壊れる。
    #[test]
    fn an_auth_error_is_its_own_variant() {
        let e = ProviderError::Auth("ログインしていない".into());
        assert!(matches!(e, ProviderError::Auth(_)));
        assert!(
            !matches!(ProviderError::Http("x".into()), ProviderError::Auth(_)),
            "Http が Auth と一致してしまう"
        );
    }
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-provider`
Expected: コンパイルエラー。`TokenSource`、`Token`、`ProviderError::Auth` が無い。

- [ ] **Step 3: 実装を書く**

`lib.rs` の `ProviderError` へ 1 つ variant を足す。

```rust
    #[error("認証: {0}")]
    Auth(String),
```

`Provider` トレイトの定義の下へ次を足す。

```rust
/// 1 回の要求に使う資格情報。プロバイダはこれ以上のことを知らない。
#[derive(Debug, Clone)]
pub struct Token {
    pub access_token: String,
    pub account_id: String,
}

/// トークンの供給元。`token()` は「いま使えるもの」を返し、`refreshed()`
/// は期限に関わらず更新したものを返す。401 を受けたあとの再試行が後者を
/// 使う。
///
/// このトレイトを `polaris-provider` に置き、実装を `polaris-cli` に
/// 置くことで、`polaris-auth` がこのクレートへ依存せずに済む。同時に、
/// プロバイダのテストが OAuth もファイルもブラウザも要らなくなる。
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<Token, ProviderError>;
    async fn refreshed(&self) -> Result<Token, ProviderError>;
}
```

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-provider`
Expected: 既存の本数 + 2 が passed、0 failed。

- [ ] **Step 5: コミット**

```bash
git add crates/polaris-provider/src/lib.rs
git commit -m "feat(provider): add an Auth error variant and the TokenSource trait"
```

---

### Task 7: 要求の組み立て

**Files:**
- Create: `crates/polaris-provider/src/codex.rs`
- Modify: `crates/polaris-provider/src/lib.rs`（`pub mod codex;`）

**Interfaces:**
- Consumes: `CompletionRequest`、`Message`、`Role`、`ToolCall`、`polaris_tools::ToolSpec`
- Produces: `polaris_provider::codex::input_items(messages: &[Message]) -> Vec<serde_json::Value>`、`polaris_provider::codex::tool_wire_shape(tools: &[ToolSpec]) -> Vec<serde_json::Value>`、`polaris_provider::codex::build_body(model: &str, req: &CompletionRequest) -> serde_json::Value`、`polaris_provider::codex::ENDPOINT_BASE: &str`、`polaris_provider::codex::DEFAULT_MODEL: &str`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-provider/src/codex.rs` を新規作成する。

```rust
//! ChatGPT のサブスクリプション認証で Responses API を話すプロバイダ。
//!
//! `/chat/completions` とは形が違う。ツール定義は入れ子ではなく平坦で、
//! 履歴は `messages` ではなく `input` の要素列であり、`arguments` は
//! JSON ではなく JSON を収めた文字列である。`openai.rs` と関数を共有
//! しないのは、片方を直したときにもう片方が黙って壊れる形にしないため。

use serde_json::Value;

use crate::{CompletionRequest, Message, Role};

/// 要求先。`store` を使わないので、この 1 本しか叩かない。
pub const ENDPOINT_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// `POLARIS_MODEL` を省いたときの既定。
pub const DEFAULT_MODEL: &str = "gpt-5.3-codex";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;

    #[test]
    fn a_user_message_becomes_an_input_text_item() {
        let items = input_items(&[Message::user("こんにちは")]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "こんにちは");
    }

    #[test]
    fn an_assistant_message_becomes_an_output_text_item() {
        let items = input_items(&[Message::assistant("はい")]);
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
        assert_eq!(items[0]["content"][0]["text"], "はい");
    }

    /// ツール呼び出しは `function_call` になり、`arguments` は JSON では
    /// なく JSON を収めた文字列である。ここを Value のまま送ると、
    /// サーバは型が違うと言って 400 を返す。
    #[test]
    fn a_tool_call_becomes_a_function_call_with_stringified_arguments() {
        let items = input_items(&[Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "Cargo.toml" }),
            }],
        )]);
        assert_eq!(items.len(), 1, "本文が空のときに空の message を足している");
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["name"], "read");
        let raw = items[0]["arguments"].as_str().expect("arguments が文字列でない");
        let parsed: Value = serde_json::from_str(raw).expect("arguments が JSON でない");
        assert_eq!(parsed["path"], "Cargo.toml");
    }

    /// 本文とツール呼び出しの両方を持つターンは、message を先に、
    /// function_call を後に並べる。順序が逆だと、モデルは自分の発話より
    /// 先に自分の呼び出しを見ることになる。
    #[test]
    fn a_turn_with_both_text_and_calls_emits_the_message_first() {
        let items = input_items(&[Message::assistant_with_tool_calls(
            "読みます",
            vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
        )]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn a_tool_result_becomes_a_function_call_output() {
        let items = input_items(&[Message::tool_result("call_1", "42 行")]);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["output"], "42 行");
    }

    /// Responses のツール定義は平坦である。`/chat/completions` の
    /// `{"type":"function","function":{…}}` を送ると受け付けられない。
    #[test]
    fn tool_definitions_are_flat_not_nested() {
        let specs = polaris_tools::all_specs();
        let wire = tool_wire_shape(&specs);
        assert_eq!(wire.len(), specs.len());
        for (w, s) in wire.iter().zip(specs.iter()) {
            assert_eq!(w["type"], "function");
            assert_eq!(w["name"], s.name, "name が平坦に置かれていない");
            assert!(w.get("function").is_none(), "入れ子の function が残っている");
            assert!(w["description"].is_string());
            assert_eq!(w["parameters"], s.parameters);
        }
    }

    #[test]
    fn the_body_carries_instructions_and_never_stores_state() {
        let req = CompletionRequest {
            system: "システム".into(),
            messages: vec![Message::user("やって")],
            tools: polaris_tools::all_specs(),
        };
        let body = build_body("gpt-5.3-codex", &req);

        assert_eq!(body["model"], "gpt-5.3-codex");
        assert_eq!(body["instructions"], "システム");
        assert_eq!(body["store"], false, "サーバに会話状態を持たせている");
        assert_eq!(body["stream"], true);
        assert!(body.get("previous_response_id").is_none(), "会話の再利用を使っている");
        assert_eq!(body["input"].as_array().expect("input が配列でない").len(), 1);
        assert_eq!(
            body["tools"].as_array().expect("tools が配列でない").len(),
            req.tools.len()
        );
    }

    /// ツールが 1 本も無いときは `tools` を送らない。空配列を送ると、
    /// 「ツールを使うな」の指示と受け取られうる。
    #[test]
    fn an_empty_tool_list_is_omitted_rather_than_sent_empty() {
        let req = CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("x")],
            tools: vec![],
        };
        let body = build_body("m", &req);
        assert!(body.get("tools").is_none(), "空の tools を送っている");
    }
}
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-provider codex`
Expected: コンパイルエラー。`input_items`、`tool_wire_shape`、`build_body` が無い。

`crates/polaris-provider/Cargo.toml` の `[dev-dependencies]` に `polaris-tools` は不要である（既に `[dependencies]` にある）。

- [ ] **Step 3: 実装を書く**

`codex.rs` の定数の下、`#[cfg(test)]` の上へ挿入する。

```rust
/// 履歴を Responses の `input` 要素列へ変換する。
pub fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::User => out.push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": m.content }],
            })),
            Role::Assistant => {
                // 本文が空でツール呼び出しだけのターンは珍しくない。
                // 空の message を足すと、内容の無い発話が履歴に増える。
                if !m.content.is_empty() {
                    out.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": m.content }],
                    }));
                }
                for c in &m.tool_calls {
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.name,
                        // JSON そのものではなく、JSON を収めた文字列。
                        "arguments": c.arguments.to_string(),
                    }));
                }
            }
            Role::Tool => out.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.content,
            })),
        }
    }
    out
}

/// ツール定義を Responses の平坦な形へ変換する。
pub fn tool_wire_shape(tools: &[polaris_tools::ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect()
}

/// 要求本文を組み立てる。
pub fn build_body(model: &str, req: &CompletionRequest) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "instructions": req.system,
        "input": input_items(&req.messages),
        // サーバに会話状態を持たせない。毎ターン全文を送る。送るものと
        // 測るものが一致し、接頭辞も動かない。
        "store": false,
        "stream": true,
    });
    let tools = tool_wire_shape(&req.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}
```

`lib.rs` の `pub mod openai;` の上へ `pub mod codex;` を足す（アルファベット順）。

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-provider`
Expected: 8 本増えて passed、0 failed。

- [ ] **Step 5: 変異で確かめる**

1. `"arguments": c.arguments.to_string()` を `"arguments": c.arguments.clone()` にする → `a_tool_call_becomes_a_function_call_with_stringified_arguments` が落ちる
2. `tool_wire_shape` を `openai::tool_wire_shape` と同じ入れ子の形にする → `tool_definitions_are_flat_not_nested` が落ちる
3. `"store": false` を `true` にする → `the_body_carries_instructions_and_never_stores_state` が落ちる
4. `if !m.content.is_empty()` の条件を消す → `a_tool_call_becomes_a_function_call_with_stringified_arguments` が落ちる（要素数が 2 になる）

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-provider
git commit -m "feat(provider): build Responses API request bodies for codex"
```

---

### Task 8: SSE の畳み込み

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: `sse::SseDecoder`、`CompletionResponse`、`ToolCall`、`ProviderError`
- Produces: `polaris_provider::codex::Folder`（`new()`、`push(&mut self, bytes: &[u8]) -> Result<(), ProviderError>`、`finish(self) -> Result<CompletionResponse, ProviderError>`）

畳み込みを HTTP から切り離した構造体にするのは、イベント意味論だけを純粋に試験するためである。ここが混ざっていると、落ちたテストがネットワークの問題かイベント解釈の問題かを指さない。

- [ ] **Step 1: 失敗するテストを書く**

`codex.rs` の `mod tests` へ追加する。

```rust
    fn frame(kind: &str, extra: Value) -> Vec<u8> {
        let mut v = serde_json::json!({ "type": kind });
        if let Some(o) = extra.as_object() {
            for (k, val) in o {
                v[k] = val.clone();
            }
        }
        format!("data: {v}\n\n").into_bytes()
    }

    fn message_item(text: &str) -> Value {
        serde_json::json!({
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }]
            }
        })
    }

    fn function_call_item(id: &str, name: &str, args: &str) -> Value {
        serde_json::json!({
            "item": { "type": "function_call", "call_id": id, "name": name, "arguments": args }
        })
    }

    #[test]
    fn a_text_only_stream_folds_into_text() {
        let mut f = Folder::new();
        f.push(&frame("response.output_item.done", message_item("42 行"))).expect("押せる");
        f.push(&frame("response.completed", serde_json::json!({}))).expect("押せる");
        let r = f.finish().expect("完了しているべき");
        assert_eq!(r.text, "42 行");
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn a_function_call_item_becomes_a_tool_call() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_item.done",
            function_call_item("call_9", "read", r#"{"path":"a.txt"}"#),
        ))
        .expect("押せる");
        f.push(&frame("response.completed", serde_json::json!({}))).expect("押せる");
        let r = f.finish().expect("完了しているべき");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "call_9");
        assert_eq!(r.tool_calls[0].name, "read");
        assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    }

    /// フレームがどこで分割されても結果が変わらない。分割耐性は
    /// `sse.rs` の責任だが、この経路が実際にそれを通っていることは
    /// 別に確かめる。通っていなければ、ここで結合をやり直している。
    #[test]
    fn a_stream_split_mid_frame_folds_the_same_way() {
        let whole: Vec<u8> = frame("response.output_item.done", message_item("分割耐性"))
            .into_iter()
            .chain(frame("response.completed", serde_json::json!({})))
            .collect();

        for cut in 1..whole.len() {
            let mut f = Folder::new();
            f.push(&whole[..cut]).expect("押せる");
            f.push(&whole[cut..]).expect("押せる");
            let r = f.finish().expect("完了しているべき");
            assert_eq!(r.text, "分割耐性", "{cut} バイト目で分割したときに壊れた");
        }
    }

    /// `response.completed` を見ないまま終わった応答を、正常終了として
    /// 返してはならない。エージェントループはツール呼び出しが無いことを
    /// 「完了」と読むため、黙って空の最終回答を返す。
    #[test]
    fn a_stream_that_never_completes_is_an_error() {
        let mut f = Folder::new();
        f.push(&frame("response.output_item.done", message_item("途中"))).expect("押せる");
        let err = f.finish().expect_err("完了していないので失敗すべき");
        assert!(matches!(err, ProviderError::Decode(_)), "Decode 以外: {err:?}");
    }

    /// 中身が空でも、完了していれば成功である。空文字列と不在を
    /// 取り違えない。上のテストの対であり、これが無いと「常に失敗」の
    /// 実装が通る。
    #[test]
    fn an_empty_but_completed_stream_is_a_success() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({}))).expect("押せる");
        let r = f.finish().expect("完了しているので成功すべき");
        assert!(r.text.is_empty());
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn a_failed_response_carries_its_message() {
        let mut f = Folder::new();
        let err = f
            .push(&frame(
                "response.failed",
                serde_json::json!({ "response": { "error": { "message": "model overloaded" } } }),
            ))
            .expect_err("失敗すべき");
        let ProviderError::Http(msg) = err else {
            panic!("Http 以外: {err:?}");
        };
        assert!(msg.contains("model overloaded"), "理由が文面に無い: {msg}");
    }

    #[test]
    fn a_cancelled_response_is_an_error() {
        let mut f = Folder::new();
        let err = f
            .push(&frame("response.cancelled", serde_json::json!({})))
            .expect_err("失敗すべき");
        assert!(matches!(err, ProviderError::Http(_)), "Http 以外: {err:?}");
    }

    /// 差分イベントは読み飛ばす。拾って二重に積むと本文が重複する。
    #[test]
    fn delta_events_are_ignored() {
        let mut f = Folder::new();
        f.push(&frame(
            "response.output_text.delta",
            serde_json::json!({ "delta": "重複" }),
        ))
        .expect("押せる");
        f.push(&frame("response.output_item.done", message_item("重複"))).expect("押せる");
        f.push(&frame("response.completed", serde_json::json!({}))).expect("押せる");
        let r = f.finish().expect("完了");
        assert_eq!(r.text, "重複", "差分を拾って二重に積んでいる");
    }

    /// `arguments` が JSON として壊れている呼び出しは、ディスパッチャへ
    /// 渡す前にここで止める。渡すと「未知の引数」に見え、原因が遡れない。
    #[test]
    fn a_function_call_with_broken_arguments_is_a_decode_error() {
        let mut f = Folder::new();
        let err = f
            .push(&frame(
                "response.output_item.done",
                function_call_item("c", "read", "{ not json"),
            ))
            .expect_err("失敗すべき");
        assert!(matches!(err, ProviderError::Decode(_)), "Decode 以外: {err:?}");
    }

    /// `[DONE]` という番兵は JSON ではない。解釈しようとして落ちない。
    #[test]
    fn the_done_sentinel_is_not_parsed_as_json() {
        let mut f = Folder::new();
        f.push(&frame("response.completed", serde_json::json!({}))).expect("押せる");
        f.push(b"data: [DONE]\n\n").expect("番兵で落ちてはいけない");
        let r = f.finish().expect("完了");
        assert!(r.text.is_empty());
    }
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-provider codex`
Expected: コンパイルエラー。`Folder` が無い。

- [ ] **Step 3: 実装を書く**

`codex.rs` の `build_body` の下、`#[cfg(test)]` の上へ挿入する。ファイル冒頭の `use` を次にする。

```rust
use serde_json::Value;

use crate::{CompletionRequest, CompletionResponse, Message, ProviderError, Role, ToolCall, sse};
```

```rust
/// SSE の意味論。フレーミングは `sse::SseDecoder` に任せ、ここは
/// イベントの解釈だけを持つ。HTTP から切り離してあるので、ネットワーク
/// 無しで試験できる。
pub struct Folder {
    decoder: sse::SseDecoder,
    text: String,
    tool_calls: Vec<ToolCall>,
    completed: bool,
}

impl Default for Folder {
    fn default() -> Self {
        Self::new()
    }
}

impl Folder {
    pub fn new() -> Self {
        Self {
            decoder: sse::SseDecoder::new(),
            text: String::new(),
            tool_calls: Vec::new(),
            completed: false,
        }
    }

    /// 受け取ったバイト片を押し込む。完成したイベントだけを解釈する。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProviderError> {
        for ev in self.decoder.push(bytes) {
            let data = ev.data.trim();
            // 番兵。JSON ではないので解釈しない。
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let v: Value = serde_json::from_str(data)
                .map_err(|e| ProviderError::Decode(format!("SSE の data が JSON でない: {e}")))?;

            match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                "response.output_item.done" => self.take_item(&v)?,
                "response.completed" => self.completed = true,
                "response.failed" => {
                    let msg = v
                        .pointer("/response/error/message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("理由が示されていない");
                    return Err(ProviderError::Http(format!("応答が失敗した: {msg}")));
                }
                "response.cancelled" => {
                    return Err(ProviderError::Http("応答が取り消された".into()));
                }
                // 差分やその他は読み飛ばす。確定したアイテムだけを見れば
                // 同じ結果になり、再結合の失敗という壊れ方を持ち込まない。
                _ => {}
            }
        }
        Ok(())
    }

    fn take_item(&mut self, v: &Value) -> Result<(), ProviderError> {
        let Some(item) = v.get("item") else {
            return Ok(());
        };
        match item.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "message" => {
                if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            self.text.push_str(t);
                        }
                    }
                }
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::Decode("function_call に call_id が無い".into()))?;
                let name = item
                    .get("name")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ProviderError::Decode("function_call に name が無い".into()))?;
                let raw = item
                    .get("arguments")
                    .and_then(|s| s.as_str())
                    .unwrap_or("{}");
                let arguments: Value = serde_json::from_str(raw).map_err(|e| {
                    ProviderError::Decode(format!("function_call の arguments が JSON でない: {e}"))
                })?;
                self.tool_calls.push(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// 畳んだ結果を返す。完了を見ていなければ硬い失敗にする。
    pub fn finish(self) -> Result<CompletionResponse, ProviderError> {
        if !self.completed {
            return Err(ProviderError::Decode(
                "response.completed を見ないままストリームが終わった".into(),
            ));
        }
        Ok(CompletionResponse {
            text: self.text,
            tool_calls: self.tool_calls,
        })
    }
}
```

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-provider`
Expected: 10 本増えて passed、0 failed。

- [ ] **Step 5: 変異で確かめる**

1. `finish` の `if !self.completed` を消す → `a_stream_that_never_completes_is_an_error` が落ち、`an_empty_but_completed_stream_is_a_success` は通ったまま
2. `finish` を常に `Err` にする → `an_empty_but_completed_stream_is_a_success` が落ちる（対の肯定側の確認）
3. `"response.output_text.delta"` も本文へ積むように足す → `delta_events_are_ignored` が落ちる
4. `arguments` の `from_str` を `unwrap_or(Value::Null)` にする → `a_function_call_with_broken_arguments_is_a_decode_error` が落ちる
5. `Folder::push` の中で `self.decoder` を使わず、`bytes` を毎回新しいデコーダに通す → `a_stream_split_mid_frame_folds_the_same_way` が落ちる

- [ ] **Step 6: コミット**

```bash
git add crates/polaris-provider
git commit -m "feat(provider): fold Responses SSE events into a completion"
```

---

### Task 9: `CodexProvider` の HTTP 経路

**Files:**
- Modify: `crates/polaris-provider/src/codex.rs`

**Interfaces:**
- Consumes: `Folder`、`build_body`、`TokenSource`、`Token`、`Provider`、`ProviderError`
- Produces: `polaris_provider::codex::CodexProvider::new(base: String, model: String, tokens: std::sync::Arc<dyn TokenSource>) -> Self`、`CodexProvider::with_idle_timeout(base, model, tokens, idle: Duration) -> Self`、`impl Provider for CodexProvider`

`base` を引数で受けるのは、テストが `wiremock` を指せるようにするためである。本番は `ENDPOINT_BASE` を渡す。

- [ ] **Step 1: 失敗するテストを書く**

`codex.rs` の `mod tests` へ追加する。

```rust
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct Tokens {
        calls: AtomicUsize,
        refreshes: AtomicUsize,
    }

    impl Tokens {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                refreshes: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::TokenSource for Tokens {
        async fn token(&self) -> Result<crate::Token, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::Token {
                access_token: "first".into(),
                account_id: "acct-1".into(),
            })
        }
        async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(crate::Token {
                access_token: "second".into(),
                account_id: "acct-1".into(),
            })
        }
    }

    fn req() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            messages: vec![Message::user("やって")],
            tools: vec![],
        }
    }

    fn sse_body(frames: &[Vec<u8>]) -> String {
        frames
            .iter()
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect()
    }

    /// 送出したヘッダと本文が仕様どおりであること。ここが違うと、
    /// 応答の解釈がいくら正しくてもサーバは相手にしない。
    #[tokio::test]
    async fn the_request_carries_the_bearer_and_the_account_id() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer first"))
            .and(header("chatgpt-account-id", "acct-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame("response.output_item.done", message_item("ok")),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        let r = p.complete(req()).await.expect("成功すべき");
        assert_eq!(r.text, "ok");
    }

    /// 401 を受けたら更新して 1 回だけ再試行し、成功する。
    #[tokio::test]
    async fn a_401_is_retried_once_with_a_refreshed_token() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer first"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer second"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[
                frame("response.output_item.done", message_item("再試行で成功")),
                frame("response.completed", serde_json::json!({})),
            ])))
            .mount(&s)
            .await;

        let t = Tokens::new();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone());
        let r = p.complete(req()).await.expect("再試行で成功すべき");
        assert_eq!(r.text, "再試行で成功");
        assert_eq!(t.refreshes.load(Ordering::SeqCst), 1, "更新の回数が 1 でない");
    }

    /// 401 が 2 回続いたら諦める。無限に再試行しない。種類は Auth で
    /// あり、Http ではない。
    #[tokio::test]
    async fn a_second_401_gives_up_as_an_auth_error() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;

        let t = Tokens::new();
        let p = CodexProvider::new(s.uri(), "m".into(), t.clone());
        let err = p.complete(req()).await.expect_err("失敗すべき");
        assert!(matches!(err, ProviderError::Auth(_)), "Auth 以外: {err:?}");
        assert_eq!(t.refreshes.load(Ordering::SeqCst), 1, "再試行が 1 回で止まっていない");
    }

    /// 429 はリセット情報を文面へ含める。掴めない拒否は同じ失敗を
    /// 繰り返させる。
    #[tokio::test]
    async fn a_429_surfaces_the_retry_hint() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "37")
                    .set_body_string("rate limited"),
            )
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        let err = p.complete(req()).await.expect_err("失敗すべき");
        let ProviderError::Http(msg) = err else {
            panic!("Http 以外: {err:?}");
        };
        assert!(msg.contains("37"), "retry-after が文面に無い: {msg}");
    }

    /// 完了を見ないまま切れたストリームは失敗である。HTTP は 200 なので、
    /// ここを通すと空の最終回答が返る。
    #[tokio::test]
    async fn a_truncated_stream_is_an_error_even_on_200() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse_body(&[frame(
                "response.output_item.done",
                message_item("途中で切れた"),
            )])))
            .mount(&s)
            .await;

        let p = CodexProvider::new(s.uri(), "m".into(), Tokens::new());
        let err = p.complete(req()).await.expect_err("失敗すべき");
        assert!(matches!(err, ProviderError::Decode(_)), "Decode 以外: {err:?}");
    }

    /// トークンが取れない時点で Auth である。ネットワークへ出ない。
    #[tokio::test]
    async fn a_token_source_failure_is_an_auth_error() {
        struct NoTokens;
        #[async_trait::async_trait]
        impl crate::TokenSource for NoTokens {
            async fn token(&self) -> Result<crate::Token, ProviderError> {
                Err(ProviderError::Auth("ログインしていない".into()))
            }
            async fn refreshed(&self) -> Result<crate::Token, ProviderError> {
                Err(ProviderError::Auth("ログインしていない".into()))
            }
        }

        let p = CodexProvider::new(
            "http://127.0.0.1:1/unreachable".into(),
            "m".into(),
            Arc::new(NoTokens),
        );
        let err = p.complete(req()).await.expect_err("失敗すべき");
        assert!(matches!(err, ProviderError::Auth(_)), "Auth 以外: {err:?}");
    }
```

`crates/polaris-provider/Cargo.toml` の `[dev-dependencies]` に `async-trait` を足す（テスト内でトレイトを実装するため）。

```toml
[dev-dependencies]
tokio = { workspace = true }
wiremock = { workspace = true }
async-trait = { workspace = true }
```

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-provider codex`
Expected: コンパイルエラー。`CodexProvider` が無い。

- [ ] **Step 3: 実装を書く**

`codex.rs` の `Folder` の下、`#[cfg(test)]` の上へ挿入する。冒頭の `use` へ足す。

```rust
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
```

`futures-util` はワークスペースの依存に無いので、`Cargo.toml` の `[workspace.dependencies]` へ `futures-util = "0.3"` を足し、`crates/polaris-provider/Cargo.toml` の `[dependencies]` へ `futures-util = { workspace = true }` を足す。`reqwest` の `stream` feature は既に有効である。

```rust
/// 無通信がこの時間続いたら切る。応答全体で測ると正常な長考を打ち切る。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub struct CodexProvider {
    base: String,
    model: String,
    tokens: Arc<dyn crate::TokenSource>,
    client: reqwest::Client,
    idle: Duration,
}

impl CodexProvider {
    pub fn new(base: String, model: String, tokens: Arc<dyn crate::TokenSource>) -> Self {
        Self::with_idle_timeout(base, model, tokens, DEFAULT_IDLE_TIMEOUT)
    }

    pub fn with_idle_timeout(
        base: String,
        model: String,
        tokens: Arc<dyn crate::TokenSource>,
        idle: Duration,
    ) -> Self {
        Self {
            base,
            model,
            tokens,
            client: reqwest::Client::new(),
            idle,
        }
    }

    /// 1 回の要求を投げ、SSE を畳む。401 はここでは畳まず、そのまま
    /// 呼び出し側へ返して再試行の判断をさせる。
    async fn attempt(
        &self,
        token: &crate::Token,
        body: &Value,
    ) -> Result<CompletionResponse, ProviderError> {
        let resp = self
            .client
            .post(format!("{}/responses", self.base))
            .bearer_auth(&token.access_token)
            .header("chatgpt-account-id", &token.account_id)
            .header("accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // 呼び出し側が更新して再試行するかを決める。
            return Err(ProviderError::Auth(format!("status {status}")));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let hint = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("不明")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http(format!(
                "レート制限。retry-after: {hint} 秒。{body}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http(format!("status {status}: {body}")));
        }

        let mut folder = Folder::new();
        let mut stream = resp.bytes_stream();
        loop {
            // 無通信で測る。応答全体の長さは正常に伸びる。
            let next = tokio::time::timeout(self.idle, stream.next()).await;
            match next {
                Err(_) => {
                    return Err(ProviderError::Http(format!(
                        "{} 秒のあいだ応答が届かなかった",
                        self.idle.as_secs()
                    )));
                }
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    let bytes = chunk.map_err(|e| ProviderError::Http(e.to_string()))?;
                    folder.push(&bytes)?;
                }
            }
        }
        folder.finish()
    }
}

#[async_trait::async_trait]
impl crate::Provider for CodexProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let body = build_body(&self.model, &req);

        let token = self.tokens.token().await?;
        match self.attempt(&token, &body).await {
            Err(ProviderError::Auth(_)) => {
                // 1 回だけ。無限に再試行しない。
                let token = self.tokens.refreshed().await?;
                self.attempt(&token, &body).await.map_err(|e| match e {
                    ProviderError::Auth(_) => ProviderError::Auth(
                        "更新後も認証を拒否された。`polaris login` をやり直すこと".into(),
                    ),
                    other => other,
                })
            }
            other => other,
        }
    }
}
```

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p polaris-provider`
Expected: 6 本増えて passed、0 failed。

- [ ] **Step 5: 変異で確かめる**

1. `complete` の再試行の腕を消し、`attempt` の結果をそのまま返す → `a_401_is_retried_once_with_a_refreshed_token` が落ちる
2. 再試行を `loop` にして繰り返す → `a_second_401_gives_up_as_an_auth_error` が落ちる（更新回数が 1 でなくなる）
3. `.header("chatgpt-account-id", …)` を消す → `the_request_carries_the_bearer_and_the_account_id` が落ちる
4. 429 の腕を消して汎用の非成功へ落とす → `a_429_surfaces_the_retry_hint` が落ちる
5. `folder.finish()` を `Ok(CompletionResponse::default())` にする → `a_truncated_stream_is_an_error_even_on_200` と `the_request_carries_the_bearer_and_the_account_id` が落ちる

- [ ] **Step 6: コミット**

```bash
git add Cargo.toml Cargo.lock crates/polaris-provider
git commit -m "feat(provider): add CodexProvider with one refresh-and-retry on 401"
```

---

### Task 10: CLI への統合と README

**Files:**
- Modify: `crates/polaris-cli/Cargo.toml`（`polaris-auth`、`async-trait` を追加）
- Modify: `crates/polaris-cli/src/main.rs`
- Create: `crates/polaris-cli/tests/subcommands.rs`
- Modify: `README.md`

**Interfaces:**
- Consumes: `polaris_auth::{ensure_fresh, force_refresh, logout, login, store, ISSUER, AuthError}`、`polaris_provider::{Token, TokenSource, codex::{CodexProvider, ENDPOINT_BASE, DEFAULT_MODEL}}`
- Produces: なし（最終段）

**この作業の既知の罠.** 現行の `--prompt` は
`required_unless_present = "confined_apply"` である。ここへサブコマンドを足すと、
`polaris login` が「`--prompt` が無い」で弾かれる。clap の
`required_unless_present` は引数名しか見ず、サブコマンドの有無を見ないためである。
M2 の Task 8 では、同じ形で `--confined-apply` が到達不能になっていた。
**`prompt` を完全に任意にし、解析後に手で検証する。** 目で読んで正しく見えることは、
到達することの証拠にならない。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-cli/tests/subcommands.rs` を新規作成する。

```rust
//! サブコマンドが実際に到達することを固定する。
//!
//! M2 の Task 8 で、`--prompt` の必須検証によって `--confined-apply` が
//! 到達不能になっていた。同じ形の罠なので、引数の組み立てを目で読むのでは
//! なく、実バイナリを起動して確かめる。
//!
//! `HOME` を一時ディレクトリへ向けるので、実の `~/.polaris` にも
//! `~/.codex` にも触れない。

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_polaris")
}

/// `login` は `--prompt` 無しで解析を通る。`--help` で止めるので
/// ブラウザは開かず、ネットワークにも出ない。
#[test]
fn the_login_subcommand_is_reachable_without_a_prompt() {
    let out = Command::new(bin())
        .args(["login", "--help"])
        .output()
        .expect("起動できない");
    assert!(
        out.status.success(),
        "login --help が失敗した。stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `logout` は `--prompt` 無しで最後まで走る。ログインしていない状態でも
/// 失敗しない。
#[test]
fn the_logout_subcommand_runs_without_a_prompt() {
    let home = tempfile::tempdir().expect("一時ディレクトリ");
    let out = Command::new(bin())
        .arg("logout")
        .env("HOME", home.path())
        .output()
        .expect("起動できない");
    assert!(
        out.status.success(),
        "logout が失敗した。stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `logout` は自分の store だけを消す。`~/.codex/auth.json` には触れない。
#[test]
fn logout_never_touches_the_codex_store() {
    let home = tempfile::tempdir().expect("一時ディレクトリ");
    let codex = home.path().join(".codex");
    std::fs::create_dir_all(&codex).expect("作れない");
    let codex_auth = codex.join("auth.json");
    std::fs::write(&codex_auth, b"{\"sentinel\":true}").expect("書けない");

    let polaris_dir = home.path().join(".polaris");
    std::fs::create_dir_all(&polaris_dir).expect("作れない");
    let polaris_auth = polaris_dir.join("auth.json");
    std::fs::write(&polaris_auth, b"{\"access_token\":\"a\",\"refresh_token\":\"r\",\"account_id\":\"x\"}")
        .expect("書けない");

    let out = Command::new(bin())
        .arg("logout")
        .env("HOME", home.path())
        .output()
        .expect("起動できない");
    assert!(out.status.success(), "logout が失敗した");

    assert!(!polaris_auth.exists(), "自分の store を消していない");
    assert_eq!(
        std::fs::read(&codex_auth).expect("読めない"),
        b"{\"sentinel\":true}",
        "codex の store に触れている"
    );
}

/// 通常経路では `--prompt` が要る。任意にしたことで、指示なしの実行が
/// 黙って走り出してはいけない。上の 3 本の対であり、これが無いと
/// 「prompt を一切見ない」実装が通る。
#[test]
fn the_normal_path_still_requires_a_prompt() {
    let out = Command::new(bin()).output().expect("起動できない");
    assert!(!out.status.success(), "指示なしで成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("prompt"),
        "何が足りないかを言っていない: {stderr}"
    );
}

/// 受け入れ基準 5。`POLARIS_PROVIDER` を設定しない既定の実行が、これまで
/// どおり openai の経路へ入る。キーが無いことを openai の言葉で叱ることで、
/// codex の経路へ逸れていないことが分かる。
#[test]
fn the_default_provider_is_still_openai() {
    let out = Command::new(bin())
        .args(["-p", "x"])
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("起動できない");
    assert!(!out.status.success(), "キー無しで成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("POLARIS_API_KEY"),
        "openai の経路へ入っていない: {stderr}"
    );
}

/// 受け入れ基準 4。ログアウト状態で codex を指すと、`polaris login` を
/// 名指しするエラーが出る。HTTP エラーにはならない。ネットワークへ出る前に
/// 止まるので、実 API は叩かない。
#[test]
fn a_logged_out_codex_run_names_the_login_command() {
    let home = tempfile::tempdir().expect("一時ディレクトリ");
    let out = Command::new(bin())
        .args(["-p", "何行か"])
        .env("HOME", home.path())
        .env("POLARIS_PROVIDER", "codex")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("起動できない");
    assert!(!out.status.success(), "ログインしていないのに成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("polaris login"),
        "やるべきことを名指ししていない: {stderr}"
    );
    assert!(
        !stderr.contains("status "),
        "HTTP エラーとして出ている: {stderr}"
    );
}

/// 未知のプロバイダ名は起動時に落とす。実行してから「モデルが応答しない」
/// で気付くのでは遅い。
#[test]
fn an_unknown_provider_name_fails_fast() {
    let out = Command::new(bin())
        .args(["-p", "x"])
        .env("POLARIS_PROVIDER", "nonesuch")
        .env("POLARIS_API_KEY", "dummy")
        .output()
        .expect("起動できない");
    assert!(!out.status.success(), "未知のプロバイダで成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nonesuch"), "名前を出していない: {stderr}");
}
```

`crates/polaris-cli/Cargo.toml` の `[dev-dependencies]` に `tempfile = { workspace = true }` が無ければ足す。

- [ ] **Step 2: テストが落ちることを確かめる**

Run: `cargo test -p polaris-cli --test subcommands`
Expected: `the_login_subcommand_is_reachable_without_a_prompt` などが落ちる。`login` というサブコマンドが無いため。

- [ ] **Step 3: `Cargo.toml` を直す**

`crates/polaris-cli/Cargo.toml` の `[dependencies]` へ足す。

```toml
polaris-auth = { path = "../polaris-auth" }
async-trait = { workspace = true }
```

- [ ] **Step 4: 引数の形を変える**

`main.rs` の `Args` を次にする。`prompt` から `required_unless_present` を外し、
サブコマンドを足す。

```rust
#[derive(Parser)]
#[command(
    name = "polaris",
    about = "最小コンテキストのコーディングエージェント",
    after_help = "\
環境変数:
  POLARIS_PROVIDER  openai（既定）または codex。codex は `polaris login` の認証を使う。
  POLARIS_API_KEY   provider=openai のとき必須。OpenAI 互換エンドポイントの API キー。
  POLARIS_BASE_URL  provider=openai のとき、省略時 https://api.openai.com/v1
  POLARIS_MODEL     省略時 gpt-5.4（openai）/ gpt-5.3-codex（codex）
"
)]
struct Args {
    /// 実行する指示。サブコマンドと `--confined-apply` のときは不要。
    ///
    /// clap の `required_unless_present` を使わないのは、それが引数名しか
    /// 見ず、サブコマンドの有無を見ないためである。ここを必須にすると
    /// `polaris login` が「--prompt が無い」で弾かれる。M2 の Task 8 で
    /// `--confined-apply` が同じ形で到達不能になった。検証は解析後に手で行う。
    #[arg(short, long)]
    prompt: Option<String>,

    /// 監査ログの出力先。省略すると `~/.polaris/state/<project-id>/audit.jsonl` を使う。
    #[arg(long)]
    audit: Option<PathBuf>,

    /// 1 回の実行で許すターン数の上限。
    #[arg(long, default_value_t = 20)]
    max_turns: u32,

    /// 拘束された子として 1 件の変更操作を標準入力から読んで実行する。
    /// 内部用であり、利用者が直接使うものではない。
    #[arg(long, hide = true)]
    confined_apply: bool,

    /// サンドボックスの方針。
    #[arg(long, value_enum, default_value_t = SandboxModeArg::WorkspaceWrite)]
    sandbox: SandboxModeArg,

    /// 承認境界の方針。
    #[arg(long, value_enum, default_value_t = ApprovalPolicyArg::OnRequest)]
    approval: ApprovalPolicyArg,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// ChatGPT のサブスクリプションで認証する。ブラウザが開く。
    Login,
    /// 保管した資格情報を消す。`~/.codex/` には触れない。
    Logout,
}
```

- [ ] **Step 5: `TokenSource` の実装と分岐を書く**

`main.rs` の `TerminalApprover` の下へ次を足す。

```rust
/// `polaris-auth` を `polaris-provider` の `TokenSource` へ繋ぐ。この
/// 変換をここへ置くことで、`polaris-auth` がプロバイダのクレートへ依存
/// しないで済む。
struct AuthTokens {
    issuer: String,
    store: PathBuf,
}

fn to_provider_error(e: polaris_auth::AuthError) -> polaris_provider::ProviderError {
    match e {
        polaris_auth::AuthError::NotLoggedIn => polaris_provider::ProviderError::Auth(
            "ログインしていない。`polaris login` を実行すること".into(),
        ),
        other => polaris_provider::ProviderError::Auth(other.to_string()),
    }
}

#[async_trait::async_trait]
impl polaris_provider::TokenSource for AuthTokens {
    async fn token(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let c = polaris_auth::ensure_fresh(&self.issuer, &self.store)
            .await
            .map_err(to_provider_error)?;
        Ok(polaris_provider::Token {
            access_token: c.access_token,
            account_id: c.account_id,
        })
    }

    async fn refreshed(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let c = polaris_auth::force_refresh(&self.issuer, &self.store)
            .await
            .map_err(to_provider_error)?;
        Ok(polaris_provider::Token {
            access_token: c.access_token,
            account_id: c.account_id,
        })
    }
}
```

`main` の冒頭、`Args::parse()` の直後にサブコマンドの処理を置く。プロバイダの
組み立てや常時コンテキストの用意より前に返すこと。ログインにモデルは要らない。

```rust
    match args.command {
        Some(Command::Login) => {
            let store = match polaris_auth::store::default_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("保管先を決められない: {e}");
                    return ExitCode::FAILURE;
                }
            };
            return match polaris_auth::login::run(polaris_auth::ISSUER, &store).await {
                Ok(_) => {
                    println!("ログインしました: {}", store.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ログインできない: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(Command::Logout) => {
            let store = match polaris_auth::store::default_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("保管先を決められない: {e}");
                    return ExitCode::FAILURE;
                }
            };
            return match polaris_auth::logout(&store) {
                Ok(true) => {
                    println!("ログアウトしました: {}", store.display());
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    println!("ログインしていません");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ログアウトできない: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }
```

`--confined-apply` の処理はこの下、これまでと同じ位置に残す。その次に
`prompt` の検証を置く。

```rust
    let Some(prompt) = args.prompt.clone() else {
        eprintln!("--prompt が要る（`polaris --help` を見ること）");
        return ExitCode::FAILURE;
    };
```

プロバイダの組み立てを差し替える。現行の `OpenAiProvider` を作っている箇所を
次にする。

```rust
    let model = std::env::var("POLARIS_MODEL").ok();
    let provider_name =
        std::env::var("POLARIS_PROVIDER").unwrap_or_else(|_| "openai".to_string());

    let provider: Box<dyn polaris_provider::Provider> = match provider_name.as_str() {
        "openai" => {
            // 既存の組み立てをそのまま使う。挙動は変わらない。
            let base = std::env::var("POLARIS_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
            let key = match std::env::var("POLARIS_API_KEY") {
                Ok(k) => k,
                Err(_) => {
                    eprintln!("POLARIS_API_KEY が設定されていない");
                    return ExitCode::FAILURE;
                }
            };
            let model = model.unwrap_or_else(|| "gpt-5.4".to_string());
            match OpenAiProvider::new(base, key, model) {
                Ok(p) => Box::new(p),
                Err(e) => {
                    eprintln!("クライアントを構築できない: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "codex" => {
            let store = match polaris_auth::store::default_path() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("保管先を決められない: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let model = model
                .unwrap_or_else(|| polaris_provider::codex::DEFAULT_MODEL.to_string());
            Box::new(polaris_provider::codex::CodexProvider::new(
                polaris_provider::codex::ENDPOINT_BASE.to_string(),
                model,
                std::sync::Arc::new(AuthTokens {
                    issuer: polaris_auth::ISSUER.to_string(),
                    store,
                }),
            ))
        }
        other => {
            eprintln!("POLARIS_PROVIDER が未知の値 {other}。openai か codex を指定すること");
            return ExitCode::FAILURE;
        }
    };
```

`agent::run(&provider, …)` は `&dyn Provider` を受け取れる形になっているか確認し、
必要なら `provider.as_ref()` を渡す。`OpenAiProvider::new` の実際の引数の並びは
`crates/polaris-provider/src/openai.rs` を読んで合わせること。

- [ ] **Step 6: テストが通ることを確かめる**

Run: `cargo test --workspace`
Expected: 全て passed、0 failed。

Run: `cargo clippy --workspace --all-targets -- -D warnings` と `cargo fmt --all -- --check`
Expected: どちらも clean。

- [ ] **Step 7: 予算が動いていないことを確かめる**

Run: `cargo test -p polaris-core budget` と `cargo test -p polaris-core constitution`
Expected: 値を変えずに通る。プロバイダはシステムプロンプトにもツール定義にも
現れないので、常時コンテキストは 1 トークンも動かないはずである。動いていたら
何かを取り違えている。

- [ ] **Step 8: 変異で確かめる**

1. `prompt` の検証（`let Some(prompt) = …`）を消す → `the_normal_path_still_requires_a_prompt` が落ちる
2. サブコマンドの処理を `--confined-apply` より後ろへ動かす → どのテストも落ちなければ、順序は固定できていない。報告に書く
3. `other => { eprintln! … }` の腕を `_ => openai と同じ扱い` にする → `an_unknown_provider_name_fails_fast` が落ちる
4. `logout` の対象を `~/.codex/auth.json` にする → `logout_never_touches_the_codex_store` が落ちる。**この変異は必ず走らせること。** 仕様が最も強く禁じている一線であり、テストが本当にそれを見ているかを確かめる価値がある
5. `to_provider_error` の `NotLoggedIn` の腕を消し、すべて `other.to_string()` にする → `a_logged_out_codex_run_names_the_login_command` が落ちる
6. `provider_name` の既定を `"codex"` にする → `the_default_provider_is_still_openai` が落ちる

- [ ] **Step 9: README を直す**

`README.md` の「環境変数」の表を次にする。

```markdown
| 変数 | 既定値 | 意味 |
| --- | --- | --- |
| `POLARIS_PROVIDER` | `openai` | `openai` か `codex`。`codex` は `polaris login` で得た ChatGPT のサブスクリプション認証を使い、API キーを要しない。 |
| `POLARIS_API_KEY` | なし | `POLARIS_PROVIDER=openai` のとき必須。OpenAI 互換エンドポイントの API キー。 |
| `POLARIS_BASE_URL` | `https://api.openai.com/v1` | `POLARIS_PROVIDER=openai` のときのベース URL。 |
| `POLARIS_MODEL` | `gpt-5.4` / `gpt-5.3-codex` | 使用するモデル名。既定はプロバイダごとに異なる。 |
```

「実行」の節の下へ次を足す。

```markdown
## ChatGPT のサブスクリプションで使う

API キーを持たない場合は、ChatGPT の認証で繋げる。

```
polaris login
POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"
```

`polaris login` はブラウザを開き、`http://localhost:1455/auth/callback` で
認可を受け取る。このポートは登録済みの redirect_uri のものなので選び直せず、
`codex login` とは同時に走らない。資格情報は `~/.polaris/auth.json` に 0600 で
保管する。`~/.codex/` には読み書きとも触れない。

`polaris logout` で保管した資格情報を消す。
```

- [ ] **Step 10: コミット**

```bash
git add crates/polaris-cli README.md
git commit -m "feat(cli): add login/logout and select the provider by environment"
```

`docs/filemap.md` と `Cargo.lock` が動くので、同じコミットへ含めること。M2 では
ソースだけを入れてブランチを赤にした前例がある。

```bash
UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap
git add docs/filemap.md Cargo.lock
```

コミット後に `git status --short` が空であることを確かめる。

---

## 手で確かめること（無人ではできない）

受け入れ基準の 1 から 3 は実際の資格情報を要する。実装が終わったら、利用者に
次を依頼する。

1. `polaris login` を実行し、ブラウザで認可する。完了後に
   `ls -l ~/.polaris/auth.json` が `-rw-------` であること
2. `md5 ~/.codex/auth.json` を login の前後で取り、変わっていないこと
3. `POLARIS_PROVIDER=codex polaris -p "Cargo.toml は何行か"` が行数を含む
   答えを返すこと
4. `~/.polaris/auth.json` の `expires_at` を過去の値へ書き換えてから 3 を
   再実行し、更新を経て成立すること
5. `polaris logout` の後に 3 を実行し、`polaris login` を名指しするエラーが
   出ること。HTTP エラーにならないこと

これが済んだ時点で、M1 の受け入れ基準のうち唯一残っていた「実キーでの一発実行」
が満たされる。v1.0.0 のタグ付けはその後である。
