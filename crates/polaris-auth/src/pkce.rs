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
        assert!(
            !s.contains('+') && !s.contains('/'),
            "URL 安全でない文字がある: {s}"
        );
        assert!(!s.is_empty());
    }
}
