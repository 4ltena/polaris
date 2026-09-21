//! Bounded M12 fixtures, value-free response instructions and deterministic scoring.

use polaris_core::{
    budget::count_tokens, conversation_state::content_hash, input_preparation::InputPreparationMode,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const CASE_IDS: [&str; 4] = ["I01", "I02", "I03", "I04"];
const MAX_CASE_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_FEE_CAP_MICROUSD: u64 = 5_000_000;
const MAX_EVIDENCE_CHARS: usize = 160;

#[derive(clap::Parser, Debug)]
#[command(about = "M12 input comparison; defaults to a no-send, no-write plan")]
pub(super) struct Args {
    #[arg(long)]
    pub work_dir: PathBuf,
    #[arg(long)]
    pub cases: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long = "case", value_parser = CASE_IDS)]
    pub case_id: String,
    #[arg(long)]
    pub live: bool,
    #[arg(long)]
    pub auth_file: Option<PathBuf>,
    /// Explicit observed fee stop; no live allowance is supplied by default.
    #[arg(long)]
    pub fee_cap_microusd: Option<u64>,
}

impl Args {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_inputs()?;
        match fs::symlink_metadata(&self.output) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err("output must be a new path".into()),
        }
    }

    // Source rechecks run after live() has exclusively claimed the output.
    // The entry point still calls validate() to reject existing campaigns.
    fn validate_inputs(&self) -> Result<(), String> {
        canonical_directory(&self.work_dir)?;
        canonical_file(&self.cases)?;
        if !self.cases.starts_with(&self.work_dir)
            || !self.output.is_absolute()
            || self.output.parent() != Some(self.work_dir.as_path())
            || self.output.file_name().is_none()
        {
            return Err("cases and a new output must be inside the explicit work directory".into());
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .ok_or("repository identity unavailable")?
            .canonicalize()
            .map_err(|_| "repository identity unavailable")?;
        if self.work_dir.starts_with(&repo) || repo.starts_with(&self.work_dir) {
            return Err("benchmark work must be separate from the repository".into());
        }
        if !CASE_IDS.contains(&self.case_id.as_str()) {
            return Err("case must be I01, I02, I03 or I04".into());
        }
        if self
            .fee_cap_microusd
            .is_some_and(|cap| !(1..=MAX_FEE_CAP_MICROUSD).contains(&cap))
        {
            return Err(
                "fee stop must be in 1..=5000000 micro-USD; it is not authorization".into(),
            );
        }
        if self
            .auth_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err("auth-file must be an explicit absolute path".into());
        }
        if self.live && (self.auth_file.is_none() || self.fee_cap_microusd.is_none()) {
            return Err("live mode requires explicit auth-file and fee-cap-microusd".into());
        }
        // Do not inspect authentication here: even a supplied path is unread in plan mode.
        Ok(())
    }
}

fn canonical_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.canonicalize().ok().as_deref() != Some(path)
        || !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
    {
        return Err("work-dir must be an absolute canonical directory".into());
    }
    Ok(())
}

fn canonical_file(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.canonicalize().ok().as_deref() != Some(path)
        || !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
    {
        return Err("cases must be an absolute canonical regular file".into());
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct Case {
    pub id: String,
    pub input: String,
    pub input_hash: String,
    pub case_file_hash: String,
    pub expected_projection: String,
    expected: Value,
    expected_evidence: Value,
}

#[derive(Clone, Copy)]
enum FieldType {
    String,
    Boolean,
    Number,
    NullableString,
}

impl FieldType {
    fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::NullableString => "string|null",
        }
    }

    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Boolean => value.is_boolean(),
            Self::Number => value.is_number(),
            Self::NullableString => value.is_string() || value.is_null(),
        }
    }
}

