//! Remna のプライバシーフィルタ。捕捉したイベント（コマンド文字列やウィンドウ
//! タイトルなど）を暗号化 DB へ保存する **前** に通す純粋ロジック。
//!
//! OS API には一切触れず、`&str` の分類・書き換えのみを行う。呼び出し側は
//! [`FilterResult::Drop`] を保存せず、[`FilterResult::Redacted`] は伏字化後の
//! 文字列を保存すること。
//!
//! このフィルタは補助であり保証ではない。見逃し（伏字化漏れ）より過検出
//! （通常のコマンドを誤って伏字化・破棄すること）を避ける方向に倒しつつも、
//! シークレットらしき値は積極的に伏字化・破棄する。

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// テキストを screen_text に通した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterResult {
    /// 機微情報は見つからなかった。そのまま保存してよい。
    Keep(String),
    /// 機微な値を伏字化した。この文字列を保存すること（元の文字列は保存しない）。
    Redacted(String),
    /// 機微情報らしき内容を検知したが、周囲の文脈から安全に伏字箇所だけを
    /// 切り出せない（=判定不能）。何も保存しない。
    Drop,
}

const PLACEHOLDER: &str = "[REDACTED]";

// --- 伏字化ルール -----------------------------------------------------------
// いずれも「保持する接頭辞（キャプチャ 1）+ 伏字プレースホルダ」の形で置換する。
// zsh 側のプローブ（shell/remna-hook.zsh、Task 7 で導入予定）で実証済みの
// ルールを Rust に移植したもの。

/// 値キャプチャ用の共通パターン: ダブルクオート文字列 / シングルクオート文字列
/// （いずれも空白を含んでよい）、またはクオートなしの非空白ラン。
/// クオートされた値は空白を含みうる（例: `"correct horse battery staple"`）ため、
/// 単純な `\S+` だと最初の空白で止まり、値の後半が平文で残ってしまう
/// （Critical 1 の是正）。
const VALUE_PATTERN: &str = r#""[^"]*"|'[^']*'|\S+"#;

/// 環境変数代入 `NAME=value` で、NAME が秘密を示す語を含む場合、値だけを伏字化する
/// （変数名は残す）。キーワード集合は zsh フック・VS Code 拡張の一次フィルタと揃える。
/// 権威フィルタ（この関数）が一次フィルタより弱いと、P2 でセンサをここへ配線した際に退行する。
/// PWD は MYSQL_PWD など、末尾ワイルドカードは API_/AUTH_/ACCESS_ 系を拾う。
static ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)\b([A-Za-z_][A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|PWD|CREDENTIAL|KEY|API|AUTH|ACCESS)[A-Za-z0-9_]*=)({VALUE_PATTERN})"#,
    ))
    .unwrap()
});

/// `Authorization:` ヘッダの値（コロン以降、引用符の手前まで）を丸ごと伏字化する。
static AUTH_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(authorization\s*:\s*)([^'"\r\n]+)"#).unwrap());

/// `Authorization:` ヘッダの外に出てくる素の `Bearer <token>`。
static BARE_BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(bearer\s+)(\S+)"#).unwrap());

/// `--password`, `--token`, `--secret`, `--api-key`, `--access-token` 等の
/// 長いフラグに続く値。
static LONG_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)(--(?:password|passwd|token|secret|api[-_]?key|access[-_]?token)[=\s]+)({VALUE_PATTERN})"#,
    ))
    .unwrap()
});

/// curl 形式の Basic 認証 `-u user:pass`。ユーザー名・パスワードそれぞれを
/// 個別にキャプチャする（`redact_basic_auth` で UID:GID 形との衝突を判定する
/// ため。Important 5 の是正）。
static BASIC_AUTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(-u\s+)([^:\s]+):(\S+)"#).unwrap());

