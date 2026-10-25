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
    /// Allow unrequested cleanup assessment. Required to pass admission.
    #[arg(long)]
    allow: bool,
    /// Percentage of capacity reserved for your work (0–100).
    #[arg(long, default_value_t = 50)]
    reserve: u32,
    /// Number of cleanup candidates. Zero means no work is available.
    #[arg(long, default_value_t = 0)]
    candidates: usize,
}

fn refusal_text(reason: Refusal) -> &'static str {
    match reason {
        Refusal::NotAllowed => {
            "Unrequested work is not authorized. Specify --allow to continue assessment."
        }
        Refusal::QuotaUnknown => {
            "No usage limit observation. Real account retrieval has not been performed; stopping here."
        }
        Refusal::NoSpare => "Remaining capacity is reserved for your work.",
        Refusal::Busy => "A run is active. Its capacity is not spare.",
        Refusal::NothingToDo => "No actionable candidates.",
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
                    eprintln!("Cannot read the working folder: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let state = match polaris_core::project::state_dir(&cwd) {
                Ok(state) => state,
                Err(error) => {
                    eprintln!("Cannot determine the destination: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let graph = polaris_core::similarity::graph(&[]);
            match polaris_core::spare::proposal::save(&state, &held, graph.proposals) {
                Ok(path) => println!(
                    "Spare capacity permits cleanup planning. A proposal was written to {}. Integration requires user approval.",
                    path.display()
                ),
                Err(_) => println!(
                    "Spare capacity permits cleanup, but no proposal is available. Nothing was saved."
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
