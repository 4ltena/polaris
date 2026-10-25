//! M7b-5: the `polaris sync` entry points. Every command works on this
//! project's own state; nothing here writes into the project directly, and
//! C10 sending consumes the shared owner's prepared token.
use clap::{Args as ClapArgs, Subcommand};
#[cfg(not(target_os = "linux"))]
use polaris_core::sync::exchange::{Kept, Resolution};
use polaris_core::sync::participation::{self, Participation};
use polaris_core::sync::repository::SyncRepository;
use polaris_core::sync::share::{ShareLink, ShareManifest, SyncPaths};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join this project to a share without storing credential values.
    Join {
        /// Canonical HTTPS remote dedicated to sync.
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        share_id: Option<String>,
        #[arg(long)]
        invitation_id: Option<String>,
        /// Invitation expiry in UNIX seconds. Expired invitations cannot join.
        #[arg(long)]
        expires: Option<u64>,
        /// Additional names to exclude from sharing, beyond mandatory exclusions.
        #[arg(long = "exclude")]
        excluded: Vec<String>,
        /// Explicitly rebind credentials for the current share participation.
        #[arg(long, conflicts_with_all = ["recover", "abandon"])]
        rebind_credential: bool,
        /// Reconcile and complete the specified C08 operation.
        #[arg(long, value_name = "OPERATION_ID", conflicts_with_all = ["rebind_credential", "abandon"])]
        recover: Option<String>,
        /// Explicitly abandon the specified C08 operation.
        #[arg(long, value_name = "OPERATION_ID", conflicts_with_all = ["rebind_credential", "recover"])]
        abandon: Option<String>,
        /// Explicitly confirm the C08 operation for noninteractive execution.
        #[arg(long)]
        yes: bool,
        /// Account name to select in Keychain.
        #[arg(long)]
        account: Option<String>,
        /// Redacted prefix for selecting an ambiguous Keychain candidate.
        #[arg(long)]
        credential_choice: Option<String>,
        /// Reconstruct the exact confirmed join timestamp when abandoning a join without intent.
        #[arg(long, requires_all = ["abandon", "remote"])]
        joined_at: Option<u64>,
    },
    /// Show participation state and unresolved conflicts.
    Status,
    /// Record shared content and send it to the peer.
    Push {
        /// Reconcile the specified C10 operation without resending.
        #[arg(long, value_name = "OPERATION_ID", conflicts_with = "abandon")]
        recover: Option<String>,
        /// Abandon the specified C10 operation after fresh confirmation.
        #[arg(long, value_name = "OPERATION_ID", conflicts_with = "recover")]
        abandon: Option<String>,
        /// Explicitly approve finalizing the displayed operation noninteractively.
        #[arg(long)]
        yes: bool,
    },
    /// Reconcile the specified C10 operation without resending.
    Recover { operation_id: String },
    /// Abandon the specified C10 operation after fresh confirmation.
    Abandon {
        operation_id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Receive peer changes and merge only mechanically resolvable changes.
    Pull,
    /// Apply received, merged content to the project only when no unresolved conflicts remain.
    Apply,
    /// Leave the share on this device. Shared history and peers remain unchanged.
    Leave,
}

pub async fn run(args: Args) -> ExitCode {
    match execute(args.command) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn state_dir() -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|_| "Cannot retrieve the working directory".to_string())?;
    polaris_core::project::state_dir(&cwd).map_err(|_| "Cannot prepare the state directory".into())
}

