//! Offline, content-free measurements of one M12 input case and preparation mode per process.
use polaris_core::{
    budget::count_tokens,
    conversation_state::content_hash,
    input_preparation::{InputPreparationMode, prepare},
};
use serde_json::{Value, json};
use std::{fs::File, io::Read, path::PathBuf, time::Instant};

const CASE_IDS: [&str; 4] = ["I01", "I02", "I03", "I04"];
const MAX_CASE_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_REPETITIONS: usize = 20;
const USAGE: &str = "使用法: m12_input_eval --cases ABSOLUTE_JSON --case I01|I02|I03|I04 --mode off|repeat_refs --repetitions 1..20";

#[derive(Debug)]
struct Args {
    cases: PathBuf,
    case: String,
    mode: InputPreparationMode,
    repetitions: usize,
}

fn parse_args(values: &[String]) -> Result<Args, String> {
    if values.len() != 8 {
        return Err(USAGE.into());
    }
    let (mut cases, mut case, mut mode, mut repetitions) = (None, None, None, None);
    for pair in values.chunks_exact(2) {
        let target = match pair[0].as_str() {
            "--cases" => &mut cases,
            "--case" => &mut case,
            "--mode" => &mut mode,
            "--repetitions" => &mut repetitions,
            _ => return Err(USAGE.into()),
        };
        if target.replace(pair[1].clone()).is_some() {
            return Err(USAGE.into());
        }
    }
    let cases = PathBuf::from(cases.ok_or(USAGE)?);
    let case = case.ok_or(USAGE)?;
    let mode = match mode.as_deref() {
        Some("off") => InputPreparationMode::Off,
        Some("repeat_refs") => InputPreparationMode::RepeatRefs,
        _ => return Err(USAGE.into()),
    };
    let repetitions = repetitions
        .ok_or(USAGE)?
        .parse::<usize>()
        .map_err(|_| USAGE)?;
    if !cases.is_absolute()
        || !CASE_IDS.contains(&case.as_str())
        || !(1..=MAX_REPETITIONS).contains(&repetitions)
    {
        return Err(USAGE.into());
    }
    Ok(Args {
        cases,
        case,
        mode,
        repetitions,
    })
}

struct Case {
    id: String,
    input: String,
    expected_projection: String,
    sha256: String,
}

struct Cases {
    sha256: String,
    cases: Vec<Case>,
}