/// URL に埋め込まれた資格情報 `scheme://user:password@host`。
/// パスワード部分（キャプチャ 1）だけを伏字化し、ユーザー名・ホストは残す
/// （Critical 2 の是正）。変数名にキーワードが無い `DATABASE_URL=postgres://...`
/// のような代入も、この独立したルールで拾える。
static URL_CREDENTIALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"://[^:/\s@]+:([^@\s]+)@"#).unwrap());

/// URL に埋め込まれた資格情報のうち、コロン区切りではなく
/// `scheme://token@host` という単一トークン形（例:
/// `curl https://secretTokenAbc123@ftp.example.com/file`）。[`URL_CREDENTIALS`]
/// は `user:password@` の2要素構造だけを見るため、コロンを含まない単一トークン
/// はこのルールでは拾えず平文で残ってしまう（再レビュー指摘 修正 A）。
///
/// キャプチャ 1 が token 本体。`://` の直後から `@` の直前までにコロンが
/// 一切無い場合にのみマッチする（`user:pass@` はコロンを含むためここには
/// マッチしない = `URL_CREDENTIALS` と排他的）。
///
/// マッチした token を無条件に伏字化すると `ssh://git@github.com` の `git` や
/// `https://anonymous@host` の `anonymous` のような無害なユーザー名まで壊して
/// しまうため、実際に伏字化するかどうかは
/// [`redact_url_single_token_credential`] 側でガードする。
static URL_SINGLE_TOKEN_CREDENTIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"://([^:@/\s]+)@"#).unwrap());

/// 高エントロピーらしき裸の文字列（20 文字以上、base64/hex ライクな文字集合）の候補。
/// 実際に伏字化するかは [`looks_like_secret`] で追加判定する。
static HIGH_ENTROPY_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[A-Za-z0-9+/_=-]{20,}"#).unwrap());

/// キャプチャ 1（保持する接頭辞）+ プレースホルダで置換する。
/// マッチが 1 件でもあれば true を返す。
fn redact_with_prefix(text: &str, re: &Regex) -> (String, bool) {
    let mut matched = false;
    let out = re.replace_all(text, |caps: &Captures| {
        matched = true;
        format!("{}{PLACEHOLDER}", &caps[1])
    });
    (out.into_owned(), matched)
}

/// マッチ全体のうち指定したキャプチャグループだけをプレースホルダに置換し、
/// マッチ内でそのグループの前後にある文字列はそのまま残す。
/// `scheme://user:password@host` のように、伏字化したい値（password）の前後
/// （`://user:` と `@host` の `@` 側）にも残したい文脈がある場合に使う
/// （`redact_with_prefix` は接頭辞だけ残してマッチ全体を切り詰めるため使えない）。
fn redact_group(text: &str, re: &Regex, group: usize) -> (String, bool) {
    let mut matched = false;
    let out = re.replace_all(text, |caps: &Captures| {
        matched = true;
        let full = caps.get(0).unwrap();
        let g = caps.get(group).unwrap();
        format!(
            "{}{PLACEHOLDER}{}",
            &text[full.start()..g.start()],
            &text[g.end()..full.end()]
        )
    });
    (out.into_owned(), matched)
}

/// [`URL_SINGLE_TOKEN_CREDENTIAL`] でマッチした `scheme://token@host` の
/// token 部分を、[`looks_like_secret`] が true か、または 16 文字以上の場合
/// にのみ伏字化する（再レビュー指摘 修正 A）。
///
/// `ssh://git@github.com` の `git` や `https://anonymous@host` の
/// `anonymous` のような短い無害なユーザー名は、この両条件のいずれにも
/// 該当しないため残る。一方 `curl https://secretTokenAbc123@ftp.example.com`
/// のような token は `looks_like_secret` 判定（英大小文字+数字の混在）で
/// 捕捉されるほか、文字種に関わらず 16 文字以上あれば長さだけを根拠に
/// 安全側で伏字化する（制約「迷ったら Redact 寄り」に従うフォールバック）。
fn redact_url_single_token_credential(text: &str) -> (String, bool) {
    let mut matched = false;
    let out = URL_SINGLE_TOKEN_CREDENTIAL.replace_all(text, |caps: &Captures| {
        let full = caps.get(0).unwrap();
        let token = caps.get(1).unwrap();
        let token_str = token.as_str();
        if !looks_like_secret(token_str) && token_str.chars().count() < 16 {
            // 無害なユーザー名の可能性が高いため残す。
            return full.as_str().to_string();
        }
        matched = true;
        format!(
            "{}{PLACEHOLDER}{}",
            &text[full.start()..token.start()],
            &text[token.end()..full.end()]
        )
    });
    (out.into_owned(), matched)
}

