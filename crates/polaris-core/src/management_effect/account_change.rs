//! Account management uses the existing auth owner and project effect journal.
//! Only non-secret target identities enter the journal; a failed mutation is
//! unknown and is never automatically repeated.

use super::*;
use crate::project_mutation::ProjectMutationLeaseV1;
use polaris_auth::accounts::{owner::AccountOwner, store::AccountSummary};
use std::{future::Future, sync::Arc};

#[derive(Clone, Serialize)]
pub enum AccountChangeV1 {
    LegacyLogout {
        identity: polaris_auth::store::CredentialIdentity,
    },
    Login {
        label: Option<String>,
    },
    Reauth {
        registration: String,
        generation: u64,
    },
    Unlink {
        registration: String,
        generation: u64,
    },
    Label {
        registration: String,
        generation: u64,
        label: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3a_account_local_confirmation_and_stale_target() {
        // Auth protection pins one HOME per process, including its test child.
        if std::env::var_os("POLARIS_M3A_ACCOUNT_CHILD").is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["management_effect::account_change::tests::m3a_account_local_confirmation_and_stale_target", "--exact", "--nocapture"])
                .env("POLARIS_M3A_ACCOUNT_CHILD", "1").env("HOME", root.path())
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let state = crate::project::state_dir(&project).unwrap();
        let workspace = crate::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let store = polaris_auth::accounts::store::AccountStore::open().unwrap();
        let reservation = store.begin_registration(None).unwrap();
        store.mark_exchange_pending(&reservation).unwrap();
        let (generation, _) = store
            .complete_registration(
                &reservation,
                polaris_auth::Credentials {
                    access_token: "synthetic-access-not-for-network".into(),
                    refresh_token: "synthetic-refresh-not-for-network".into(),
                    account_id: "fixture-account".into(),
                    expires_at: Some(u64::MAX),
                },
            )
            .unwrap();
        let auth = AccountOwner::open(polaris_auth::ISSUER).unwrap();
        let owner = AccountChangeOwnerV1 {
            auth,
            effects: &effects,
            lease: &lease,
        };
        let id = reservation.registration_id;
        let prepared = owner
            .prepare(AccountChangeV1::Label {
                registration: id.clone(),
                generation,
                label: "kept label".into(),
            })
            .unwrap();
        assert_eq!(
            owner.apply_local(&prepared, true).unwrap(),
            Some(id.clone())
        );
        assert_eq!(
            owner.target(&id).unwrap().label.as_deref(),
            Some("kept label")
        );
        let prepared = owner
            .prepare(AccountChangeV1::Unlink {
                registration: id.clone(),
                generation,
            })
            .unwrap();
        assert!(owner.apply_local(&prepared, false).unwrap().is_none());
        assert_eq!(
            owner.target(&id).unwrap().state,
            polaris_auth::accounts::store::AccountState::Ready
        );
        let stale = owner
            .prepare(AccountChangeV1::Label {
                registration: id.clone(),
                generation,
                label: "stale".into(),
            })
            .unwrap();
        store
            .set_label(&id, generation, Some("concurrent label".into()))
            .unwrap();
        assert!(owner.apply_local(&stale, true).is_err());
        assert_eq!(
            owner.target(&id).unwrap().label.as_deref(),
            Some("concurrent label")
        );
        let prepared = owner
            .prepare(AccountChangeV1::Unlink {
                registration: id.clone(),
                generation,
            })
            .unwrap();
        owner.apply_local(&prepared, true).unwrap();
        assert_eq!(
            owner.target(&id).unwrap().state,
            polaris_auth::accounts::store::AccountState::Disconnected
        );
        assert!(
            owner
                .prepare(AccountChangeV1::Label {
                    registration: id,
                    generation,
                    label: "must refuse".into()
                })
                .is_err()
        );
        assert!(effects.blocker().unwrap().is_none());
        assert!(
            effects
                .effects()
                .unwrap()
                .iter()
                .any(|effect| matches!(effect.state, ManagementEffectStateV1::Completed { .. }))
        );
    }
}

