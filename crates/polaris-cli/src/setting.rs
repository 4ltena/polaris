//! 同梱の定型コマンド。引数の受け取りと表示だけを行い、検出・計画・記録は
//! coreの`recipe`が持つ。実際のパッケージ導入は利用者の確認を経る。

#[cfg(unix)]
use polaris_core::recipe::{
    self,
    confirm::Verdict,
    git::{GitPlan, GitRefusal, Head, Placement},
    ignore_rules::{Withheld, WithheldReason},
};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// Target project; defaults to the current directory.
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Detect Git and add missing .gitignore rules.
    Git,
}

#[cfg(unix)]
pub fn run(args: Args) -> ExitCode {
    let cwd = args
        .project
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let result = match args.command {
        Command::Git => git(&cwd),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(text) => {
            eprintln!("{text}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn git_refusal(reason: &GitRefusal) -> String {
    match reason {
        GitRefusal::NoRepository => {
            "No Git repository. Creating one is the user's decision.".into()
        }
        GitRefusal::InsideAnother { top_level } => {
            format!(
                "This directory belongs to the repository at {top_level}. Nothing will be created here."
            )
        }
        GitRefusal::BareRepository => "A bare repository has no working tree.".into(),
        GitRefusal::Unreadable => "Could not read Git status.".into(),
    }
}

#[cfg(unix)]
fn withheld_reason(held: &Withheld) -> String {
    match &held.reason {
        WithheldReason::AlreadyPresent => format!("  {}\talready exists", held.rule),
        WithheldReason::ContradictsNegation { line } => {
            format!("  {}\tdeferred due to conflict with {line}", held.rule)
        }
    }
}

/// M7c-3c の規則。既存の行・順序・否定規則を保ったまま、不足分だけを足す。
#[cfg(unix)]
const NEXT_JS_IGNORES: &[&str] = &[".next/", "node_modules/", "coverage/"];

#[cfg(unix)]
fn placement_text(found: &Placement) -> String {
    let committed = |head: &Head| match head {
        Head::Committed => "has commits",
        Head::Unborn => "no commits",
    };
    match found {
        Placement::None => "No repository".into(),
        Placement::Own { head } => format!(
            "This directory is the repository root ({})",
            committed(head)
        ),
        Placement::Worktree { head } => {
            format!("Linked worktree ({})", committed(head))
        }
        Placement::Inside { top_level } => format!("Inside the repository at {top_level}"),
        Placement::Bare => "Bare repository".into(),
    }
}

#[cfg(unix)]
fn git(cwd: &std::path::Path) -> Result<(), String> {
    let root = polaris_core::project::resolve_root(cwd);
    let opened = std::fs::File::open(&root).map_err(|_| "Cannot open the target.".to_owned())?;
    let placement = recipe::git::placement(&opened);
    if let Ok(found) = &placement {
        println!("Git: {}", placement_text(found));
    }
    match recipe::git::plan(placement) {
        GitPlan::NotApplicable { reason } => {
            println!("{}", git_refusal(&reason));
            Ok(())
        }
        GitPlan::TidyOnly => tidy(cwd, &root),
    }
}

/// 不足規則の追加を、台帳へ記録しながら行う。実行の前にintentを、実行の後に
/// 終端を保存する。終端は事後条件を読み直してから決める。
#[cfg(unix)]
fn tidy(cwd: &std::path::Path, root: &std::path::Path) -> Result<(), String> {
    let existing = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    let (add, withheld) = recipe::ignore_rules::plan_additions(&existing, NEXT_JS_IGNORES);
    for held in &withheld {
        println!("{}", withheld_reason(held));
    }
    if add.is_empty() {
        println!("No additions needed in .gitignore.");
        return Ok(());
    }

    let fail = |e: polaris_core::desktop_store::StoreError| recipe::session::refusal(&e).to_owned();
    let (mut writer, _, session) = recipe::session::open(cwd).map_err(fail)?;
    let request = format!("git-tidy-{}", now_id());
    let target = recipe::session::start_run(&mut writer, &session, &request).map_err(fail)?;
    let operation = recipe::session::operation("gitignore").map_err(fail)?;
    let digest = polaris_core::conversation_state::content_hash(existing.as_bytes());
    writer
        .record_recipe_intent(
            &target,
            operation.clone(),
            polaris_core::desktop_store::SavedRecipeIntent {
                schema_version: polaris_core::desktop_store::RECIPE_SCHEMA_VERSION,
                recipe_id: "git".into(),
                recipe_version: recipe::SCHEMA_VERSION,
                step_id: "gitignore".into(),
                after: Vec::new(),
                input_digest: digest,
                lockfile_digest_before: None,
                source: None,
                requested_versions: add.iter().map(|held| (*held).to_owned()).collect(),
                environment: std::env::consts::OS.into(),
            },
        )
        .map_err(fail)?;

    let written = recipe::ignore_rules::append(root, &add);
    // 書いた結果をもう一度読み直して判定する。書き込みが成功を返したことは、
    // 規則がそこにあることと同じではない。
    let after = std::fs::read_to_string(root.join(".gitignore"));
    let verdict = match (&written, &after) {
        (Err(_), _) | (_, Err(_)) => Verdict::Unknown {
            reason: "Could not read or write .gitignore".into(),
        },
        (Ok(()), Ok(body)) => {
            let missing: Vec<&str> = add
                .iter()
                .copied()
                .filter(|rule| !body.lines().any(|line| line.trim() == *rule))
                .collect();
            if missing.is_empty() {
                Verdict::Met {
                    evidence: format!(".gitignore: {}", add.join(", ")),
                }
            } else {
                Verdict::NotMet {
                    missing: format!("Rules missing from .gitignore: {}", missing.join(", ")),
                }
            }
        }
    };
    let terminal = recipe::confirm::terminal(
        recipe::confirm::Ended {
            exit_code: Some(if written.is_ok() { 0 } else { 1 }),
            signal: None,
            cancelled: false,
        },
        verdict.clone(),
        None,
    );
    let outcome = terminal.outcome;
    writer
        .record_recipe_terminal(&target, &operation, terminal)
        .map_err(fail)?;
    match verdict {
        Verdict::Met { evidence } => {
            println!("Added: {evidence}");
            Ok(())
        }
        Verdict::NotMet { missing } | Verdict::Unknown { reason: missing } => {
            Err(format!("{missing} (recorded outcome: {outcome:?})"))
        }
    }
}

/// 呼出しごとの識別子。同じ呼出しの再送は同じrunになる。
#[cfg(unix)]
fn now_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|held| held.as_nanos())
        .unwrap_or_default()
}

#[cfg(not(unix))]
pub fn run(_args: Args) -> ExitCode {
    eprintln!(
        "This platform lacks the protections required for configuration changes. No changes were made."
    );
    ExitCode::FAILURE
}
