//! M7b-5: the `polaris sync` entry points. Every command works on this
//! project's own state; nothing here writes into the project directly, and
//! the exchange uses the credentials Git already has.
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
        /// 同期専用のGit remote（SSH/HTTPS）。
        #[arg(long)]
        remote: String,
        #[arg(long)]
        share_id: String,
        #[arg(long)]
        invitation_id: String,
        /// 招待の期限（UNIX秒）。過ぎていれば参加しない。
        #[arg(long)]
        expires: u64,
        /// 常に除外するものに加えて共有しない名前。
        #[arg(long = "exclude")]
        excluded: Vec<String>,
    },
    /// 参加状態と、解消待ちの衝突を表示する。
    Status,
    /// 共有対象を記録して相手へ送る。
    Push,
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
    participation::read(state).ok_or_else(|| "このprojectは共有に参加していません。".to_string())
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
    let project = polaris_core::project::resolve_root(
        &std::env::current_dir().map_err(|_| "作業directoryを取得できません".to_string())?,
    );
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
        } => {
            let share = ShareLink {
                remote,
                share_id,
                invitation_id,
                expires_unix_seconds: expires,
            };
            let manifest = ShareManifest {
                version: 1,
                excluded,
            };
            #[cfg(target_os = "linux")]
            let participation = owner
                .join_confirmed(share, manifest, now(), true)
                .map(|p| p.expect("explicit join accepted"));
            #[cfg(not(target_os = "linux"))]
            let participation = participation::join(&state, share, manifest, now());
            let participation =
                participation.map_err(|error| format!("参加できません: {error}"))?;
            Ok(format!(
                "参加しました: {} （共有対象の規則: {}）",
                participation.share.share_id,
                participation.manifest.digest()
            ))
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
        Command::Push => {
            #[cfg(target_os = "linux")]
            return Err(
                "共有送信ownerは未実装です。credential実機検証が必要なため送信できません。".into(),
            );
            #[cfg(not(target_os = "linux"))]
            {
                let held = joined(&state)?;
                if held.agreed_commit != held.applied_commit {
                    // Sending from the project now would record the other side's
                    // changes as removals, because the project does not hold them.
                    return Err(
                    "受け取った内容をまだprojectへ反映していません。先に`polaris sync apply`を実行してください。"
                        .into(),
                );
                }
                let repository = SyncRepository::open(SyncPaths::under(&state))
                    .map_err(|error| format!("同期用の記録を開けません: {error}"))?;
                let staged = repository
                    .stage_from(&project, &held.manifest)
                    .map_err(|error| format!("共有対象を集められません: {error}"))?;
                let commit = repository
                    .commit("polaris sync")
                    .map_err(|error| format!("記録できません: {error}"))?;
                repository.send(&held.share.remote).map_err(|_| {
                    "送信できません。先に`polaris sync pull`を実行してください。".to_string()
                })?;
                let mut held = held;
                held.agreed_commit = commit.clone().or(held.agreed_commit);
                // What was sent came from the project, so the two already match.
                held.applied_commit = held.agreed_commit.clone();
                #[cfg(target_os = "linux")]
                let published = owner.write_participation(&held);
                #[cfg(not(target_os = "linux"))]
                let published = participation::write(&state, &held);
                published.map_err(|_| "参加状態を保存できません".to_string())?;
                Ok(format!(
                    "送信しました: {}件（共有しなかったもの: {}件）",
                    staged.files,
                    staged.skipped.len()
                ))
            }
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
        target_participation: serde_json::to_vec_pretty(&held)
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