/// The same bounds the shared tree is collected under.
fn limits() -> polaris_core::isolated_workspace::Limits {
    polaris_core::isolated_workspace::Limits {
        max_entries: 40_000,
        max_files: 20_000,
        max_file_bytes: 16 * 1024 * 1024,
        max_total_bytes: 256 * 1024 * 1024,
        max_depth: 32,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn joined(state: &std::path::Path) -> Result<Participation, String> {
    participation::read(state)
        .map_err(|error| format!("Cannot safely verify participation state: {error}"))?
        .participation()
        .cloned()
        .ok_or_else(|| "This project has not joined a share.".to_string())
}

/// Why the merge could not settle a path, in the words a person acts on.
/// The classification is the merge's own; this only spells it out.
#[cfg(not(target_os = "linux"))]
fn kept_text(reason: Kept) -> &'static str {
    match reason {
        Kept::Overlapping => "Both sides changed the same region",
        Kept::Binary => "The format cannot be merged line by line",
        Kept::DeleteVersusEdit => "One side deleted a file that the other changed",
        Kept::AmbiguousRename => "Cannot uniquely resolve the rename target",
    }
}

fn execute(command: Command) -> Result<String, String> {
    if cfg!(windows) {
        return Err(match &command {
            Command::Apply => {
                "Applying shared content is unsupported on Windows. No changes were made."
            }
            _ => "Windows lacks shared-state protection support. No changes were made.",
        }
        .into());
    }
    let project = polaris_core::project::resolve_root(
        &std::env::current_dir()
            .map_err(|_| "Cannot retrieve the working directory".to_string())?,
    );
    // Recovery must reach its domain owner before ordinary composite admission.
    let command = match command {
        Command::Recover { operation_id } => {
            return c10_command(&project, Some(operation_id), None, false);
        }
        Command::Abandon { operation_id, yes } => {
            return c10_command(&project, None, Some(operation_id), yes);
        }
        Command::Push {
            recover,
            abandon,
            yes,
        } => {
            return c10_command(&project, recover, abandon, yes);
        }
        command => command,
    };
    #[cfg(not(test))]
    if let Command::Join {
        remote,
        share_id,
        invitation_id,
        expires,
        excluded,
        rebind_credential,
        recover,
        abandon,
        yes,
        account,
        credential_choice,
        joined_at,
    } = command
    {
        let reconstruct_join = abandon.is_some() && remote.is_some();
        let join = if rebind_credential
            || recover.is_some()
            || (abandon.is_some() && !reconstruct_join)
        {
            None
        } else {
            Some((
                ShareLink {
                    remote: remote.ok_or_else(|| "--remote is required".to_string())?,
                    share_id: share_id.ok_or_else(|| "--share-id is required".to_string())?,
                    invitation_id: invitation_id
                        .ok_or_else(|| "--invitation-id is required".to_string())?,
                    expires_unix_seconds: expires
                        .ok_or_else(|| "--expires is required".to_string())?,
                },
                ShareManifest {
                    version: 1,
                    excluded,
                },
                if reconstruct_join {
                    joined_at.ok_or_else(|| {
                            "Reconstructing a join abandonment requires the confirmed --joined-at timestamp".to_string()
                        })?
                } else {
                    now()
                },
            ))
        };
        return c08_command(
            &project,
            rebind_credential,
            recover,
            abandon,
            yes,
            (account, credential_choice),
            join,
        );
    }
    #[cfg(target_os = "linux")]
    let owner =
        polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).map_err(
            |error| match error {
                polaris_core::apply_owner::ApplyOwnerError::Admission(error) => {
                    String::from_utf8(error.canonical_bytes()).expect("closed UTF-8 diagnostic")
                }
                error => format!("Cannot start the sharing operation: {error}"),
            },
        )?;
    let state = state_dir()?;
    #[cfg(target_os = "linux")]
    if !matches!(&command, Command::Apply | Command::Status) {
        owner.ensure_sync_idle().map_err(|_| {
            "An operation is incomplete or active. Verify apply state before resuming sharing operations.".to_string()
        })?;
    }
    match command {
        Command::Join {
            remote,
            share_id,
            invitation_id,
            expires,
            excluded,
            rebind_credential,
            recover,
            abandon,
            yes,
            account,
            credential_choice,
            joined_at: _,
        } => {
            if rebind_credential || recover.is_some() || abandon.is_some() {
                return c08_command(
                    &project,
                    rebind_credential,
                    recover,
                    abandon,
                    yes,
                    (account, credential_choice),
                    None,
                );
            }
            let share = ShareLink {
                remote: remote.ok_or_else(|| "--remote is required".to_string())?,
                share_id: share_id.ok_or_else(|| "--share-id is required".to_string())?,
                invitation_id: invitation_id
                    .ok_or_else(|| "--invitation-id is required".to_string())?,
                expires_unix_seconds: expires.ok_or_else(|| "--expires is required".to_string())?,
            };
            let manifest = ShareManifest {
                version: 1,
                excluded,
            };
            #[cfg(not(test))]
            {
                c08_command(
                    &project,
                    false,
                    None,
                    None,
                    yes,
                    (account, credential_choice),
                    Some((share, manifest, now())),
                )
            }
            #[cfg(test)]
            {
                let _ = (&account, &credential_choice, yes);
                #[cfg(target_os = "linux")]
                let participation = {
                    let joined_unix_seconds = now();
                    share
                        .accept(joined_unix_seconds)
                        .map_err(|_| "Cannot create the participation fixture".to_string())?;
                    let value = Participation {
                        share,
                        manifest,
                        joined_unix_seconds,
                        agreed_commit: None,
                        applied_commit: None,
                        unresolved: Vec::new(),
                    };
                    let sync = state.join("sync");
                    std::fs::create_dir_all(&sync).map_err(|_| "Cannot create fixture storage")?;
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&sync, std::fs::Permissions::from_mode(0o700))
                        .map_err(|_| "Cannot protect fixture storage")?;
                    let path = sync.join("participation.json");
                    std::fs::write(
                        &path,
                        serde_json::to_vec(&value).map_err(|_| "Cannot encode the fixture")?,
                    )
                    .map_err(|_| "Cannot save the fixture")?;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|_| "Cannot protect the fixture")?;
                    value
                };
                #[cfg(not(target_os = "linux"))]
                let participation = participation::join(&state, share, manifest, now())
                    .map_err(|_| "Cannot create the participation fixture".to_string())?;
                Ok(format!(
                    "Joined: {} (sharing rules: {})",
                    participation.share.share_id,
                    participation.manifest.digest()
                ))
            }
        }
        Command::Status => {
            let held = joined(&state)?;
            #[cfg(target_os = "linux")]
            if let Some(intent) = owner
                .active()
                .map_err(|error| format!("Cannot verify apply state: {error}"))?
            {
                return Ok(format!(
                    "Share: {}\nLast agreed content: {}\nApplied content: {}\nIncomplete apply: {} ({:?})\nRecovery backup: {}",
                    held.share.share_id,
                    held.agreed_commit.as_deref().unwrap_or("none"),
                    held.applied_commit.as_deref().unwrap_or("none"),
                    intent.descriptor().operation_key.as_hex(),
                    intent.latest().phase,
                    intent.descriptor().recovery_child
                ));
            }
            let paths = SyncPaths::under(&state);
            let head = SyncRepository::open(paths)
                .ok()
                .and_then(|repository| repository.head());
            let message = format!(
                "Share: {}\nLast agreed content: {}\nLocal record: {}\nUnresolved: {}",
                held.share.share_id,
                held.agreed_commit.as_deref().unwrap_or("none"),
                head.as_deref().unwrap_or("none"),
                if held.unresolved.is_empty() {
                    "none".to_string()
                } else {
                    held.unresolved.join(", ")
                }
            );
            Ok(message)
        }
        Command::Push { .. } | Command::Recover { .. } | Command::Abandon { .. } => {
            unreachable!("C10 dispatch precedes ordinary admission")
        }
        Command::Pull => {
            #[cfg(target_os = "linux")]
            {
                let exchange = owner
                    .pull_confirmed(true)
                    .map_err(|_| "Cannot finalize the receive result. Check shared state.")?
                    .expect("explicit pull accepted");
                return Ok(format!(
                    "Received: {} items, {} unresolved. Applying to the project is a separate operation.",
                    exchange.changes.len(),
                    exchange.needs_attention().len()
                ));
            }
            #[cfg(not(target_os = "linux"))]
            {
                let mut held = joined(&state)?;
                let repository = SyncRepository::open(SyncPaths::under(&state))
                    .map_err(|error| format!("Cannot open sync records: {error}"))?;
                // The local edits are recorded first, so a clash with what is
                // arriving is found now instead of silently disappearing.
                repository
                    .stage_from(&project, &held.manifest)
                    .map_err(|error| format!("Cannot collect shared content: {error}"))?;
                repository
                    .commit("polaris sync")
                    .map_err(|error| format!("Cannot record: {error}"))?;
                repository
                    .receive(&held.share.remote)
                    .map_err(|_| "Cannot receive. Check the remote connection.".to_string())?;
                let exchange = repository
                    .merge_received()
                    .map_err(|error| format!("Cannot merge: {error}"))?;
                held.unresolved = exchange
                    .needs_attention()
                    .iter()
                    .map(|change| change.path.clone())
                    .collect();
                held.agreed_commit = exchange.merged_commit.clone().or(held.agreed_commit);
                if exchange.changes.is_empty() && !exchange.unresolved {
                    // Nothing arrived that the project does not already hold.
                    held.applied_commit = held.agreed_commit.clone();
                }
                #[cfg(target_os = "linux")]
                let published = owner.write_participation(&held);
                #[cfg(not(target_os = "linux"))]
                let published = participation::write(&state, &held);
                published.map_err(|_| "Cannot save participation state".to_string())?;
                if exchange.unresolved {
                    // The merge already measured why each path was left: keeping
                    // only the path would make the person open every one of them
                    // to find out what this side already knows.
                    let waiting: Vec<String> = exchange
                        .needs_attention()
                        .iter()
                        .map(|change| match change.resolution {
                            Resolution::BothKept { reason } => {
                                format!("{}\t{}", change.path, kept_text(reason))
                            }
                            Resolution::Merged => change.path.clone(),
                        })
                        .collect();
                    return Ok(format!(
                        "Both versions retained ({} items). Resolve them before running `polaris sync push`.\n{}",
                        waiting.len(),
                        waiting.join("\n")
                    ));
                }
                Ok(format!(
                    "Received: {} changes. Apply them to the project separately.",
                    exchange.changes.len()
                ))
            }
        }
        Command::Apply => {
            #[cfg(target_os = "linux")]
            return apply_linux(&owner, &project, &state);
            #[cfg(not(target_os = "linux"))]
            {
                let held = joined(&state)?;
                if !held.unresolved.is_empty() {
                    return Err(format!(
                        "{} unresolved items. Resolve them first.\n{}",
                        held.unresolved.len(),
                        held.unresolved.join("\n")
                    ));
                }
                let Some(agreed) = held.agreed_commit.clone() else {
                    return Err("No agreed content yet. Run `polaris sync pull` first.".into());
                };
                if held.applied_commit.as_deref() == Some(agreed.as_str()) {
                    // The same agreed state is never applied twice, however often
                    // it is redelivered.
                    return Ok("This content has already been applied.".into());
                }
                let paths = SyncPaths::under(&state);
                let staging = paths.work_tree.clone();
                let repository = SyncRepository::open(paths)
                    .map_err(|error| format!("Cannot open sync records: {error}"))?;
                if repository.head().as_deref() != Some(agreed.as_str()) {
                    return Err(
                    "Sync records do not match the agreed content. Run `polaris sync pull` first."
                        .into(),
                );
                }
                // The staging copy is put back to the agreed commit, so what is
                // read here is the agreement and not what a previous command left.
                repository
                    .restore(&agreed)
                    .map_err(|error| format!("Cannot retrieve agreed content: {error}"))?;
                let (written, removed) = repository
                    .changed_paths(held.applied_commit.as_deref(), &agreed)
                    .map_err(|error| format!("Cannot read changes: {error}"))?;
                let changed = polaris_core::sync::apply::Changed { written, removed };
                let recovery = state.join("sync/recovery");
                std::fs::create_dir_all(&recovery)
                    .map_err(|_| "Cannot prepare the recovery location".to_string())?;
                let (report, shaped) = polaris_core::sync::apply::apply_agreed(
                    &project,
                    &staging,
                    &held.manifest,
                    &changed,
                    limits(),
                    &recovery,
                )
                .map_err(|error| format!("Cannot apply: {error}"))?;
                if let Some(failure) = &report.failure {
                    return Err(format!("Cannot apply: {failure:?}"));
                }
                let mut held = held;
                held.applied_commit = Some(agreed);
                participation::write(&state, &held)
                    .map_err(|_| "Cannot save participation state".to_string())?;
                let deferred = if shaped.deferred.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nNot applied because new directories are required ({} items):\n{}",
                        shaped.deferred.len(),
                        shaped
                            .deferred
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                Ok(format!(
                    "Applied {} items. Recovery backup: {}.{deferred}",
                    report.entries.len(),
                    report
                        .recovery_path
                        .as_ref()
                        .map_or_else(|| "none".into(), |path| path.display().to_string())
                ))
            }
        }
        Command::Leave => {
            #[cfg(target_os = "linux")]
            let left = owner
                .leave_confirmed(true)
                .map(|p| p.expect("explicit leave accepted"));
            #[cfg(not(target_os = "linux"))]
            let left = participation::leave(&state);
            if left.map_err(|error| format!("Cannot leave: {error}"))? {
                Ok("This device left the share. Shared history and peers remain unchanged.".into())
            } else {
                Ok("Not participating.".into())
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_linux(
    owner: &polaris_core::apply_owner::ApplyOwnerV1,
    project: &std::path::Path,
    state: &std::path::Path,
) -> Result<String, String> {
    use polaris_core::apply_intent::{RevisionKindV1, RevisionV1};
    use polaris_core::apply_owner::{ApplyOwnerRecoveryV1, C12ApplyAdmissionV1};
    use polaris_core::management_effect::ManagementOperationIdV1;
    use std::os::unix::fs::MetadataExt;

    fn revision(value: &str) -> Result<RevisionV1, String> {
        let kind = match value.len() {
            40 => RevisionKindV1::GitSha1,
            64 => RevisionKindV1::GitSha256,
            _ => return Err("Cannot verify the agreed revision format".into()),
        };
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Cannot verify the agreed revision format".into());
        }
        Ok(RevisionV1 {
            kind,
            value: value.into(),
        })
    }

    let recovery = state.join("sync/recovery");
    let active = owner
        .active()
        .map_err(|error| format!("Cannot verify apply state: {error}"))?;
    if let Some(active) = active {
        if active.descriptor().inventory_id != polaris_core::apply_intent::ApplyInventoryV1::C12 {
            return Err(format!(
                "Desktop apply is incomplete ({}). Shared content was not applied.",
                active.descriptor().operation_key.as_hex()
            ));
        }
        let parent = owner
            .c12_recovery_parent(false)
            .map_err(|error| format!("Cannot safely pin the recovery location: {error}"))?;
        let key = active.descriptor().operation_key.as_hex();
        return match owner
            .recover_current(&parent)
            .map_err(|error| format!("Cannot recover the previous apply ({key}): {error}"))?
        {
            ApplyOwnerRecoveryV1::OutcomeUnknown(_) => Err(format!(
                "Apply outcome is unknown ({key}). No new apply was performed. Recovery backup: {}",
                recovery.display()
            )),
            ApplyOwnerRecoveryV1::Archived(_) => Err(format!(
                "Recovered and closed the previous operation ({key}). No new apply was performed. Check participation state."
            )),
            ApplyOwnerRecoveryV1::Idle => {
                Err("Apply state changed. Check participation state.".into())
            }
        };
    }
    owner
        .ensure_sync_idle()
        .map_err(|_| "Cannot apply shared content while an operation is incomplete.".to_string())?;
    let mut held = joined(state)?;
    if !held.unresolved.is_empty() {
        return Err(format!(
            "{} unresolved items. Resolve them first.\n{}",
            held.unresolved.len(),
            held.unresolved.join("\n")
        ));
    }
    let agreed = held
        .agreed_commit
        .clone()
        .ok_or_else(|| "No agreed content yet. Run `polaris sync pull` first.".to_string())?;
    if held.applied_commit.as_deref() == Some(agreed.as_str()) {
        return Ok("This content has already been applied.".into());
    }
    // Reject a cross-device recovery location before creating its directory or
    // restoring a candidate. The retained owner covers all subsequent staging.
    let source_meta =
        std::fs::metadata(project).map_err(|_| "Cannot verify the project".to_string())?;
    let state_meta =
        std::fs::metadata(state).map_err(|_| "Cannot verify the storage location".to_string())?;
    if source_meta.dev() != state_meta.dev() {
        return Err("Applying shared content requires a recovery location on the same filesystem as the project.".into());
    }
    let parent = owner
        .c12_recovery_parent(true)
        .map_err(|error| format!("Cannot safely pin the recovery location: {error}"))?;
    let paths = SyncPaths::under(state);
    let staging = paths.work_tree.clone();
    let repository = SyncRepository::open(paths)
        .map_err(|error| format!("Cannot open sync records: {error}"))?;
    if repository.head().as_deref() != Some(agreed.as_str()) {
        return Err(
            "Sync records do not match the agreed content. Run `polaris sync pull` first.".into(),
        );
    }
    repository
        .restore(&agreed)
        .map_err(|error| format!("Cannot retrieve agreed content: {error}"))?;
    let (written, removed) = repository
        .changed_paths(held.applied_commit.as_deref(), &agreed)
        .map_err(|error| format!("Cannot read changes: {error}"))?;
    let agreed_revision = revision(&agreed)?;
    let previous_applied_revision = held.applied_commit.as_deref().map(revision).transpose()?;
    held.applied_commit = Some(agreed);
    let admission = C12ApplyAdmissionV1 {
        management_operation_id: ManagementOperationIdV1::random()
            .map_err(|error| format!("Cannot prepare an operation ID: {error}"))?,
        agreed_revision: agreed_revision.clone(),
        staged_revision: agreed_revision,
        previous_applied_revision,
        target_participation: owner
            .participation_target_bytes(&held)
            .map_err(|error| format!("Cannot verify participation state: {error}"))?,
    };
    let (report, _) = polaris_core::sync::apply::apply_agreed_m2a(
        owner,
        admission,
        project,
        &staging,
        &held.manifest,
        &polaris_core::sync::apply::Changed { written, removed },
        limits(),
        &parent,
    )
    .map_err(|error| {
        format!(
            "Cannot finalize the apply. Check participation state and the recovery backup: {error}"
        )
    })?;
    if let Some(failure) = report.failure {
        return Err(format!(
            "Apply outcome is unknown: {failure:?}. Recovery backup: {}",
            recovery.display()
        ));
    }
    Ok(format!(
        "Applied {} items. Recovery backup: {}.",
        report.entries.len(),
        report
            .recovery_path
            .as_ref()
            .map_or_else(|| "none".into(), |path| path.display().to_string())
    ))
}

fn c10_command(
    project: &std::path::Path,
    recover: Option<String>,
    abandon: Option<String>,
    yes: bool,
) -> Result<String, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (project, recover, abandon, yes);
        Err(
            "c10_unavailable_platform: shared sends require the macOS Data Protection Keychain."
                .into(),
        )
    }
    #[cfg(target_os = "macos")]
    {
        use polaris_core::management_effect::ManagementOperationIdV1;
        use polaris_core::sync::push::SyncPushOwnerV1;
        use std::str::FromStr;

        let parse = |value: Option<String>| {
            value
                .map(|value| {
                    ManagementOperationIdV1::from_str(&value)
                        .map_err(|_| "Invalid C10 operation ID".to_string())
                })
                .transpose()
        };
        let recover = parse(recover)?;
        let abandon = parse(abandon)?;
        if recover.is_some() && abandon.is_some() {
            return Err("C10 recover and abandon cannot be specified together".into());
        }
        let host = CliC10Host {
            yes,
            recover,
            abandon,
            abandon_consent: std::sync::Mutex::new(None),
        };
        if recover.is_none() {
            host.require_confirmation_channel()
                .map_err(|error| error.to_string())?;
        }
        let owner = SyncPushOwnerV1::for_project(project, Box::new(host))
            .map_err(|error| error.to_string())?;
        let result = if let Some(operation) = recover {
            owner.recover(operation)
        } else if let Some(operation) = abandon {
            owner.abandon(operation)
        } else {
            let preview = owner.preview().map_err(|error| error.to_string())?;
            use std::io::Write;
            let mut output = std::io::stderr().lock();
            writeln!(
                output,
                "C10 preview (confirmation follows preparation)\n{}",
                c10_summary(preview.summary())
            )
            .map_err(|_| "Cannot display the C10 preview")?;
            output
                .flush()
                .map_err(|_| "Cannot display the C10 preview")?;
            drop(output);
            let token = owner.prepare(preview).map_err(|error| error.to_string())?;
            match c10_confirm(yes, &c10_summary(token.summary()))? {
                C08Confirmation::Accepted => token.confirm(),
                C08Confirmation::Declined => token.deny(),
            }
        };
        c10_outcome(result.map_err(|error| error.to_string())?)
    }
}

