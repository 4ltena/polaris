//! Workspace projections remain UI-only and bound to the startup source.
use crate::{approver::KeyReader, render};
use polaris_core::{management_effect::*, rewind_workspace::WorkspaceOwner};
use polaris_desktop_protocol::workspace_view::ViewState;
use polaris_desktop_service::{WorkspaceSourceIdentity, WorkspaceViewReader};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{
    cell::RefCell,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(crate) struct WorkspaceCommands {
    source: PathBuf,
    identity: WorkspaceSourceIdentity,
    reader: WorkspaceViewReader,
}

impl WorkspaceCommands {
    pub(crate) fn new(source: PathBuf, identity: WorkspaceSourceIdentity) -> Result<Self, String> {
        let reader = WorkspaceViewReader::new(source.clone(), identity)
            .map_err(|_| "Cannot verify the startup target folder")?;
        Ok(Self {
            source,
            identity,
            reader,
        })
    }

    fn require_source(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.source)
            .map_err(|_| "Cannot verify the target folder")?;
        if !metadata.is_dir()
            || metadata.dev() != self.identity.device
            || metadata.ino() != self.identity.inode
        {
            return Err("The startup target folder changed. Restart".into());
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn decided<B: Backend, R: KeyReader>(
    text: &str,
    at: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    if text.trim().is_empty()
        || text.len() > 8192
        || at.is_empty()
        || at.len() > 64
        || text
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
        || at.chars().any(char::is_control)
    {
        return Err("Specify non-whitespace decision text within 8 KiB".into());
    }
    if has_pending {
        return Err("Cannot save decisions while input is queued".into());
    }
    let lease = workspace
        .try_mutation()
        .map_err(|_| "Cannot save while an operation is active")?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|_| "An operation awaits recovery")?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let cursor = effects
        .cursor()
        .map_err(|_| "Cannot verify the management effect")?;
    let request = PrepareManagementEffectV1 {
        operation_id: ManagementOperationIdV1::random()
            .map_err(|_| "Cannot create an operation ID")?,
        inventory: ManagementInventoryIdV1::parse("C35")
            .map_err(|_| "Cannot verify the operation")?,
        scope: ManagementEffectScopeV1 {
            project: effects.project_scope_hash(),
            session: None,
            generation: cursor.generation(),
            revision: 0,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(at, text)).map_err(|_| "Cannot verify decisions")?,
        ),
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    };
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = effects
        .prepare(&lease, cursor, request)
        .map_err(|_| "Cannot prepare the management effect")?
    else {
        return Err("The same operation is already recorded".into());
    };
    if !crate::account::confirm(
        terminal,
        reader,
        "Save decision",
        ".polaris/decisions.csv",
        text,
    ) {
        effects
            .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
            .map_err(|_| "Cannot record cancellation")?;
        return Ok(vec![Line::from("Decision save cancelled")]);
    }
    let started = effects
        .start(&lease, &prepared)
        .map_err(|_| "Cannot record the save start")?;
    if polaris_core::decisions::append(workspace.project_root(), at, text).is_err() {
        effects
            .mark_outcome_unknown(
                &lease,
                &started,
                ManagementUnknownReasonV1::UnclassifiedEffect,
            )
            .map_err(|_| "Cannot verify the save outcome or management effect. Do not rerun")?;
        return Err("Decision save outcome is unknown. No automatic retry will occur".into());
    }
    effects
        .complete_started(
            &lease,
            &started,
            ManagementEffectHashV1::digest(b"decision append durably acknowledged"),
        )
        .map_err(|_| "Recording failed after saving. Do not rerun")?;
    Ok(vec![Line::from(
        "Decision saved. This is a historical record, not current approval.",
    )])
}

impl WorkspaceCommands {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decided<B: Backend, R: KeyReader>(
        &self,
        text: &str,
        at: &str,
        terminal: &RefCell<Terminal<B>>,
        reader: &mut R,
        workspace: &WorkspaceOwner,
        effects: &ManagementEffectStoreV1,
        has_pending: bool,
    ) -> Result<Vec<Line<'static>>, String> {
        self.require_source()?;
        if workspace.project_root() != self.source {
            return Err("Storage location does not match the startup target folder".into());
        }
        decided(text, at, terminal, reader, workspace, effects, has_pending)
    }
}

