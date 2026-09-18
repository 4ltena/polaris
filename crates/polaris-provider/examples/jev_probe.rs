//! J-5: a real request to the configured decider, with synthetic questions
//! only. Nothing from this project's files, memory or conversation is sent.
//!
//! Run with the key in the environment:
//!   set -a; . ./.env.local; set +a
//!   cargo run -p polaris-provider --example jev_probe
use polaris_provider::decision::{Decider, Question, jev::JevDecider};
use std::collections::BTreeMap;

#[tokio::main]
async fn main() {
    let decider = match JevDecider::from_environment() {
        Ok(held) => held,
        Err(error) => {
            eprintln!("判定器を作れません: {error:?}（POLARIS_JEV_API_KEY を確認）");
            std::process::exit(1);
        }
    };
    let state = serde_json::json!({
        "task": "画像のファイルサイズを小さくしたい",
        "phase": "plan",
    });
    let asks = [
        ("clear-yes", "resize and optimize raster assets", true),
        ("clear-no", "send an email to a mailing list", false),
        (
            "near-miss",
            "read image metadata without modifying the file",
            false,
        ),
        ("synonym", "圧縮してファイル容量を落とす", true),
    ];
    let mut questions = BTreeMap::new();
    for (key, description, _) in &asks {
        questions.insert(
            (*key).to_owned(),
            Question::Noul {
                instructions: format!(
                    "このtoolは依頼が求めていることを行うか。依頼: 画像のファイルサイズを小さくしたい / tool: {description}"
                ),
            },
        );
    }
    // Choice と Score でも確信度が返るかを見る。Noul だけの結果では
    // 「この型では返らない」と「そもそも返らない」を区別できない。
    questions.insert(
        "which-tool".to_owned(),
        Question::Choice {
            instructions:
                "この依頼に最も合うtoolはどれか。依頼: 画像のファイルサイズを小さくしたい"
                    .to_owned(),
            criteria: BTreeMap::from([
                ("resize".to_owned(), "画像を縮小して最適化する".to_owned()),
                ("send-email".to_owned(), "メールを送る".to_owned()),
                ("read-metadata".to_owned(), "메타情報を読むだけ".to_owned()),
            ]),
        },
    );
    questions.insert(
        "how-hard".to_owned(),
        Question::Score {
            instructions: "この依頼の難しさはどれか。依頼: 画像のファイルサイズを小さくしたい"
                .to_owned(),
            criteria: vec![
                "すぐ終わる".to_owned(),
                "少し調べる".to_owned(),
                "設計が要る".to_owned(),
            ],
        },
    );
    let started = std::time::Instant::now();
    match decider.decide(&state, &questions).await {
        Ok(decision) => {
            let elapsed = started.elapsed();
            println!("応答 {} ms", elapsed.as_millis());
            println!(
                "使用量 入力 {:?} / 出力 {:?}",
                decision.input_tokens, decision.output_tokens
            );
            for (key, held) in &decision.answers {
                println!(
                    "  {key}: noul {:?} / choice {:?} / score {:?} / confidence {:?} / probabilities {:?}",
                    held.noul, held.choice, held.score, held.confidence, held.probabilities
                );
            }
            for (key, description, expected) in &asks {
                let yes = decision.answers.get(*key).and_then(|held| held.noul);
                let confidence = decision.answers.get(*key).and_then(|held| held.confidence);
                println!(
                    "{key}\t期待 {}\tyes {:?}\tconfidence {:?}\t{description}",
                    if *expected { "有用" } else { "無関係" },
                    yes,
                    confidence
                );
            }
        }
        Err(error) => {
            eprintln!("応答なし: {error:?}");
            std::process::exit(1);
        }
    }
}
