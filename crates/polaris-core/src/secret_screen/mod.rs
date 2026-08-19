//! Remna's privacy filter. Pure logic that runs **before** a captured event
//! (a command string, a window title, etc.) is saved to the encrypted DB.
//!
//! Touches no OS API at all — it only classifies and rewrites `&str`. The
//! caller must not save [`FilterResult::Drop`], and for
//! [`FilterResult::Redacted`] must save the redacted string.
//!
//! This filter is an aid, not a guarantee. It leans toward avoiding
//! over-detection (mistakenly redacting or discarding an ordinary command)
//! rather than misses (failing to redact), while still actively redacting or
//! discarding values that look like secrets.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// The result of passing text through screen_text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterResult {
    /// No sensitive information was found. Safe to save as-is.
    Keep(String),
    /// A sensitive value was redacted. Save this string (do not save the original).
    Redacted(String),
    /// Content that looks sensitive was detected, but the redacted portion
    /// cannot safely be carved out from the surrounding context (i.e.
    /// undecidable). Save nothing.
    Drop,
}

const PLACEHOLDER: &str = "[REDACTED]";

// --- Redaction rules ---------------------------------------------------------
// Every one of these replaces in the form "prefix to keep (capture 1) +
// redaction placeholder". Ported to Rust from rules proven out on the zsh
// side probe (shell/remna-hook.zsh, scheduled for introduction in Task 7).

/// The common pattern for value capture: a double-quoted string / a
/// single-quoted string (either may contain whitespace), or an unquoted run
/// of non-whitespace. A quoted value can contain whitespace (e.g.
/// `"correct horse battery staple"`), so a plain `\S+` would stop at the
/// first whitespace and leave the rest of the value in plaintext
/// (fix for Critical 1).
const VALUE_PATTERN: &str = r#""[^"]*"|'[^']*'|\S+"#;

/// For an environment variable assignment `NAME=value`, when NAME contains a
/// word indicating a secret, redacts only the value (keeps the variable
/// name). The keyword set is kept aligned with the zsh hook / VS Code
/// extension's primary filter. If this authoritative filter (this function)
/// were weaker than the primary filter, wiring the sensor to this one in P2
/// would be a regression. PWD picks up things like MYSQL_PWD, and the
/// trailing wildcard picks up the API_/AUTH_/ACCESS_ family.
static ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)\b([A-Za-z_][A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|PWD|CREDENTIAL|KEY|API|AUTH|ACCESS)[A-Za-z0-9_]*=)({VALUE_PATTERN})"#,
    ))
    .unwrap()
});

/// Redacts the entire value of an `Authorization:` header (from after the colon up to just before the quote).
static AUTH_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(authorization\s*:\s*)([^'"\r\n]+)"#).unwrap());

/// A bare `Bearer <token>` appearing outside an `Authorization:` header.
static BARE_BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(bearer\s+)(\S+)"#).unwrap());

/// The value following a long flag such as `--password`, `--token`,
/// `--secret`, `--api-key`, `--access-token`, etc.
static LONG_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)(--(?:password|passwd|token|secret|api[-_]?key|access[-_]?token)[=\s]+)({VALUE_PATTERN})"#,
    ))
    .unwrap()
});

/// curl-style Basic auth `-u user:pass`. Captures the username and password
/// separately (needed by `redact_basic_auth` to judge collision with the
/// UID:GID form. Fix for Important 5).
static BASIC_AUTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(-u\s+)([^:\s]+):(\S+)"#).unwrap());

/// Credentials embedded in a URL, `scheme://user:password@host`. Redacts
/// only the password portion (capture 1), keeping the username and host
/// (fix for Critical 2). This independent rule also picks up an assignment
/// like `DATABASE_URL=postgres://...` where the variable name carries no keyword.
static URL_CREDENTIALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"://[^:/\s@]+:([^@\s]+)@"#).unwrap());

/// Credentials embedded in a URL as a single token form,
/// `scheme://token@host`, rather than colon-separated (e.g.
/// `curl https://secretTokenAbc123@ftp.example.com/file`). [`URL_CREDENTIALS`]
/// only looks at the two-part `user:password@` structure, so a single token
/// without a colon slips past that rule and stays in plaintext
/// (re-review finding, fix A).
///
/// Capture 1 is the token body. Matches only when there is no colon at all
/// between right after `://` and right before `@` (`user:pass@` contains a
/// colon, so it does not match here — mutually exclusive with
/// `URL_CREDENTIALS`).
///
/// Unconditionally redacting every matched token would also destroy
/// harmless usernames such as `git` in `ssh://git@github.com` or
/// `anonymous` in `https://anonymous@host`, so whether it's actually
/// redacted is guarded on the [`redact_url_single_token_credential`] side.
static URL_SINGLE_TOKEN_CREDENTIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"://([^:@/\s]+)@"#).unwrap());

