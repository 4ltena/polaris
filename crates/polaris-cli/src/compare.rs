//! フォルダ同士がどれだけ似ているかを示す。名前とサイズだけを読み、
//! 内容は開かない。提案は見に行く理由であって、移動の指示ではない。

use polaris_core::similarity::{self, Suggestion};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// 比較するフォルダ。2つ以上。
    #[arg(required = true, num_args = 2..)]
    folders: Vec<PathBuf>,
}

fn suggestion_text(suggestion: Suggestion) -> &'static str {
    match suggestion {
        Suggestion::SameWorkInTwoPlaces => "同じ作業が2箇所にある可能性",
        Suggestion::SharedPart => "共通部分を切り出せる可能性",
    }
}

pub fn run(args: Args) -> ExitCode {
    let mut folders = Vec::new();
    let mut truncated = Vec::new();
    for path in &args.folders {
        // 表示名はフォルダ名。同じ名前が2つあると読めなくなるので、
        // その場合だけ path をそのまま使う。
        let id = path.display().to_string();
        match similarity::sample::folder(&id, path) {
            Ok(sampled) => {
                if sampled.truncated {
                    truncated.push(id);
                }
                folders.push(sampled.folder);
            }
            Err(_) => {
                eprintln!("{id} を読めません。");
                return ExitCode::FAILURE;
            }
        }
    }
    let graph = similarity::graph(&folders);
    for id in &truncated {
        println!("{id}: 上限まで読んだ一部です。似ていないという結果は当てになりません。");
    }
    if graph.omitted > 0 {
        println!("{}件は上限のため比較していません。", graph.omitted);
    }
    if graph.edges.is_empty() {
        println!("似ているフォルダはありませんでした。");
        return ExitCode::SUCCESS;
    }
    for edge in &graph.edges {
        println!(
            "{}\t{}\t類似度 {}\t共通 {}件",
            edge.left,
            edge.right,
            edge.similarity,
            edge.shared.len()
        );
    }
    for proposal in &graph.proposals {
        println!(
            "提案\t{} と {}\t{}\t{}",
            proposal.left,
            proposal.right,
            suggestion_text(proposal.suggestion),
            proposal.because
        );
    }
    println!("提案は見に行く理由です。移動も削除も行いません。");
    ExitCode::SUCCESS
}
