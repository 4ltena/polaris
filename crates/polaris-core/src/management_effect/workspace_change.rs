//! Confirmed Git recipe changes share the existing recipe ledger and effect journal.
#![cfg(any(target_os = "macos", target_os = "linux"))]
use super::*;
use crate::{recipe, rewind_workspace::WorkspaceOwner};
use std::{fs::File, os::unix::fs::MetadataExt, path::Path};

const RULES: &[&str] = &[".next/", "node_modules/", "coverage/"];

pub struct GitRecipePreviewV1 {
    before: String,
    pub additions: Vec<String>,
    pub withheld: Vec<String>,
}
#[derive(Clone)]
pub struct GitRecipeReceiptV1 {
    pub run_id: String,
    pub attempt_id: String,
    pub postimage_hash: String,
    pub additions: Vec<String>,
}
pub struct WorkspaceChangeOwnerV1<'a> {
    pub workspace: &'a WorkspaceOwner,
    pub effects: &'a ManagementEffectStoreV1,
    pub source_identity: (u64, u64),
}
impl WorkspaceChangeOwnerV1<'_> {
    pub fn preview_git(&self, plan: recipe::git::GitPlan) -> Result<GitRecipePreviewV1, String> {
        let root = crate::project_mutation::open_directory_nofollow(self.workspace.project_root())
            .map_err(|_| "対象フォルダを開けません")?;
        let identity = root.metadata().map_err(|_| "対象を確認できません")?;
        if self.source_identity != (identity.dev(), identity.ino())
            || ManagementEffectHashV1::digest(
                self.workspace.project_root().as_os_str().as_encoded_bytes(),
            ) != self.effects.project_scope_hash()
        {
            return Err("起動時のsourceまたはproject束縛が変更されました".into());
        }
        match plan {
            recipe::git::GitPlan::TidyOnly => {}
            recipe::git::GitPlan::NotApplicable { reason } => {
                return Err(format!("Git設定は適用できません: {reason:?}"));
            }
        }
        let before = recipe::ignore_rules::read(self.workspace.project_root())
            .map_err(|_| ".gitignoreを安全に読めません")?;
        let (additions, withheld) = recipe::ignore_rules::plan_additions(&before, RULES);
        Ok(GitRecipePreviewV1 {
            before,
            additions: additions.into_iter().map(str::to_owned).collect(),
            withheld: withheld
                .into_iter()
                .map(|held| format!("{}: {:?}", held.rule, held.reason))
                .collect(),
        })
    }

    pub fn git(
        &self,
        preview: &GitRecipePreviewV1,
        accepted: bool,
        current_plan: recipe::git::GitPlan,
    ) -> Result<Option<GitRecipeReceiptV1>, String> {
        if preview.additions.is_empty() {
            return Ok(None);
        }
        let lease = self
            .workspace
            .try_mutation()
            .map_err(|_| "実行中の操作があります")?;
        self.workspace
            .require_quiescent_with_lease(&lease)
            .map_err(|_| "復旧待ちの操作があります")?;
        crate::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
            self.workspace.project_root(),
            self.effects.state_dir(),
            &lease,
        )
        .map_err(|error| error.to_string())?;
        let cursor = self
            .effects
            .cursor()
            .map_err(|_| "管理効果を確認できません")?;
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random().map_err(|_| "操作IDを作れません")?,
            inventory: ManagementInventoryIdV1::parse("C31").map_err(|_| "操作を確認できません")?,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: None,
                generation: cursor.generation(),
                revision: 0,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(
                &serde_json::to_vec(&(&preview.before, &preview.additions))
                    .map_err(|_| "計画を確認できません")?,
            ),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = self
            .effects
            .prepare(&lease, cursor, request)
            .map_err(|_| "Git設定の管理効果を準備できません")?
        else {
            return Err("操作が既に記録されています".into());
        };
        if !accepted {
            self.effects
                .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
                .map_err(|_| "取消を記録できません")?;
            return Ok(None);
        }
        let fresh = match self.preview_git(current_plan) {
            Ok(fresh) => fresh,
            Err(error) => {
                self.effects
                    .deny(
                        &lease,
                        &prepared,
                        ManagementDenialReasonV1::ResourceUnavailable,
                    )
                    .map_err(|_| "拒否を記録できません")?;
                return Err(error);
            }
        };
        if fresh.before != preview.before || fresh.additions != preview.additions {
            self.effects
                .deny(&lease, &prepared, ManagementDenialReasonV1::StaleRequest)
                .map_err(|_| "変更された計画の拒否を記録できません")?;
            return Err(".gitignoreまたはGit配置が確認中に変更されました".into());
        }
        let started = self
            .effects
            .start(&lease, &prepared)
            .map_err(|_| "開始を記録できません")?;
        let result = self.execute_git(&prepared, preview);
        match result {
            Ok(receipt) => {
                self.effects
                    .complete_started(
                        &lease,
                        &started,
                        ManagementEffectHashV1::digest(receipt.postimage_hash.as_bytes()),
                    )
                    .map_err(|_| "保存後の記録に失敗しました。再実行しないでください")?;
                Ok(Some(receipt))
            }
            Err(error) => {
                self.effects
                    .mark_outcome_unknown(
                        &lease,
                        &started,
                        ManagementUnknownReasonV1::UnclassifiedEffect,
                    )
                    .map_err(|_| "結果と管理効果が不明です。再実行しないでください")?;
                Err(error)
            }
        }
    }

    fn execute_git(
        &self,
        prepared: &PreparedManagementEffectV1,
        preview: &GitRecipePreviewV1,
    ) -> Result<GitRecipeReceiptV1, String> {
        let root = self.workspace.project_root();
        let (mut writer, _, session) =
            recipe::session::open(root).map_err(|_| "既存recipe台帳を開けません")?;
        let run_id = format!("git-tidy-{}", prepared.operation_id().as_hex());
        let target = recipe::session::start_run_with_attempt(
            &mut writer,
            &session,
            &run_id,
            &prepared.operation_id().as_hex(),
        )
        .map_err(|_| "recipe実行を記録できません")?;
        let operation =
            recipe::session::operation(&format!("gitignore-{}", prepared.operation_id().as_hex()))
                .map_err(|_| "recipe操作を確認できません")?;
        writer
            .record_recipe_intent(
                &target,
                operation.clone(),
                crate::desktop_store::SavedRecipeIntent {
                    schema_version: crate::desktop_store::RECIPE_SCHEMA_VERSION,
                    recipe_id: "git".into(),
                    recipe_version: recipe::SCHEMA_VERSION,
                    step_id: "gitignore".into(),
                    after: Vec::new(),
                    input_digest: crate::conversation_state::content_hash(
                        preview.before.as_bytes(),
                    ),
                    lockfile_digest_before: None,
                    source: None,
                    requested_versions: preview.additions.clone(),
                    environment: std::env::consts::OS.into(),
                },
            )
            .map_err(|_| "recipe intentを保存できません")?;
        let rules = preview
            .additions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let written = recipe::ignore_rules::append_checked(
            root,
            &rules,
            &crate::conversation_state::content_hash(preview.before.as_bytes()),
            Some(self.source_identity),
        );
        let verdict = match &written {
            Ok(after) => recipe::confirm::Verdict::Met {
                evidence: format!(
                    ".gitignore: {}",
                    crate::conversation_state::content_hash(after.as_bytes())
                ),
            },
            Err(_) => recipe::confirm::Verdict::Unknown {
                reason: "保存結果を確認できません".into(),
            },
        };
        let terminal = recipe::confirm::terminal(
            recipe::confirm::Ended {
                exit_code: Some(if written.is_ok() { 0 } else { 1 }),
                signal: None,
                cancelled: false,
            },
            verdict,
            None,
        );
        writer
            .record_recipe_terminal(&target, &operation, terminal)
            .map_err(|_| "recipe終端を保存できません")?;
        let result_id = polaris_desktop_protocol::ids::ResultId::new(format!(
            "gitignore-{}",
            prepared.operation_id().as_hex()
        ))
        .map_err(|_| "recipe結果IDを確認できません")?;
        writer
            .record_operation_result(&target, &operation, result_id.clone())
            .map_err(|_| "recipe結果を記録できません")?;
        writer
            .finish(
                &target,
                if written.is_ok() {
                    polaris_desktop_protocol::run_state::Observation::Succeeded
                } else {
                    polaris_desktop_protocol::run_state::Observation::OutcomeUnknown
                },
                result_id,
            )
            .map_err(|_| "recipe実行の終端を保存できません")?;
        let after = written.map_err(|_| "Git設定の保存結果が不明です。再実行しません")?;
        Ok(GitRecipeReceiptV1 {
            run_id,
            attempt_id: target.attempt_id.as_str().to_owned(),
            postimage_hash: crate::conversation_state::content_hash(after.as_bytes()),
            additions: preview.additions.clone(),
        })
    }
}

