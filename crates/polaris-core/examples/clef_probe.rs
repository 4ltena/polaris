//! Offline integration smoke for an explicitly pinned Clef installation.
//! Run through the project's common benchmark runner, with synthetic state.

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() {
    use polaris_core::clef::{ClefConfig, ClefDecider, ClefDevice};
    use polaris_provider::decision::{Decider, Question};
    use std::{collections::BTreeMap, path::PathBuf, time::Instant};
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert_eq!(
        args.len(),
        7,
        "worker worker_sha256 model model_bytes model_sha256 generation private_root"
    );
    let config = ClefConfig {
        model_identity: None,
        admission: None,
        worker: PathBuf::from(&args[0]),
        worker_sha256: args[1].clone(),
        model: PathBuf::from(&args[2]),
        model_bytes: args[3].parse().expect("model bytes"),
        model_sha256: args[4].clone(),
        generation: args[5].clone(),
        private_root: PathBuf::from(&args[6]),
        device: ClefDevice::Cpu,
        context_tokens: 1024,
        request_timeout_ms: 120000,
        load_timeout_ms: 120000,
    };
    let decider = ClefDecider::new(config).expect("pinned configuration");
    let questions = BTreeMap::from([
        (
            "is/billing?".into(),
            Question::Noul {
                instructions: "Is this a billing or refund problem?".into(),
            },
        ),
        (
            "route/担当".into(),
            Question::Choice {
                instructions: "Which team should handle this?".into(),
                criteria: BTreeMap::from([
                    (
                        "billing/請求".into(),
                        "payments, duplicate charges, refunds".into(),
                    ),
                    ("shipping/配送".into(), "parcels, delivery, tracking".into()),
                    (
                        "engineering/開発".into(),
                        "software bugs and code changes".into(),
                    ),
                ]),
            },
        ),
        (
            "urgency".into(),
            Question::Score {
                instructions: "How urgent is the request?".into(),
                criteria: vec!["can wait".into(), "today".into(), "right now".into()],
            },
        ),
    ]);
    let billing = "I was charged twice for one order. This is a billing problem. Please refund the duplicate charge right now.";
    let delivery = "My package is missing and its tracking has stopped. This is a delivery problem, not a billing or refund request.";
    let mut probabilities = Vec::new();
    for (state, expected) in [
        (billing, "billing/請求"),
        (delivery, "shipping/配送"),
        (billing, "billing/請求"),
    ] {
        let started = Instant::now();
        let result = decider
            .decide(&serde_json::json!(state), &questions)
            .await
            .expect("native owner decision");
        assert_eq!(
            result.answers["route/担当"].choice.as_deref(),
            Some(expected)
        );
        probabilities.push(result.answers["is/billing?"].noul.unwrap());
        println!(
            "{}",
            serde_json::json!({"stage":"answered","seconds":started.elapsed().as_secs_f64(),
            "backend":decider.name(),"input_tokens":result.input_tokens,"output_tokens":result.output_tokens})
        );
    }
    assert!(probabilities[0] > probabilities[1]);
    assert!((probabilities[0] - probabilities[2]).abs() < 0.001);
    decider.shutdown().await.expect("confirmed owner shutdown");
    println!(
        "{}",
        serde_json::json!({"status":"passed","requests":3,"answers":9,"shutdown":"confirmed",
        "limits":"CPU synthetic smoke; not calibration, GPU or capacity acceptance"})
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The native Clef worker currently supports macOS only.");
    std::process::exit(2);
}
