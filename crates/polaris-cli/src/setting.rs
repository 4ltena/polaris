//! 同梱の定型コマンド。引数の受け取りと表示だけを行い、検出・計画・記録は
//! coreの`recipe`が持つ。実際のパッケージ導入は利用者の確認を経る。

#[cfg(unix)]
use polaris_core::recipe::{
    self, Manager, Plan, Refusal,
    confirm::{Postcondition, Verdict, confirm, postcondition},
    git::{GitPlan, GitRefusal, Head, Placement},
    ignore_rules::{Withheld, WithheldReason},
};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// 対象プロジェクト。既定は現在のディレクトリ。
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Next.jsの検出・導入・設定・検証。実導入は`--install`で確認する。
    Nextjs {
        /// パッケージの実導入を確認する。付けない場合は導入の手前で止まる。
        #[arg(long)]
        install: bool,
    },
    /// Gitの検出と、不足する`.gitignore`規則の追加。
    Git,
}

#[cfg(unix)]
pub fn run(args: Args) -> ExitCode {
    let cwd = args
        .project
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let result = match args.command {
        Command::Nextjs { install } => nextjs(&cwd, install),
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
fn refusal(reason: Refusal) -> &'static str {
    match reason {
        Refusal::NoManifest => "package.json がないため、この手順は当てはまりません。",
        Refusal::MixedManagers => {
            "lockfile が複数あります。どれを使うか決まらないため何もしません。"
        }
        Refusal::AlreadyComplete => "すべての工程が完了済みです。",
        Refusal::InstallNotConfirmed => {
            "導入の確認がないため、ここで止まります。`--install` を付けて実行してください。"
        }
    }
}

#[cfg(unix)]
fn manager_name(manager: Manager) -> &'static str {
    manager.program()
}

