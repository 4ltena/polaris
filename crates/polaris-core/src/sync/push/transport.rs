//! One explicitly bound C10 network attempt. The owner supplies admission,
//! credential/config revalidation, and durable intent/receipt ordering.
//!
//! An exit-zero push is only transport acceptance. An independently observed
//! exact ref and durable receipt are required before the owner reports success.

use std::ffi::OsString;
use std::fmt;

use polaris_auth::git_credentials::{
    CanonicalHttpsRemoteV1, GitChildFailurePhaseV1, GitChildFailureV1, GitChildOutputV1,
};

use crate::management_effect::ManagementOperationIdV1;
use crate::sync::remote_intent::{
    ExpectedSyncRemoteOidV1, SyncDigestV1, SyncGitObjectIdV1, SyncRemoteReceiptIdV1,
};

pub(super) const C10_REMOTE_REF: &str = "refs/heads/polaris";

/// Only the private production adapter may admit a network child. It must
/// revalidate the same credential material/binding, executable, repository
/// ancestors/config/protection, and current permission before every run.
pub(super) trait NetworkChildV1 {
    fn run(&mut self, args: &[OsString]) -> Result<GitChildOutputV1, NetworkFailureV1>;

    /// Proves ancestry through the bound local repository, never a caller
    /// boolean or a remote query. Neither failure permits a push child.
    fn require_ancestor(
        &mut self,
        expected: &SyncGitObjectIdV1,
        intended: &SyncGitObjectIdV1,
    ) -> Result<(), AncestorFailureV1>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NetworkFailureV1 {
    BeforeSpawn,
    AfterSpawn,
}

impl From<GitChildFailureV1> for NetworkFailureV1 {
    fn from(failure: GitChildFailureV1) -> Self {
        match failure.phase() {
            GitChildFailurePhaseV1::BeforeSpawn => Self::BeforeSpawn,
            GitChildFailurePhaseV1::AfterSpawn => Self::AfterSpawn,
        }
    }
}

impl fmt::Display for NetworkFailureV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BeforeSpawn => "c10_network_failed_before_spawn",
            Self::AfterSpawn => "c10_network_failed_after_spawn",
        })
    }
}

impl std::error::Error for NetworkFailureV1 {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AncestorFailureV1 {
    NotAncestor,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransportFailureV1 {
    AlreadyAttempted,
    ObjectFormatMismatch,
    AncestryRejected,
    AncestryUnavailable,
    Child(NetworkFailureV1),
    PushRejected,
    InvalidObservation,
    RecoveryFetchRejected,
}

impl fmt::Display for TransportFailureV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyAttempted => "c10_transport_already_attempted",
            Self::ObjectFormatMismatch => "c10_transport_object_format_mismatch",
            Self::AncestryRejected => "c10_transport_ancestry_rejected",
            Self::AncestryUnavailable => "c10_transport_ancestry_unavailable",
            Self::Child(NetworkFailureV1::BeforeSpawn) => "c10_network_failed_before_spawn",
            Self::Child(NetworkFailureV1::AfterSpawn) => "c10_network_failed_after_spawn",
            Self::PushRejected => "c10_transport_push_rejected",
            Self::InvalidObservation => "c10_transport_invalid_observation",
            Self::RecoveryFetchRejected => "c10_transport_recovery_fetch_rejected",
        })
    }
}

impl std::error::Error for TransportFailureV1 {}

/// A live owner-bound transport. It cannot be cloned or reconstructed from
/// persisted data; restart recovery must use observe/fetch and never send.
pub(super) struct C10CasTransportV1<N: NetworkChildV1> {
    remote: CanonicalHttpsRemoteV1,
    runner: N,
    attempted: bool,
}

impl<N: NetworkChildV1> C10CasTransportV1<N> {
    pub(super) fn new(remote: CanonicalHttpsRemoteV1, runner: N) -> Self {
        Self {
            remote,
            runner,
            attempted: false,
        }
    }

