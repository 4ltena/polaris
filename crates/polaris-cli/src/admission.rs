//! One-shot CLI admission owner. The manager commit precedes MCP prompt
//! retrieval, raw input, and provider execution.

use polaris_core::{
    desktop_store::SessionManagerStore,
    session::Session,
    session_manager::{
        AdmissionId, AdmissionState, DestinationMode, NextRunnableV1, SessionId, SessionManagerV1,
        SubmissionPayloadV1, SubmitOutcomeV1, SubmitRequestV1, TerminalStatus,
    },
};
use polaris_provider::Message;
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path};

pub struct CliAdmission {
    store: SessionManagerStore,
    state: SessionManagerV1,
    id: AdmissionId,
    payload_hash: String,
    current: SessionId,
}

impl CliAdmission {
    pub fn reserve(state_dir: &Path, session: &Session, text: &str) -> Result<Self, String> {
        let persistence = session
            .persistence
            .as_ref()
            .ok_or("CLI入力にはv2セッションが必要です")?;
        let snapshot = persistence.snapshot().map_err(|error| error.to_string())?;
        let current =
            SessionId::new(snapshot.state.session_id.clone()).map_err(|error| error.to_string())?;
        let family_root = state_dir.join("tui-admissions-v1");
        fs::create_dir_all(&family_root).map_err(|error| error.to_string())?;
        fs::set_permissions(&family_root, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        let family_root = fs::canonicalize(family_root).map_err(|error| error.to_string())?;
        let (store, mut state) = SessionManagerStore::open_family_owned(&family_root, &current)
            .map_err(|error| error.to_string())?;
        polaris_tui::recover_admission_owner(
            &store,
            &mut state,
            &persistence.data_root,
            &persistence.database,
            &snapshot.state.project_id,
        )?;
        if let Some(blocked) = state.outcome_unknown_blocker(&current) {
            return Err(format!(
                "入力 {} の結果が不明です。この会話では新しい入力を開始できません",
                blocked.id.as_str()
            ));
        }
        let revision = snapshot.state.generation;
        let goal_revision = session
            .workflow
            .as_ref()
            .map_or(0, |workflow| workflow.state().revision);
        let request = SubmitRequestV1 {
            parent_session: current.clone(),
            payload: SubmissionPayloadV1 {
                text: text.to_owned(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: revision,
                expected_goal_revision: goal_revision,
            },
            admission_id: Some(
                AdmissionId::new(format!(
                    "cli-{}",
                    polaris_core::session_store::new_session_id()
                        .map_err(|error| error.to_string())?
                ))
                .map_err(|error| error.to_string())?,
            ),
        };
        let before = state.revision;
        state
            .observe_session_revisions(&current, before, revision, goal_revision)
            .map_err(|error| error.to_string())?;
        let SubmitOutcomeV1::Accepted { admission, .. } = state
            .submit(request, state.revision)
            .map_err(|error| match error {
                polaris_core::session_manager::AdmissionError::Capacity(_) => {
                    "この会話の保存容量が不足しています。処理が静止したら --fork で継続してください"
                        .to_owned()
                }
                _ => error.to_string(),
            })?;
        let mut receipt_counts =
            BTreeMap::from([(current.clone(), snapshot.state.admission_receipts.len())]);
        for session_id in state
            .sessions
            .keys()
            .filter(|id| id.as_str() != current.as_str())
        {
            let count = match polaris_core::session_store::PersistedSession::open(
                &persistence.data_root,
                &persistence.database,
                &snapshot.state.project_id,
                session_id.as_str(),
            ) {
                Ok(saved) => saved
                    .snapshot()
                    .map_err(|error| error.to_string())?
                    .state
                    .admission_receipts
                    .len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error.to_string()),
            };
            receipt_counts.insert(session_id.clone(), count);
        }
        state
            .check_receipt_capacity(&receipt_counts)
            .map_err(|error| match error {
                polaris_core::session_manager::AdmissionError::Capacity(_) => {
                    "この会話の保存容量が不足しています。処理が静止したら --fork で継続してください"
                        .to_owned()
                }
                _ => error.to_string(),
            })?;
        store
            .persist_observed_submission(
                before,
                &state,
                &receipt_counts,
                &current,
                revision,
                goal_revision,
            )
            .map_err(|error| match error {
                polaris_core::desktop_store::StoreError::Capacity(_) => {
                    "この会話の保存容量が不足しています。処理が静止したら --fork で継続してください"
                        .to_owned()
                }
                _ => error.to_string(),
            })?;
        let before = state.revision;
        let decided = state
            .decide(
                &admission.id,
                before,
                admission.revision,
                None,
                false,
                false,
            )
            .map_err(|error| error.to_string())?;
        store
            .persist(before, &state)
            .map_err(|error| error.to_string())?;
        let before = state.revision;
        state
            .prepare_destination(&admission.id, before, decided.revision)
            .map_err(|error| error.to_string())?;
        store
            .persist(before, &state)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            store,
            state,
            id: admission.id,
            payload_hash: admission.payload_hash,
            current,
        })
    }

    pub fn cancel_before_append(&mut self) -> Result<(), String> {
        let admission = self
            .state
            .status(&self.id)
            .map_err(|error| error.to_string())?
            .clone();
        let before = self.state.revision;
        self.state
            .cancel(&self.id, before, admission.revision)
            .map_err(|error| error.to_string())?;
        self.store
            .persist(before, &self.state)
            .map_err(|error| error.to_string())
    }

    pub fn mark_append_unknown(&mut self) -> Result<(), String> {
        let admission = self
            .state
            .status(&self.id)
            .map_err(|error| error.to_string())?
            .clone();
        let before = self.state.revision;
        self.state
            .fail_preflight(
                &self.id,
                before,
                admission.revision,
                TerminalStatus::OutcomeUnknown,
            )
            .map_err(|error| error.to_string())?;
        self.store
            .persist(before, &self.state)
            .map_err(|error| error.to_string())
    }

    pub fn append_and_start(
        &mut self,
        session: &mut Session,
        text: &str,
        supplement: Option<Message>,
    ) -> Result<(), String> {
        let admitted = match supplement {
            Some(message) => session.push_admitted_user_with_supplement(
                text,
                message,
                &self.id,
                &self.payload_hash,
            ),
            None => session.push_admitted_user(text, &self.id, &self.payload_hash),
        };
        match admitted {
            Ok(true) => {}
            Ok(false) => return Err("admission入力は既に保存されています".into()),
            Err(error) => {
                self.mark_append_unknown()?;
                return Err(error.to_string());
            }
        }
        let admission = self
            .state
            .status(&self.id)
            .map_err(|error| error.to_string())?
            .clone();
        if !matches!(admission.state, AdmissionState::DestinationReady { .. }) {
            return Err("CLI admission destinationが不正です".into());
        }
        let before = self.state.revision;
        let appended = self
            .state
            .record_input_appended(&self.id, before, admission.revision, &self.current)
            .map_err(|error| error.to_string())?;
        self.store
            .persist(before, &self.state)
            .map_err(|error| error.to_string())?;
        let before = self.state.revision;
        self.state
            .queue_run(&self.id, before, appended.revision)
            .map_err(|error| error.to_string())?;
        self.store
            .persist(before, &self.state)
            .map_err(|error| error.to_string())?;
        let before = self.state.revision;
        match self
            .state
            .start_next(before)
            .map_err(|error| error.to_string())?
        {
            NextRunnableV1::Start { admission_id, .. } if admission_id == self.id => self
                .store
                .persist(before, &self.state)
                .map_err(|error| error.to_string()),
            _ => Err("CLI admission runを開始できません".into()),
        }
    }

    pub fn finish(&mut self, completed: bool) -> Result<(), String> {
        let admission = self
            .state
            .status(&self.id)
            .map_err(|error| error.to_string())?
            .clone();
        let before = self.state.revision;
        self.state
            .terminal(
                &self.id,
                before,
                admission.revision,
                if completed {
                    TerminalStatus::Completed
                } else {
                    TerminalStatus::OutcomeUnknown
                },
            )
            .map_err(|error| error.to_string())?;
        self.store
            .persist(before, &self.state)
            .map_err(|error| error.to_string())
    }
}
