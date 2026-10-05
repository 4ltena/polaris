//! Workspace projections remain UI-only and bound to the startup source.
use crate::{approver::KeyReader, render};
use polaris_core::{management_effect::*, rewind_workspace::WorkspaceOwner};
use polaris_desktop_protocol::workspace_view::{AttachmentState, ViewState};
use polaris_desktop_service::{WorkspaceSourceIdentity, WorkspaceViewReader};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{
    cell::RefCell,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

pub(crate) struct WorkspaceCommands {
    source: PathBuf,
    identity: WorkspaceSourceIdentity,
    reader: WorkspaceViewReader,
    completed_recipe:
        RefCell<Option<polaris_core::management_effect::workspace_change::GitRecipeReceiptV1>>,
}

impl WorkspaceCommands {
    pub(crate) fn new(source: PathBuf, identity: WorkspaceSourceIdentity) -> Result<Self, String> {
        let reader = WorkspaceViewReader::new(source.clone(), identity)
            .map_err(|_| "起動時の対象フォルダを確認できません")?;
        Ok(Self {
            source,
            identity,
            reader,
            completed_recipe: RefCell::new(None),
        })
    }

    fn require_source(&self) -> Result<(), String> {
        let metadata =
            std::fs::symlink_metadata(&self.source).map_err(|_| "対象フォルダを確認できません")?;
        if !metadata.is_dir()
            || metadata.dev() != self.identity.device
            || metadata.ino() != self.identity.inode
        {
            return Err("起動時の対象フォルダが変更されました。再起動してください".into());
        }
        Ok(())
    }

    pub(crate) fn browse(&self, request: &str) -> Result<Vec<Line<'static>>, String> {
        self.require_source()?;
        let request = request.trim();
        let selected = if request.is_empty() || request == "list" {
            None
        } else {
            let value = request
                .strip_prefix("read ")
                .ok_or("/browse [list|read <相対パス>]")?;
            if !valid_relative(value) {
                return Err("対象フォルダ内の相対パスを指定してください".into());
            }
            Some(value)
        };
        let view = self.reader.read(selected);
        if view.state != ViewState::Ready {
            return Err(view
                .git
                .reason
                .unwrap_or_else(|| "対象フォルダを読み取れません".into()));
        }
        let mut lines = vec![Line::from(
            "フォルダの読み取り専用表示。モデルへの送信は行いません。",
        )];
        if let Some(selected) = selected {
            let file = view
                .files
                .iter()
                .find(|file| file.path == selected)
                .ok_or("対象ファイルが存在しないか、保護対象または一覧上限の範囲外です")?;
            if let Some(reason) = &file.unavailable_reason {
                return Err(reason.clone());
            }
            lines.push(Line::from(render::sanitize(&file.path)));
            lines.extend(
                file.body
                    .lines()
                    .map(|line| Line::from(render::sanitize(line))),
            );
            if file.body.is_empty() {
                lines.push(Line::from("空のファイルです"));
            }
        } else {
            lines.push(Line::from(format!(
                "ファイル: {}件（最大256件）",
                view.files.len()
            )));
            lines.extend(
                view.files
                    .iter()
                    .map(|file| Line::from(render::sanitize(&file.path))),
            );
        }
        lines.extend(
            view.notices
                .into_iter()
                .map(|line| Line::from(render::sanitize(&line))),
        );
        if let Some(reason) = view.git.reason {
            lines.push(Line::from(format!(
                "Git情報: {}",
                render::sanitize(&reason)
            )));
        }
        Ok(lines)
    }

    pub(crate) fn attachment(&self, request: &str) -> Result<Vec<Line<'static>>, String> {
        self.require_source()?;
        let request = request
            .trim()
            .strip_prefix("read ")
            .unwrap_or(request.trim());
        if request.is_empty() || request.len() > 4096 || request.chars().any(char::is_control) {
            return Err("/attachment <パス>（UTF-8テキスト、最大64KiB）".into());
        }
        let requested = Path::new(request);
        let path = if requested.is_absolute() {
            if !requested
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
            {
                return Err("添付のパスに . や .. を含められません".into());
            }
            requested.to_owned()
        } else {
            if !valid_relative(request) {
                return Err("添付の相対パスを確認してください".into());
            }
            self.source.join(requested)
        };
        let view = WorkspaceViewReader::read_attachment(&path);
        if view.state != AttachmentState::Ready {
            return Err(view.reason.unwrap_or_else(|| "添付を読み取れません".into()));
        }
        let mut lines = vec![Line::from(format!(
            "添付プレビュー: {}（未送信・会話には保存しません）",
            render::sanitize(view.name.as_deref().unwrap_or("添付"))
        ))];
        lines.extend(
            view.text
                .unwrap_or_default()
                .lines()
                .map(|line| Line::from(render::sanitize(line))),
        );
        Ok(lines)
    }
}

fn valid_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !Path::new(value).is_absolute()
}