impl WorkspaceCommands {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn setting<B: Backend, R: KeyReader>(
        &self,
        request: &str,
        terminal: &RefCell<Terminal<B>>,
        reader: &mut R,
        workspace: &WorkspaceOwner,
        effects: &ManagementEffectStoreV1,
        has_pending: bool,
    ) -> Result<Vec<Line<'static>>, String> {
        use polaris_core::management_effect::workspace_change::WorkspaceChangeOwnerV1;
        self.require_source()?;
        if workspace.project_root() != self.source {
            return Err("Target project mismatch".into());
        }
        match request.trim() {
            "git" => {
                if has_pending {
                    return Err("Cannot start Git setup while input is queued".into());
                }
                let owner = WorkspaceChangeOwnerV1 {
                    workspace,
                    effects,
                    source_identity: (self.identity.device, self.identity.inode),
                };
                let preview = owner.preview_git(self.git_plan()?)?;
                let mut lines = preview
                    .withheld
                    .iter()
                    .map(|text| Line::from(render::sanitize(text)))
                    .collect::<Vec<_>>();
                if preview.additions.is_empty() {
                    lines.push(Line::from("No rules to add to .gitignore"));
                    return Ok(lines);
                }
                let detail = format!(
                    "Rules to add:\n{}\nExisting lines, order, and negation rules are retained.",
                    preview.additions.join("\n")
                );
                let accepted = crate::account::confirm(
                    terminal,
                    reader,
                    "Add Git ignore rules",
                    ".gitignore",
                    &detail,
                );
                let current_plan = if accepted {
                    self.git_plan()?
                } else {
                    polaris_core::recipe::git::GitPlan::TidyOnly
                };
                match owner.git(&preview, accepted, current_plan)? {
                    Some(receipt) => {
                        lines.push(Line::from(format!(
                            "Already added: {}",
                            receipt.additions.join(", ")
                        )));
                    }
                    None => lines.push(Line::from("Git setup cancelled")),
                }
                Ok(lines)
            }
            _ => Err("Usage: /setting git".into()),
        }
    }

    fn git_plan(&self) -> Result<polaris_core::recipe::git::GitPlan, String> {
        use polaris_core::recipe::git::{Head, Placement, plan};
        self.require_source()?;
        let view = self.reader.read(None);
        if view.state != ViewState::Ready
            || view.git.state != polaris_desktop_protocol::workspace_view::GitState::Ready
        {
            return Err(view
                .git
                .reason
                .unwrap_or_else(|| "Cannot verify safe Git layout".into()));
        }
        // Ready requires this source's direct .git directory and its bounded
        // protected-tree check; external gitdirs and nested projects refuse.
        let head = match view.git.head.as_deref().map(str::trim) {
            Some(value)
                if value.len() >= 12 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Head::Committed
            }
            _ => Head::Unborn,
        };
        self.require_source()?;
        Ok(plan(Ok(Placement::Own { head })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
    use std::collections::VecDeque;
    struct Keys(VecDeque<KeyCode>);
    impl KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            self.0
                .pop_front()
                .ok_or_else(|| std::io::Error::other("no key"))
        }
    }

    #[test]
    fn m3f_workspace_and_decision_routes_are_bound_and_explicit() {
        if std::env::var_os("POLARIS_M3F_WORKSPACE_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["workspace_commands::tests::m3f_workspace_and_decision_routes_are_bound_and_explicit", "--exact", "--nocapture"])
                .env("POLARIS_M3F_WORKSPACE_CHILD", "1").env("HOME", home.path()).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let source = home.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("visible.txt"), "visible body").unwrap();
        std::os::unix::fs::symlink(source.join("visible.txt"), source.join("link.txt")).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let commands = WorkspaceCommands::new(
            source.clone(),
            WorkspaceSourceIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        )
        .unwrap();
        let state = polaris_core::project::state_dir(&source).unwrap();
        crate::source_apply::prepare_state(&source, &state).unwrap();
        let workspace =
            WorkspaceOwner::open(&source, &state.join("rewind-workspace"), Default::default())
                .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&source, &state, &lease).unwrap();
        drop(lease);
        let terminal = RefCell::new(Terminal::new(TestBackend::new(160, 45)).unwrap());
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        commands
            .decided(
                "  exact statement  ",
                "2026-10-04",
                &terminal,
                &mut yes,
                &workspace,
                &effects,
                false,
            )
            .unwrap();
        assert_eq!(
            polaris_core::decisions::load(&source)[0].what,
            "  exact statement  "
        );
        let mut cancel = Keys(VecDeque::from([KeyCode::Esc]));
        commands
            .decided(
                "must not save",
                "2026-10-04",
                &terminal,
                &mut cancel,
                &workspace,
                &effects,
                false,
            )
            .unwrap();
        assert_eq!(polaris_core::decisions::load(&source).len(), 1);
        let before = std::fs::read(source.join(polaris_core::decisions::PATH)).unwrap();
        let retained = source.join(".polaris/retained.csv");
        std::fs::rename(source.join(polaris_core::decisions::PATH), &retained).unwrap();
        std::os::unix::fs::symlink(&retained, source.join(polaris_core::decisions::PATH)).unwrap();
        assert!(polaris_core::decisions::append(&source, "now", "must refuse").is_err());
        assert_eq!(std::fs::read(&retained).unwrap(), before);
        let moved = home.join("old-project");
        std::fs::rename(&source, moved).unwrap();
        std::fs::create_dir(&source).unwrap();
    }

    #[test]
    fn m3f_setting_uses_real_recipe_results() {
        if std::env::var_os("POLARIS_M3F_SETTING_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "workspace_commands::tests::m3f_setting_uses_real_recipe_results",
                    "--exact",
                    "--nocapture",
                ])
                .env("POLARIS_M3F_SETTING_CHILD", "1")
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let source = home.join("project");
        std::fs::create_dir(&source).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .args(["-c", "core.hooksPath=/dev/null", "init", "--quiet"])
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(source.join("package.json"), "{}").unwrap();
        let before = "# user rule\n!coverage/summary.json\nnode_modules/\n";
        std::fs::write(source.join(".gitignore"), before).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let commands = WorkspaceCommands::new(
            source.clone(),
            WorkspaceSourceIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        )
        .unwrap();
        let state = polaris_core::project::state_dir(&source).unwrap();
        crate::source_apply::prepare_state(&source, &state).unwrap();
        let workspace =
            WorkspaceOwner::open(&source, &state.join("rewind-workspace"), Default::default())
                .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&source, &state, &lease).unwrap();
        drop(lease);
        let terminal = RefCell::new(Terminal::new(TestBackend::new(160, 45)).unwrap());
        let mut no_keys = Keys(VecDeque::new());
        for removed in ["nextjs", "nextjs --install"] {
            assert!(
                commands
                    .setting(
                        removed,
                        &terminal,
                        &mut no_keys,
                        &workspace,
                        &effects,
                        false
                    )
                    .is_err()
            );
        }
        assert!(!source.join("node_modules").exists());
        let mut cancel = Keys(VecDeque::from([KeyCode::Esc]));
        commands
            .setting("git", &terminal, &mut cancel, &workspace, &effects, false)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(source.join(".gitignore")).unwrap(),
            before
        );
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        commands
            .setting("git", &terminal, &mut yes, &workspace, &effects, false)
            .unwrap();
        let after = std::fs::read_to_string(source.join(".gitignore")).unwrap();
        assert_eq!(after, format!("{before}.next/\n"));
        let (writer, _, _) = polaris_core::recipe::session::open(&source).unwrap();
        let steps = writer.recipe_steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].terminal.as_ref().unwrap().outcome,
            polaris_core::desktop_store::StepOutcome::Succeeded
        );
        let published = writer.snapshot().unwrap();
        assert!(published.state.runs.last().unwrap().run.state.is_terminal());
        drop(writer);
        let owner = polaris_core::management_effect::workspace_change::WorkspaceChangeOwnerV1 {
            workspace: &workspace,
            effects: &effects,
            source_identity: (metadata.dev(), metadata.ino()),
        };
        std::fs::write(source.join(".gitignore"), "node_modules/\n").unwrap();
        let preview = owner.preview_git(commands.git_plan().unwrap()).unwrap();
        std::fs::write(source.join(".gitignore"), "concurrent\n").unwrap();
        assert!(
            owner
                .git(&preview, true, commands.git_plan().unwrap())
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(source.join(".gitignore")).unwrap(),
            "concurrent\n"
        );
        assert!(effects.blocker().unwrap().is_none());
    }
}