fn fields(id: &str) -> &'static [(&'static str, FieldType)] {
    use FieldType::*;
    match id {
        "I01" => &[
            ("project", String),
            ("rename_variable", Boolean),
            ("timeout", Number),
            ("unit", String),
            ("production_write", Boolean),
            ("local_condition", String),
            ("database_password", NullableString),
        ],
        "I02" => &[
            ("occurrences", Number),
            ("name", String),
            ("identifier", String),
        ],
        "I03" => &[("final_limit", Number)],
        "I04" => &[("last_entry_number", Number)],
        _ => &[],
    }
}

impl Case {
    pub fn load(args: &Args) -> Result<Self, String> {
        args.validate_inputs()?;
        let file = fs::File::open(&args.cases).map_err(|_| "case file unavailable")?;
        let metadata = file
            .metadata()
            .map_err(|_| "case file metadata unavailable")?;
        if !metadata.is_file() || metadata.len() > MAX_CASE_FILE_BYTES {
            return Err("case file must be regular and at most 2 MiB".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_CASE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "case file unreadable")?;
        Self::from_bytes(&bytes, &args.case_id)
    }

    fn from_bytes(bytes: &[u8], selected: &str) -> Result<Self, String> {
        if bytes.len() as u64 > MAX_CASE_FILE_BYTES {
            return Err("case file exceeds 2 MiB".into());
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| "invalid case JSON")?;
        if value["schema_version"] != 1 || value["model_requests"] != 0 {
            return Err("invalid case schema or offline provenance".into());
        }
        let rows = value["cases"].as_array().ok_or("missing cases")?;
        if rows.len() != CASE_IDS.len() {
            return Err("exactly four fixed cases are required".into());
        }
        let case_file_hash = content_hash(bytes);
        let mut seen = std::collections::BTreeSet::new();
        let mut chosen = None;
        for row in rows {
            let text = |name| {
                row.get(name)
                    .and_then(Value::as_str)
                    .ok_or("invalid case string field")
            };
            let id = text("id")?;
            let input = text("input")?;
            let input_hash = text("sha256")?;
            let expected_projection = text("expected_projection")?;
            let expected_reason = match id {
                "I01" | "I02" => "applied",
                "I03" => "protected_syntax",
                "I04" => "insufficient_savings",
                _ => return Err("unexpected case ID".into()),
            };
            if !seen.insert(id)
                || input.is_empty()
                || input.len() > MAX_INPUT_BYTES
                || row["input_bytes"].as_u64() != Some(input.len() as u64)
                || content_hash(input.as_bytes()) != input_hash
                || expected_projection != expected_reason
            {
                return Err(
                    "case identity, input bytes, hash or projection expectation mismatch".into(),
                );
            }
            if !(4096..=32_000).contains(&count_tokens(input)) {
                return Err("case input must contain 4096..=32000 o200k reference tokens".into());
            }
            let expected = row["expected"]
                .as_object()
                .ok_or("expected fields must be an object")?;
            let schema = fields(id);
            if expected.len() != schema.len()
                || schema.iter().any(|(name, kind)| {
                    expected.get(*name).is_none_or(|value| !kind.accepts(value))
                })
            {
                return Err("expected fields or JSON types do not match the fixed case".into());
            }
            validate_evidence(&row["expected_evidence"], input, schema)?;
            if id == selected {
                chosen = Some(Self {
                    id: id.into(),
                    input: input.into(),
                    input_hash: input_hash.into(),
                    case_file_hash: case_file_hash.clone(),
                    expected_projection: expected_projection.into(),
                    expected: row["expected"].clone(),
                    expected_evidence: row["expected_evidence"].clone(),
                });
            }
        }
        chosen.ok_or_else(|| "selected case is unavailable".into())
    }

    pub fn modes(&self) -> [InputPreparationMode; 2] {
        use InputPreparationMode::{Off, RepeatRefs};
        match self.id.as_str() {
            "I01" | "I03" => [Off, RepeatRefs],
            _ => [RepeatRefs, Off],
        }
    }

    pub fn instructions(&self) -> String {
        let answer: serde_json::Map<_, _> = fields(&self.id)
            .iter()
            .map(|(name, kind)| ((*name).to_owned(), Value::String(kind.name().into())))
            .collect();
        let evidence: serde_json::Map<_, _> = fields(&self.id)
            .iter()
            .map(|(name, _)| ((*name).to_owned(), Value::String("string".into())))
            .collect();
        format!(
            "Return only one JSON object with exactly these field names and JSON types, with no additional keys or prose: {}. Each evidence string must be a verbatim, contiguous quotation of at most {MAX_EVIDENCE_CHARS} Unicode characters from the original input. Quote the complete supporting sentence, record, or code statement, including its label and surrounding values, not just the answer value. A long source line may contain several sentences; quote only the complete sentence needed for the field. Do not quote reference markers or paraphrase. For repeated records, quote one complete original record and count its occurrences separately.",
            json!({"answer":answer,"evidence":evidence}),
        )
    }

    pub fn score(&self, text: &str) -> Value {
        let parsed = serde_json::from_str::<Value>(text).ok();
        let object = parsed.as_ref().and_then(Value::as_object);
        let schema = fields(&self.id);
        let answer = object.and_then(|object| object.get("answer"));
        let evidence = object.and_then(|object| object.get("evidence"));
        let answer_keys = answer.is_some_and(|value| has_field_keys(value, schema));
        let evidence_keys = evidence.is_some_and(|value| has_field_keys(value, schema));
        let exact_keys = answer_keys
            && evidence_keys
            && object.is_some_and(|object| {
                object.len() == 2
                    && object.contains_key("answer")
                    && object.contains_key("evidence")
            });
        let field_answer_correct: serde_json::Map<_, _> = schema
            .iter()
            .map(|(name, kind)| {
                let correct = answer
                    .and_then(|value| value.get(*name))
                    .is_some_and(|actual| {
                        kind.accepts(actual) && same_value(actual, &self.expected[*name])
                    });
                ((*name).to_owned(), Value::Bool(correct))
            })
            .collect();
        let field_evidence_correct: serde_json::Map<_, _> = schema
            .iter()
            .map(|(name, _)| {
                let correct = evidence
                    .and_then(|value| value.get(*name))
                    .and_then(Value::as_str)
                    .is_some_and(|quote| {
                        quote.chars().count() <= MAX_EVIDENCE_CHARS
                            && self.input.contains(quote)
                            && self.expected_evidence[*name]["anchor"]
                                .as_str()
                                .is_some_and(|anchor| quote.contains(anchor))
                    });
                ((*name).to_owned(), Value::Bool(correct))
            })
            .collect();
        let answer_correct = answer_keys
            && field_answer_correct
                .values()
                .all(|value| value == &Value::Bool(true));
        let evidence_correct = evidence_keys
            && field_evidence_correct
                .values()
                .all(|value| value == &Value::Bool(true));
        let quality_complete = exact_keys && answer_correct && evidence_correct;
        json!({
            "json_valid":parsed.is_some(), "exact_keys":exact_keys,
            "field_answer_correct":field_answer_correct,
            "field_evidence_correct":field_evidence_correct,
            "answer_correct":answer_correct, "evidence_correct":evidence_correct,
            "quality_complete":quality_complete, "all_correct":quality_complete,
        })
    }

    pub fn plan(&self, args: &Args) -> Value {
        json!({
            "schema_version":1, "kind":"m12_model_comparison_plan",
            "live_requested":args.live, "execution_authorized":false,
            "default_mode":"plan_only", "model_requests":0, "auth_reads":0, "result_writes":0,
            "case_id":self.id, "input_sha256":self.input_hash, "case_file_sha256":self.case_file_hash,
            "input_bytes":self.input.len(), "input_reference_tokens":count_tokens(&self.input),
            "expected_projection":self.expected_projection,
            "cases":args.cases, "work_dir":args.work_dir, "output":args.output,
            "modes":self.modes(), "model":"gpt-6-astra", "effort":"xhigh",
            "caps":{
                "physical_sends_per_condition":1, "physical_sends_per_pair":2,
                "condition_seconds":120, "pair_seconds":240,
                "request_bytes":131072, "observed_output_tokens":4096,
                "observed_fee_stop_microusd":args.fee_cap_microusd,
                "maximum_fee_stop_microusd":MAX_FEE_CAP_MICROUSD,
                "fee_stop_is_post_response":true, "endpoint_output_cap_verified":false,
                "fee_ceiling_is_strict_billing_limit":false,
            },
            "live_requirements":{
                "explicit_auth_file":true, "explicit_fee_cap":true,
                "readonly_auth_grant":true, "POLARIS_AUTH_READ_ONLY":"1",
                "websocket":false, "auth_refresh":false, "automatic_retry":false,
            },
        })
    }
}

fn has_field_keys(value: &Value, schema: &[(&str, FieldType)]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == schema.len() && schema.iter().all(|(name, _)| object.contains_key(*name))
    })
}