/// curl の Basic 認証 `-u user:pass` を伏字化する。ただし
/// `docker run -u 1000:1000` のような UID:GID 指定（ユーザー名・パスワード
/// 双方が数字のみ）は資格情報ではないため伏字化しない（Important 5 の是正）。
fn redact_basic_auth(text: &str) -> (String, bool) {
    let mut matched = false;
    let out = BASIC_AUTH.replace_all(text, |caps: &Captures| {
        let user = &caps[2];
        let pass = &caps[3];
        let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if all_digits(user) && all_digits(pass) {
            // 両方が数字のみ = UID:GID の指定（例: `docker run -u 1000:1000`）
            // であって資格情報ではないため、伏字化しない。
            return caps[0].to_string();
        }
        matched = true;
        format!("{}{user}:{PLACEHOLDER}", &caps[1])
    });
    (out.into_owned(), matched)
}

/// git のコミットハッシュ（短縮 7/8 桁、SHA-1 の 40 桁、SHA-256 の 64 桁）に
/// 典型的な「長さがちょうど 7/8/40/64 で、全文字が小文字 16 進数」の形か。
///
/// 注意: これは長さと文字種だけを見た構造的な判定であり、意味的に SHA だと
/// 確認しているわけではない。40 文字ちょうどの小文字 16 進文字列は、旧形式の
/// GitHub Personal Access Token 等とも構造的に区別できないため、そのような
/// 実際のシークレットがこの例外に該当してしまい Keep されうる。これは
/// 「通常の git コマンドを誤って伏字化しない」ことを優先した意図的なトレード
/// オフとして許容する。
fn is_git_sha_shape(token: &str) -> bool {
    matches!(token.len(), 7 | 8 | 40 | 64)
        && token.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// トークンが英小文字と数字だけで構成されているか（ハイフン等の記号や大文字を
/// 含まない）。`remna-collector-macos` のようなケバブケース識別子はハイフンを
/// 含むためここには該当せず、誤って伏字化対象にならない。
fn is_lowercase_alnum(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_lowercase())
        && token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// [`looks_like_secret`] の「英小文字+数字のみ」分岐が対象にする最小長。
///
/// これより短い純粋な小文字英数字の文字列（`git`, `anonymous` のような
/// ありふれた短いユーザー名等）は、文字種の情報量だけではシークレットと区別
/// できないため対象外とする。既存の高エントロピー候補（[`HIGH_ENTROPY_CANDIDATE`]）
/// は 20 文字以上でしか呼ばれないため、この下限（16）を導入しても高エントロピー
/// 経由の既存の判定結果に影響はない。
///
/// この下限は、[`redact_url_single_token_credential`]（修正 A）が
/// `looks_like_secret` を `scheme://token@host` の token（20 文字未満もあり得る）
/// に対しても呼ぶようになったことで必要になった。下限が無いと `ssh://git@host`
/// の `git` のような短い無害なユーザー名まで「秘密らしい」と誤判定してしまう。
const LOWERCASE_ALNUM_SECRET_MIN_LEN: usize = 16;

/// 高エントロピー候補のうち、実際に秘密情報らしい見た目のものだけを伏字化する。
///
/// 条件は次のいずれか:
/// - 英大文字・英小文字・数字が全て混在している（典型的なランダムトークン）。
/// - [`LOWERCASE_ALNUM_SECRET_MIN_LEN`] 文字以上あり、英小文字と数字だけで
///   構成されており、かつ [`is_git_sha_shape`] の形ではない（=長さが git
///   コミットハッシュの典型的な長さと一致しない）。全小文字 hex の旧形式
///   トークンや webhook secret はこちらで捕捉する（Critical 3 の是正: 以前は
///   「大小英数字の混在」のみを条件にしていたため、全小文字のシークレットを
///   取りこぼしていた）。
///
/// `remna-collector-macos` のような素のケバブケース識別子はハイフンを含み
/// [`is_lowercase_alnum`] の対象外のため、誤検出しない
/// （通常のコマンドを誤って伏字化しないための保守的な判定）。
///
/// **意図的な過検出について**: SHA 長（7/8/40/64）以外の全小文字 hex（MD5 等の
/// 32 文字ハッシュを含む）は、実際のシークレットと構造的に区別できないため
/// 安全側に倒して Redact する。例えば 32 文字の MD5 ハッシュ値は
/// [`is_git_sha_shape`] の例外に該当せず、[`LOWERCASE_ALNUM_SECRET_MIN_LEN`]
/// 以上であれば無条件に秘密情報とみなされる。これはバグではなく、本 crate の
/// 制約「見逃しを減らす方向に倒す（迷ったら Redact 寄り）」に従う意図的な挙動
/// である（レビュー指摘: 挙動は変えず、この意図をここに明記する）。
fn looks_like_secret(token: &str) -> bool {
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    if has_upper && has_lower && has_digit {
        return true;
    }
    token.chars().count() >= LOWERCASE_ALNUM_SECRET_MIN_LEN
        && is_lowercase_alnum(token)
        && !is_git_sha_shape(token)
}

fn redact_high_entropy(text: &str) -> (String, bool) {
    let mut matched = false;
    let out = HIGH_ENTROPY_CANDIDATE.replace_all(text, |caps: &Captures| {
        let token = &caps[0];
        if looks_like_secret(token) {
            matched = true;
            PLACEHOLDER.to_string()
        } else {
            token.to_string()
        }
    });
    (out.into_owned(), matched)
}

/// プレースホルダを取り除いた後、英数字が一切残らないか（=行全体が
/// シークレットそのものだったか）を調べる。
fn nothing_useful_remains(text_with_placeholders: &str) -> bool {
    !text_with_placeholders
        .replace(PLACEHOLDER, "")
        .chars()
        .any(|c| c.is_alphanumeric())
}

/// 捕捉したテキスト（コマンド文字列やウィンドウタイトル）を保存前に screen する。
///
/// - 何も機微な値が見つからなければ [`FilterResult::Keep`]。
/// - 機微な値を伏字化できれば [`FilterResult::Redacted`]（伏字化後の文字列）。
/// - 機微らしき内容を検知したが、伏字化した結果コマンドの形すら残らない
///   （=行全体が丸ごとシークレットだった）場合は [`FilterResult::Drop`]。
pub fn screen_text(s: &str) -> FilterResult {
    let mut text = s.to_string();
    let mut any_redacted = false;

    // Authorization ヘッダ・裸の Bearer を先に処理する
    // （後段の高エントロピー判定がトークン単体に重複適用されるのを避けるため）。
    for re in [&*AUTH_HEADER, &*BARE_BEARER, &*ENV_ASSIGNMENT, &*LONG_FLAG] {
        let (next, matched) = redact_with_prefix(&text, re);
        text = next;
        any_redacted |= matched;
    }

    // URL 埋め込み資格情報（`scheme://user:pass@host`）はマッチ全体ではなく
    // password 部分だけを消したいので専用の redact_group を使う。
    let (next, matched) = redact_group(&text, &URL_CREDENTIALS, 1);
    text = next;
    any_redacted |= matched;

    // URL 埋め込み資格情報のうちコロン無し単一トークン形（`scheme://token@host`）。
    // 上のコロン区切りルールとは排他的にマッチするため、順序はどちらが先でも
    // 干渉しない（修正 A）。
    let (next, matched) = redact_url_single_token_credential(&text);
    text = next;
    any_redacted |= matched;

    // curl の `-u user:pass` は UID:GID との衝突判定が要るため専用関数を使う。
    let (next, matched) = redact_basic_auth(&text);
    text = next;
    any_redacted |= matched;

    let (next, matched) = redact_high_entropy(&text);
    text = next;
    any_redacted |= matched;

    if !any_redacted {
        return FilterResult::Keep(s.to_string());
    }

    if nothing_useful_remains(&text) {
        return FilterResult::Drop;
    }

    FilterResult::Redacted(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bearer_token() {
        match screen_text("curl -H 'Authorization: Bearer sk-abc123DEF456ghi789'") {
            FilterResult::Redacted(s) => assert!(!s.contains("sk-abc123DEF456ghi789")),
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn redacts_env_assignment() {
        match screen_text("export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY") {
            FilterResult::Redacted(s) => assert!(!s.contains("wJalrXUtnFEMI")),
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn keeps_ordinary_command() {
        assert!(matches!(
            screen_text("cargo test -p remna-store"),
            FilterResult::Keep(_)
        ));
    }

    #[test]
    fn redacts_short_pwd_var() {
        // MYSQL_PWD=hunter2 は 20 文字未満で高エントロシーにも当たらないが、
        // PWD 語尾で NAME に一致するため値だけ伏字化される。センサ一次フィルタと同等。
        match screen_text("MYSQL_PWD=hunter2 mysql -e 'select 1'") {
            FilterResult::Redacted(s) => {
                assert!(!s.contains("hunter2"), "PWD の値は伏字化されるべき: {s}");
                assert!(s.contains("mysql"), "コマンド本体は残る");
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    // --- 追加テスト: brief Step 3 が要求する残りのルール ---------------------
    // (env代入・Authorizationヘッダに加えて、--token / -u user:pass / 高エントロ
    // ピー文字列の伏字化、および追加のパス除外パターンを検証する。)

    #[test]
    fn redacts_token_flag() {
        match screen_text("curl --token abc123SECRETxyz789ZZZ999 https://example.com") {
            FilterResult::Redacted(s) => {
                assert!(!s.contains("abc123SECRETxyz789ZZZ999"));
                assert!(
                    s.contains("https://example.com"),
                    "command shape should survive: {s}"
                );
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn redacts_basic_auth_password_but_keeps_username() {
        match screen_text("curl -u admin:s3cr3tPW123456789ABC https://example.com") {
            FilterResult::Redacted(s) => {
                assert!(!s.contains("s3cr3tPW123456789ABC"));
                assert!(s.contains("admin"), "username should be kept: {s}");
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn drops_bare_high_entropy_secret_with_no_surrounding_context() {
        // 行全体が裸のシークレットだけの場合、伏字化しても中身が残らないので Drop する。
        match screen_text("sk-abc123DEF456ghi789XYZ000aaa111") {
            FilterResult::Drop => {}
            other => panic!("expected drop, got {other:?}"),
        }
    }

    #[test]
    fn keeps_git_command_with_commit_hash() {
        // 小文字だけの hex（git のコミットハッシュ等）を高エントロピー判定で
        // 誤って伏字化しないこと。通常のコマンドを壊さない制約の確認。
        // 注: 元のテストが使っていたトークンは実は 39 文字（真の SHA-1 は 40 文字）
        // だったため、Critical 3 で SHA 例外を「長さがちょうど 7/8/40/64」に
        // 厳密化するのに合わせ、正しい 40 文字の SHA-1 相当に修正した。
        let sha40 = "8f3a1c2e9b7d4560112233445566778899aabbc0";
        assert_eq!(sha40.len(), 40);
        assert!(matches!(
            screen_text(&format!("git show {sha40}")),
            FilterResult::Keep(_)
        ));
    }

    // --- レビュー指摘の是正テスト（Critical 1〜3, Important 4〜5） -----------

    #[test]
    fn redacts_url_embedded_credentials() {
        // Critical 2: `scheme://user:password@host` 形式はどの既存ルールにも
        // 掛からず、変数名にキーワードが無くてもパスワードが平文で残る。
        match screen_text("export DATABASE_URL=postgres://user:pass@host") {
            FilterResult::Redacted(s) => {
                assert!(!s.contains("pass"), "password should be redacted: {s}");
                assert!(s.contains("user"), "username should be kept: {s}");
                assert!(s.contains("host"), "host should be kept: {s}");
            }
            other => panic!("expected redaction, got {other:?}"),
        }

        match screen_text("psql postgres://admin:s3cr3t@db:5432/app") {
            FilterResult::Redacted(s) => {
                assert!(!s.contains("s3cr3t"), "password should be redacted: {s}");
                assert!(s.contains("admin"), "username should be kept: {s}");
                assert!(
                    s.contains("db:5432/app"),
                    "host/port/path should be kept: {s}"
                );
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn redacts_full_lowercase_alnum_secret_not_sha_shaped() {
        // Critical 3: looks_like_secret が「大小英数字の混在」を要求するため、
        // 全小文字（+数字）のトークンは git SHA でなくても Keep されてしまう。
        // SHA 例外は「長さがちょうど 7/8/40/64 の全小文字16進数」だけに限定し、
        // それ以外の高エントロピートークンは伏字化されるべき。
        // 前後に文脈語（legacy webhook token）を残しつつ、他のどの伏字化ルール
        // （env代入・フラグ・URL資格情報等）にも掛からない裸のトークンにして、
        // 高エントロピー判定単体の挙動を検証する。
        let token = "a0b1c2d3e4f5g6h7i8j9a0b1c2d3e4f5g6h7i8j9a0b1c2d3e4";
        assert_eq!(token.len(), 50);
        match screen_text(&format!("legacy webhook token: {token}")) {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains(token),
                    "legacy lowercase token should be redacted: {s}"
                );
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn keeps_docker_uid_gid_but_redacts_curl_basic_auth() {
        // Important 5: BASIC_AUTH の `-u user:pass` ルールが
        // `docker run -u 1000:1000 img` の UID:GID 指定まで伏字化してしまう。
        assert!(
            matches!(
                screen_text("docker run -u 1000:1000 img"),
                FilterResult::Keep(_)
            ),
            "UID:GID form must not be treated as credentials"
        );

        match screen_text("curl -u admin:s3cr3tPass example.com") {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains("s3cr3tPass"),
                    "password should be redacted: {s}"
                );
                assert!(s.contains("admin"), "username should be kept: {s}");
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    // --- 再レビュー指摘（修正 A・修正 B）--------------------------------------

    #[test]
    fn redacts_single_token_url_credential() {
        // 修正 A: `URL_CREDENTIALS` は `user:password@` のコロン区切り2要素形しか
        // 見ないため、`scheme://token@host`（コロン無しの単一トークン）は素通り
        // して平文で残ってしまう。
        //
        // token は意図的に 16 文字（境界ちょうど）かつ英数字混在だが数字は含まない
        // 形にしてある。これにより `looks_like_secret`（大小英数字混在 or SHA形
        // でない全小文字英数字）のどちらの条件にも該当せず、`token.len() >= 16`
        // という長さフォールバックだけで伏字化されるかを検証できる。また 20 文字
        // 未満なので、既存の `HIGH_ENTROPY_CANDIDATE`（20文字以上）の高エントロ
        // ピー判定が偶然カバーしてしまい、修正 A の新規ロジックを経由せずに
        // グリーンになる、という偽陽性を避けている。
        let token = "SecretApiTokenAB";
        assert_eq!(token.len(), 16);
        assert!(
            !looks_like_secret(token),
            "test token must not already satisfy looks_like_secret"
        );

        match screen_text(&format!("curl https://{token}@ftp.example.com/file")) {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains(token),
                    "single-token credential should be redacted: {s}"
                );
                assert!(s.contains("ftp.example.com"), "host should be kept: {s}");
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    #[test]
    fn keeps_harmless_ssh_and_anonymous_url_usernames() {
        // 修正 A のガード: token が looks_like_secret でもなく 16 文字未満なら、
        // `ssh://git@github.com` の `git` や `https://anonymous@host` の
        // `anonymous` のような無害なユーザー名として残し、誤って壊さない。
        match screen_text("git clone ssh://git@github.com/user/repo") {
            FilterResult::Keep(s) => assert!(s.contains("git@github.com")),
            other => panic!("expected keep, got {other:?}"),
        }
        match screen_text("wget https://anonymous@ftp.gnu.org/pub/file") {
            FilterResult::Keep(s) => assert!(s.contains("anonymous@ftp.gnu.org")),
            other => panic!("expected keep, got {other:?}"),
        }
    }

    #[test]
    fn redacts_basic_auth_when_second_part_is_non_numeric() {
        // 修正 B（境界テスト a）: `-u UID:GID` の例外は「両方が数字のみ」の場合に
        // 限る。GID 側が数字でなければ資格情報の可能性を排除できないため、
        // 安全側で redact する（`docker run -u 1000:1000` の Keep 自体は既存の
        // `keeps_docker_uid_gid_but_redacts_curl_basic_auth` で確認済み）。
        match screen_text("docker run -u 1000:pass img") {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains("pass"),
                    "non-numeric part should be redacted: {s}"
                );
                assert!(s.contains("1000"), "numeric uid part should be kept: {s}");
            }
            other => panic!("expected redaction, got {other:?}"),
        }
        assert!(matches!(
            screen_text("docker run -u 1000:1000 img"),
            FilterResult::Keep(_)
        ));
    }

    #[test]
    fn redacts_32_char_lowercase_hex_as_intentional_over_detection() {
        // 修正 B（境界テスト b）: 32 文字の全小文字16進数（MD5 ハッシュ等）は
        // `is_git_sha_shape` の SHA 例外（7/8/40/64 文字）に該当しないため、
        // 実際のシークレットと構造的に区別できず安全側で Redact される。
        // これはバグではなく、制約「見逃しを減らす方向に倒す（迷ったら Redact
        // 寄り）」に従う意図的な過検出である（レビュー指摘への対応: 挙動は
        // 変えず、この意図を doc コメントとテストの両方で明記する）。
        let md5_like = "0123456789abcdef0123456789abcdef";
        assert_eq!(md5_like.len(), 32);
        match screen_text(&format!("md5sum output: {md5_like}")) {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains(md5_like),
                    "32-char lowercase hex is intentionally over-redacted: {s}"
                );
            }
            other => panic!("expected redaction (intentional over-detection), got {other:?}"),
        }
    }

    #[test]
    fn redacts_multi_word_quoted_env_value() {
        // Critical 1: 値キャプチャが `(\S+)` のままだと最初の空白で止まり、
        // クオート内の残り（"horse battery staple"）が平文で残ってしまう。
        match screen_text(r#"export DB_PASSWORD="correct horse battery staple""#) {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains("correct"),
                    "value should be fully redacted: {s}"
                );
                assert!(!s.contains("horse"), "value should be fully redacted: {s}");
                assert!(
                    !s.contains("battery"),
                    "value should be fully redacted: {s}"
                );
                assert!(!s.contains("staple"), "value should be fully redacted: {s}");
                assert!(
                    s.contains("DB_PASSWORD"),
                    "variable name should be kept: {s}"
                );
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }
}
