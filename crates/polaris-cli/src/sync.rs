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
    /// このprojectを共有へ参加させる。資格情報は保存しない。
    Join {
        /// 同期専用のcanonical HTTPS remote。
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        share_id: Option<String>,
        #[arg(long)]
        invitation_id: Option<String>,
        /// 招待の期限（UNIX秒）。過ぎていれば参加しない。
        #[arg(long)]
        expires: Option<u64>,
        /// 常に除外するものに加えて共有しない名前。
        #[arg(long = "exclude")]
        excluded: Vec<String>,
        /// 現在の共有参加の資格情報を明示的に再束縛する。
        #[arg(long, conflicts_with_all = ["recover", "abandon"])]
        rebind_credential: bool,
        /// 指定したC08操作を照合して完了する。
        #[arg(long, value_name = "OPERATION_ID", conflicts_with_all = ["rebind_credential", "abandon"])]
        recover: Option<String>,
        /// 指定したC08操作を明示的に中止する。
        #[arg(long, value_name = "OPERATION_ID", conflicts_with_all = ["rebind_credential", "recover"])]
        abandon: Option<String>,
        /// C08操作を明示確認して非対話で実行する。
        #[arg(long)]
        yes: bool,
        /// Keychainで選択するアカウント名。
        #[arg(long)]
        account: Option<String>,
        /// 曖昧なKeychain候補のredacted prefix。
        #[arg(long)]
        credential_choice: Option<String>,
        /// intentのないjoin中止で、確認時の参加時刻を正確に再構成する。
        #[arg(long, requires_all = ["abandon", "remote"])]
        joined_at: Option<u64>,
    },
    /// 参加状態と、解消待ちの衝突を表示する。
    Status,
    /// 共有対象を記録して相手へ送る。
    Push {
        /// 指定したC10操作を再送せず照合する。
        #[arg(long, value_name = "OPERATION_ID", conflicts_with = "abandon")]
        recover: Option<String>,
        /// 指定したC10操作を新たな確認の後に中止する。
        #[arg(long, value_name = "OPERATION_ID", conflicts_with = "recover")]
        abandon: Option<String>,
        /// 表示された操作の確定を非対話で明示的に許可する。
        #[arg(long)]
        yes: bool,
    },
    /// 指定したC10操作を再送せず照合する。
    Recover { operation_id: String },
    /// 指定したC10操作を新たな確認の後に中止する。
    Abandon {
        operation_id: String,
        #[arg(long)]
        yes: bool,
    },
    /// 相手の変更を受け取り、機械的に解けるものだけ統合する。
    Pull,
    /// 受け取って統合済みの内容をprojectへ反映する。解消待ちがあれば行わない。
    Apply,
    /// この端末の参加をやめる。共有履歴と相手側はそのまま。
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
    let cwd = std::env::current_dir().map_err(|_| "作業directoryを取得できません".to_string())?;
    polaris_core::project::state_dir(&cwd).map_err(|_| "state directoryを用意できません".into())
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
        .map_err(|error| format!("参加状態を安全に確認できません: {error}"))?
        .participation()
        .cloned()
        .ok_or_else(|| "このprojectは共有に参加していません。".to_string())
}

/// Why the merge could not settle a path, in the words a person acts on.
/// The classification is the merge's own; this only spells it out.
#[cfg(not(target_os = "linux"))]
fn kept_text(reason: Kept) -> &'static str {
    match reason {
        Kept::Overlapping => "同じ箇所を双方が変更した",
        Kept::Binary => "行単位で統合できない形式である",
        Kept::DeleteVersusEdit => "一方が削除し、他方が変更した",
        Kept::AmbiguousRename => "改名の対応先を一つに定められない",
    }
}

