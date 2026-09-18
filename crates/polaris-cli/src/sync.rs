//! M7b-5: the `polaris sync` entry points. Every command works on this
//! project's own state; nothing here writes into the project directly, and
//! the exchange uses the credentials Git already has.
use clap::{Args as ClapArgs, Subcommand};
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
fn kept_text(reason: Kept) -> &'static str {
    match reason {
        Kept::Overlapping => "同じ箇所を双方が変更した",
        Kept::Binary => "行単位で統合できない形式である",
        Kept::DeleteVersusEdit => "一方が削除し、他方が変更した",
        Kept::AmbiguousRename => "改名の対応先を一つに定められない",
    }
}

fn execute(command: Command) -> Result<String, String> {
    let state = state_dir()?;
    let project = polaris_core::project::resolve_root(
        &std::env::current_dir().map_err(|_| "作業directoryを取得できません".to_string())?,
    );
    match command {
        Command::Join {
            remote,
            share_id,
            invitation_id,
            expires,
            excluded,
        } => {
            let participation = participation::join(
                &state,
                ShareLink {
                    remote,
                    share_id,
                    invitation_id,
                    expires_unix_seconds: expires,
                },
                ShareManifest {
                    version: 1,
                    excluded,
                },
                now(),
            )
            .map_err(|error| format!("参加できません: {error}"))?;
            Ok(format!(
                "参加しました: {} （共有対象の規則: {}）",
                participation.share.share_id,
                participation.manifest.digest()
            ))
        }
        Command::Status => {
            let held = joined(&state)?;
            let paths = SyncPaths::under(&state);
            let head = SyncRepository::open(paths)
                .ok()
                .and_then(|repository| repository.head());
            Ok(format!(
                "共有: {}\n最後に合意した内容: {}\n手元の記録: {}\n解消待ち: {}",
                held.share.share_id,
                held.agreed_commit.as_deref().unwrap_or("なし"),
                head.as_deref().unwrap_or("なし"),
                if held.unresolved.is_empty() {
                    "なし".to_string()
                } else {
                    held.unresolved.join(", ")
                }
            ))
        }
        Command::Push => {
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
            participation::write(&state, &held)
                .map_err(|_| "参加状態を保存できません".to_string())?;
            Ok(format!(
                "送信しました: {}件（共有しなかったもの: {}件）",
                staged.files,
                staged.skipped.len()
            ))
        }
        Command::Pull => {
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
            repository
                .receive(&held.share.remote)
                .map_err(|_| "受信できません。remoteへの接続を確認してください。".to_string())?;
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
            participation::write(&state, &held)
                .map_err(|_| "参加状態を保存できません".to_string())?;
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
        Command::Apply => {
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
        Command::Leave => {
            if participation::leave(&state).map_err(|_| "やめられません".to_string())? {
                Ok("この端末の参加をやめました。共有履歴と相手側はそのままです。".into())
            } else {
                Ok("参加していません。".into())
            }
        }
    }
}