fn validate_evidence(
    evidence: &Value,
    input: &str,
    schema: &[(&str, FieldType)],
) -> Result<(), String> {
    if !has_field_keys(evidence, schema) {
        return Err("evidence fields do not match the fixed case".into());
    }
    for (name, _) in schema {
        let item = &evidence[*name];
        let object = item.as_object().ok_or("evidence must be an object")?;
        let required = [
            "anchor",
            "source_byte_start",
            "source_byte_end",
            "sha256",
            "occurrences",
        ];
        if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
            return Err("evidence descriptor fields do not match the fixed schema".into());
        }
        let anchor = item["anchor"].as_str().ok_or("invalid evidence anchor")?;
        let number = |key: &str| {
            item[key]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or("invalid evidence integer")
        };
        let start = number("source_byte_start")?;
        let end = number("source_byte_end")?;
        let occurrences = number("occurrences")?;
        if anchor.is_empty()
            || anchor.chars().count() > MAX_EVIDENCE_CHARS
            || start >= end
            || input.get(start..end) != Some(anchor)
            || input.find(anchor) != Some(start)
            || item["sha256"].as_str() != Some(content_hash(anchor.as_bytes()).as_str())
            || input.matches(anchor).count() != occurrences
        {
            return Err("evidence anchor, UTF-8 span, hash or occurrence count mismatch".into());
        }
    }
    Ok(())
}