    /// Consumes the one send attempt, including pre-spawn or ancestry failure.
    /// The owner may call this only after durable BeforeSend. Ok means only
    /// the push child returned zero, without a receipt or terminal transition.
    pub(super) fn send_once(
        &mut self,
        intended: &SyncGitObjectIdV1,
        expected: &ExpectedSyncRemoteOidV1,
    ) -> Result<SyncRemoteReceiptIdV1, TransportFailureV1> {
        if self.attempted {
            return Err(TransportFailureV1::AlreadyAttempted);
        }
        self.attempted = true;
        let lease = match expected {
            ExpectedSyncRemoteOidV1::Missing => format!("--force-with-lease={C10_REMOTE_REF}:"),
            ExpectedSyncRemoteOidV1::Present { oid } => {
                if oid.as_str().len() != intended.as_str().len() {
                    return Err(TransportFailureV1::ObjectFormatMismatch);
                }
                self.runner
                    .require_ancestor(oid, intended)
                    .map_err(|failure| match failure {
                        AncestorFailureV1::NotAncestor => TransportFailureV1::AncestryRejected,
                        AncestorFailureV1::Unavailable => TransportFailureV1::AncestryUnavailable,
                    })?;
                format!("--force-with-lease={C10_REMOTE_REF}:{}", oid.as_str())
            }
        };
        let args = [
            OsString::from("push"),
            OsString::from("--porcelain"),
            OsString::from("--no-verify"),
            OsString::from("--no-follow-tags"),
            OsString::from("--recurse-submodules=no"),
            OsString::from(lease),
            OsString::from("--"),
            OsString::from(self.remote.transport_url()),
            OsString::from(format!("{}:{C10_REMOTE_REF}", intended.as_str())),
        ];
        let output = self.runner.run(&args).map_err(TransportFailureV1::Child)?;
        if !output.success() || output.exit_code() != Some(0) {
            return Err(TransportFailureV1::PushRejected);
        }
        let mut receipt = b"polaris-c10-git-transport-receipt-v1\0".to_vec();
        receipt.extend(
            serde_json::to_vec(&(&self.remote, intended, expected))
                .map_err(|_| TransportFailureV1::ObjectFormatMismatch)?,
        );
        SyncRemoteReceiptIdV1::parse(SyncDigestV1::digest(&receipt).as_hex())
            .map_err(|_| TransportFailureV1::ObjectFormatMismatch)
    }

    /// Read-only exact observation. Arbitrary child output stays in its
    /// zeroizing owner; only a validated 40/64-byte OID is copied out.
    pub(super) fn observe_exact(&mut self) -> Result<ExpectedSyncRemoteOidV1, TransportFailureV1> {
        let args = [
            OsString::from("ls-remote"),
            OsString::from("--refs"),
            OsString::from("--exit-code"),
            OsString::from("--"),
            OsString::from(self.remote.transport_url()),
            OsString::from(C10_REMOTE_REF),
        ];
        let output = self.runner.run(&args).map_err(TransportFailureV1::Child)?;
        decode_exact_observation(&output)
    }

    /// Fetches one exact remote ref into the same operation's isolated local
    /// namespace. It never resends. The owner must first observe the intended
    /// OID, and then fsync/reopen/verify the fetched commit and full descriptor
    /// before using it to repair any local ref, receipt, or participation.
    pub(super) fn fetch_exact_for_recovery(
        &mut self,
        operation: &ManagementOperationIdV1,
    ) -> Result<(), TransportFailureV1> {
        // This is the exact P2b physical record identity, not a new operation
        // ID. Management IDs are 16 bytes; their canonical hex hashes to the
        // 64-byte ref component accepted by the repository recovery reader.
        let operation = SyncDigestV1::digest(operation.as_hex().as_bytes()).as_hex();
        let args = [
            OsString::from("fetch"),
            OsString::from("--no-tags"),
            OsString::from("--no-recurse-submodules"),
            OsString::from("--no-write-fetch-head"),
            OsString::from("--no-auto-maintenance"),
            OsString::from("--"),
            OsString::from(self.remote.transport_url()),
            OsString::from(format!(
                "{C10_REMOTE_REF}:refs/polaris/recovery/{operation}"
            )),
        ];
        let output = self.runner.run(&args).map_err(TransportFailureV1::Child)?;
        if !output.success() || output.exit_code() != Some(0) {
            return Err(TransportFailureV1::RecoveryFetchRejected);
        }
        Ok(())
    }
}

/// The absent repository reader and the bound transport share one strict
/// exact-ref decoder. Raw child bytes remain owned by the zeroizing output.
pub(super) fn decode_exact_observation(
    output: &GitChildOutputV1,
) -> Result<ExpectedSyncRemoteOidV1, TransportFailureV1> {
    match output.exit_code() {
        Some(2) if output.stdout().is_empty() => Ok(ExpectedSyncRemoteOidV1::Missing),
        Some(0) if output.success() => {
            let line = output
                .stdout()
                .strip_suffix(b"\n")
                .ok_or(TransportFailureV1::InvalidObservation)?;
            let tab = line
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or(TransportFailureV1::InvalidObservation)?;
            let oid = &line[..tab];
            if !matches!(oid.len(), 40 | 64)
                || !oid
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                || &line[tab + 1..] != C10_REMOTE_REF.as_bytes()
            {
                return Err(TransportFailureV1::InvalidObservation);
            }
            let oid =
                std::str::from_utf8(oid).map_err(|_| TransportFailureV1::InvalidObservation)?;
            Ok(ExpectedSyncRemoteOidV1::Present {
                oid: SyncGitObjectIdV1::parse(oid)
                    .map_err(|_| TransportFailureV1::InvalidObservation)?,
            })
        }
        _ => Err(TransportFailureV1::InvalidObservation),
    }
}

#[cfg(test)]
mod tests;