#[cfg(any(target_os = "macos", all(unix, test)))]
struct CliC10Host {
    yes: bool,
    recover: Option<polaris_core::management_effect::ManagementOperationIdV1>,
    abandon: Option<polaris_core::management_effect::ManagementOperationIdV1>,
    abandon_consent: std::sync::Mutex<
        Option<(
            polaris_core::management_effect::ManagementOperationIdV1,
            polaris_core::sync::remote_intent::SyncDigestV1,
        )>,
    >,
}

#[cfg(any(target_os = "macos", all(unix, test)))]
impl CliC10Host {
    fn require_confirmation_channel(
        &self,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        use std::io::IsTerminal;
        if self.yes || (std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            Ok(())
        } else {
            Err(polaris_core::sync::push::SyncPushErrorV1::Permission)
        }
    }
}

#[cfg(any(target_os = "macos", all(unix, test)))]
impl polaris_core::sync::push::SyncPushHostV1 for CliC10Host {
    fn require_confirmation_path(
        &self,
        _: &polaris_core::sync::push::SyncPushConfirmationSummaryV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        self.require_confirmation_channel()
    }

    fn require_recovery_invocation(
        &self,
        operation: polaris_core::management_effect::ManagementOperationIdV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        if self.recover == Some(operation) && self.abandon.is_none() {
            Ok(())
        } else {
            Err(polaris_core::sync::push::SyncPushErrorV1::Permission)
        }
    }

