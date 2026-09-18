//! 頼まれていない整理へ余剰枠を使ってよいかを判定し、その理由を示す。
//!
//! 判定だけを行う。ここから整理が始まることはない。今の同梱profileでは
//! 利用枠の読み取りが確定しないため、ほぼ必ず「読み取りが無い」で止まる。
//! それが意図した状態であり、実アカウントで読み取りが取れるまで変わらない。

use polaris_core::spare::{
    Decision, Refusal,
    from_quota::{Allowance, observed},
};
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// 頼まれていない整理を許す。付けない限り判定は通らない。
    #[arg(long)]
    allow: bool,
    /// 自分の作業のために残しておく割合（0〜100）。
    #[arg(long, default_value_t = 50)]
    reserve: u32,
    /// 整理の候補数。0なら行うことが無い。
    #[arg(long, default_value_t = 0)]
    candidates: usize,
}

fn refusal_text(reason: Refusal) -> &'static str {
    match reason {
        Refusal::NotAllowed => {
            "頼まれていない作業は許可されていません。`--allow` を付けると判定を続けます。"
        }
        Refusal::QuotaUnknown => {
            "利用枠の読み取りがありません。実アカウントでの取得は未実施のため、ここで止まります。"
        }
        Refusal::NoSpare => "残りは自分の作業に要る分です。",
        Refusal::Busy => "実行中のrunがあります。その枠は余剰ではありません。",
        Refusal::NothingToDo => "行うだけの候補がありません。",
    }
}

pub fn run(args: Args) -> ExitCode {
    // 実アカウントからの取得は未実施gate。読み取りが無いことを「0件だから
    // 全部既知」に変換しないよう、空の集合を渡して判定させる。
    let held = observed(
        &[],
        Allowance {
            allowed_by_person: args.allow,
            reserved_percent: args.reserve,
        },
        false,
        args.candidates,
    );
    match polaris_core::spare::decide(&held) {
        Decision::Start => {
            // The proposals are the analysis's, not this command's: it has
            // no folders to compare. Saving an empty set is refused by the
            // store, so a run with nothing to propose says so rather than
            // leaving a file that reads as "nothing was found".
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(error) => {
                    eprintln!("作業フォルダを読めません: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let state = match polaris_core::project::state_dir(&cwd) {
                Ok(state) => state,
                Err(error) => {
                    eprintln!("保存先を決められません: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let graph = polaris_core::similarity::graph(&[]);
            match polaris_core::spare::proposal::save(&state, &held, graph.proposals) {
                Ok(path) => println!(
                    "余剰枠での整理を始められます。案は {} へ書きました。統合は利用者の承認を経ます。",
                    path.display()
                ),
                Err(_) => println!(
                    "余剰枠での整理は始められますが、書くだけの案がありません。何も保存していません。"
                ),
            }
            ExitCode::SUCCESS
        }
        Decision::Refuse { reason } => {
            println!("{}", refusal_text(reason));
            ExitCode::SUCCESS
        }
    }
}