pub(crate) fn recipes(request: &str) -> Result<Vec<Line<'static>>, String> {
    let request = request.trim();
    if !request.is_empty() && request != "list" {
        return Err("/recipe list（実行せず定義を表示）".into());
    }
    let mut lines = vec![Line::from(
        "組み込みレシピ定義。インストール・設定・検証は実行しません。",
    )];
    for recipe in polaris_core::recipe::RECIPES {
        lines.push(Line::from(format!(
            "{} v{} — {}",
            recipe.id, recipe.schema_version, recipe.manifest
        )));
        for step in recipe.steps {
            lines.push(Line::from(format!("  {}: {:?}", step.id, step.kind)));
        }
    }
    Ok(lines)
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
        return Err("決定事項は空白以外の本文を8KiB以内で指定してください".into());
    }
    if has_pending {
        return Err("送信待ちの入力があるため決定事項を保存できません".into());
    }
    let lease = workspace
        .try_mutation()
        .map_err(|_| "実行中の操作があるため保存できません")?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|_| "復旧待ちの操作があります")?;
    let cursor = effects.cursor().map_err(|_| "管理効果を確認できません")?;
    let request = PrepareManagementEffectV1 {
        operation_id: ManagementOperationIdV1::random().map_err(|_| "操作IDを作れません")?,
        inventory: ManagementInventoryIdV1::parse("C35").map_err(|_| "操作を確認できません")?,
        scope: ManagementEffectScopeV1 {
            project: effects.project_scope_hash(),
            session: None,
            generation: cursor.generation(),
            revision: 0,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(at, text)).map_err(|_| "決定事項を確認できません")?,
        ),
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    };
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = effects
        .prepare(&lease, cursor, request)
        .map_err(|_| "管理効果を準備できません")?
    else {
        return Err("同じ操作が記録されています".into());
    };
    if !crate::account::confirm(
        terminal,
        reader,
        "決定事項の保存",
        ".polaris/decisions.csv",
        text,
    ) {
        effects
            .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
            .map_err(|_| "取消を記録できません")?;
        return Ok(vec![Line::from("決定事項の保存を取り消しました")]);
    }
    let started = effects
        .start(&lease, &prepared)
        .map_err(|_| "保存開始を記録できません")?;
    if polaris_core::decisions::append(workspace.project_root(), at, text).is_err() {
        effects
            .mark_outcome_unknown(
                &lease,
                &started,
                ManagementUnknownReasonV1::UnclassifiedEffect,
            )
            .map_err(|_| "保存結果と管理効果を確認できません。再実行しないでください")?;
        return Err("決定事項の保存結果が不明です。自動で再実行しません".into());
    }
    effects
        .complete_started(
            &lease,
            &started,
            ManagementEffectHashV1::digest(b"decision append durably acknowledged"),
        )
        .map_err(|_| "保存後の記録に失敗しました。再実行しないでください")?;
    Ok(vec![Line::from(
        "決定事項を保存しました。過去の記録であり、現在の承認にはなりません。",
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
            return Err("保存先と起動時の対象フォルダが一致しません".into());
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
        use polaris_core::management_effect::workspace_change::{
            WorkspaceChangeOwnerV1, nextjs_plan,
        };
        self.require_source()?;
        if workspace.project_root() != self.source {
            return Err("対象projectが一致しません".into());
        }
        match request.trim() {
            "nextjs" => {
                let plan = nextjs_plan(&self.source)?;
                self.require_source()?;
                let mut lines = vec![Line::from(
                    "Next.js: 検出と計画のみ。導入・設定・起動確認は未実装です。",
                )];
                match plan {
                    polaris_core::recipe::Plan::NotApplicable { reason } => {
                        lines.push(Line::from(format!("適用不可: {reason:?}")))
                    }
                    polaris_core::recipe::Plan::Run { manager, steps } => {
                        lines.push(Line::from(format!("管理ツール: {}", manager.program())));
                        for step in steps {
                            let command = polaris_core::recipe::command(
                                &polaris_core::recipe::NEXT_JS,
                                step,
                                manager,
                            )
                            .map(|(program, args)| format!("{program} {}", args.join(" ")))
                            .unwrap_or_default();
                            lines.push(Line::from(format!("予定: {step} — {command}（未実行）")));
                        }
                    }
                }
                Ok(lines)
            }
            "git" => {
                if has_pending {
                    return Err("送信待ちの入力があるためGit設定を開始できません".into());
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
                    lines.push(Line::from(".gitignoreに追加する規則はありません"));
                    return Ok(lines);
                }
                let detail = format!(
                    "追加する規則:\n{}\n既存の行・順序・否定規則を保持します。",
                    preview.additions.join("\n")
                );
                let accepted = crate::account::confirm(
                    terminal,
                    reader,
                    "Git ignore規則の追加",
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
                            "追加済み: {}",
                            receipt.additions.join(", ")
                        )));
                        *self.completed_recipe.borrow_mut() = Some(receipt);
                    }
                    None => lines.push(Line::from("Git設定を取り消しました")),
                }
                Ok(lines)
            }
            _ => Err("/setting nextjs|git。Next.jsの実導入は利用できません。".into()),
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
                .unwrap_or_else(|| "Gitの安全な配置を確認できません".into()));
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

    pub(crate) fn handoff(&self, request: &str) -> Result<Vec<Line<'static>>, String> {
        self.require_source()?;
        if request.len() > 4096 || request.chars().any(|ch| ch.is_control() && ch != '\n') {
            return Err("引継ぎの内容が上限を超えています".into());
        }
        let mut parts = request.split('|').map(str::trim);
        let summary = parts.next().unwrap_or_default();
        let checks = parts
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if summary.is_empty() || checks.is_empty() {
            return Err("/handoff <概要> | <実機で確認する項目> [| <追加項目>]".into());
        }
        let (run_id, attempt_id, paths) = if let Some(receipt) =
            self.completed_recipe.borrow().as_ref()
        {
            let current = polaris_core::recipe::ignore_rules::read(&self.source)
                .map_err(|_| "記録した変更の現在値を確認できません")?;
            if polaris_core::conversation_state::content_hash(current.as_bytes())
                != receipt.postimage_hash
            {
                return Err(
                    "Git設定の完了記録から対象が変更されています。旧結果を引き継ぎません。".into(),
                );
            }
            (
                receipt.run_id.clone(),
                receipt.attempt_id.clone(),
                vec![".gitignore".into()],
            )
        } else {
            let view = self.reader.read(None);
            if view.state != ViewState::Ready
                || view.git.state != polaris_desktop_protocol::workspace_view::GitState::Ready
            {
                return Err("保護対象を除いたGit変更を確認できません".into());
            }
            let run_id = view
                .git
                .head
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_owned();
            if run_id.len() < 12 || !run_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(
                    "実装世代がありません。GitのHEADまたはこのTUIで完了した設定が必要です。".into(),
                );
            }
            let mut paths = view
                .files
                .iter()
                .filter(|file| file.modified || file.staged)
                .map(|file| file.path.clone())
                .collect::<Vec<_>>();
            if paths.len() > polaris_core::handoff::MAX_PATHS {
                return Err("変更ファイルが32件を超えます".into());
            }
            paths.sort();
            paths.dedup();
            (run_id, "1".into(), paths)
        };
        let implementation = polaris_core::handoff::Implementation {
            run_id,
            attempt_id,
            project_root: self.source.display().to_string(),
            paths,
            summary: summary.to_owned(),
            checks,
        };
        let text = polaris_core::handoff::prompt(&implementation)
            .map_err(|reason| format!("引継ぎを作成できません: {reason:?}"))?;
        self.require_source()?;
        Ok(text
            .lines()
            .map(|line| Line::from(render::sanitize(line)))
            .collect())
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
        std::fs::write(source.join("large.txt"), vec![b'x'; 65537]).unwrap();
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
        let show = |lines: Vec<Line<'static>>| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(show(commands.browse("list").unwrap()).contains("visible.txt"));
        assert!(show(commands.browse("read visible.txt").unwrap()).contains("visible body"));
        assert!(commands.browse("read ../visible.txt").is_err());
        assert!(commands.browse("read large.txt").is_err());
        assert!(show(commands.attachment("visible.txt").unwrap()).contains("未送信"));
        assert!(commands.attachment("link.txt").is_err());
        assert!(show(recipes("list").unwrap()).contains("nextjs"));
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
        assert!(commands.browse("list").is_err());
        assert!(commands.attachment("visible.txt").is_err());
    }

    #[test]
    fn m3f_setting_and_handoff_use_real_recipe_results() {
        if std::env::var_os("POLARIS_M3F_SETTING_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "workspace_commands::tests::m3f_setting_and_handoff_use_real_recipe_results",
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
        let text = |lines: Vec<Line<'static>>| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut no_keys = Keys(VecDeque::new());
        let planned = text(
            commands
                .setting(
                    "nextjs",
                    &terminal,
                    &mut no_keys,
                    &workspace,
                    &effects,
                    false,
                )
                .unwrap(),
        );
        assert!(planned.contains("予定: detect") && planned.contains("未実装"));
        assert!(!source.join("node_modules").exists());
        assert!(
            commands
                .setting(
                    "nextjs --install",
                    &terminal,
                    &mut no_keys,
                    &workspace,
                    &effects,
                    false
                )
                .is_err()
        );
        assert!(commands.handoff("概要").is_err());
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
        let handoff = text(
            commands
                .handoff("ignore追加 | .gitignoreを実機で確認")
                .unwrap(),
        );
        assert!(
            handoff.contains("git-tidy-")
                && handoff.contains(".gitignore")
                && handoff.contains("実機での合格は含まない")
        );
        std::fs::write(source.join(".gitignore"), format!("{after}changed\n")).unwrap();
        assert!(commands.handoff("概要 | 確認").is_err());
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
