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
            .map_err(|_| "Cannot open the target folder")?;
        let identity = root.metadata().map_err(|_| "Cannot verify the target")?;
        if self.source_identity != (identity.dev(), identity.ino())
            || ManagementEffectHashV1::digest(
                self.workspace.project_root().as_os_str().as_encoded_bytes(),
            ) != self.effects.project_scope_hash()
        {
            return Err("Startup source or project binding changed".into());
        }
        match plan {
            recipe::git::GitPlan::TidyOnly => {}
            recipe::git::GitPlan::NotApplicable { reason } => {
                return Err(format!("Cannot apply Git settings: {reason:?}"));
            }
        }
        let before = recipe::ignore_rules::read(self.workspace.project_root())
            .map_err(|_| "Cannot safely read .gitignore")?;
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
            .map_err(|_| "An operation is active")?;
        self.workspace
            .require_quiescent_with_lease(&lease)
            .map_err(|_| "An operation awaits recovery")?;
        crate::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
            self.workspace.project_root(),
            self.effects.state_dir(),
            &lease,
        )
        .map_err(|error| error.to_string())?;
        let cursor = self
            .effects
            .cursor()
            .map_err(|_| "Cannot verify the management effect")?;
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random()
                .map_err(|_| "Cannot create an operation ID")?,
            inventory: ManagementInventoryIdV1::parse("C31")
                .map_err(|_| "Cannot verify the operation")?,
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
                    .map_err(|_| "Cannot verify the plan")?,
            ),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = self
            .effects
            .prepare(&lease, cursor, request)
            .map_err(|_| "Cannot prepare the Git settings management effect")?
        else {
            return Err("The operation is already recorded".into());
        };
        if !accepted {
            self.effects
                .deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)
                .map_err(|_| "Cannot record cancellation")?;
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
                    .map_err(|_| "Cannot record refusal")?;
                return Err(error);
            }
        };
        if fresh.before != preview.before || fresh.additions != preview.additions {
            self.effects
                .deny(&lease, &prepared, ManagementDenialReasonV1::StaleRequest)
                .map_err(|_| "Cannot record refusal of the changed plan")?;
            return Err(".gitignore or Git layout changed during confirmation".into());
        }
        let started = self
            .effects
            .start(&lease, &prepared)
            .map_err(|_| "Cannot record the start")?;
        let result = self.execute_git(&prepared, preview);
        match result {
            Ok(receipt) => {
                self.effects
                    .complete_started(
                        &lease,
                        &started,
                        ManagementEffectHashV1::digest(receipt.postimage_hash.as_bytes()),
                    )
                    .map_err(|_| "Recording failed after saving. Do not rerun")?;
                Ok(Some(receipt))
            }
            Err(error) => {
                self.effects
                    .mark_outcome_unknown(
                        &lease,
                        &started,
                        ManagementUnknownReasonV1::UnclassifiedEffect,
                    )
                    .map_err(|_| "The outcome and management effect are unknown. Do not rerun")?;
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
            recipe::session::open(root).map_err(|_| "Cannot open the existing recipe ledger")?;
        let run_id = format!("git-tidy-{}", prepared.operation_id().as_hex());
        let target = recipe::session::start_run_with_attempt(
            &mut writer,
            &session,
            &run_id,
            &prepared.operation_id().as_hex(),
        )
        .map_err(|_| "Cannot record recipe execution")?;
        let operation =
            recipe::session::operation(&format!("gitignore-{}", prepared.operation_id().as_hex()))
                .map_err(|_| "Cannot verify the recipe operation")?;
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
            .map_err(|_| "Cannot save recipe intent")?;
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
                reason: "Cannot verify the saved result".into(),
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
            .map_err(|_| "Cannot save the recipe terminal state")?;
        let result_id = polaris_desktop_protocol::ids::ResultId::new(format!(
            "gitignore-{}",
            prepared.operation_id().as_hex()
        ))
        .map_err(|_| "Cannot verify the recipe result ID")?;
        writer
            .record_operation_result(&target, &operation, result_id.clone())
            .map_err(|_| "Cannot record the recipe result")?;
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
            .map_err(|_| "Cannot save the recipe execution terminal state")?;
        let after = written
            .map_err(|_| "The Git settings save outcome is unknown. It will not be rerun")?;
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
    let directory = crate::project_mutation::open_directory_nofollow(root)
        .map_err(|_| "Cannot open the target")?;
    let has = |name: &str,
               registry: &polaris_auth::protection::ProtectedPathsSnapshot|
     -> Result<bool, String> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let name = std::ffi::CString::new(name).map_err(|_| "Cannot verify the target name")?;
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
                Err("Cannot safely verify the manifest or lockfile".into())
            };
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|_| "Cannot verify the manifest")?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() > 1024 * 1024
            || registry
                .identities()
                .iter()
                .any(|protected| protected.dev == metadata.dev() && protected.ino == metadata.ino())
        {
            return Err("The manifest or lockfile does not meet display requirements".into());
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
                return Err("Protected manifests will not be read".into());
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
    .map_err(|_| "Cannot verify authentication protection")?
}