fn same_value(actual: &Value, expected: &Value) -> bool {
    match (actual.as_number(), expected.as_number()) {
        (Some(actual), Some(expected)) => {
            if let (Some(actual), Some(expected)) = (actual.as_i64(), expected.as_i64()) {
                actual == expected
            } else if let (Some(actual), Some(expected)) = (actual.as_u64(), expected.as_u64()) {
                actual == expected
            } else {
                actual.as_f64() == expected.as_f64()
            }
        }
        _ => actual == expected,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use clap::Parser;

    fn fixtures() -> Value {
        let paragraph =
            "This background is repeated without replacing later instructions. ".repeat(64);
        let expected = [
            json!({"project":"GOLDEN_PROJECT_SENTINEL","rename_variable":false,"timeout":41,"unit":"GOLDEN_UNIT_SENTINEL","production_write":false,"local_condition":"GOLDEN_CONDITION_SENTINEL","database_password":null}),
            json!({"occurrences":8,"name":"GOLDEN_NAME_SENTINEL","identifier":"7319"}),
            json!({"final_limit":41}),
            json!({"last_entry_number":7}),
        ];
        let anchors: [&[&str]; 4] = [
            &[
                "案件は GOLDEN_PROJECT_SENTINEL です。",
                "Do not rename any variable.",
                "Timeout is 41 GOLDEN_UNIT_SENTINEL.",
                "Timeout is 41 GOLDEN_UNIT_SENTINEL.",
                "Production writes are forbidden.",
                "Use GOLDEN_CONDITION_SENTINEL locally.",
                "No database password is provided.",
            ],
            &[
                "Exactly 8 occurrences are requested.",
                "The name is GOLDEN_NAME_SENTINEL.",
                "The identifier is 7319.",
            ],
            &["const FINAL_LIMIT: u64 = 41;"],
            &["The last entry number is 7."],
        ];
        let cases: Vec<_> = CASE_IDS.iter().enumerate().map(|(index, id)| {
            let mut input = if index == 3 {
                (0..8).map(|number| format!("{paragraph}Distinct {number}.\n\n")).collect::<String>()
            } else {
                format!("{paragraph}\n\n").repeat(8)
            };
            if index == 2 { input.push_str("```rust\nconst FINAL_LIMIT: u64 = 41;\n```\n"); }
            for &anchor in anchors[index] {
                input.push_str(anchor);
                input.push('\n');
            }
            let evidence: serde_json::Map<_, _> = fields(id).iter().zip(anchors[index].iter().copied()).map(|((field, _), anchor)| {
                let start = input.find(anchor).unwrap();
                ((*field).to_owned(), json!({
                    "anchor":anchor,"source_byte_start":start,"source_byte_end":start+anchor.len(),
                    "sha256":content_hash(anchor.as_bytes()),"occurrences":input.matches(anchor).count(),
                }))
            }).collect();
            let reason = match index { 0 | 1 => "applied", 2 => "protected_syntax", _ => "insufficient_savings" };
            json!({"id":id,"input_bytes":input.len(),"sha256":content_hash(input.as_bytes()),"input":input,"expected":expected[index],"expected_evidence":evidence,"expected_projection":reason,"kind":"UNTRUSTED_METADATA_SENTINEL"})
        }).collect();
        json!({"schema_version":1,"model_requests":0,"cases":cases,"metadata":"UNTRUSTED_METADATA_SENTINEL"})
    }

    pub(crate) fn correct_reply(case: &Case) -> Value {
        let evidence: serde_json::Map<_, _> = fields(&case.id)
            .iter()
            .map(|(name, _)| {
                (
                    (*name).to_owned(),
                    case.expected_evidence[*name]["anchor"].clone(),
                )
            })
            .collect();
        json!({"answer":case.expected,"evidence":evidence})
    }

    pub(crate) fn args(directory: &Path) -> Args {
        let work = directory.canonicalize().unwrap();
        let cases = work.join("cases.json");
        fs::write(&cases, serde_json::to_vec(&fixtures()).unwrap()).unwrap();
        Args {
            output: work.join("result.json"),
            work_dir: work,
            cases,
            case_id: "I01".into(),
            live: false,
            auth_file: None,
            fee_cap_microusd: None,
        }
    }

    #[test]
    fn instructions_plan_and_scores_never_reveal_golden_or_free_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let args = args(directory.path());
        let case = Case::load(&args).unwrap();
        let instructions = case.instructions();
        assert!(instructions.contains("database_password"));
        assert!(instructions.contains("string|null"));
        let plan = case.plan(&args);
        assert_eq!(plan["model_requests"], 0);
        assert_eq!(plan["auth_reads"], 0);
        assert_eq!(plan["result_writes"], 0);
        assert!(plan["caps"]["observed_fee_stop_microusd"].is_null());
        let score = case.score(&correct_reply(&case).to_string());
        assert_eq!(score["all_correct"], true);
        for report in [instructions, plan.to_string(), score.to_string()] {
            assert!(!report.contains("GOLDEN_"));
            assert!(!report.contains("UNTRUSTED_METADATA_SENTINEL"));
            assert!(!report.contains(&case.input));
            for (name, _) in fields(&case.id) {
                assert!(
                    !report.contains(case.expected_evidence[*name]["anchor"].as_str().unwrap())
                );
            }
        }
        assert!(!args.output.exists());
    }

    #[test]
    fn scoring_preserves_numeric_types_negation_unknown_and_exact_keys() {
        let case = Case::from_bytes(&serde_json::to_vec(&fixtures()).unwrap(), "I01").unwrap();
        let mut reply = correct_reply(&case);
        reply["answer"]["timeout"] = json!(41.0);
        assert_eq!(case.score(&reply.to_string())["all_correct"], true);
        reply["answer"]["timeout"] = json!("41");
        assert_eq!(
            case.score(&reply.to_string())["field_answer_correct"]["timeout"],
            false
        );
        reply = correct_reply(&case);
        reply["answer"]["rename_variable"] = true.into();
        reply["answer"]["production_write"] = true.into();
        reply["answer"]["database_password"] = "unknown".into();
        let score = case.score(&reply.to_string());
        for field in ["rename_variable", "production_write", "database_password"] {
            assert_eq!(score["field_answer_correct"][field], false);
        }
        assert_eq!(score["answer_correct"], false);
        assert_eq!(score["evidence_correct"], true);
        assert_eq!(score["quality_complete"], false);
        reply = correct_reply(&case);
        reply["extra"] = "ACTUAL_SENTINEL".into();
        let score = case.score(&reply.to_string());
        assert_eq!(score["exact_keys"], false);
        assert_eq!(score["all_correct"], false);
        assert!(!score.to_string().contains("ACTUAL_SENTINEL"));
        reply.as_object_mut().unwrap().remove("extra");
        reply["answer"]
            .as_object_mut()
            .unwrap()
            .remove("database_password");
        assert_eq!(case.score(&reply.to_string())["all_correct"], false);
        assert_eq!(case.score("{invalid")["json_valid"], false);
        assert_eq!(case.score("null")["json_valid"], true);
        assert_eq!(case.score("null")["exact_keys"], false);
    }

    #[test]
    fn correct_answers_need_field_specific_bounded_verbatim_evidence() {
        let mut case = Case::from_bytes(&serde_json::to_vec(&fixtures()).unwrap(), "I01").unwrap();
        let mut reply = correct_reply(&case);
        reply.as_object_mut().unwrap().remove("evidence");
        let score = case.score(&reply.to_string());
        assert_eq!(score["answer_correct"], true);
        assert_eq!(score["evidence_correct"], false);
        assert_eq!(score["all_correct"], false);

        for quote in [
            "",
            "The correct project was named earlier.",
            "Production writes are forbidden.",
        ] {
            reply = correct_reply(&case);
            reply["evidence"]["project"] = json!(quote);
            let score = case.score(&reply.to_string());
            assert_eq!(score["answer_correct"], true);
            assert_eq!(score["field_evidence_correct"]["project"], false);
            assert_eq!(score["quality_complete"], false);
        }
        let anchor = case.expected_evidence["project"]["anchor"]
            .as_str()
            .unwrap()
            .to_owned();
        let bounded_quote = format!(
            "{}{anchor}",
            "界".repeat(MAX_EVIDENCE_CHARS - anchor.chars().count())
        );
        let overlong_quote = format!("界{bounded_quote}");
        case.input.push_str(&overlong_quote);
        reply = correct_reply(&case);
        reply["evidence"]["project"] = json!(bounded_quote);
        assert_eq!(case.score(&reply.to_string())["all_correct"], true);
        reply["evidence"]["project"] = json!(overlong_quote);
        assert_eq!(case.score(&reply.to_string())["all_correct"], false);
        reply["evidence"]["project"] = json!(format!("invented prefix {anchor}"));
        assert_eq!(case.score(&reply.to_string())["all_correct"], false);
        reply = correct_reply(&case);
        reply["evidence"]["extra"] = json!("ACTUAL_SENTINEL");
        let score = case.score(&reply.to_string());
        assert_eq!(score["exact_keys"], false);
        assert_eq!(score["all_correct"], false);
        assert!(!score.to_string().contains("ACTUAL_SENTINEL"));
    }

    #[test]
    fn all_four_fixed_schemas_and_alternating_orders_are_used() {
        let bytes = serde_json::to_vec(&fixtures()).unwrap();
        for (index, id) in CASE_IDS.iter().enumerate() {
            let case = Case::from_bytes(&bytes, id).unwrap();
            assert_eq!(
                case.score(&correct_reply(&case).to_string())["all_correct"],
                true
            );
            let first = if index % 2 == 0 {
                InputPreparationMode::Off
            } else {
                InputPreparationMode::RepeatRefs
            };
            assert_eq!(case.modes()[0], first);
            assert_ne!(case.modes()[0], case.modes()[1]);
            let mut missing = correct_reply(&case);
            missing["answer"]
                .as_object_mut()
                .unwrap()
                .remove(fields(id)[0].0);
            assert_eq!(case.score(&missing.to_string())["all_correct"], false);
        }
    }

    #[test]
    fn whole_file_hash_tracks_bytes_and_input_tampering_is_rejected() {
        let fixture = fixtures();
        let bytes = serde_json::to_vec(&fixture).unwrap();
        let original = Case::from_bytes(&bytes, "I01").unwrap();
        let mut spaced = bytes.clone();
        spaced.push(b'\n');
        let formatted = Case::from_bytes(&spaced, "I01").unwrap();
        assert_ne!(original.case_file_hash, formatted.case_file_hash);
        assert_eq!(original.input_hash, formatted.input_hash);
        assert_eq!(original.case_file_hash, content_hash(&bytes));
        for field in ["input", "sha256", "input_bytes"] {
            let mut broken = fixture.clone();
            broken["cases"][0][field] = json!("tampered");
            assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        }
        let mut broken = fixture.clone();
        broken["cases"][3]["id"] = json!("I01");
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        broken = fixture.clone();
        broken["cases"][1]["expected"]["identifier"] = json!(7319);
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        broken = fixture;
        broken["cases"][0]["expected"]["extra"] = Value::Null;
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        assert!(Case::from_bytes(&vec![b' '; MAX_CASE_FILE_BYTES as usize + 1], "I01").is_err());
    }

    #[test]
    fn evidence_manifest_is_bound_to_first_utf8_span_hash_and_occurrences() {
        let fixture = fixtures();
        for field in [
            "anchor",
            "source_byte_start",
            "source_byte_end",
            "sha256",
            "occurrences",
        ] {
            let mut broken = fixture.clone();
            broken["cases"][0]["expected_evidence"]["project"][field] = json!("tampered");
            assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I02").is_err());
        }
        let mut broken = fixture.clone();
        let start = broken["cases"][0]["expected_evidence"]["project"]["source_byte_start"]
            .as_u64()
            .unwrap();
        broken["cases"][0]["expected_evidence"]["project"]["source_byte_start"] = json!(start + 1);
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        broken = fixture.clone();
        let anchor = broken["cases"][0]["expected_evidence"]["timeout"]["anchor"]
            .as_str()
            .unwrap()
            .to_owned();
        let input = broken["cases"][0]["input"].as_str().unwrap();
        let second = input.match_indices(anchor.as_str()).nth(1).unwrap().0;
        broken["cases"][0]["expected_evidence"]["timeout"]["source_byte_start"] = json!(second);
        broken["cases"][0]["expected_evidence"]["timeout"]["source_byte_end"] =
            json!(second + anchor.len());
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        broken = fixture.clone();
        broken["cases"][0]["expected_evidence"]["timeout"]["occurrences"] = json!(1);
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        broken = fixture.clone();
        broken["cases"][0]["expected_evidence"]
            .as_object_mut()
            .unwrap()
            .remove("project");
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
        broken = fixture;
        broken["cases"][0]["expected_evidence"]["project"]["extra"] = json!(true);
        assert!(Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01").is_err());
    }

    #[test]
    fn case_inputs_cannot_exceed_token_or_byte_bounds_even_with_updated_hashes() {
        let fixture = fixtures();
        for (input, expected_error) in [
            (
                "Too short.".to_owned(),
                "case input must contain 4096..=32000 o200k reference tokens",
            ),
            (
                "x ".repeat(32_001),
                "case input must contain 4096..=32000 o200k reference tokens",
            ),
            (
                "x".repeat(MAX_INPUT_BYTES + 1),
                "case identity, input bytes, hash or projection expectation mismatch",
            ),
        ] {
            let mut broken = fixture.clone();
            broken["cases"][0]["input_bytes"] = json!(input.len());
            broken["cases"][0]["sha256"] = json!(content_hash(input.as_bytes()));
            broken["cases"][0]["input"] = json!(input);
            assert_eq!(
                Case::from_bytes(&serde_json::to_vec(&broken).unwrap(), "I01")
                    .err()
                    .as_deref(),
                Some(expected_error)
            );
        }
    }

    #[test]
    fn claimed_output_does_not_prevent_case_recheck_but_still_blocks_restart() {
        let directory = tempfile::tempdir().unwrap();
        let args = args(directory.path());
        args.validate().unwrap();
        let original = Case::load(&args).unwrap();
        fs::create_dir(&args.output).unwrap();
        for _mode in original.modes() {
            let reloaded = Case::load(&args).expect("recheck source after output claim");
            assert_eq!(reloaded.case_file_hash, original.case_file_hash);
        }
        assert_eq!(args.validate().unwrap_err(), "output must be a new path");
        let mut changed = fixtures();
        changed["metadata"] = json!("changed after claim");
        fs::write(&args.cases, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_ne!(
            Case::load(&args).unwrap().case_file_hash,
            original.case_file_hash
        );
    }

    #[test]
    fn arguments_have_no_default_live_allowance_and_do_not_read_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let mut args = args(directory.path());
        let parsed = Args::try_parse_from([
            "m12-model-eval",
            "--work-dir",
            args.work_dir.to_str().unwrap(),
            "--cases",
            args.cases.to_str().unwrap(),
            "--output",
            args.output.to_str().unwrap(),
            "--case",
            "I01",
        ])
        .unwrap();
        assert!(!parsed.live);
        assert_eq!(parsed.fee_cap_microusd, None);
        parsed.validate().unwrap();
        args.auth_file = Some(args.work_dir.join("absent-auth.json"));
        args.validate().unwrap();
        args.live = true;
        assert!(args.validate().is_err());
        for fee in [1, 5_000_000] {
            args.fee_cap_microusd = Some(fee);
            args.validate().unwrap();
        }
        for fee in [0, 5_000_001] {
            args.fee_cap_microusd = Some(fee);
            assert!(args.validate().is_err());
        }
        args.fee_cap_microusd = Some(1);
        args.auth_file = Some(PathBuf::from("relative-auth"));
        assert!(args.validate().is_err());
        args.auth_file = None;
        assert!(args.validate().is_err());
        assert!(!args.output.exists());
    }

    #[test]
    fn argument_paths_and_case_ids_cannot_escape_the_fixed_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut args = args(directory.path());
        args.case_id = "I05".into();
        assert!(args.validate().is_err());
        args.case_id = "I01".into();
        args.output = args.work_dir.join("nested/result.json");
        assert!(args.validate().is_err());
        args.output = args.work_dir.join("existing.json");
        fs::write(&args.output, b"existing").unwrap();
        assert!(args.validate().is_err());
        args.output = args.work_dir.join("new.json");
        args.cases = PathBuf::from("relative.json");
        assert!(args.validate().is_err());
    }
}
