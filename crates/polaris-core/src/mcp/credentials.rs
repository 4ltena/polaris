//! Typed ownership of MCP credential mutations.
//!
//! Interactive hosts keep masked bytes in `SecretInputV1` until this owner
//! accepts them. They never call the MCP store directly or convert a secret to
//! an ordinary string argument.

use std::fmt;

use polaris_mcp::{secret_input::SecretInputV1, store::Store};

/// A validated MCP credential reference. References are deliberately visible:
/// they identify a stored credential without revealing its value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpSecretReferenceV1(String);

impl McpSecretReferenceV1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, McpCredentialErrorV1> {
        let value = value.into();
        if !(1..=64).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(McpCredentialErrorV1::InvalidReference);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed request for a masked replacement. Its value remains private and is
/// consumed exactly once by the credential owner.
pub struct McpSecretSetRequestV1 {
    expected_secret_revision: u64,
    reference: McpSecretReferenceV1,
    value: SecretInputV1,
}

impl fmt::Debug for McpSecretSetRequestV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpSecretSetRequestV1")
            .field("expected_secret_revision", &self.expected_secret_revision)
            .field("reference", &self.reference)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl McpSecretSetRequestV1 {
    pub fn new(
        expected_secret_revision: u64,
        reference: McpSecretReferenceV1,
        value: SecretInputV1,
    ) -> Self {
        Self {
            expected_secret_revision,
            reference,
            value,
        }
    }
}

/// Typed request for a credential deletion. It contains no credential value.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct McpSecretDeleteRequestV1 {
    expected_secret_revision: u64,
    reference: McpSecretReferenceV1,
}

impl McpSecretDeleteRequestV1 {
    pub fn new(expected_secret_revision: u64, reference: McpSecretReferenceV1) -> Self {
        Self {
            expected_secret_revision,
            reference,
        }
    }
}

/// The resulting durable credential document revision.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct McpCredentialMutationV1 {
    pub secret_revision: u64,
}

/// Closed errors that are safe for normal TUI status and audit projections.
#[derive(Debug, Clone, Copy, Eq, PartialEq, thiserror::Error)]
pub enum McpCredentialErrorV1 {
    #[error("mcp_credential_invalid_reference")]
    InvalidReference,
    #[error("mcp_credential_revision_conflict")]
    RevisionConflict,
    #[error("mcp_credential_limit")]
    Limit,
    #[error("mcp_credential_auth")]
    Auth,
    #[error("mcp_credential_storage")]
    Storage,
}

impl From<polaris_mcp::Error> for McpCredentialErrorV1 {
    fn from(value: polaris_mcp::Error) -> Self {
        match value {
            polaris_mcp::Error::InvalidRegistration => Self::InvalidReference,
            polaris_mcp::Error::RevisionConflict => Self::RevisionConflict,
            polaris_mcp::Error::Limit => Self::Limit,
            polaris_mcp::Error::Auth => Self::Auth,
            polaris_mcp::Error::Storage
            | polaris_mcp::Error::Protection
            | polaris_mcp::Error::RecoveryRequired
            | polaris_mcp::Error::Transport
            | polaris_mcp::Error::Protocol
            | polaris_mcp::Error::Cancelled
            | polaris_mcp::Error::Timeout
            | polaris_mcp::Error::StopUnconfirmed => Self::Storage,
        }
    }
}

/// The only core owner that changes MCP credential values for an interactive
/// host. The store remains responsible for durable CAS and protected writes.
pub struct McpCredentialOwnerV1<'a> {
    store: &'a Store,
}