impl AccountChangeV1 {
    pub fn target(&self) -> &str {
        match self {
            Self::LegacyLogout { .. } => "legacy default authentication",
            Self::Login { .. } => "new registration (current binding is retained)",
            Self::Reauth { registration, .. }
            | Self::Unlink { registration, .. }
            | Self::Label { registration, .. } => registration,
        }
    }

    fn generation(&self) -> u64 {
        match self {
            Self::LegacyLogout { .. } => 0,
            Self::Login { .. } => 0,
            Self::Reauth { generation, .. }
            | Self::Unlink { generation, .. }
            | Self::Label { generation, .. } => *generation,
        }
    }

    fn inventory(&self) -> &str {
        match self {
            Self::LegacyLogout { .. } => "C07",
            Self::Login { .. } => "C02",
            Self::Reauth { .. } => "C03",
            Self::Unlink { .. } => "C04",
            Self::Label { .. } => "C05",
        }
    }
}

pub struct PreparedAccountChangeV1 {
    change: AccountChangeV1,
    registry_revision: u64,
    effect: PreparedManagementEffectV1,
}

impl PreparedAccountChangeV1 {
    pub fn change(&self) -> &AccountChangeV1 {
        &self.change
    }
}

pub struct AccountChangeOwnerV1<'a> {
    pub auth: Arc<AccountOwner>,
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
}