fn parse_cases(bytes: &[u8]) -> Result<Cases, String> {
    if bytes.len() as u64 > MAX_CASE_FILE_BYTES {
        return Err("ケースファイルが上限を超えています".into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "ケースJSONが不正です")?;
    if value["schema_version"] != 1 || value["model_requests"] != 0 {
        return Err("ケースの版またはオフライン条件が不正です".into());
    }
    let rows = value["cases"].as_array().ok_or("ケース一覧がありません")?;
    if rows.len() != CASE_IDS.len() {
        return Err("ケース数は4件である必要があります".into());
    }
    let mut cases: Vec<Case> = Vec::with_capacity(rows.len());
    for row in rows {
        let field = |name| {
            row.get(name)
                .and_then(Value::as_str)
                .ok_or("ケースの文字列項目が不正です")
        };
        let id = field("id")?;
        let input = field("input")?;
        let sha256 = field("sha256")?;
        let expected_projection = field("expected_projection")?;
        if !CASE_IDS.contains(&id)
            || cases.iter().any(|case| case.id == id)
            || input.is_empty()
            || input.len() > MAX_INPUT_BYTES
            || row["input_bytes"].as_u64() != Some(input.len() as u64)
            || content_hash(input.as_bytes()) != sha256
            || !row["expected"].is_object()
            || !matches!(
                expected_projection,
                "applied" | "protected_syntax" | "insufficient_savings"
            )
        {
            return Err("ケースの識別子・原文・期待値が不正です".into());
        }
        cases.push(Case {
            id: id.into(),
            input: input.into(),
            expected_projection: expected_projection.into(),
            sha256: sha256.into(),
        });
    }
    Ok(Cases {
        sha256: content_hash(bytes),
        cases,
    })
}

fn load_cases(path: &PathBuf) -> Result<Cases, String> {
    let file = File::open(path).map_err(|_| "ケースファイルを開けません")?;
    let metadata = file.metadata().map_err(|_| "ケース情報を取得できません")?;
    if !metadata.is_file() || metadata.len() > MAX_CASE_FILE_BYTES {
        return Err("ケースは上限内の通常ファイルである必要があります".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CASE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "ケースファイルを読めません")?;
    parse_cases(&bytes)
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn evaluate(cases: &Cases, args: &Args) -> Result<Value, String> {
    let case = cases
        .cases
        .iter()
        .find(|case| case.id == args.case)
        .ok_or("対象ケースがありません")?;
    let expected_reason = match args.mode {
        InputPreparationMode::Off => "off",
        InputPreparationMode::RepeatRefs => &case.expected_projection,
    };
    let mut records = Vec::with_capacity(args.repetitions);
    let mut invariants_passed = true;
    // This executable has no earlier tokenizer call. Unit tests do not establish cold timings.
    let mut tokenizer_called = false;
    for repetition in 1..=args.repetitions {
        let tokenizer_called_before_prepare = tokenizer_called;
        let started = Instant::now();
        let prepared = prepare(&case.input, args.mode);
        let prepare_elapsed_nanos = elapsed_nanos(started);
        let first_call_in_prepare = !tokenizer_called && prepared.metrics.input_tokens.is_some();
        tokenizer_called |= prepared.metrics.input_tokens.is_some();

        // Measurement and reconstruction are outside the preparation timing. In off mode,
        // this is the first tokenizer call; no artificial warmup is hidden in the first arm.
        let started = Instant::now();
        let first_call_in_accounting = !tokenizer_called;
        let input_tokens = count_tokens(&case.input);
        tokenizer_called = true;
        let request = prepared
            .projection
            .as_ref()
            .map_or(case.input.as_str(), |projection| {
                projection.encoded.as_str()
            });
        let request_tokens = count_tokens(request);
        let reversible_match = prepared.projection.as_ref().is_none_or(|projection| {
            projection.reconstruct().as_deref() == Some(case.input.as_str())
        });
        let original_unchanged = content_hash(case.input.as_bytes()) == case.sha256;
        let reason = prepared.metrics.reason.as_str();
        let expected_reason_matches = reason == expected_reason;
        let input_token_bounds_verified = (4096..=32_000).contains(&input_tokens);
        let accounting_elapsed_nanos = elapsed_nanos(started);
        invariants_passed &= reversible_match
            && original_unchanged
            && expected_reason_matches
            && input_token_bounds_verified;
        records.push(json!({
            "repetition": repetition,
            "phase": if repetition == 1 { "first" } else { "warm" },
            "input_sha256": case.sha256,
            "input_bytes": case.input.len(),
            "input_tokens": input_tokens,
            "request_bytes": request.len(),
            "request_tokens": request_tokens,
            "projection_applied": prepared.projection.is_some(),
            "reversible_match": reversible_match,
            "original_unchanged": original_unchanged,
            "reason": reason,
            "expected_reason_matches": expected_reason_matches,
            "input_token_bounds_verified": input_token_bounds_verified,
            "prepare_elapsed_nanos": prepare_elapsed_nanos,
            "accounting_elapsed_nanos": accounting_elapsed_nanos,
            "host_metrics": prepared.metrics,
            "tokenizer": {
                "called_before_prepare": tokenizer_called_before_prepare,
                "first_call_in": if first_call_in_prepare {
                    Some("prepare")
                } else if first_call_in_accounting {
                    Some("accounting")
                } else {
                    None
                },
                "initialization_expected_in_first_call": !tokenizer_called_before_prepare,
                "initialization_separately_instrumented": false
            }
        }));
    }
    let warm = records.split_off(1);
    Ok(json!({
        "schema_version": 1,
        "measurement": "m12_input_preparation_offline",
        "case_file_sha256": cases.sha256,
        "case_id": case.id,
        "expected_projection": case.expected_projection,
        "mode": args.mode,
        "input_sha256": case.sha256,
        "model_requests": 0,
        "semantic_evaluation": "not_performed",
        "token_count_basis": "reference_o200k_base",
        "request_scope": "user_input_projection_only_no_request_sent",
        "build": {
            "profile_class": if cfg!(debug_assertions) { "debug" } else { "release" },
            "profile_basis": "cfg(debug_assertions)",
            "debug_assertions": cfg!(debug_assertions)
        },
        "tokenizer_initialization_basis": "fresh_process_first_use_no_prior_harness_tokenizer_calls",
        "repetitions": args.repetitions,
        "first": records.remove(0),
        "warm": warm,
        "invariants_passed": invariants_passed
    }))
}

fn main() {
    let result = parse_args(&std::env::args().skip(1).collect::<Vec<_>>())
        .and_then(|args| load_cases(&args.cases).and_then(|cases| evaluate(&cases, &args)));
    match result {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("JSON report")
            );
            if report["invariants_passed"] != true {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(case: &str, mode: &str, repetitions: &str) -> Vec<String> {
        [
            "--cases",
            "/fixture/cases.json",
            "--case",
            case,
            "--mode",
            mode,
            "--repetitions",
            repetitions,
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn fixture(input: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "status": "synthetic_candidate_inputs_only",
            "model_requests": 0,
            "cases": CASE_IDS.map(|id| json!({
                "id": id, "kind": "offline_fixture", "input": input,
                "expected": {"marker": "unchanged"}, "expected_projection": "applied",
                "sha256": content_hash(input.as_bytes()), "input_bytes": input.len()
            }))
        }))
        .unwrap()
    }

    #[test]
    fn arguments_require_one_known_case_mode_and_bounded_repetitions() {
        for repetitions in ["1", "20"] {
            assert!(parse_args(&arguments("I01", "repeat_refs", repetitions)).is_ok());
        }
        for (case, mode, repetitions) in [
            ("I05", "off", "1"),
            ("I01", "unknown", "1"),
            ("I01", "off", "0"),
            ("I01", "off", "21"),
        ] {
            assert!(parse_args(&arguments(case, mode, repetitions)).is_err());
        }
        let mut args = arguments("I01", "off", "1");
        args[0] = "--case".into();
        assert!(parse_args(&args).is_err());
        assert!(parse_args(&args[..6]).is_err());
    }

    #[test]
    fn fixture_identity_bytes_and_hash_are_validated_without_modification() {
        let bytes = fixture("original input");
        let original = bytes.clone();
        let cases = parse_cases(&bytes).unwrap();
        assert_eq!(bytes, original);
        assert_eq!(cases.sha256, content_hash(&bytes));
        for field in ["id", "input", "input_bytes", "sha256"] {
            let mut value: Value = serde_json::from_slice(&bytes).unwrap();
            value["cases"][0][field] = json!("tampered");
            assert!(parse_cases(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["cases"][1]["id"] = json!("I01");
        assert!(parse_cases(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn report_does_not_echo_freeform_fixture_metadata() {
        let mut value: Value = serde_json::from_slice(&fixture("synthetic input")).unwrap();
        let marker = "private-body-sentinel";
        value["status"] = json!(marker);
        value["cases"][0]["kind"] = json!(marker);
        value["cases"][0]["expected"] = json!({"text": marker});
        let cases = parse_cases(&serde_json::to_vec(&value).unwrap()).unwrap();
        let args = parse_args(&arguments("I01", "off", "1")).unwrap();
        let report = evaluate(&cases, &args).unwrap();
        assert!(!report.to_string().contains(marker));
    }

    #[test]
    fn off_and_repeat_reports_preserve_original_and_separate_first_from_warm() {
        let original = format!("{}\n\n", "alpha beta gamma delta epsilon. ".repeat(140)).repeat(8);
        let bytes = fixture(&original);
        let cases = parse_cases(&bytes).unwrap();
        for mode in ["off", "repeat_refs"] {
            let args = parse_args(&arguments("I01", mode, "2")).unwrap();
            let report = evaluate(&cases, &args).unwrap();
            assert_eq!(report["invariants_passed"], true);
            assert_eq!(report["model_requests"], 0);
            assert_eq!(report["semantic_evaluation"], "not_performed");
            assert!(report.get("expected").is_none());
            assert_eq!(report["first"]["phase"], "first");
            assert_eq!(report["warm"].as_array().unwrap().len(), 1);
            assert_eq!(report["warm"][0]["phase"], "warm");
            assert_eq!(report["first"]["reversible_match"], true);
            assert_eq!(report["first"]["original_unchanged"], true);
            assert_eq!(cases.cases[0].input, original);
            assert!(!report.to_string().contains("alpha beta gamma"));
            if mode == "off" {
                assert_eq!(report["first"]["host_metrics"]["input_tokens"], Value::Null);
                assert_eq!(
                    report["first"]["input_tokens"],
                    report["first"]["request_tokens"]
                );
                assert_eq!(report["first"]["tokenizer"]["first_call_in"], "accounting");
            } else {
                assert_eq!(report["first"]["projection_applied"], true);
                assert!(
                    report["first"]["request_tokens"].as_u64().unwrap()
                        < report["first"]["input_tokens"].as_u64().unwrap()
                );
                assert_eq!(report["first"]["tokenizer"]["first_call_in"], "prepare");
            }
        }
    }
}