fn execute(command: Command) -> Result<String, String> {
    if cfg!(windows) {
        return Err(match &command {
            Command::Apply => "Windowsでは共有内容の反映は未対応です。変更していません。",
            _ => "Windowsでは共有状態の保護機構が未対応です。変更していません。",
        }
        .into());
    }
    let project = polaris_core::project::resolve_root(
        &std::env::current_dir().map_err(|_| "作業directoryを取得できません".to_string())?,
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
        let join =
            if rebind_credential || recover.is_some() || (abandon.is_some() && !reconstruct_join) {
                None
            } else {
                Some((
                    ShareLink {
                        remote: remote.ok_or_else(|| "--remoteが必要です".to_string())?,
                        share_id: share_id.ok_or_else(|| "--share-idが必要です".to_string())?,
                        invitation_id: invitation_id
                            .ok_or_else(|| "--invitation-idが必要です".to_string())?,
                        expires_unix_seconds: expires
                            .ok_or_else(|| "--expiresが必要です".to_string())?,
                    },
                    ShareManifest {
                        version: 1,
                        excluded,
                    },
                    if reconstruct_join {
                        joined_at.ok_or_else(|| {
                            "join中止の再構成には確認時の--joined-atが必要です".to_string()
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
                error => format!("共有操作を開始できません: {error}"),
            },
        )?;
    let state = state_dir()?;
    #[cfg(target_os = "linux")]
    if !matches!(&command, Command::Apply | Command::Status) {
        owner.ensure_sync_idle().map_err(|_| {
            "未完了または実行中の操作があります。適用状態を確認してから共有操作を再開してください。".to_string()
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
                remote: remote.ok_or_else(|| "--remoteが必要です".to_string())?,
                share_id: share_id.ok_or_else(|| "--share-idが必要です".to_string())?,
                invitation_id: invitation_id
                    .ok_or_else(|| "--invitation-idが必要です".to_string())?,
                expires_unix_seconds: expires.ok_or_else(|| "--expiresが必要です".to_string())?,
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
                        .map_err(|_| "参加fixtureを作成できません".to_string())?;
                    let value = Participation {
                        share,
                        manifest,
                        joined_unix_seconds,
                        agreed_commit: None,
                        applied_commit: None,
                        unresolved: Vec::new(),
                    };
                    let sync = state.join("sync");
                    std::fs::create_dir_all(&sync)
                        .map_err(|_| "fixture保存場所を作成できません")?;
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&sync, std::fs::Permissions::from_mode(0o700))
                        .map_err(|_| "fixture保存場所を保護できません")?;
                    let path = sync.join("participation.json");
                    std::fs::write(
                        &path,
                        serde_json::to_vec(&value).map_err(|_| "fixtureを符号化できません")?,
                    )
                    .map_err(|_| "fixtureを保存できません")?;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                        .map_err(|_| "fixtureを保護できません")?;
                    value
                };
                #[cfg(not(target_os = "linux"))]
                let participation = participation::join(&state, share, manifest, now())
                    .map_err(|_| "参加fixtureを作成できません".to_string())?;
                Ok(format!(
                    "参加しました: {} （共有対象の規則: {}）",
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
                .map_err(|error| format!("適用状態を確認できません: {error}"))?
            {
                return Ok(format!(
                    "共有: {}\n最後に合意した内容: {}\n反映済みの内容: {}\n未完了の適用: {} ({:?})\n復旧用の控え: {}",
                    held.share.share_id,
                    held.agreed_commit.as_deref().unwrap_or("なし"),
                    held.applied_commit.as_deref().unwrap_or("なし"),
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
                "共有: {}\n最後に合意した内容: {}\n手元の記録: {}\n解消待ち: {}",
                held.share.share_id,
                held.agreed_commit.as_deref().unwrap_or("なし"),
                head.as_deref().unwrap_or("なし"),
                if held.unresolved.is_empty() {
                    "なし".to_string()
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
                    .map_err(|_| "受信結果を確定できません。共有状態を確認してください。")?
                    .expect("explicit pull accepted");
                return Ok(format!(
                    "受け取りました: {}件、解消待ち{}件。projectへの反映は別操作です。",
                    exchange.changes.len(),
                    exchange.needs_attention().len()
                ));
            }
            #[cfg(not(target_os = "linux"))]
            {
                let mut held = joined(&state)?;
                let repository = SyncRepository::open(SyncPaths::under(&state))
                    .map_err(|error| format!("同期用の記録を開けません: {error}"))?;
                // The local edits are recorded first, so a clash with what is
                // arriving is found now instead of silently disappearing.
                repository
                    .stage_from(&project, &held.manifest)
                    .map_err(|error| format!("共有対象を集められません: {error}"))?;
                repository
                    .commit("polaris sync")
                    .map_err(|error| format!("記録できません: {error}"))?;
                repository.receive(&held.share.remote).map_err(|_| {
                    "受信できません。remoteへの接続を確認してください。".to_string()
                })?;
                let exchange = repository
                    .merge_received()
                    .map_err(|error| format!("統合できません: {error}"))?;
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
                published.map_err(|_| "参加状態を保存できません".to_string())?;
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
                        "双方の内容を残しました（{}件）。解消してから`polaris sync push`を実行してください。\n{}",
                        waiting.len(),
                        waiting.join("\n")
                    ));
                }
                Ok(format!(
                    "受け取りました: {}件の変更。projectへの反映は別の操作で行います。",
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
                        "解消待ちが{}件あります。先に解消してください。\n{}",
                        held.unresolved.len(),
                        held.unresolved.join("\n")
                    ));
                }
                let Some(agreed) = held.agreed_commit.clone() else {
                    return Err(
                        "まだ合意した内容がありません。先に`polaris sync pull`を実行してください。"
                            .into(),
                    );
                };
                if held.applied_commit.as_deref() == Some(agreed.as_str()) {
                    // The same agreed state is never applied twice, however often
                    // it is redelivered.
                    return Ok("この内容は反映済みです。".into());
                }
                let paths = SyncPaths::under(&state);
                let staging = paths.work_tree.clone();
                let repository = SyncRepository::open(paths)
                    .map_err(|error| format!("同期用の記録を開けません: {error}"))?;
                if repository.head().as_deref() != Some(agreed.as_str()) {
                    return Err(
                    "同期用の記録が合意内容と一致しません。先に`polaris sync pull`を実行してください。"
                        .into(),
                );
                }
                // The staging copy is put back to the agreed commit, so what is
                // read here is the agreement and not what a previous command left.
                repository
                    .restore(&agreed)
                    .map_err(|error| format!("合意内容を取り出せません: {error}"))?;
                let (written, removed) = repository
                    .changed_paths(held.applied_commit.as_deref(), &agreed)
                    .map_err(|error| format!("変更点を読み取れません: {error}"))?;
                let changed = polaris_core::sync::apply::Changed { written, removed };
                let recovery = state.join("sync/recovery");
                std::fs::create_dir_all(&recovery)
                    .map_err(|_| "復旧用の場所を用意できません".to_string())?;
                let (report, shaped) = polaris_core::sync::apply::apply_agreed(
                    &project,
                    &staging,
                    &held.manifest,
                    &changed,
                    limits(),
                    &recovery,
                )
                .map_err(|error| format!("反映できません: {error}"))?;
                if let Some(failure) = &report.failure {
                    return Err(format!("反映できません: {failure:?}"));
                }
                let mut held = held;
                held.applied_commit = Some(agreed);
                participation::write(&state, &held)
                    .map_err(|_| "参加状態を保存できません".to_string())?;
                let deferred = if shaped.deferred.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n新しいdirectoryが要るため反映していないもの（{}件）:\n{}",
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
                    "{}件を反映しました。復旧用の控えは{}にあります。{deferred}",
                    report.entries.len(),
                    report
                        .recovery_path
                        .as_ref()
                        .map_or_else(|| "なし".into(), |path| path.display().to_string())
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
            if left.map_err(|error| format!("やめられません: {error}"))? {
                Ok("この端末の参加をやめました。共有履歴と相手側はそのままです。".into())
            } else {
                Ok("参加していません。".into())
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
            _ => return Err("合意revisionの形式を確認できません".into()),
        };
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("合意revisionの形式を確認できません".into());
        }
        Ok(RevisionV1 {
            kind,
            value: value.into(),
        })
    }

    let recovery = state.join("sync/recovery");
    let active = owner
        .active()
        .map_err(|error| format!("適用状態を確認できません: {error}"))?;
    if let Some(active) = active {
        if active.descriptor().inventory_id != polaris_core::apply_intent::ApplyInventoryV1::C12 {
            return Err(format!(
                "Desktopの適用が未完了です ({})。共有内容の適用は行っていません。",
                active.descriptor().operation_key.as_hex()
            ));
        }
        let parent = owner
            .c12_recovery_parent(false)
            .map_err(|error| format!("復旧用の場所を安全に固定できません: {error}"))?;
        let key = active.descriptor().operation_key.as_hex();
        return match owner
            .recover_current(&parent)
            .map_err(|error| format!("前回の適用を復旧できません ({key}): {error}"))?
        {
            ApplyOwnerRecoveryV1::OutcomeUnknown(_) => Err(format!(
                "適用結果が未確定です ({key})。新しい適用は行っていません。復旧用の控え: {}",
                recovery.display()
            )),
            ApplyOwnerRecoveryV1::Archived(_) => Err(format!(
                "前回の操作を復旧して閉じました ({key})。今回は新しい適用を行っていません。参加状態を確認してください。"
            )),
            ApplyOwnerRecoveryV1::Idle => {
                Err("適用状態が変わりました。参加状態を確認してください。".into())
            }
        };
    }
    owner
        .ensure_sync_idle()
        .map_err(|_| "未完了の操作があるため共有内容を反映できません。".to_string())?;
    let mut held = joined(state)?;
    if !held.unresolved.is_empty() {
        return Err(format!(
            "解消待ちが{}件あります。先に解消してください。\n{}",
            held.unresolved.len(),
            held.unresolved.join("\n")
        ));
    }
    let agreed = held.agreed_commit.clone().ok_or_else(|| {
        "まだ合意した内容がありません。先に`polaris sync pull`を実行してください。".to_string()
    })?;
    if held.applied_commit.as_deref() == Some(agreed.as_str()) {
        return Ok("この内容は反映済みです。".into());
    }
    // Reject a cross-device recovery location before creating its directory or
    // restoring a candidate. The retained owner covers all subsequent staging.
    let source_meta =
        std::fs::metadata(project).map_err(|_| "projectを確認できません".to_string())?;
    let state_meta =
        std::fs::metadata(state).map_err(|_| "保存場所を確認できません".to_string())?;
    if source_meta.dev() != state_meta.dev() {
        return Err("共有内容の適用にはprojectと同じファイルシステムの復旧場所が必要です。".into());
    }
    let parent = owner
        .c12_recovery_parent(true)
        .map_err(|error| format!("復旧用の場所を安全に固定できません: {error}"))?;
    let paths = SyncPaths::under(state);
    let staging = paths.work_tree.clone();
    let repository = SyncRepository::open(paths)
        .map_err(|error| format!("同期用の記録を開けません: {error}"))?;
    if repository.head().as_deref() != Some(agreed.as_str()) {
        return Err(
            "同期用の記録が合意内容と一致しません。先に`polaris sync pull`を実行してください。"
                .into(),
        );
    }
    repository
        .restore(&agreed)
        .map_err(|error| format!("合意内容を取り出せません: {error}"))?;
    let (written, removed) = repository
        .changed_paths(held.applied_commit.as_deref(), &agreed)
        .map_err(|error| format!("変更点を読み取れません: {error}"))?;
    let agreed_revision = revision(&agreed)?;
    let previous_applied_revision = held.applied_commit.as_deref().map(revision).transpose()?;
    held.applied_commit = Some(agreed);
    let admission = C12ApplyAdmissionV1 {
        management_operation_id: ManagementOperationIdV1::random()
            .map_err(|error| format!("操作IDを用意できません: {error}"))?,
        agreed_revision: agreed_revision.clone(),
        staged_revision: agreed_revision,
        previous_applied_revision,
        target_participation: owner
            .participation_target_bytes(&held)
            .map_err(|error| format!("参加状態を確認できません: {error}"))?,
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
        format!("反映を確定できません。参加状態と復旧用の控えを確認してください: {error}")
    })?;
    if let Some(failure) = report.failure {
        return Err(format!(
            "適用結果が未確定です: {failure:?}。復旧用の控え: {}",
            recovery.display()
        ));
    }
    Ok(format!(
        "{}件を反映しました。復旧用の控えは{}にあります。",
        report.entries.len(),
        report
            .recovery_path
            .as_ref()
            .map_or_else(|| "なし".into(), |path| path.display().to_string())
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
        Err("c10_unavailable_platform: 共有送信はmacOSのData Protection Keychainでのみ利用できます。".into())
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
                        .map_err(|_| "C10 operation IDが不正です".to_string())
                })
                .transpose()
        };
        let recover = parse(recover)?;
        let abandon = parse(abandon)?;
        if recover.is_some() && abandon.is_some() {
            return Err("C10 recoverとabandonは同時に指定できません".into());
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
                "C10 preview（確定はprepare後）\n{}",
                c10_summary(preview.summary())
            )
            .map_err(|_| "C10 previewを表示できません")?;
            output.flush().map_err(|_| "C10 previewを表示できません")?;
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
            "C10中止: operation={operation}\nrequest={request}\n送信されなかったことの証明にはなりません。"
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
        "C10送信: operation={}\nrequest={}\nproject={}\n共有={}\nremote={}\nref={}\nmanifest={}\nexpected remote={:?}\npayload={}\n対象={}件・除外={}件\naccount registration={}\n操作={:?}",
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
    writeln!(output, "{summary}").map_err(|_| "C10確認を表示できません")?;
    output.flush().map_err(|_| "C10確認を表示できません")?;
    if yes {
        return Ok(C08Confirmation::Accepted);
    }
    write!(output, "このC10操作を確定しますか [y/N]: ").map_err(|_| "C10確認を表示できません")?;
    output.flush().map_err(|_| "C10確認を表示できません")?;
    read_c08_confirmation(&mut std::io::stdin().lock().take(257))
}

#[cfg(any(target_os = "macos", all(unix, test)))]
fn c10_outcome(outcome: polaris_core::sync::push::SyncPushOutcomeV1) -> Result<String, String> {
    use polaris_core::sync::push::SyncPushOutcomeV1;
    match outcome {
        SyncPushOutcomeV1::NoChanges => Ok("C10: 共有内容は一致しています".into()),
        SyncPushOutcomeV1::Completed {
            operation_id,
            commit,
        } => Ok(format!(
            "C10送信完了: operation={operation_id} commit={commit}"
        )),
        SyncPushOutcomeV1::Denied { operation_id } => {
            Ok(format!("C10操作を取り消しました: operation={operation_id}"))
        }
        SyncPushOutcomeV1::ReconciledAbandon { operation_id } => Ok(format!(
            "C10操作を照合して中止しました: operation={operation_id}（未送信の証明ではありません）"
        )),
        SyncPushOutcomeV1::OutcomeUnknown { operation_id } => Err(format!(
            "C10結果未確定: operation={operation_id}。再送せず polaris sync recover <operation-id> で照合してください"
        )),
        SyncPushOutcomeV1::PartialComplete { operation_id } => Err(format!(
            "C10保存未完了: operation={operation_id}。再送せず polaris sync recover <operation-id> で照合してください"
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
            "共有参加とcredential rebindはmacOSのData Protection Keychainでのみ利用できます。"
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
            return Err("C08の確定にはTTY確認または--yesが必要です。".into());
        }
        let home = std::env::var_os("HOME").ok_or_else(|| "HOMEを取得できません".to_string())?;
        let exe = std::env::current_exe().map_err(|_| "実行binaryを取得できません".to_string())?;
        let askpass = exe
            .parent()
            .ok_or_else(|| "askpassの場所を取得できません".to_string())?
            .join("polaris-git-askpass");
        let credentials = GitCredentialOwnerV1::new(
            std::path::Path::new(&home),
            std::path::Path::new("/usr/bin/git"),
            &askpass,
        )
        .map_err(|_| "credential ownerを安全に初期化できません".to_string())?;
        let owner = CredentialRebindOwnerV1::acquire(project, credentials)
            .map_err(|_| "C08 ownerを取得できません".to_string())?;
        let id = recover
            .as_ref()
            .or(abandon.as_ref())
            .map(|value| {
                ManagementOperationIdV1::from_str(value)
                    .map_err(|_| "operation IDが不正です".to_string())
            })
            .transpose()?;
        if let Some(id) = id {
            let needs_selection = owner
                .recovery_needs_selection(id)
                .map_err(|_| "C08の復旧条件を確認できません".to_string())?;
            let preview = if needs_selection {
                if recover.is_some() {
                    return Err(
                        "intentのない操作はexact requestを再構成した明示abandonが必要です。".into(),
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
                    .map_err(|_| "復旧確認内容を取得できません".to_string())?,
            };
            if c08_confirm(yes, confirmation.as_ref())? == C08Confirmation::Declined {
                return Err("C08操作を取り消しました。".into());
            }
            if recover.is_some() {
                owner
                    .recover(id)
                    .map_err(|_| "C08操作を復旧できません".to_string())?;
                return Ok("資格情報の再束縛を照合して完了しました。".into());
            }
            owner
                .abandon(id, preview)
                .map_err(|_| "C08操作を中止できません".to_string())?;
            return Ok("資格情報の再束縛を中止しました。".into());
        }
        if !rebind && join.is_none() {
            return Err("C08操作を指定してください。".into());
        }
        let preview =
            c08_select_preview(&owner, project, account.as_deref(), choice.as_deref(), join)?;
        let summary = preview.confirmation.clone();
        let operation_id = preview.operation_id;
        let token = owner
            .prepare(preview)
            .map_err(|_| "C08操作を準備できません".to_string())?;
        println!("C08 operation: {}", operation_id.as_hex());
        // An input/output failure drops Prepared; it is not user denial.
        if c08_confirm(yes, Some(&summary))? == C08Confirmation::Declined {
            token
                .deny()
                .map_err(|_| "C08取消の記録を確定できません".to_string())?;
            return Err("C08操作を取り消しました。".into());
        }
        token
            .confirm()
            .map_err(|_| "C08操作を確定できません".to_string())?;
        Ok(if rebind {
            "資格情報の再束縛を確定しました。".into()
        } else {
            "資格情報を束縛して共有へ参加しました。".into()
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
    let account = account.ok_or_else(|| "--accountが必要です".to_string())?;
    if let Some((share, manifest, timestamp)) = join {
        let remote = share
            .production_remote()
            .map_err(|_| "canonical HTTPS remoteが必要です".to_string())?;
        let binding = owner
            .select_candidate(&remote, account, 1, choice)
            .map_err(|_| "Keychain候補を安全に選択できません".to_string())?;
        return owner
            .preview_join(binding, share, manifest, timestamp)
            .map_err(|_| "joinの事前確認に失敗しました".to_string());
    }
    let state = polaris_core::project::state_dir(project)
        .map_err(|_| "stateを取得できません".to_string())?;
    let held =
        participation::read(&state).map_err(|_| "参加状態を安全に確認できません".to_string())?;
    let source = held
        .participation()
        .ok_or_else(|| "共有参加がありません。新規joinの入力が必要です。".to_string())?;
    let remote = source
        .share
        .production_remote()
        .map_err(|_| "canonical HTTPS remoteが必要です".to_string())?;
    let generation = match &held {
        participation::ParticipationReadV2::LegacyParticipationOnly(_) => 1,
        participation::ParticipationReadV2::StaleCredentialBindingOnly(value) => value
            .credential_binding
            .generation()
            .checked_add(1)
            .ok_or_else(|| "generationが上限です".to_string())?,
        _ => return Err("再束縛の対象ではありません".into()),
    };
    let binding = owner
        .select_candidate(&remote, account, generation, choice)
        .map_err(|_| "Keychain候補を安全に選択できません".to_string())?;
    owner
        .preview(binding)
        .map_err(|_| "再束縛の事前確認に失敗しました".to_string())
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
        return Err("C08の確定にはTTY確認または--yesが必要です。".into());
    }
    if let Some(summary) = summary {
        let summary =
            serde_json::to_string(summary).map_err(|_| "確認内容を表示できません".to_string())?;
        println!("{summary}");
    }
    print!("このC08操作を確定しますか [y/N]: ");
    std::io::stdout()
        .flush()
        .map_err(|_| "確認を表示できません".to_string())?;
    read_c08_confirmation(&mut std::io::stdin().lock().take(257))
}
#[cfg(any(unix, test))]
fn read_c08_confirmation(reader: &mut impl std::io::BufRead) -> Result<C08Confirmation, String> {
    let mut answer = String::new();
    let count = reader
        .read_line(&mut answer)
        .map_err(|_| "確認を読めません".to_string())?;
    if count == 0 || answer.len() > 256 || !answer.ends_with('\n') {
        return Err("確認入力が不完全です".into());
    }
    match answer.trim() {
        "y" | "Y" => Ok(C08Confirmation::Accepted),
        "" | "n" | "N" => Ok(C08Confirmation::Declined),
        _ => Err("確認入力が不正です".into()),
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
            assert!(error.contains("再送せず"));
        }
        assert!(c10_outcome(SyncPushOutcomeV1::NoChanges).is_ok());
        assert!(
            c10_outcome(SyncPushOutcomeV1::Denied { operation_id })
                .unwrap()
                .contains("取り消し")
        );
        assert!(
            c10_outcome(SyncPushOutcomeV1::ReconciledAbandon { operation_id })
                .unwrap()
                .contains("未送信の証明ではありません")
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
        assert_eq!(error, "C10 operation IDが不正です");
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
