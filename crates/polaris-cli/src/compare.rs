//! フォルダ同士がどれだけ似ているかを示す。名前とサイズだけを読み、
//! 内容は開かない。提案は見に行く理由であって、移動の指示ではない。

use polaris_core::similarity::{self, Suggestion};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// Folders to compare; specify at least two.
    #[arg(required = true, num_args = 2..)]
    folders: Vec<PathBuf>,
}

fn suggestion_text(suggestion: Suggestion) -> &'static str {
    match suggestion {
        Suggestion::SameWorkInTwoPlaces => "The same work may exist in two locations",
        Suggestion::SharedPart => "A shared component may be extractable",
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
                eprintln!("Cannot read {id}.");
                return ExitCode::FAILURE;
            }
        }
    }
    let graph = similarity::graph(&folders);
    for id in &truncated {
        println!("{id}: only a bounded portion was read. A low similarity result is unreliable.");
    }
    if graph.omitted > 0 {
        println!(
            "{} items were not compared because of the limit.",
            graph.omitted
        );
    }
    if graph.edges.is_empty() {
        println!("No similar folders found.");
        return ExitCode::SUCCESS;
    }
    for edge in &graph.edges {
        println!(
            "{}\t{}\tsimilarity {}\t{} shared items",
            edge.left,
            edge.right,
            edge.similarity,
            edge.shared.len()
        );
    }
    for proposal in &graph.proposals {
        println!(
            "Suggestion\t{} and {}\t{}\t{}",
            proposal.left,
            proposal.right,
            suggestion_text(proposal.suggestion),
            proposal.because
        );
    }
    println!("Suggestions identify locations to inspect. Nothing is moved or deleted.");
    ExitCode::SUCCESS
}