    fn confirm_abandon(
        &self,
        operation: polaris_core::management_effect::ManagementOperationIdV1,
        request: polaris_core::sync::remote_intent::SyncDigestV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        use polaris_core::sync::push::SyncPushErrorV1;
        if self.abandon != Some(operation) || self.recover.is_some() {
            return Err(SyncPushErrorV1::Permission);
        }
        self.require_confirmation_channel()?;
        let summary = format!(
            "C10 abandon: operation={operation}\nrequest={request}\nThis does not prove that nothing was sent."
        );
        if c10_confirm(self.yes, &summary).map_err(|_| SyncPushErrorV1::Permission)?
            != C08Confirmation::Accepted
        {
            return Err(SyncPushErrorV1::Permission);
        }
        *self
            .abandon_consent
            .lock()
            .map_err(|_| SyncPushErrorV1::Permission)? = Some((operation, request));
        Ok(())
    }

    fn require_abandon_invocation(
        &self,
        operation: polaris_core::management_effect::ManagementOperationIdV1,
        request: polaris_core::sync::remote_intent::SyncDigestV1,
    ) -> Result<(), polaris_core::sync::push::SyncPushErrorV1> {
        use polaris_core::sync::push::SyncPushErrorV1;
        self.require_confirmation_channel()?;
        if self.abandon == Some(operation)
            && self.recover.is_none()
            && *self
                .abandon_consent
                .lock()
                .map_err(|_| SyncPushErrorV1::Permission)?
                == Some((operation, request))
        {
            Ok(())
        } else {
            Err(SyncPushErrorV1::Permission)
        }
    }
}

