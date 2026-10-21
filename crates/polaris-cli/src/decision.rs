//! Explicit local decision management, separate from agent/provider startup.
use polaris_core::managed_decision;
use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// 登録profileと容量の推薦を表示。導入・認証・通信・推論なし。
    List,
    /// 明示導入の内容をJSONで表示。ファイルの取得・copyなし。
    Plan(Sources),
    /// 確認した固定artifactをcopy。backend設定や推論は開始しない。
    Import {
        #[command(flatten)]
        sources: Sources,
        /// 表示する今回の計画で導入することを明示承認。
        #[arg(long)]
        yes: bool,
        /// Internal same-process confirmation channel for the packaged GUI.
        #[arg(long, hide = true, conflicts_with = "yes")]
        stdio_confirm: bool,
    },
}

#[derive(clap::Args)]
struct Sources {
    profile: String,
    /// 固定SHAと照合するworkerの絶対path。
    #[arg(long)]
    worker: PathBuf,
    /// 固定SHAと照合するmodelの絶対path。
    #[arg(long)]
    model: PathBuf,
}

pub async fn run(args: Args) -> ExitCode {
    match perform(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn perform(args: Args) -> Result<(), String> {
    let (sources, import) = match args.command {
        Command::List => {
            let registered = managed_decision::registered_profiles().map_err(|e| e.to_string())?;
            let observation = polaris_core::decision_capacity::observation::host_memory();
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "schema_version":1,
                "registered_profiles":registered,
                "capacity":polaris_core::decision_capacity::observation::diagnostic_lines(&observation),
                "note":"登録は導入・実行可否の確認ではありません。Jev/SemIfの設定は変更しません。"
            })).map_err(|e| e.to_string())?);
            return Ok(());
        }
        Command::Plan(sources) => (sources, None),
        Command::Import {
            sources,
            yes,
            stdio_confirm,
        } => (sources, Some((yes, stdio_confirm))),
    };
    let plan = managed_decision::plan_local(&sources.profile, &sources.worker, &sources.model)
        .map_err(|e| e.to_string())?;
    let consent = plan.consent_sha256().map_err(|e| e.to_string())?;
    let frame =
        serde_json::json!({"schema_version":1,"type":"plan","consent_sha256":consent,"plan":&plan});
    let stdio = import.is_some_and(|(_, stdio)| stdio);
    println!(
        "{}",
        if stdio {
            serde_json::to_string(&frame)
        } else {
            serde_json::to_string_pretty(&frame)
        }
        .map_err(|e| e.to_string())?
    );
    io::stdout().flush().map_err(|e| e.to_string())?;
    let Some((yes, stdio)) = import else {
        return Ok(());
    };
    if stdio {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let expected = consent.clone();
        std::thread::spawn(move || {
            let _ = sender.send(decode_confirmation(io::stdin().lock(), &expected));
        });
        let approved = tokio::task::spawn_blocking(move || {
            receiver.recv_timeout(std::time::Duration::from_secs(120))
        })
        .await
        .map_err(|_| "導入確認の待機を終了できませんでした")?
        .map_err(|_| "導入確認が期限内に届きませんでした。変更していません。")??;
        if !approved {
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"type":"complete","status":"cancelled","consent_sha256":consent})
            );
            return Ok(());
        }
    } else if !yes {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err("導入確認には対話端末、または今回の選択への明示的な --yes が必要です。変更していません。".into());
        }
        eprint!("表示した計画で導入しますか？ [y/n]: ");
        io::stderr().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        io::stdin()
            .lock()
            .take(16)
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        if answer.trim() != "y" {
            eprintln!("Clef導入を取り消しました。変更していません。");
            return Ok(());
        }
    }
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    let execution_consent = consent.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        managed_decision::install_local(plan, &execution_consent, || {
            flag.load(std::sync::atomic::Ordering::Acquire)
        })
    });
    let result = tokio::select! {
        result = &mut task => result,
        signal = tokio::signal::ctrl_c() => {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            let result = task.await;
            signal.map_err(|e| e.to_string())?;
            result
        }
    };
    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if stdio {
        println!(
            "{}",
            serde_json::json!({"schema_version":1,"type":"complete","status":"installed","consent_sha256":consent})
        );
    } else {
        println!("Clef generationを導入しました。backendの選択・有効化は変更していません。");
    }
    Ok(())
}

fn decode_confirmation(reader: impl Read, expected: &str) -> Result<bool, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Confirmation {
        schema_version: u32,
        consent_sha256: String,
        decision: String,
    }
    let mut bytes = Vec::new();
    io::BufReader::new(reader)
        .take(4097)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "導入確認を読み取れませんでした")?;
    if bytes.len() > 4096 || bytes.last() != Some(&b'\n') {
        return Err("導入確認のframeが不正です。変更していません。".into());
    }
    let frame: Confirmation = serde_json::from_slice(&bytes)
        .map_err(|_| "導入確認の形式が不正です。変更していません。")?;
    if frame.schema_version != 1 || frame.consent_sha256 != expected {
        return Err("導入確認が今回の計画と一致しません。変更していません。".into());
    }
    match frame.decision.as_str() {
        "approve" => Ok(true),
        "cancel" => Ok(false),
        _ => Err("導入確認の応答が不明です。変更していません。".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_an_exact_bounded_confirmation_can_approve_the_live_plan() {
        let hash = "a".repeat(64);
        for (choice, expected) in [("approve", true), ("cancel", false)] {
            let frame = format!(
                "{{\"schema_version\":1,\"consent_sha256\":\"{hash}\",\"decision\":\"{choice}\"}}\n"
            );
            assert_eq!(
                decode_confirmation(frame.as_bytes(), &hash).unwrap(),
                expected
            );
            assert!(decode_confirmation(frame.as_bytes(), &"b".repeat(64)).is_err());
        }
        for bad in ["{}\n", "{}", "", "{\"schema_version\":2}\n"] {
            assert!(decode_confirmation(bad.as_bytes(), &hash).is_err());
        }
        assert!(decode_confirmation(&vec![b' '; 4097][..], &hash).is_err());
        let extra = format!(
            "{{\"schema_version\":1,\"consent_sha256\":\"{hash}\",\"decision\":\"approve\",\"other\":true}}\n"
        );
        assert!(decode_confirmation(extra.as_bytes(), &hash).is_err());
    }
}
