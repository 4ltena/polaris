//! 実装が終わったことを、実機で試せる人へ渡す文面にする。
//!
//! 変更したファイルは git から読む。手で並べた一覧は、書き忘れたものを
//! 「変えていない」と言ってしまう。文面は実装が終わったことだけを示し、
//! 実機での合格は含まない。

#[cfg(unix)]
use polaris_core::handoff::{Implementation, Refusal, prompt};
#[cfg(unix)]
use polaris_core::project_git::read_only_git;
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[derive(clap::Args)]
pub struct Args {
    /// 対象プロジェクト。既定は現在のディレクトリ。
    #[arg(long)]
    project: Option<PathBuf>,
    /// 実装の概要。
    #[arg(long)]
    summary: String,
    /// 実機で確認してほしいこと。繰り返して複数渡す。
    #[arg(long = "check", required = true)]
    checks: Vec<String>,
    /// 実装の世代。省略すると現在の HEAD を使う。
    #[arg(long)]
    run: Option<String>,
    #[arg(long, default_value = "1")]
    attempt: String,
}

#[cfg(unix)]
fn refusal_text(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::NoChanges => {
            "変更されたファイルがありません。試してもらうものがないため文面を作りません。"
        }
        Refusal::NoChecks => "確認してほしいことがありません。`--check` を渡してください。",
        Refusal::Bounds => "内容が上限を超えています。概要か確認項目を短くしてください。",
    }
}

/// 追跡済みの変更と、追跡されていないファイルの両方。どちらも「試す対象」
/// であり、新規ファイルを落とすと実機で足りないものが出る。
#[cfg(unix)]
fn changed(root: &std::path::Path) -> Result<Vec<String>, String> {
    let opened = std::fs::File::open(root).map_err(|_| "対象を開けません。".to_owned())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let bytes = read_only_git(
        &opened,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=all",
        ],
        deadline,
    )
    .map_err(|_| "Git の状態を読めません。".to_owned())?;
    let mut paths = Vec::new();
    for record in bytes.split(|byte| *byte == 0).filter(|held| held.len() > 3) {
        if let Ok(path) = std::str::from_utf8(&record[3..]) {
            paths.push(path.to_owned());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(unix)]
fn head(root: &std::path::Path) -> Option<String> {
    let opened = std::fs::File::open(root).ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let bytes = read_only_git(
        &opened,
        &["rev-parse", "--verify", "--short=12", "HEAD"],
        deadline,
    )
    .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(unix)]
pub fn run(args: Args) -> ExitCode {
    let cwd = args
        .project
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let root = polaris_core::project::resolve_root(&cwd);
    let paths = match changed(&root) {
        Ok(paths) => paths,
        Err(text) => {
            eprintln!("{text}");
            return ExitCode::FAILURE;
        }
    };
    // 世代が分からないまま文面を出すと、後の run が確認済みのものと取り違え
    // られる。指定も HEAD も無ければ止まる。
    let Some(run_id) = args.run.or_else(|| head(&root)) else {
        eprintln!("実装の世代が分かりません。`--run` を渡してください。");
        return ExitCode::FAILURE;
    };
    let implementation = Implementation {
        run_id,
        attempt_id: args.attempt,
        project_root: root.display().to_string(),
        paths,
        summary: args.summary,
        checks: args.checks,
    };
    match prompt(&implementation) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(refusal) => {
            eprintln!("{}", refusal_text(refusal));
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
pub fn run(_args: Args) -> ExitCode {
    eprintln!(
        "このプラットフォームではGitからの引き継ぎ情報取得に必要な保護機構が未対応です。変更していません。"
    );
    ExitCode::FAILURE
}