#[cfg(any(target_os = "macos", all(unix, test)))]
fn c10_summary(summary: &polaris_core::sync::push::SyncPushConfirmationSummaryV1) -> String {
    format!(
        "C10 send: operation={}\nrequest={}\nproject={}\nshare={}\nremote={}\nref={}\nmanifest={}\nexpected remote={:?}\npayload={}\nincluded={} excluded={}\naccount registration={}\noperation={:?}",
        summary.operation_id,
        summary.request_hash,
        summary.project,
        summary.share_id,
        summary.remote_label,
        summary.remote_ref,
        summary.manifest_digest,
        summary.expected_remote,
        summary.payload_digest,
        summary.file_count,
        summary.excluded_count,
        summary.account_registration_id,
        summary.action
    )
}

#[cfg(any(target_os = "macos", all(unix, test)))]
fn c10_confirm(yes: bool, summary: &str) -> Result<C08Confirmation, String> {
    use std::io::{IsTerminal, Read, Write};
    if !(yes || std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err("c10_permission_unavailable".into());
    }
    let mut output = std::io::stderr().lock();
    writeln!(output, "{summary}").map_err(|_| "Cannot display C10 confirmation")?;
    output
        .flush()
        .map_err(|_| "Cannot display C10 confirmation")?;
    if yes {
        return Ok(C08Confirmation::Accepted);
    }
    write!(output, "Confirm this C10 operation? [y/N]: ")
        .map_err(|_| "Cannot display C10 confirmation")?;
    output
        .flush()
        .map_err(|_| "Cannot display C10 confirmation")?;
    read_c08_confirmation(&mut std::io::stdin().lock().take(257))
}

