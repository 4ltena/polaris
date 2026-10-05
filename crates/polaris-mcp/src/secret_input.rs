//! Masked, short-lived credential input for trusted interactive hosts.
//!
//! This type intentionally has no plaintext accessor, `Clone`, `Serialize`,
//! or length accessor. Hosts may append and erase complete Unicode scalars,
//! then transfer ownership directly to the credential store boundary.

use std::fmt;

use zeroize::{Zeroize, Zeroizing};

use crate::store::SecretValue;

/// Hard ceiling for one credential entry. It bounds terminal memory before a
/// caller can reach the separately bounded persistent MCP store.
pub const MAX_SECRET_INPUT_BYTES_V1: usize = 64 * 1024;

/// A bounded terminal-only input buffer. Dropping it wipes the underlying
/// bytes through `Zeroizing`, including cancellation and unwinding.
pub struct SecretInputV1 {
    bytes: Zeroizing<Vec<u8>>,
    max_bytes: usize,
}

impl fmt::Debug for SecretInputV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretInputV1(<redacted>)")
    }
}

/// Closed errors for masked editing. They carry neither entered bytes nor a
/// length, so an error can safely reach ordinary TUI status rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SecretInputErrorV1 {
    #[error("secret_input_limit")]
    Limit,
}

impl Default for SecretInputV1 {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretInputV1 {
    /// Creates a buffer using the fixed hard ceiling.
    pub fn new() -> Self {
        Self::with_max_bytes(MAX_SECRET_INPUT_BYTES_V1)
    }

    /// Creates a buffer with a stricter caller-selected ceiling. It cannot be
    /// widened beyond the hard ceiling.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        let max_bytes = max_bytes.min(MAX_SECRET_INPUT_BYTES_V1);
        Self {
            // A fixed allocation avoids freeing an unzeroized old buffer
            // when a growing Vec would otherwise reallocate mid-entry.
            bytes: Zeroizing::new(Vec::with_capacity(max_bytes)),
            max_bytes,
        }
    }

    /// Appends one complete Unicode scalar or leaves the buffer unchanged.
    pub fn push(&mut self, value: char) -> Result<(), SecretInputErrorV1> {
        let mut encoded = [0_u8; 4];
        let encoded = value.encode_utf8(&mut encoded);
        let next_len = self
            .bytes
            .len()
            .checked_add(encoded.len())
            .ok_or(SecretInputErrorV1::Limit)?;
        if next_len > self.max_bytes {
            return Err(SecretInputErrorV1::Limit);
        }
        self.bytes.extend_from_slice(encoded.as_bytes());
        Ok(())
    }

    /// Erases one complete Unicode scalar without returning its value.
    pub fn pop(&mut self) -> bool {
        let Some(value) = std::str::from_utf8(&self.bytes)
            .expect("SecretInputV1 only stores complete UTF-8 scalars")
            .chars()
            .next_back()
        else {
            return false;
        };
        let next_len = self.bytes.len() - value.len_utf8();
        self.bytes[next_len..].zeroize();
        self.bytes.truncate(next_len);
        true
    }

    /// Lets a host distinguish an empty entry without exposing its length.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Cancels input. Consuming `self` ensures the zeroizing buffer is dropped
    /// now rather than remaining in a host draft or input queue.
    pub fn cancel(self) {}

    /// Transfers the secret to the typed credential owner. There is no public
    /// string or byte accessor on either side of this boundary.
    pub fn into_secret_value(self) -> SecretValue {
        SecretValue::from_secret_bytes(self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::store::Store;

    #[test]
    fn redacts_values_and_lengths_in_debug() {
        let mut short = SecretInputV1::new();
        short.push('a').unwrap();
        let mut long = SecretInputV1::new();
        for value in "secret-with-a-different-length".chars() {
            long.push(value).unwrap();
        }
        assert_eq!(format!("{short:?}"), "SecretInputV1(<redacted>)");
        assert_eq!(format!("{short:?}"), format!("{long:?}"));

        let secret = long.into_secret_value();
        assert_eq!(format!("{secret:?}"), "SecretValue(<redacted>)");
    }

    #[test]
    fn edits_unicode_scalars_without_splitting_them() {
        let mut input = SecretInputV1::new();
        input.push('a').unwrap();
        input.push('界').unwrap();
        input.push('🦀').unwrap();
        assert!(input.pop());
        assert_eq!(input.bytes.len(), 4);
        assert!(input.pop());
        assert_eq!(input.bytes.len(), 1);
        assert!(input.pop());
        assert!(!input.pop());
        assert!(input.is_empty());
    }

    #[test]
    fn limit_refuses_without_partial_scalar_write() {
        let mut input = SecretInputV1::with_max_bytes(3);
        input.push('界').unwrap();
        assert_eq!(input.push('a'), Err(SecretInputErrorV1::Limit));
        assert!(input.pop());
        assert!(input.is_empty());
    }

    #[test]
    fn cancel_error_and_unwind_drop_the_masked_buffer() {
        let mut cancelled = SecretInputV1::new();
        cancelled.push('秘').unwrap();
        cancelled.cancel();

        let mut limited = SecretInputV1::with_max_bytes(0);
        assert_eq!(limited.push('x'), Err(SecretInputErrorV1::Limit));
        drop(limited);

        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let mut input = SecretInputV1::new();
            input.push('密').unwrap();
            panic!("synthetic cancellation");
        }));
        assert!(unwind.is_err());
    }

    #[test]
    fn transfers_only_to_the_credential_store_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().canonicalize().unwrap();
        let store = Store::open(&home).unwrap();
        let mut input = SecretInputV1::new();
        for value in "synthetic-secret".chars() {
            input.push(value).unwrap();
        }
        store
            .update_secret(0, "credential", Some(input.into_secret_value()))
            .unwrap();
        let stored = store.load_secrets().unwrap().unwrap();
        assert_eq!(stored.resolve("credential").unwrap(), "synthetic-secret");
    }
}