pub fn nextjs_plan(root: &Path) -> Result<recipe::Plan, String> {
    // Read-only planning does not create a recipe ledger or run install.
    let directory =
        crate::project_mutation::open_directory_nofollow(root).map_err(|_| "対象を開けません")?;
    let has = |name: &str,
               registry: &polaris_auth::protection::ProtectedPathsSnapshot|
     -> Result<bool, String> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let name = std::ffi::CString::new(name).map_err(|_| "対象名を確認できません")?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err("manifestまたはlockfileを安全に確認できません".into())
            };
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|_| "manifestを確認できません")?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() > 1024 * 1024
            || registry
                .identities()
                .iter()
                .any(|protected| protected.dev == metadata.dev() && protected.ino == metadata.ino())
        {
            return Err("manifestまたはlockfileが表示条件を満たしません".into());
        }
        Ok(true)
    };
    polaris_auth::protection::with_protected_paths(|registry| {
        for name in [
            "package.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ] {
            let path = root.join(name);
            if registry
                .paths()
                .iter()
                .any(|protected| path.starts_with(protected))
            {
                return Err("保護対象のmanifestは読み取りません".into());
            }
        }
        let has_manifest = has("package.json", registry)?;
        let mut lockfiles = Vec::new();
        for manager in [
            recipe::Manager::Npm,
            recipe::Manager::Pnpm,
            recipe::Manager::Yarn,
        ] {
            if has(manager.lockfile(), registry)? {
                lockfiles.push(manager);
            }
        }
        Ok(recipe::plan(
            &recipe::NEXT_JS,
            &recipe::Observed {
                has_manifest,
                lockfiles,
                completed: Vec::new(),
                install_confirmed: false,
            },
        ))
    })
    .map_err(|_| "認証保護を確認できません")?
}