#[cfg(any(target_os = "macos", all(unix, test)))]
fn c10_outcome(outcome: polaris_core::sync::push::SyncPushOutcomeV1) -> Result<String, String> {
    use polaris_core::sync::push::SyncPushOutcomeV1;
    match outcome {
        SyncPushOutcomeV1::NoChanges => Ok("C10: shared content matches".into()),
        SyncPushOutcomeV1::Completed {
            operation_id,
            commit,
        } => Ok(format!(
            "C10 send complete: operation={operation_id} commit={commit}"
        )),
        SyncPushOutcomeV1::Denied { operation_id } => {
            Ok(format!("C10 operation cancelled: operation={operation_id}"))
        }
        SyncPushOutcomeV1::ReconciledAbandon { operation_id } => Ok(format!(
            "C10 operation reconciled and abandoned: operation={operation_id} (does not prove that nothing was sent)"
        )),
        SyncPushOutcomeV1::OutcomeUnknown { operation_id } => Err(format!(
            "C10 outcome unknown: operation={operation_id}. Do not resend; reconcile with polaris sync recover <operation-id>"
        )),
        SyncPushOutcomeV1::PartialComplete { operation_id } => Err(format!(
            "C10 persistence incomplete: operation={operation_id}. Do not resend; reconcile with polaris sync recover <operation-id>"
        )),
    }
}

