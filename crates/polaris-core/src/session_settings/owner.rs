//! Preview and commit session settings through the management-effect owner.

use super::*;
use crate::{
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementEffectStateV1, ManagementEffectStoreError, ManagementEffectStoreV1,
        ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
        ManagementRewindDispositionV1, ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1,
        PrepareManagementEffectV1, PreparedManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
};

#[derive(Clone, Debug)]
pub struct SessionSettingsPreviewV1 {
    inventory: &'static str,
    before: ConversationSnapshot,
    candidate: ConversationStateV2,
    before_hash: String,
    after_hash: String,
    revision: u64,
}

impl SessionSettingsPreviewV1 {
    pub fn inventory(&self) -> &'static str {
        self.inventory
    }
    pub fn before_generation(&self) -> u64 {
        self.before.state.generation
    }
    pub fn session_id(&self) -> &str {
        &self.before.state.session_id
    }
    pub fn candidate(&self) -> &ConversationStateV2 {
        &self.candidate
    }
}

#[derive(Clone, Debug)]
pub struct PreparedSessionSettingsV1 {
    effect: PreparedManagementEffectV1,
    preview: SessionSettingsPreviewV1,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionSettingsError {
    #[error("Session storage: {0}")]
    Session(#[from] io::Error),
    #[error("Management effect: {0}")]
    Effect(#[from] ManagementEffectStoreError),
    #[error("Settings already have the same values")]
    NoChange,
    #[error("Session state changed after confirmation")]
    StalePreview,
    #[error("This management operation has already started or finalized. It will not be rerun")]
    ExistingOperation,
}

pub struct SessionSettingsOwnerV1<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
}

impl SessionSettingsOwnerV1<'_> {
    /// `initial` is the host's effective startup configuration, including the
    /// actual current provider. Saved values take precedence after restart.
    pub fn preview(
        session: &PersistedSession,
        initial: &SavedSessionSettingsV1,
        command: SessionSettingsCommandV1,
    ) -> Result<SessionSettingsPreviewV1, SessionSettingsError> {
        let before = session.snapshot()?;
        if before.raw_offset != before.state.raw_commit_offset
            || before.raw_hash != before.state.raw_commit_hash
        {
            return Err(
                invalid("Unpublished source text exists before session configuration").into(),
            );
        }
        let inventory = command.inventory();
        let mut candidate = before.state.clone();
        let revision = match command {
            SessionSettingsCommandV1::Draft {
                expected_revision,
                text,
            } => {
                cas(expected_revision, before.state.draft.revision)?;
                if text.len() > MAX_DRAFT_BYTES {
                    return Err(invalid("The draft exceeds the storage limit").into());
                }
                let text = if text.trim().is_empty() {
                    String::new()
                } else {
                    text
                };
                if text == before.state.draft.text {
                    return Err(SessionSettingsError::NoChange);
                }
                candidate.draft = SavedSessionDraftV1 {
                    revision: advance(expected_revision)?,
                    text,
                };
                expected_revision
            }
            SessionSettingsCommandV1::Goal {
                expected_revision,
                operation,
            } => {
                candidate.goal = Some(change_goal(
                    before.state.goal.as_ref(),
                    expected_revision,
                    operation,
                )?);
                expected_revision
            }
            command => {
                initial.validate()?;
                let mut settings = before
                    .state
                    .session_settings
                    .clone()
                    .unwrap_or_else(|| initial.clone());
                let revision = settings.revision;
                match command {
                    SessionSettingsCommandV1::RoleBindings {
                        expected_revision,
                        bindings,
                    } => {
                        cas(expected_revision, revision)?;
                        settings.role_bindings = bindings;
                        settings.role_bindings_active = true;
                    }
                    SessionSettingsCommandV1::Remember { enabled } => settings.remember = enabled,
                    SessionSettingsCommandV1::ToolMemory { mode } => settings.tool_memory = mode,
                    SessionSettingsCommandV1::EmbeddingEndpoint { embedding }
                    | SessionSettingsCommandV1::EmbeddingModel { embedding } => {
                        settings.tool_memory_embedding = embedding
                    }
                    SessionSettingsCommandV1::CompactThreshold { threshold } => {
                        settings.compact_threshold = threshold
                    }
                    SessionSettingsCommandV1::Configure {
                        expected_revision,
                        provider,
                        model,
                        effort,
                    } => {
                        cas(expected_revision, revision)?;
                        if provider != initial.provider {
                            return Err(invalid("Changing the provider requires a restart").into());
                        }
                        if !matches!(effort.as_str(), "" | "auto") && effort != initial.effort {
                            return Err(invalid(
                                "Cannot switch to a fixed effort during an interactive session",
                            )
                            .into());
                        }
                        settings.provider = provider;
                        settings.model = model;
                        settings.effort = effort;
                    }
                    SessionSettingsCommandV1::History { mode } => {
                        if mode != before.state.history_mode {
                            validate_empty_history(&before)?;
                            session
                                .store
                                .lock()?
                                .validate_history_initialization(&before)?;
                            candidate.history_mode = mode;
                        }
                    }
                    _ => unreachable!("draft and goal handled above"),
                }
                settings.validate()?;
                if before.state.session_settings.as_ref() == Some(&settings)
                    && before.state.history_mode == candidate.history_mode
                {
                    return Err(SessionSettingsError::NoChange);
                }
                settings.revision = advance(revision)?;
                candidate.session_settings = Some(settings);
                revision
            }
        };
        candidate.validate()?;
        let before_hash = state_hash(&before.state)?;
        let mut published = candidate.clone();
        published.generation = advance(before.state.generation)?;
        published.recent_turn_ids = before
            .active_turns()
            .into_iter()
            .rev()
            .take(crate::conversation_state::REAL_TURN_WINDOW)
            .collect();
        published.recent_turn_ids.reverse();
        let after_hash = state_hash(&published)?;
        Ok(SessionSettingsPreviewV1 {
            inventory,
            before,
            candidate,
            before_hash,
            after_hash,
            revision,
        })
    }

    pub fn prepare(
        &self,
        session: &PersistedSession,
        operation_id: ManagementOperationIdV1,
        preview: &SessionSettingsPreviewV1,
    ) -> Result<PreparedSessionSettingsV1, SessionSettingsError> {
        check_preimage(session, preview, self.effects)?;
        let permission = if matches!(preview.inventory, "F07" | "F08" | "F09") {
            ManagementPermissionScopeV1::Project
        } else {
            ManagementPermissionScopeV1::LocalSession
        };
        let rewind = if matches!(preview.inventory, "D17" | "D20") {
            ManagementRewindDispositionV1::CapturedRestorable
        } else {
            ManagementRewindDispositionV1::IrreversibleLocal
        };
        let request = PrepareManagementEffectV1 {
            operation_id,
            inventory: ManagementInventoryIdV1::parse(preview.inventory)
                .expect("fixed session setting inventory"),
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: Some(ManagementEffectHashV1::digest(
                    preview.session_id().as_bytes(),
                )),
                generation: preview.before_generation(),
                revision: preview.revision,
                permission,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(
                format!(
                    "polaris-session-change-v1:{}:{}:{}",
                    preview.inventory, preview.before_hash, preview.after_hash
                )
                .as_bytes(),
            ),
            rewind,
        };
        match self
            .effects
            .prepare(self.lease, self.effects.cursor()?, request)?
        {
            PrepareManagementEffectOutcomeV1::Prepared(effect) => Ok(PreparedSessionSettingsV1 {
                effect,
                preview: preview.clone(),
            }),
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                Err(SessionSettingsError::ExistingOperation)
            }
        }
    }