impl AccountChangeOwnerV1<'_> {
    pub fn target(&self, registration: &str) -> Result<AccountSummary, String> {
        self.auth
            .snapshot()
            .map_err(|_| "Account registry cannot be read".to_owned())?
            .accounts
            .into_iter()
            .find(|account| account.registration_id == registration)
            .ok_or_else(|| "Registration does not exist".to_owned())
    }

    pub fn prepare(&self, change: AccountChangeV1) -> Result<PreparedAccountChangeV1, String> {
        let snapshot = self
            .auth
            .snapshot()
            .map_err(|_| "Account registry cannot be read")?;
        if !matches!(
            change,
            AccountChangeV1::Login { .. } | AccountChangeV1::LegacyLogout { .. }
        ) {
            let target = snapshot
                .accounts
                .iter()
                .find(|a| a.registration_id == change.target())
                .ok_or("Registration does not exist")?;
            if target.generation != change.generation() {
                return Err("Stale registration".into());
            }
        }
        if let AccountChangeV1::LegacyLogout { identity } = &change {
            let path =
                polaris_auth::store::default_path().map_err(|_| "Cannot locate legacy auth")?;
            if polaris_auth::store::credential_identity(&path)
                .map_err(|_| "Cannot inspect legacy auth")?
                .as_ref()
                != Some(identity)
            {
                return Err("Legacy credential store changed".into());
            }
        }
        if let AccountChangeV1::Label { label, .. } = &change {
            if label.len() > 256 || label.chars().any(char::is_control) {
                return Err("Invalid account label".into());
            }
        }
        if let AccountChangeV1::Login { label: Some(label) } = &change {
            if label.len() > 256 || label.chars().any(char::is_control) {
                return Err("Invalid account label".into());
            }
        }
        let request_hash = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&change).map_err(|_| "Invalid account request")?,
        );
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random()
                .map_err(|_| "Cannot reserve operation identity")?,
            inventory: ManagementInventoryIdV1::parse(change.inventory())
                .map_err(|_| "Invalid inventory")?,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: None,
                generation: change.generation(),
                revision: snapshot.registry_revision,
                permission: ManagementPermissionScopeV1::System,
                account_binding: if matches!(change, AccountChangeV1::Login { .. }) {
                    None
                } else {
                    Some(
                        ManagementEffectHashV1::account_binding("codex", change.target())
                            .map_err(|_| "Invalid binding")?,
                    )
                },
            },
            request_hash,
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let prepared = self
            .effects
            .prepare(
                self.lease,
                self.effects.cursor().map_err(|e| e.to_string())?,
                request,
            )
            .map_err(|e| e.to_string())?;
        let PrepareManagementEffectOutcomeV1::Prepared(effect) = prepared else {
            return Err("Operation already exists".into());
        };
        Ok(PreparedAccountChangeV1 {
            change,
            registry_revision: snapshot.registry_revision,
            effect,
        })
    }

    fn start(
        &self,
        prepared: &PreparedAccountChangeV1,
        accepted: bool,
    ) -> Result<Option<StartedManagementEffectV1>, String> {
        if !accepted {
            self.effects
                .deny(
                    self.lease,
                    &prepared.effect,
                    ManagementDenialReasonV1::UserDeclined,
                )
                .map_err(|e| e.to_string())?;
            return Ok(None);
        }
        let current = self
            .auth
            .snapshot()
            .map_err(|_| "Account registry cannot be read")?;
        if current.registry_revision != prepared.registry_revision {
            self.effects
                .deny(
                    self.lease,
                    &prepared.effect,
                    ManagementDenialReasonV1::StaleRequest,
                )
                .map_err(|e| e.to_string())?;
            return Err("Account registry changed during confirmation".into());
        }
        self.effects
            .start(self.lease, &prepared.effect)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    fn finish(
        &self,
        started: &StartedManagementEffectV1,
        result: Result<String, polaris_auth::AuthError>,
    ) -> Result<String, String> {
        match result {
            Ok(target) => {
                // Auth's owner has persisted the result before returning.
                let snapshot = self
                    .auth
                    .snapshot()
                    .map_err(|_| "Cannot verify account receipt")?;
                let proof = ManagementEffectHashV1::digest(
                    format!("account receipt {} {}", target, snapshot.registry_revision).as_bytes(),
                );
                self.effects
                    .complete_started(self.lease, started, proof)
                    .map_err(|_| "Account result saved; management receipt is unknown")?;
                Ok(target)
            }
            Err(_) => {
                self.effects
                    .mark_outcome_unknown(
                        self.lease,
                        started,
                        ManagementUnknownReasonV1::UnclassifiedEffect,
                    )
                    .map_err(|e| e.to_string())?;
                Err(
                    "Account operation outcome is unknown; inspect before any further mutation"
                        .into(),
                )
            }
        }
    }

    pub fn apply_local(
        &self,
        prepared: &PreparedAccountChangeV1,
        accepted: bool,
    ) -> Result<Option<String>, String> {
        if !matches!(
            prepared.change,
            AccountChangeV1::Unlink { .. }
                | AccountChangeV1::Label { .. }
                | AccountChangeV1::LegacyLogout { .. }
        ) {
            return Err("Browser operation requires async owner".into());
        }
        let Some(started) = self.start(prepared, accepted)? else {
            return Ok(None);
        };
        let result = match &prepared.change {
            AccountChangeV1::LegacyLogout { identity } => polaris_auth::store::default_path()
                .and_then(|path| polaris_auth::store::delete_exact(&path, identity)),
            AccountChangeV1::Unlink {
                registration,
                generation,
            } => self.auth.logout(registration, *generation),
            AccountChangeV1::Label {
                registration,
                generation,
                label,
            } => self
                .auth
                .set_label(registration, *generation, Some(label.clone())),
            _ => unreachable!(),
        };
        self.finish(
            &started,
            result.map(|()| prepared.change.target().to_owned()),
        )
        .map(Some)
    }

    pub async fn apply_browser<C, F>(
        &self,
        prepared: &PreparedAccountChangeV1,
        accepted: bool,
        callback: C,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<Option<String>, String>
    where
        C: FnOnce(String, String) -> F,
        F: Future<Output = Result<String, polaris_auth::AuthError>>,
    {
        if !matches!(
            prepared.change,
            AccountChangeV1::Login { .. } | AccountChangeV1::Reauth { .. }
        ) {
            return Err("Local operation does not open a browser".into());
        }
        let Some(started) = self.start(prepared, accepted)? else {
            return Ok(None);
        };
        let result = match &prepared.change {
            AccountChangeV1::Login { label } => {
                self.auth.register(label.clone(), callback, cancel).await
            }
            AccountChangeV1::Reauth {
                registration,
                generation,
            } => {
                self.auth
                    .reauthenticate(registration, *generation, callback, cancel)
                    .await
            }
            _ => unreachable!(),
        };
        self.finish(&started, result.map(|token| token.registration_id))
            .map(Some)
    }
}