impl<'a> McpCredentialOwnerV1<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }

    /// Reads the revision to show with a confirmation. Callers must include it
    /// in the following typed request; this does not expose credential values.
    pub fn current_secret_revision(&self) -> Result<u64, McpCredentialErrorV1> {
        Ok(self
            .store
            .load_secrets()?
            .map_or(0, |secrets| secrets.revision()))
    }

    /// Replaces one credential after a preflight revision check. The store
    /// repeats CAS during the write, so a concurrent change fails closed and
    /// the moved zeroizing value drops on every error path.
    pub fn set(
        &self,
        request: McpSecretSetRequestV1,
    ) -> Result<McpCredentialMutationV1, McpCredentialErrorV1> {
        self.require_revision(request.expected_secret_revision)?;
        let next = request
            .expected_secret_revision
            .checked_add(1)
            .ok_or(McpCredentialErrorV1::Limit)?;
        self.store.update_secret(
            request.expected_secret_revision,
            request.reference.as_str(),
            Some(request.value.into_secret_value()),
        )?;
        Ok(McpCredentialMutationV1 {
            secret_revision: next,
        })
    }

    /// Deletes one credential after the same revision preflight and durable
    /// store CAS. A missing reference is reported without creating state.
    pub fn delete(
        &self,
        request: McpSecretDeleteRequestV1,
    ) -> Result<McpCredentialMutationV1, McpCredentialErrorV1> {
        self.require_revision(request.expected_secret_revision)?;
        let next = request
            .expected_secret_revision
            .checked_add(1)
            .ok_or(McpCredentialErrorV1::Limit)?;
        self.store.update_secret(
            request.expected_secret_revision,
            request.reference.as_str(),
            None,
        )?;
        Ok(McpCredentialMutationV1 {
            secret_revision: next,
        })
    }

    fn require_revision(&self, expected_secret_revision: u64) -> Result<(), McpCredentialErrorV1> {
        if self.current_secret_revision()? != expected_secret_revision {
            return Err(McpCredentialErrorV1::RevisionConflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().canonicalize().unwrap();
        let store = Store::open(&home).unwrap();
        (directory, store)
    }

    fn reference() -> McpSecretReferenceV1 {
        McpSecretReferenceV1::parse("credential").unwrap()
    }

    fn input(value: &str) -> SecretInputV1 {
        let mut input = SecretInputV1::new();
        for character in value.chars() {
            input.push(character).unwrap();
        }
        input
    }

    #[test]
    fn set_and_delete_use_the_store_cas_with_exact_revisions() {
        let (_directory, store) = store();
        let owner = McpCredentialOwnerV1::new(&store);
        assert_eq!(owner.current_secret_revision().unwrap(), 0);
        assert_eq!(
            owner
                .set(McpSecretSetRequestV1::new(
                    0,
                    reference(),
                    input("synthetic")
                ))
                .unwrap(),
            McpCredentialMutationV1 { secret_revision: 1 }
        );
        assert_eq!(owner.current_secret_revision().unwrap(), 1);
        assert_eq!(
            owner
                .delete(McpSecretDeleteRequestV1::new(1, reference()))
                .unwrap(),
            McpCredentialMutationV1 { secret_revision: 2 }
        );
        assert_eq!(owner.current_secret_revision().unwrap(), 2);
    }

    #[test]
    fn stale_set_refuses_without_persisting_the_masked_value() {
        let (_directory, store) = store();
        let owner = McpCredentialOwnerV1::new(&store);
        owner
            .set(McpSecretSetRequestV1::new(0, reference(), input("first")))
            .unwrap();
        let stale = McpSecretSetRequestV1::new(0, reference(), input("second"));
        assert_eq!(
            owner.set(stale),
            Err(McpCredentialErrorV1::RevisionConflict)
        );
        assert_eq!(
            store
                .load_secrets()
                .unwrap()
                .unwrap()
                .resolve("credential")
                .unwrap(),
            "first"
        );
    }

    #[test]
    fn rejects_invalid_references_and_redacts_typed_requests() {
        assert_eq!(
            McpSecretReferenceV1::parse("bad id"),
            Err(McpCredentialErrorV1::InvalidReference)
        );
        let request = McpSecretSetRequestV1::new(7, reference(), input("synthetic-secret"));
        let debug = format!("{request:?}");
        assert!(!debug.contains("synthetic-secret"));
        assert!(!debug.contains("16"));
    }

    #[test]
    fn stale_preflight_drops_the_masked_value() {
        let (_directory, store) = store();
        let first = McpCredentialOwnerV1::new(&store);
        let second = McpCredentialOwnerV1::new(&store);
        first
            .set(McpSecretSetRequestV1::new(0, reference(), input("first")))
            .unwrap();
        let stale = McpSecretSetRequestV1::new(0, reference(), input("second"));
        assert_eq!(
            second.set(stale),
            Err(McpCredentialErrorV1::RevisionConflict)
        );
    }
}