    pub fn deny(&self, prepared: &PreparedSessionSettingsV1) -> Result<(), SessionSettingsError> {
        self.effects.deny(
            self.lease,
            &prepared.effect,
            ManagementDenialReasonV1::UserDeclined,
        )?;
        Ok(())
    }

    pub fn execute(
        &self,
        session: &PersistedSession,
        prepared: &PreparedSessionSettingsV1,
    ) -> Result<ConversationSnapshot, SessionSettingsError> {
        if let Err(error) = check_preimage(session, &prepared.preview, self.effects) {
            self.effects.deny(
                self.lease,
                &prepared.effect,
                ManagementDenialReasonV1::StaleRequest,
            )?;
            return Err(error);
        }
        let started = self.effects.start(self.lease, &prepared.effect)?;
        let result = session.publish_session_change(
            prepared.preview.before_generation(),
            &prepared.preview.before_hash,
            prepared.preview.candidate.clone(),
        );
        let published = match result {
            Ok(published) => published,
            Err(error) => {
                let _ = self.effects.mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                );
                return Err(error.into());
            }
        };
        if state_hash(&published.state)? != prepared.preview.after_hash
            || published.raw_offset != prepared.preview.before.raw_offset
            || published.raw_hash != prepared.preview.before.raw_hash
        {
            let _ = self.effects.mark_outcome_unknown(
                self.lease,
                &started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            );
            return Err(SessionSettingsError::StalePreview);
        }
        let evidence = ManagementEffectHashV1::digest(
            format!(
                "polaris-session-receipt-v1:{}:{}",
                prepared.effect.operation_id(),
                prepared.preview.after_hash
            )
            .as_bytes(),
        );
        let completion = self
            .effects
            .complete_started(self.lease, &started, evidence);
        if !self.effects.effect(prepared.effect.operation_id())?.is_some_and(|effect| matches!(effect.state, ManagementEffectStateV1::Completed { evidence_hash } if evidence_hash == evidence)) {
            let _ = self.effects.mark_outcome_unknown(self.lease, &started, ManagementUnknownReasonV1::ReceiptPersistenceFailed);
            completion?;
            return Err(SessionSettingsError::StalePreview);
        }
        Ok(published)
    }

    pub fn apply(
        &self,
        session: &PersistedSession,
        initial: &SavedSessionSettingsV1,
        command: SessionSettingsCommandV1,
        operation_id: ManagementOperationIdV1,
    ) -> Result<ConversationSnapshot, SessionSettingsError> {
        let preview = Self::preview(session, initial, command)?;
        let prepared = self.prepare(session, operation_id, &preview)?;
        self.execute(session, &prepared)
    }
}

fn state_hash(state: &ConversationStateV2) -> io::Result<String> {
    Ok(content_hash(
        &serde_json::to_vec(state).map_err(io::Error::other)?,
    ))
}

fn check_preimage(
    session: &PersistedSession,
    preview: &SessionSettingsPreviewV1,
    effects: &ManagementEffectStoreV1,
) -> Result<(), SessionSettingsError> {
    let current = session.snapshot()?;
    if ManagementEffectHashV1::digest(current.state.project_id.as_bytes())
        != effects.project_scope_hash()
        || std::fs::canonicalize(session.store.directory())?
            != std::fs::canonicalize(
                session
                    .data_root
                    .join("sessions-v2")
                    .join(&current.state.session_id),
            )?
    {
        return Err(invalid("Management effect and target session project mismatch").into());
    }
    if current.state != preview.before.state
        || current.raw_offset != preview.before.raw_offset
        || current.raw_hash != preview.before.raw_hash
    {
        return Err(SessionSettingsError::StalePreview);
    }
    Ok(())
}