fn c08_command(
    project: &std::path::Path,
    rebind: bool,
    recover: Option<String>,
    abandon: Option<String>,
    yes: bool,
    (account, choice): (Option<String>, Option<String>),
    join: Option<(ShareLink, ShareManifest, u64)>,
) -> Result<String, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (
            project, rebind, recover, abandon, yes, account, choice, join,
        );
        Err(
            "Joining shares and rebinding credentials require the macOS Data Protection Keychain."
                .into(),
        )
    }
    #[cfg(target_os = "macos")]
    {
        use polaris_auth::git_credentials::GitCredentialOwnerV1;
        use polaris_core::management_effect::ManagementOperationIdV1;
        use polaris_core::sync::credential_rebind::CredentialRebindOwnerV1;
        use std::str::FromStr;
        if !yes && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err("C08 confirmation requires a TTY or --yes.".into());
        }
        let home = std::env::var_os("HOME").ok_or_else(|| "Cannot retrieve HOME".to_string())?;
        let exe = std::env::current_exe()
            .map_err(|_| "Cannot retrieve the executable path".to_string())?;
        let askpass = exe
            .parent()
            .ok_or_else(|| "Cannot locate askpass".to_string())?
            .join("polaris-git-askpass");
        let credentials = GitCredentialOwnerV1::new(
            std::path::Path::new(&home),
            std::path::Path::new("/usr/bin/git"),
            &askpass,
        )
        .map_err(|_| "Cannot safely initialize the credential owner".to_string())?;
        let owner = CredentialRebindOwnerV1::acquire(project, credentials)
            .map_err(|_| "Cannot retrieve the C08 owner".to_string())?;
        let id = recover
            .as_ref()
            .or(abandon.as_ref())
            .map(|value| {
                ManagementOperationIdV1::from_str(value)
                    .map_err(|_| "Invalid operation ID".to_string())
            })
            .transpose()?;
        if let Some(id) = id {
            let needs_selection = owner
                .recovery_needs_selection(id)
                .map_err(|_| "Cannot verify C08 recovery conditions".to_string())?;
            let preview = if needs_selection {
                if recover.is_some() {
                    return Err(
                        "An operation without intent requires explicit abandonment with the exact request reconstructed.".into(),
                    );
                }
                Some(c08_select_preview(
                    &owner,
                    project,
                    account.as_deref(),
                    choice.as_deref(),
                    join,
                )?)
            } else {
                None
            };
            let confirmation = match &preview {
                Some(value) => Some(value.confirmation.clone()),
                None => owner
                    .recovery_confirmation(id)
                    .map_err(|_| "Cannot retrieve recovery confirmation details".to_string())?,
            };
            if c08_confirm(yes, confirmation.as_ref())? == C08Confirmation::Declined {
                return Err("C08 operation cancelled.".into());
            }
            if recover.is_some() {
                owner
                    .recover(id)
                    .map_err(|_| "Cannot recover the C08 operation".to_string())?;
                return Ok("Credential rebinding reconciled and completed.".into());
            }
            owner
                .abandon(id, preview)
                .map_err(|_| "Cannot abandon the C08 operation".to_string())?;
            return Ok("Credential rebinding abandoned.".into());
        }
        if !rebind && join.is_none() {
            return Err("Specify a C08 operation.".into());
        }
        let preview =
            c08_select_preview(&owner, project, account.as_deref(), choice.as_deref(), join)?;
        let summary = preview.confirmation.clone();
        let operation_id = preview.operation_id;
        let token = owner
            .prepare(preview)
            .map_err(|_| "Cannot prepare the C08 operation".to_string())?;
        println!("C08 operation: {}", operation_id.as_hex());
        // An input/output failure drops Prepared; it is not user denial.
        if c08_confirm(yes, Some(&summary))? == C08Confirmation::Declined {
            token
                .deny()
                .map_err(|_| "Cannot finalize the C08 cancellation record".to_string())?;
            return Err("C08 operation cancelled.".into());
        }
        token
            .confirm()
            .map_err(|_| "Cannot finalize the C08 operation".to_string())?;
        Ok(if rebind {
            "Credential rebinding confirmed.".into()
        } else {
            "Credentials bound and share joined.".into()
        })
    }
}
#[cfg(target_os = "macos")]
fn c08_select_preview(
    owner: &polaris_core::sync::credential_rebind::CredentialRebindOwnerV1,
    project: &std::path::Path,
    account: Option<&str>,
    choice: Option<&str>,
    join: Option<(ShareLink, ShareManifest, u64)>,
) -> Result<polaris_core::sync::credential_rebind::CredentialRebindPreviewV1, String> {
    let account = account.ok_or_else(|| "--account is required".to_string())?;
    if let Some((share, manifest, timestamp)) = join {
        let remote = share
            .production_remote()
            .map_err(|_| "A canonical HTTPS remote is required".to_string())?;
        let binding = owner
            .select_candidate(&remote, account, 1, choice)
            .map_err(|_| "Cannot safely select a Keychain candidate".to_string())?;
        return owner
            .preview_join(binding, share, manifest, timestamp)
            .map_err(|_| "Join preflight failed".to_string());
    }
    let state = polaris_core::project::state_dir(project)
        .map_err(|_| "Cannot retrieve state".to_string())?;
    let held = participation::read(&state)
        .map_err(|_| "Cannot safely verify participation state".to_string())?;
    let source = held
        .participation()
        .ok_or_else(|| "No share participation. New join input is required.".to_string())?;
    let remote = source
        .share
        .production_remote()
        .map_err(|_| "A canonical HTTPS remote is required".to_string())?;
    let generation = match &held {
        participation::ParticipationReadV2::LegacyParticipationOnly(_) => 1,
        participation::ParticipationReadV2::StaleCredentialBindingOnly(value) => value
            .credential_binding
            .generation()
            .checked_add(1)
            .ok_or_else(|| "Generation limit reached".to_string())?,
        _ => return Err("Not eligible for rebinding".into()),
    };
    let binding = owner
        .select_candidate(&remote, account, generation, choice)
        .map_err(|_| "Cannot safely select a Keychain candidate".to_string())?;
    owner
        .preview(binding)
        .map_err(|_| "Rebind preflight failed".to_string())
}
#[cfg(any(unix, test))]
#[derive(Debug, PartialEq, Eq)]
enum C08Confirmation {
    Accepted,
    Declined,
}
#[cfg(unix)]
fn c08_confirm(
    yes: bool,
    summary: Option<&polaris_core::sync::credential_rebind::C08ConfirmationSummaryV1>,
) -> Result<C08Confirmation, String> {
    if yes {
        return Ok(C08Confirmation::Accepted);
    }
    use std::io::{IsTerminal, Read, Write};
    if !std::io::stdin().is_terminal() {
        return Err("C08 confirmation requires a TTY or --yes.".into());
    }
    if let Some(summary) = summary {
        let summary = serde_json::to_string(summary)
            .map_err(|_| "Cannot display confirmation details".to_string())?;
        println!("{summary}");
    }
    print!("Confirm this C08 operation? [y/N]: ");
    std::io::stdout()
        .flush()
        .map_err(|_| "Cannot display confirmation".to_string())?;
    read_c08_confirmation(&mut std::io::stdin().lock().take(257))
}
#[cfg(any(unix, test))]
fn read_c08_confirmation(reader: &mut impl std::io::BufRead) -> Result<C08Confirmation, String> {
    let mut answer = String::new();
    let count = reader
        .read_line(&mut answer)
        .map_err(|_| "Cannot read confirmation".to_string())?;
    if count == 0 || answer.len() > 256 || !answer.ends_with('\n') {
        return Err("Incomplete confirmation input".into());
    }
    match answer.trim() {
        "y" | "Y" => Ok(C08Confirmation::Accepted),
        "" | "n" | "N" => Ok(C08Confirmation::Declined),
        _ => Err("Invalid confirmation input".into()),
    }
}
#[cfg(all(test, unix))]
mod c10_adapter_tests {
    use super::*;
    use polaris_core::{
        management_effect::ManagementOperationIdV1 as Operation,
        sync::{
            push::{SyncPushHostV1, SyncPushOutcomeV1},
            remote_intent::SyncDigestV1,
        },
    };

