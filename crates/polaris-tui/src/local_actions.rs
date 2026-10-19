//! Confirmed local actions share the project lease and management journal.
use crate::{account, approver::KeyReader, slash::Action};
use polaris_core::{management_effect::*, rewind_workspace::WorkspaceOwner, session::Session};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{
    cell::RefCell,
    fs::File,
    io::{Read, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

fn destination(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().len() > 4096 {
        return Err("保存先が長すぎます".into());
    }
    let name = path.file_name().ok_or("保存先のファイル名がありません")?;
    let parent = path
        .parent()
        .ok_or("保存先の親がありません")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let path = parent.join(name);
    if path.components().any(|p| p.as_os_str() == ".polaris") {
        return Err("Polarisの状態領域へexportできません".into());
    }
    polaris_auth::protection::with_protected_paths(|protected| {
        if protected
            .paths()
            .iter()
            .any(|saved| path == *saved || path.starts_with(saved))
        {
            Err("認証の保護対象へ保存できません".into())
        } else {
            Ok(path)
        }
    })
    .map_err(|_| "認証保護を確認できません".to_string())?
}
fn write_new(path: &Path, body: &str) -> Result<(), String> {
    polaris_auth::protection::with_protected_paths(|protected| {
        let parent = path.parent().ok_or("保存先の親がありません")?;
        let directory = polaris_core::project_mutation::open_directory_nofollow(parent)
            .map_err(|e| e.to_string())?;
        let expected = std::fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
        let actual = directory.metadata().map_err(|e| e.to_string())?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err("保存先が変更されました".into());
        }
        if protected
            .identities()
            .iter()
            .any(|id| id.dev == actual.dev() && id.ino == actual.ino())
        {
            return Err("保存先のdirectoryは認証の保護対象です".into());
        }
        let name = std::ffi::CString::new(
            path.file_name()
                .ok_or("保存先の名前がありません")?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let created = file.metadata().map_err(|e| e.to_string())?;
        if protected
            .identities()
            .iter()
            .any(|id| id.dev == created.dev() && id.ino == created.ino())
        {
            return Err("新しいfile inodeが認証の保護対象と衝突しました".into());
        }
        file.write_all(body.as_bytes())
            .and_then(|_| file.sync_all())
            .and_then(|_| directory.sync_all())
            .map_err(|e| e.to_string())?;
        file.rewind().map_err(|e| e.to_string())?;
        let mut readback = Vec::new();
        Read::by_ref(&mut file)
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut readback)
            .map_err(|e| e.to_string())?;
        let named = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if readback != body.as_bytes()
            || !named.is_file()
            || named.nlink() != 1
            || (named.dev(), named.ino()) != (created.dev(), created.ino())
        {
            return Err("保存後のfile identityまたは本文を確認できません".into());
        }
        Ok(())
    })
    .map_err(|_| "認証保護を確認できません".to_string())?
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle<B: Backend, R: KeyReader>(
    action: &Action,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    session: &mut Session,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    cwd: &Path,
    has_pending: bool,
    copy: impl FnOnce(&str) -> Result<(), String>,
) -> Result<Vec<Line<'static>>, String> {
    handle_with_selection(
        action,
        terminal,
        reader,
        session,
        workspace,
        effects,
        cwd,
        has_pending,
        None,
        copy,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_with_selection<B: Backend, R: KeyReader>(
    action: &Action,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    session: &mut Session,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    cwd: &Path,
    has_pending: bool,
    selected: Option<&str>,
    copy: impl FnOnce(&str) -> Result<(), String>,
) -> Result<Vec<Line<'static>>, String> {
    if has_pending {
        return Err("送信待ちの入力があるため管理操作を実行できません".into());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let saved = session
        .persistence
        .clone()
        .ok_or("保存されたv2会話が必要です")?;
    let before = saved.snapshot().map_err(|e| e.to_string())?;
    if ManagementEffectHashV1::digest(before.state.project_id.as_bytes())
        != effects.project_scope_hash()
    {
        return Err("会話のprojectが一致しません".into());
    }
    let (inventory, permission, target, body) = match action {
        Action::Clear => ("T08", ManagementPermissionScopeV1::LocalSession, None, None),
        Action::Init => (
            "T10",
            ManagementPermissionScopeV1::Project,
            Some(destination(&workspace.project_root().join("AGENTS.md"))?),
            Some(crate::agents_md_template()),
        ),
        Action::Export(request) => {
            let path = if request.is_empty() {
                cwd.join(format!("polaris-export-{}.md", crate::now_millis()))
            } else {
                cwd.join(request)
            };
            let target = destination(&path)?;
            let permission = if target.starts_with(workspace.project_root()) {
                ManagementPermissionScopeV1::Project
            } else {
                ManagementPermissionScopeV1::System
            };
            let messages = before
                .events
                .iter()
                .map(|event| event.message.clone())
                .collect::<Vec<_>>();
            let snapshot_text = crate::export_messages(&messages);
            ("T17", permission, Some(target), Some(snapshot_text))
        }
        Action::Copy => (
            "T19",
            ManagementPermissionScopeV1::Clipboard,
            None,
            Some(
                selected
                    .or_else(|| crate::clipboard::last_agent_message_text(session))
                    .ok_or("コピーできる返答がありません")?
                    .to_owned(),
            ),
        ),
        _ => return Err("この管理操作は対象外です".into()),
    };
    if body
        .as_ref()
        .is_some_and(|body| body.len() > 16 * 1024 * 1024)
    {
        return Err("保存内容が16MiBを超えています".into());
    }
    if target
        .as_ref()
        .is_some_and(|path| std::fs::symlink_metadata(path).is_ok())
    {
        return Err("保存先が既に存在します。別の名前を指定してください".into());
    }
    let lease = workspace.try_mutation().map_err(|e| e.to_string())?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|e| e.to_string())?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let detail = match action {
        Action::Clear => format!(
            "{}原文の現在epochをclearします。過去の証拠は保持されます。",
            before.events.len()
        ),
        Action::Copy => format!(
            "{} bytesをclipboardへ渡します。取り消せない効果です。",
            body.as_ref().expect("copy body").len()
        ),
        _ => format!(
            "保存先: {}\n{} bytes。既存ファイルは上書きしません。",
            target.as_ref().expect("file target").display(),
            body.as_ref().expect("body").len()
        ),
    };
    let payload = serde_json::to_vec(&(
        inventory,
        &before.state.session_id,
        before.state.generation,
        &target,
        body.as_ref()
            .map(|body| ManagementEffectHashV1::digest(body.as_bytes())),
    ))
    .map_err(|e| e.to_string())?;
    let outcome = effects
        .prepare(
            &lease,
            effects.cursor().map_err(|e| e.to_string())?,
            PrepareManagementEffectV1 {
                operation_id: ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
                inventory: ManagementInventoryIdV1::parse(inventory).map_err(|e| e.to_string())?,
                scope: ManagementEffectScopeV1 {
                    project: effects.project_scope_hash(),
                    session: Some(ManagementEffectHashV1::digest(
                        before.state.session_id.as_bytes(),
                    )),
                    generation: before.state.generation,
                    revision: before.state.generation,
                    permission,
                    account_binding: None,
                },
                request_hash: ManagementEffectHashV1::digest(&payload),
                rewind: ManagementRewindDispositionV1::IrreversibleLocal,
            },
        )
        .map_err(|e| e.to_string())?;
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = outcome else {
        return Err("既存の管理操作を再実行しません".into());
    };
    if !account::confirm(
        terminal,
        reader,
        inventory,
        &before.state.session_id,
        &detail,
    ) {
        effects
            .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
            .map_err(|e| e.to_string())?;
        return Ok(vec![Line::from("操作を取り消しました")]);
    }
    let current = saved.snapshot().map_err(|e| e.to_string())?;
    if current.state != before.state || current.raw_hash != before.raw_hash {
        effects
            .deny(&lease, &prepared, ManagementDenialReasonV1::StaleRequest)
            .map_err(|e| e.to_string())?;
        return Err("確認後に会話が変更されました".into());
    }
    let started = effects
        .start(&lease, &prepared)
        .map_err(|e| e.to_string())?;
    let result = match action {
        Action::Clear => session.clear_history().map_err(|e| e.to_string()),
        Action::Copy => copy(body.as_deref().expect("reply")),
        _ => write_new(
            target.as_ref().expect("target"),
            body.as_deref().expect("body"),
        ),
    };
    if let Err(error) = result {
        effects
            .mark_outcome_unknown(
                &lease,
                &started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )
            .map_err(|e| e.to_string())?;
        return Err(format!(
            "操作結果は不明です。再実行せず確認してください: {error}"
        ));
    }
    effects
        .complete_started(&lease, &started, ManagementEffectHashV1::digest(&payload))
        .map_err(|e| e.to_string())?;
    Ok(vec![Line::from(format!(
        "{inventory} 完了{}",
        target
            .as_ref()
            .map(|p| format!(": {}", p.display()))
            .unwrap_or_default()
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::{conversation_state::HistoryMode, session_store::PersistedSession};
    use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
    struct Key(KeyCode);
    impl KeyReader for Key {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0)
        }
    }
    #[test]
    fn m3f_local_file_clipboard_and_clear_actions_confirm_exact_targets() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let state = temp.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(
            &state,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let saved = PersistedSession::create(
            temp.path(),
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(saved.clone()).unwrap();
        session.push_user("original input");
        session.push_assistant("original reply", Vec::new());
        let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
        let run = |action: &Action, key: KeyCode, session: &mut Session| {
            handle(
                action,
                &terminal,
                &mut Key(key),
                session,
                &workspace,
                &effects,
                &project,
                false,
                |reply| {
                    assert_eq!(reply, "original reply");
                    Ok(())
                },
            )
        };
        run(&Action::Init, KeyCode::Char('y'), &mut session).unwrap();
        assert!(project.join("AGENTS.md").is_file());
        assert!(run(&Action::Init, KeyCode::Char('y'), &mut session).is_err());
        run(
            &Action::Export("saved.md".into()),
            KeyCode::Char('y'),
            &mut session,
        )
        .unwrap();
        let body = std::fs::read_to_string(project.join("saved.md")).unwrap();
        assert!(body.contains("original input") && body.contains("original reply"));
        assert!(
            run(
                &Action::Export("saved.md".into()),
                KeyCode::Char('y'),
                &mut session
            )
            .is_err()
        );
        run(
            &Action::Export("declined.md".into()),
            KeyCode::Esc,
            &mut session,
        )
        .unwrap();
        assert!(!project.join("declined.md").exists());
        run(&Action::Copy, KeyCode::Char('y'), &mut session).unwrap();
        let mut copied = false;
        handle(
            &Action::Copy,
            &terminal,
            &mut Key(KeyCode::Esc),
            &mut session,
            &workspace,
            &effects,
            &project,
            false,
            |_| {
                copied = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(!copied);
        run(&Action::Clear, KeyCode::Esc, &mut session).unwrap();
        assert_eq!(session.messages.len(), 2);
        run(&Action::Clear, KeyCode::Char('y'), &mut session).unwrap();
        assert!(session.messages.is_empty());
        assert!(saved.snapshot().unwrap().recent_messages().is_empty());
        assert_eq!(
            saved.snapshot().unwrap().events.len(),
            2,
            "clear preserves original evidence"
        );
        assert!(run(&Action::Copy, KeyCode::Char('y'), &mut session).is_err());
        assert!(effects.blocker().unwrap().is_none());
        assert_eq!(
            effects
                .effects()
                .unwrap()
                .iter()
                .filter(|effect| matches!(effect.state, ManagementEffectStateV1::Completed { .. }))
                .count(),
            4
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn compact<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    provider: &dyn polaris_provider::Provider,
    session: &mut Session,
    session_path: &Path,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    status: &mut crate::Status,
    usage: &mut polaris_provider::Usage,
    has_pending: bool,
) -> Result<bool, String> {
    if has_pending {
        return Err("送信待ちの入力があるためcompactできません".into());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let saved = session.persistence.clone().ok_or("v2保存会話が必要です")?;
    let before = saved.snapshot().map_err(|e| e.to_string())?;
    if ManagementEffectHashV1::digest(before.state.project_id.as_bytes())
        != effects.project_scope_hash()
    {
        return Err("compactのprojectが一致しません".into());
    }
    let model = provider.model().ok_or("現在のmodelを確認できません")?;
    let lease = workspace.try_mutation().map_err(|e| e.to_string())?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|e| e.to_string())?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let payload = serde_json::to_vec(&(
        &before.state.session_id,
        before.state.generation,
        &model,
        before.raw_hash.clone(),
    ))
    .map_err(|e| e.to_string())?;
    let prepared = effects
        .prepare(
            &lease,
            effects.cursor().map_err(|e| e.to_string())?,
            PrepareManagementEffectV1 {
                operation_id: ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
                inventory: ManagementInventoryIdV1::parse("T09").map_err(|e| e.to_string())?,
                scope: ManagementEffectScopeV1 {
                    project: effects.project_scope_hash(),
                    session: Some(ManagementEffectHashV1::digest(
                        before.state.session_id.as_bytes(),
                    )),
                    generation: before.state.generation,
                    revision: before.state.generation,
                    permission: ManagementPermissionScopeV1::LocalSession,
                    account_binding: None,
                },
                request_hash: ManagementEffectHashV1::digest(&payload),
                rewind: ManagementRewindDispositionV1::IrreversibleLocal,
            },
        )
        .map_err(|e| e.to_string())?;
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = prepared else {
        return Err("既存のcompactを再送しません".into());
    };
    if !account::confirm(
        terminal,
        reader,
        "会話をcompact",
        &before.state.session_id,
        &format!(
            "model: {model}\n現在の会話を既存providerへ送信して要約します。原文を保持し、送信は巻き戻せません。"
        ),
    ) {
        effects
            .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
            .map_err(|e| e.to_string())?;
        *status = crate::Status::Notice("compactを取り消しました".into());
        return Ok(false);
    }
    let current = saved.snapshot().map_err(|e| e.to_string())?;
    if current.state != before.state || current.raw_hash != before.raw_hash {
        effects
            .deny(&lease, &prepared, ManagementDenialReasonV1::StaleRequest)
            .map_err(|e| e.to_string())?;
        return Err("確認後に会話が変更されました".into());
    }
    let started = effects
        .start(&lease, &prepared)
        .map_err(|e| e.to_string())?;
    let failed = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::handle_compact(provider, session, session_path, status, usage),
    )
    .await
    {
        Ok(failed) => failed,
        Err(_) => true,
    };
    let failed = failed
        || matches!(status,crate::Status::Notice(text) if text.starts_with("compaction failed"));
    if failed {
        effects
            .mark_outcome_unknown(
                &lease,
                &started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )
            .map_err(|e| e.to_string())?;
        return Err("compact結果を確認できません。原文を保ち、再送せず復旧してください".into());
    }
    effects
        .complete_started(&lease, &started, ManagementEffectHashV1::digest(&payload))
        .map_err(|e| e.to_string())?;
    Ok(true)
}