/// A candidate for a bare high-entropy-looking string (20+ characters, a
/// base64/hex-like character set). Whether it's actually redacted gets an
/// additional judgment from [`looks_like_secret`].
static HIGH_ENTROPY_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[A-Za-z0-9+/_=-]{20,}"#).unwrap());

/// Replaces with capture 1 (the prefix to keep) + placeholder.
/// Returns true if there was even one match.
fn redact_with_prefix(text: &str, re: &Regex) -> (String, bool) {
    let mut matched = false;
    let out = re.replace_all(text, |caps: &Captures| {
        matched = true;
        format!("{}{PLACEHOLDER}", &caps[1])
    });
    (out.into_owned(), matched)
}

/// Replaces only the specified capture group within the whole match with the
/// placeholder, leaving the text before and after that group within the
/// match untouched. Used when there's context to keep both before and after
/// the value to be redacted (password), as in `scheme://user:password@host`
/// (the `://user:` side and the `@host` side) — `redact_with_prefix` can't
/// be used here because it keeps only the prefix and truncates the rest of
/// the match.
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

/// Redacts the token portion of a `scheme://token@host` matched by
/// [`URL_SINGLE_TOKEN_CREDENTIAL`] only when [`looks_like_secret`] is true,
/// or the token is 16+ characters long (re-review finding, fix A).
///
/// A short, harmless username like `git` in `ssh://git@github.com` or
/// `anonymous` in `https://anonymous@host` satisfies neither condition, so
/// it's kept. Meanwhile a token like the one in
/// `curl https://secretTokenAbc123@ftp.example.com` is caught by the
/// `looks_like_secret` judgment (a mix of upper/lowercase letters and
/// digits), and regardless of character makeup, anything 16+ characters
/// long is redacted on length alone, erring on the safe side (a fallback
/// following the constraint "when in doubt, favor Redact").
fn redact_url_single_token_credential(text: &str) -> (String, bool) {
    let mut matched = false;
    let out = URL_SINGLE_TOKEN_CREDENTIAL.replace_all(text, |caps: &Captures| {
        let full = caps.get(0).unwrap();
        let token = caps.get(1).unwrap();
        let token_str = token.as_str();
        if !looks_like_secret(token_str) && token_str.chars().count() < 16 {
            // Likely a harmless username, so keep it.
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

/// Redacts curl-style Basic auth `-u user:pass`. However, a UID:GID
/// specification like `docker run -u 1000:1000` (both username and password
/// are digits only) is not a credential and is not redacted
/// (fix for Important 5).
fn redact_basic_auth(text: &str) -> (String, bool) {
    let mut matched = false;
    let out = BASIC_AUTH.replace_all(text, |caps: &Captures| {
        let user = &caps[2];
        let pass = &caps[3];
        let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if all_digits(user) && all_digits(pass) {
            // Both are digits only = a UID:GID specification
            // (e.g. `docker run -u 1000:1000`), not a credential, so don't redact.
            return caps[0].to_string();
        }
        matched = true;
        format!("{}{user}:{PLACEHOLDER}", &caps[1])
    });
    (out.into_owned(), matched)
}

/// Whether a token has the shape typical of a git commit hash (7/8-digit
/// abbreviated, 40-digit SHA-1, 64-digit SHA-256): "length is exactly
/// 7/8/40/64, and every character is lowercase hex."
///
/// Note: this is a structural judgment that looks only at length and
/// character set — it does not semantically confirm the token is a SHA. A
/// lowercase hex string of exactly 40 characters is structurally
/// indistinguishable from, say, an old-format GitHub Personal Access Token,
/// so an actual secret of that shape can fall into this exception and be
/// Kept. This is accepted as a deliberate trade-off that prioritizes "don't
/// mistakenly redact an ordinary git command."
fn is_git_sha_shape(token: &str) -> bool {
    matches!(token.len(), 7 | 8 | 40 | 64)
        && token.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// Whether a token is made up of only lowercase letters and digits (no
/// symbols like hyphens, no uppercase). A kebab-case identifier like
/// `remna-collector-macos` contains a hyphen, so it does not qualify here
/// and is not mistakenly targeted for redaction.
fn is_lowercase_alnum(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_lowercase())
        && token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The minimum length targeted by [`looks_like_secret`]'s "lowercase
/// letters + digits only" branch.
///
/// A pure lowercase-alphanumeric string shorter than this (an ordinary short
/// username like `git` or `anonymous`, etc.) is excluded, because character
/// makeup alone doesn't carry enough information to distinguish it from a
/// secret. The existing high-entropy candidate
/// ([`HIGH_ENTROPY_CANDIDATE`]) is only ever invoked at 20+ characters, so
/// introducing this lower bound (16) does not affect existing judgments
/// made via the high-entropy path.
///
/// This lower bound became necessary once
/// [`redact_url_single_token_credential`] (fix A) started calling
/// `looks_like_secret` on the token in `scheme://token@host` too (which can
/// be under 20 characters). Without a lower bound, a short harmless
/// username like `git` in `ssh://git@host` would be misjudged as
/// "secret-looking."
const LOWERCASE_ALNUM_SECRET_MIN_LEN: usize = 16;

/// Among high-entropy candidates, redacts only the ones that actually look
/// like secret information.
///
/// The condition is either of the following:
/// - Uppercase letters, lowercase letters, and digits are all mixed
///   together (a typical random token).
/// - It's [`LOWERCASE_ALNUM_SECRET_MIN_LEN`] characters or longer, made up
///   of only lowercase letters and digits, and does not have the shape of
///   [`is_git_sha_shape`] (i.e. its length does not match the typical
///   length of a git commit hash). All-lowercase hex old-format tokens and
///   webhook secrets are caught here (fix for Critical 3: previously the
///   only condition was "a mix of upper/lowercase and digits," which missed
///   all-lowercase secrets).
///
/// A bare kebab-case identifier like `remna-collector-macos` contains a
/// hyphen and falls outside [`is_lowercase_alnum`], so it is not falsely
/// flagged (a conservative judgment aimed at not mistakenly redacting an
/// ordinary command).
///
/// **On deliberate over-detection**: all-lowercase hex outside the SHA
/// lengths (7/8/40/64) — including a 32-character MD5-style hash — is
/// structurally indistinguishable from an actual secret, so it's Redacted,
/// erring on the safe side. For example, a 32-character MD5 hash value does
/// not fall under the [`is_git_sha_shape`] exception, and at
/// [`LOWERCASE_ALNUM_SECRET_MIN_LEN`] or longer it is unconditionally
/// treated as secret information. This is not a bug — it's deliberate
/// behavior following this crate's constraint "lean toward reducing misses
/// (when in doubt, favor Redact)" (review finding: behavior is unchanged;
/// this intent is spelled out here).
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

/// After stripping placeholders, checks whether any alphanumeric characters
/// remain at all (i.e. whether the entire line was nothing but the secret itself).
fn nothing_useful_remains(text_with_placeholders: &str) -> bool {
    !text_with_placeholders
        .replace(PLACEHOLDER, "")
        .chars()
        .any(|c| c.is_alphanumeric())
}

/// Screens captured text (a command string, a window title) before it's saved.
///
/// - [`FilterResult::Keep`] if no sensitive value is found at all.
/// - [`FilterResult::Redacted`] (the redacted string) if a sensitive value could be redacted.
/// - [`FilterResult::Drop`] if sensitive-looking content was detected, but
///   after redaction not even the shape of a command remains (i.e. the
///   entire line was nothing but a secret).
pub fn screen_text(s: &str) -> FilterResult {
    let mut text = s.to_string();
    let mut any_redacted = false;

    // Process the Authorization header and bare Bearer first (to avoid the
    // later high-entropy judgment being applied redundantly to the bare token).
    for re in [&*AUTH_HEADER, &*BARE_BEARER, &*ENV_ASSIGNMENT, &*LONG_FLAG] {
        let (next, matched) = redact_with_prefix(&text, re);
        text = next;
        any_redacted |= matched;
    }

    // For URL-embedded credentials (`scheme://user:pass@host`), we want to
    // erase only the password portion rather than the whole match, so use
    // the dedicated redact_group.
    let (next, matched) = redact_group(&text, &URL_CREDENTIALS, 1);
    text = next;
    any_redacted |= matched;

    // The colon-less single-token form of URL-embedded credentials
    // (`scheme://token@host`). This matches exclusively of the
    // colon-separated rule above, so the order between the two doesn't
    // interfere either way (fix A).
    let (next, matched) = redact_url_single_token_credential(&text);
    text = next;
    any_redacted |= matched;

    // curl's `-u user:pass` needs collision judgment against UID:GID, so use the dedicated function.
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
        // MYSQL_PWD=hunter2 is under 20 characters and doesn't trip
        // high-entropy detection either, but the PWD suffix matches NAME, so
        // only the value is redacted. Equivalent to the sensor's primary filter.
        match screen_text("MYSQL_PWD=hunter2 mysql -e 'select 1'") {
            FilterResult::Redacted(s) => {
                assert!(
                    !s.contains("hunter2"),
                    "the PWD value should be redacted: {s}"
                );
                assert!(s.contains("mysql"), "the command body should survive");
            }
            other => panic!("expected redaction, got {other:?}"),
        }
    }

    // --- Additional tests: remaining rules required by brief Step 3 ---------
    // (Beyond env assignment and the Authorization header, verifies
    // redaction of --token / -u user:pass / high-entropy strings, plus
    // additional path exclusion patterns.)

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
        // When the entire line is nothing but a bare secret, redaction
        // leaves nothing behind, so it Drops.
        match screen_text("sk-abc123DEF456ghi789XYZ000aaa111") {
            FilterResult::Drop => {}
            other => panic!("expected drop, got {other:?}"),
        }
    }

    #[test]
    fn keeps_git_command_with_commit_hash() {
        // Confirms that lowercase-only hex (a git commit hash, etc.) is not
        // mistakenly redacted by high-entropy detection. A check on the
        // constraint against breaking ordinary commands.
        // Note: the token the original test used was actually 39 characters
        // (a real SHA-1 is 40), so to match Critical 3 tightening the SHA
        // exception to "length exactly 7/8/40/64," it was fixed to a proper
        // 40-character SHA-1-equivalent value.
        let sha40 = "8f3a1c2e9b7d4560112233445566778899aabbc0";
        assert_eq!(sha40.len(), 40);
        assert!(matches!(
            screen_text(&format!("git show {sha40}")),
            FilterResult::Keep(_)
        ));
    }

    // --- Fix-verification tests for review findings (Critical 1-3, Important 4-5) -----------

    #[test]
    fn redacts_url_embedded_credentials() {
        // Critical 2: the `scheme://user:password@host` form doesn't trip
        // any existing rule, so the password stays in plaintext even when
        // the variable name carries no keyword.
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
        // Critical 3: because looks_like_secret required "a mix of
        // upper/lowercase and digits," an all-lowercase (+digit) token
        // would be Kept even when it isn't a git SHA. The SHA exception
        // should be limited to just "all-lowercase hex of length exactly
        // 7/8/40/64"; any other high-entropy token should be redacted.
        // Uses a bare token that keeps surrounding context words (legacy
        // webhook token) while tripping none of the other redaction rules
        // (env assignment, flags, URL credentials, etc.), to verify the
        // behavior of high-entropy detection in isolation.
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
        // Important 5: BASIC_AUTH's `-u user:pass` rule was also redacting
        // the UID:GID specification in `docker run -u 1000:1000 img`.
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

    // --- Re-review findings (fix A, fix B) --------------------------------------

    #[test]
    fn redacts_single_token_url_credential() {
        // Fix A: `URL_CREDENTIALS` only looks at the colon-separated
        // two-part `user:password@` form, so `scheme://token@host` (a
        // colon-less single token) sails through and stays in plaintext.
        //
        // The token is deliberately made 16 characters (exactly at the
        // boundary) and a mix of letters, but with no digits. This means it
        // satisfies neither condition of `looks_like_secret` (a mix of
        // upper/lowercase and digits, or all-lowercase-alnum not shaped
        // like a SHA), letting us verify whether it's redacted purely by
        // the `token.len() >= 16` length fallback. It's also under 20
        // characters, which avoids the false positive of the existing
        // `HIGH_ENTROPY_CANDIDATE` (20+ characters) high-entropy judgment
        // happening to cover it and passing green without ever exercising
        // fix A's new logic.
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
        // Fix A's guard: when a token is neither looks_like_secret nor 16+
        // characters long, it's kept as a harmless username like `git` in
        // `ssh://git@github.com` or `anonymous` in `https://anonymous@host`,
        // rather than mistakenly destroyed.
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
        // Fix B (boundary test a): the `-u UID:GID` exception is limited to
        // the case where both sides are digits only. If the GID side isn't
        // digits, the possibility of a credential can't be ruled out, so it
        // redacts, erring on the safe side (the Keep behavior for
        // `docker run -u 1000:1000` itself is already confirmed by the
        // existing `keeps_docker_uid_gid_but_redacts_curl_basic_auth`).
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
        // Fix B (boundary test b): a 32-character all-lowercase hex string
        // (an MD5 hash, etc.) does not fall under `is_git_sha_shape`'s SHA
        // exception (7/8/40/64 characters), so it's structurally
        // indistinguishable from an actual secret and gets Redacted, erring
        // on the safe side. This is not a bug — it's deliberate
        // over-detection following the constraint "lean toward reducing
        // misses (when in doubt, favor Redact)" (response to a review
        // finding: behavior is unchanged; this intent is spelled out in
        // both the doc comment and the test).
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
        // Critical 1: if value capture were left as `(\S+)`, it would stop
        // at the first whitespace, leaving the rest inside the quotes
        // ("horse battery staple") in plaintext.
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