    fn host(yes: bool, recover: Option<Operation>, abandon: Option<Operation>) -> CliC10Host {
        CliC10Host {
            yes,
            recover,
            abandon,
            abandon_consent: std::sync::Mutex::new(None),
        }
    }

    #[test]
    fn c10_recover_is_exact_fresh_invocation_without_confirmation_grant() {
        let operation = Operation::random().unwrap();
        let host = host(false, Some(operation), None);
        assert!(host.require_recovery_invocation(operation).is_ok());
        assert!(
            host.require_recovery_invocation(Operation::random().unwrap())
                .is_err()
        );
        assert!(
            host.require_abandon_invocation(operation, SyncDigestV1::digest(b"request"))
                .is_err()
        );
    }

    #[test]
    fn c10_abandon_requires_fresh_exact_operation_and_request_consent() {
        let operation = Operation::random().unwrap();
        let request = SyncDigestV1::digest(b"durable request");
        let host = host(true, None, Some(operation));
        assert!(host.require_abandon_invocation(operation, request).is_err());
        assert!(
            host.confirm_abandon(Operation::random().unwrap(), request)
                .is_err()
        );
        host.confirm_abandon(operation, request).unwrap();
        assert!(host.require_abandon_invocation(operation, request).is_ok());
        assert!(
            host.require_abandon_invocation(operation, SyncDigestV1::digest(b"other request"))
                .is_err()
        );
        assert!(host.require_recovery_invocation(operation).is_err());
    }

    #[test]
    fn c10_unknown_and_partial_are_errors_while_terminal_results_are_distinct() {
        let operation_id = Operation::random().unwrap();
        for outcome in [
            SyncPushOutcomeV1::OutcomeUnknown { operation_id },
            SyncPushOutcomeV1::PartialComplete { operation_id },
        ] {
            let error = c10_outcome(outcome).unwrap_err();
            assert!(error.contains(&operation_id.as_hex()));
            assert!(error.contains("Do not resend"));
        }
        assert!(c10_outcome(SyncPushOutcomeV1::NoChanges).is_ok());
        assert!(
            c10_outcome(SyncPushOutcomeV1::Denied { operation_id })
                .unwrap()
                .contains("cancelled")
        );
        assert!(
            c10_outcome(SyncPushOutcomeV1::ReconciledAbandon { operation_id })
                .unwrap()
                .contains("does not prove that nothing was sent")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn c10_actual_noninteractive_gateway_refuses_before_state_or_credentials() {
        use std::io::IsTerminal;
        assert!(!std::io::stdin().is_terminal());
        let root = tempfile::tempdir().unwrap();
        let error = c10_command(root.path(), None, None, false).unwrap_err();
        assert_eq!(error, "c10_permission_unavailable");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert!(
            host(true, None, None)
                .require_confirmation_channel()
                .is_ok()
        );
        let error = c10_command(root.path(), Some("invalid".into()), None, false).unwrap_err();
        assert_eq!(error, "Invalid C10 operation ID");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn c10_actual_unavailable_gateway_refuses_without_creating_state() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            c10_command(root.path(), None, None, true)
                .unwrap_err()
                .starts_with("c10_unavailable_platform")
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod c08_confirmation_tests {
    use super::*;
    #[test]
    fn explicit_confirmation_and_decline_are_distinct() {
        for text in ["y\n", "Y\n"] {
            assert_eq!(
                read_c08_confirmation(&mut text.as_bytes()).unwrap(),
                C08Confirmation::Accepted
            );
        }
        for text in ["n\n", "N\n", "\n"] {
            assert_eq!(
                read_c08_confirmation(&mut text.as_bytes()).unwrap(),
                C08Confirmation::Declined
            );
        }
    }
    #[test]
    fn failed_truncated_or_invalid_input_retains_prepared() {
        for text in ["", "y", "invalid\n", &"y".repeat(257)] {
            assert!(read_c08_confirmation(&mut text.as_bytes()).is_err());
        }
        struct Failed;
        impl std::io::Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::Other.into())
            }
        }
        impl std::io::BufRead for Failed {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Err(std::io::ErrorKind::Other.into())
            }
            fn consume(&mut self, _: usize) {}
        }
        assert!(read_c08_confirmation(&mut Failed).is_err());
    }
}

#[cfg(test)]
mod c10_parser_tests {
    use super::*;

    #[derive(clap::Parser)]
    struct Parser {
        #[command(flatten)]
        sync: Args,
    }

    #[test]
    fn c10_push_parser_preserves_explicit_operation_and_rejects_conflicts() {
        use clap::Parser as _;
        let operation = "00000000000000000000000000000001";
        let canonical = Parser::try_parse_from(["polaris", "recover", operation]).unwrap();
        assert!(
            matches!(canonical.sync.command, Command::Recover { operation_id } if operation_id == operation)
        );
        let canonical = Parser::try_parse_from(["polaris", "abandon", operation, "--yes"]).unwrap();
        assert!(
            matches!(canonical.sync.command, Command::Abandon { operation_id, yes: true } if operation_id == operation)
        );
        let parsed = Parser::try_parse_from(["polaris", "push", "--recover", operation]).unwrap();
        assert!(
            matches!(parsed.sync.command, Command::Push { recover: Some(value), abandon: None, yes: false } if value == operation)
        );
        assert!(
            Parser::try_parse_from([
                "polaris",
                "push",
                "--recover",
                operation,
                "--abandon",
                operation
            ])
            .is_err()
        );
        let parsed =
            Parser::try_parse_from(["polaris", "push", "--abandon", operation, "--yes"]).unwrap();
        assert!(
            matches!(parsed.sync.command, Command::Push { recover: None, abandon: Some(value), yes: true } if value == operation)
        );
    }
}
