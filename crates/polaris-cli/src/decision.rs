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
        Command::Import { sources, yes } => (sources, Some(yes)),
    };
    let plan = managed_decision::plan_local(&sources.profile, &sources.worker, &sources.model)
        .map_err(|e| e.to_string())?;
    let consent = plan.consent_sha256().map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"consent_sha256":consent,"plan":&plan}))
            .map_err(|e| e.to_string())?
    );
    io::stdout().flush().map_err(|e| e.to_string())?;
    let Some(yes) = import else {
        return Ok(());
    };
    if !yes {
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
    let mut task = tokio::task::spawn_blocking(move || {
        managed_decision::install_local(plan, &consent, || {
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
    println!("Clef generationを導入しました。backendの選択・有効化は変更していません。");
    Ok(())
}