/// 台帳を読み、残りの工程と、各工程の事後条件を表示する。実行そのものは
/// M7c-3d（起動確認のprocess所有）まで未実装なので、確認待ちとして示す。
#[cfg(unix)]
fn nextjs(cwd: &std::path::Path, install: bool) -> Result<(), String> {
    let (writer, ..) =
        recipe::session::open(cwd).map_err(|e| recipe::session::refusal(&e).to_owned())?;
    let root = polaris_core::project::resolve_root(cwd);
    // The manager decides which lockfile the ledger compares, so it is
    // settled before anything is read back as finished.
    let looked = recipe::observe(&root, &recipe::NEXT_JS, Vec::new(), install);
    let manager = match recipe::manager_for(&looked) {
        Ok(manager) => manager,
        Err(reason) => {
            println!("{}", refusal(reason));
            return Ok(());
        }
    };
    let completed = recipe::reusable_steps_for(&writer, &recipe::NEXT_JS, &root, manager);
    let observed = recipe::observe(&root, &recipe::NEXT_JS, completed.clone(), install);
    if !completed.is_empty() {
        println!("完了済み（再実行しません）: {}", completed.join(", "));
    }
    match recipe::plan(&recipe::NEXT_JS, &observed) {
        Plan::NotApplicable { reason } => {
            println!("{}", refusal(reason));
            Ok(())
        }
        Plan::Run { manager, steps } => {
            println!("管理ツール: {}", manager_name(manager));
            for step in &steps {
                let command = recipe::command(&recipe::NEXT_JS, step, manager)
                    .map(|(program, args)| format!("{program} {}", args.join(" ")))
                    .unwrap_or_else(|| "（hostが設定を書きます）".into());
                let state = match postcondition(&recipe::NEXT_JS, step) {
                    Some(condition) => state_of(&root, &condition),
                    None => "確認方法が未定義".into(),
                };
                println!("  {step}\t{command}\t{state}");
            }
            if steps.contains(&"configure") {
                let (write, skip) = recipe::template::plan(&root, recipe::template::NEXT_JS);
                for (path, why) in &skip {
                    println!("  設定 {path}\t{}", skipped_reason(why));
                }
                if !write.is_empty() {
                    println!(
                        "  設定 書き込む雛形: {}",
                        write
                            .iter()
                            .map(|held| held.path)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            println!(
                "パッケージの導入と起動確認は未実装です（M7c-3d 待ち）。ここまでが検出と計画です。"
            );
            Ok(())
        }
    }
}

/// 事後条件が今の時点で満たされているかを、実行前の状態として示す。
#[cfg(unix)]
fn state_of(root: &std::path::Path, condition: &Postcondition) -> String {
    // 検出工程は管理ツールの応答が答えなので、実行前には判定できない。
    if matches!(condition, Postcondition::ManagerAnswered) {
        return "未実行".into();
    }
    match confirm(root, condition, "") {
        Verdict::Met { evidence } => format!("充足済み: {evidence}"),
        Verdict::NotMet { missing } => format!("未達: {missing}"),
        Verdict::Unknown { reason } => format!("判定不能: {reason}"),
    }
}

#[cfg(unix)]
fn skipped_reason(why: &recipe::template::Skipped) -> &'static str {
    match why {
        recipe::template::Skipped::AlreadyPresent => "既にあるので触りません",
        recipe::template::Skipped::NotAPlainFile => "通常のファイルではないので触りません",
    }
}

#[cfg(unix)]
fn git_refusal(reason: &GitRefusal) -> String {
    match reason {
        GitRefusal::NoRepository => {
            "Git リポジトリがありません。作成するかどうかは利用者の判断です。".into()
        }
        GitRefusal::InsideAnother { top_level } => {
            format!(
                "このディレクトリは {top_level} のリポジトリに属します。ここには何も作りません。"
            )
        }
        GitRefusal::BareRepository => "bare リポジトリには作業ツリーがありません。".into(),
        GitRefusal::Unreadable => "Git の状態を読めませんでした。".into(),
    }
}

#[cfg(unix)]
fn withheld_reason(held: &Withheld) -> String {
    match &held.reason {
        WithheldReason::AlreadyPresent => format!("  {}\t既にあります", held.rule),
        WithheldReason::ContradictsNegation { line } => {
            format!("  {}\t{line} と衝突するため保留", held.rule)
        }
    }
}

/// M7c-3c の規則。既存の行・順序・否定規則を保ったまま、不足分だけを足す。
#[cfg(unix)]
const NEXT_JS_IGNORES: &[&str] = &[".next/", "node_modules/", "coverage/"];

#[cfg(unix)]
fn placement_text(found: &Placement) -> String {
    let committed = |head: &Head| match head {
        Head::Committed => "コミットあり",
        Head::Unborn => "コミットなし",
    };
    match found {
        Placement::None => "リポジトリはありません".into(),
        Placement::Own { head } => format!("このディレクトリが最上位です（{}）", committed(head)),
        Placement::Worktree { head } => {
            format!("リンクされた作業ツリーです（{}）", committed(head))
        }
        Placement::Inside { top_level } => format!("{top_level} のリポジトリの内側です"),
        Placement::Bare => "bare リポジトリです".into(),
    }
}

#[cfg(unix)]
fn git(cwd: &std::path::Path) -> Result<(), String> {
    let root = polaris_core::project::resolve_root(cwd);
    let opened = std::fs::File::open(&root).map_err(|_| "対象を開けません。".to_owned())?;
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
        println!(".gitignore に足すものはありません。");
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
            reason: ".gitignore を読み書きできませんでした".into(),
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
                    missing: format!(".gitignore に無い規則: {}", missing.join(", ")),
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
            println!("追加しました: {evidence}");
            Ok(())
        }
        Verdict::NotMet { missing } | Verdict::Unknown { reason: missing } => {
            Err(format!("{missing}（記録: {outcome:?}）"))
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
    eprintln!("このプラットフォームでは設定の変更に必要な保護機構が未対応です。変更していません。");
    ExitCode::FAILURE
}
