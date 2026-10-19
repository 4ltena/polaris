//! An unsent draft belongs to its conversation and never enters raw history.
use polaris_core::{
    management_effect::{ManagementEffectStoreV1, ManagementOperationIdV1},
    rewind_workspace::WorkspaceOwner,
    session::Session,
    session_settings::{SavedSessionSettingsV1, SessionSettingsCommandV1, SessionSettingsOwnerV1},
};

pub(crate) fn save(
    session: &Session,
    initial: &SavedSessionSettingsV1,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    text: &str,
) -> Result<(), String> {
    if crate::is_secret_command(text) {
        return Err("秘密情報は下書きに保存できません".into());
    }
    // A local command is not an unsent model input. Preserve the previous
    // draft while the user operates a picker or types a management command.
    if text.trim_start().starts_with('/') {
        return Ok(());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let saved = session
        .persistence
        .as_ref()
        .ok_or("下書き保存にはv2会話が必要です")?;
    let snapshot = saved.snapshot().map_err(|e| e.to_string())?;
    let text = if text.trim().is_empty() { "" } else { text };
    if snapshot.state.draft.text == text {
        return Ok(());
    }
    let lease = workspace.try_mutation().map_err(|e| e.to_string())?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let owner = SessionSettingsOwnerV1 {
        effects,
        lease: &lease,
    };
    owner
        .apply(
            saved,
            initial,
            SessionSettingsCommandV1::Draft {
                expected_revision: snapshot.state.draft.revision,
                text: text.to_owned(),
            },
            ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::{conversation_state::HistoryMode, session_store::PersistedSession};

    #[test]
    fn m3c_tui_draft_restores_exact_text_clears_blank_and_keeps_commands_out() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let data = root.path().join("data");
        let state = root.path().join("state");
        for path in [&project, &data, &state] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::create_dir(project.join(".polaris")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let project_id = project.canonicalize().unwrap().to_str().unwrap().to_owned();
        let saved = PersistedSession::create(
            &data,
            &data.join("memory.sqlite3"),
            &project_id,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::default();
        session.attach(saved.clone()).unwrap();
        let initial =
            SavedSessionSettingsV1::new("fake".into(), "model".into(), "auto".into()).unwrap();
        save(&session, &initial, &workspace, &effects, "未送信\nそのまま").unwrap();
        let id = saved.snapshot().unwrap().state.session_id;
        let reopened =
            PersistedSession::open(&data, &data.join("memory.sqlite3"), &project_id, &id).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().state.draft.text,
            "未送信\nそのまま"
        );
        assert!(reopened.snapshot().unwrap().events.is_empty());
        save(&session, &initial, &workspace, &effects, "/status").unwrap();
        assert_eq!(
            saved.snapshot().unwrap().state.draft.text,
            "未送信\nそのまま"
        );
        save(&session, &initial, &workspace, &effects, " \n\t").unwrap();
        assert!(saved.snapshot().unwrap().state.draft.text.is_empty());
        assert!(
            save(
                &session,
                &initial,
                &workspace,
                &effects,
                "/mcp secret set s k secret"
            )
            .is_err()
        );
        assert!(saved.snapshot().unwrap().state.draft.text.is_empty());

        // Terminal-only management cannot authorize writes when a C10 file is
        // torn. The actual draft owner must check under its newly acquired EX.
        let sync = state.join("sync");
        let intents = sync.join("remote-intents-v2");
        for directory in [&sync, &intents, &intents.join("archive")] {
            std::fs::create_dir(directory).unwrap();
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for (path, bytes) in [
            (intents.join("owner.lock"), b"".as_slice()),
            (
                intents.join(format!("{}.json", "a".repeat(64))),
                b"{torn".as_slice(),
            ),
        ] {
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let before = saved.snapshot().unwrap();
        let cursor = effects.cursor().unwrap();
        assert!(
            save(
                &session,
                &initial,
                &workspace,
                &effects,
                "must stay unsaved"
            )
            .is_err()
        );
        assert_eq!(saved.snapshot().unwrap().state, before.state);
        assert_eq!(effects.cursor().unwrap(), cursor);
        assert_eq!(
            std::fs::read(intents.join(format!("{}.json", "a".repeat(64)))).unwrap(),
            b"{torn"
        );
    }
}
