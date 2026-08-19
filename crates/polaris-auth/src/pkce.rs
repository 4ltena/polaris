//! PKCE (RFC 7636) verifier and challenge.
//!
//! Randomness is read directly from `/dev/urandom`. polaris already assumes
//! Unix (checking `st_nlink`, 0600 permissions), so there's no reason to add
//! a dependency just for randomness.

use crate::AuthError;

/// A verifier paired with the challenge derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Turns randomness read from `/dev/urandom` into base64url (no padding).
pub fn random_urlsafe(bytes: usize) -> Result<String, AuthError> {
    use base64::Engine;
    use std::io::Read;

    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf))
}

/// Derives the challenge from the verifier. S256 is "SHA-256, then base64url."
pub fn challenge_for(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Creates a new pair. 32 bytes of randomness becomes 43 characters in
/// base64url, which exactly matches the lower bound the RFC sets.
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

    /// The known test vector from RFC 7636 Appendix B. With the verifier
    /// fixed to this value, if the challenge doesn't come out to this
    /// value, the S256 computation is wrong.
    #[test]
    fn s256_matches_the_rfc_test_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_for(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// verifier stays within the length the RFC sets (43-128 characters)
    /// and consists only of unreserved characters.
    #[test]
    fn a_generated_verifier_is_within_the_rfc_length_and_charset() {
        let p = generate().expect("failed to generate");
        assert!(
            (43..=128).contains(&p.verifier.chars().count()),
            "verifier length is outside the RFC's range: {}",
            p.verifier.chars().count()
        );
        assert!(
            p.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
            "verifier has a character outside unreserved: {}",
            p.verifier
        );
    }

    /// Each generation produces a different value. An implementation that
    /// returns a fixed value fails this test. The counterpart positive
    /// check — that the shape is correct — is covered by the two tests
    /// above.
    #[test]
    fn two_generations_differ() {
        let a = generate().expect("failed to generate");
        let b = generate().expect("failed to generate");
        assert_ne!(a.verifier, b.verifier, "verifier is the same every time");
        assert_ne!(a.challenge, b.challenge, "challenge is the same every time");
    }

    /// A generated pair is internally consistent. The challenge is not
    /// unrelated to the verifier.
    #[test]
    fn a_generated_pair_is_self_consistent() {
        let p = generate().expect("failed to generate");
        assert_eq!(p.challenge, challenge_for(&p.verifier));
    }

    #[test]
    fn random_urlsafe_has_no_padding_and_no_unsafe_characters() {
        let s = random_urlsafe(32).expect("failed to generate");
        assert!(!s.contains('='), "padding is still present: {s}");
        assert!(
            !s.contains('+') && !s.contains('/'),
            "has a character that isn't URL-safe: {s}"
        );
        assert!(!s.is_empty());
    }
}
