//! Strict, bounded native worker protocol. No upstream diagnostics escape it.
use super::*;
use polaris_provider::decision::{Answer, checked};
use serde::{
    Deserialize, Deserializer,
    de::{Error, MapAccess, Visitor},
};
use std::{fmt, io::Write};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(super) const MAX_FRAME: usize = 1024 * 1024;
const RUNTIME: &str = "620d2225cc436ab0b001e3200e590e3b286ef9ab+polaris-fileptr-v1";

pub(super) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
pub(super) fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

struct Budget(usize);
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("frame limit"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn validate_request(
    state: &serde_json::Value,
    questions: &BTreeMap<String, Question>,
) -> Result<(), DecisionError> {
    let label = |s: &str| !s.is_empty() && s.len() <= 512;
    let instruction = |s: &str| !s.is_empty() && s.len() <= 16384;
    if questions.is_empty() || questions.len() > 64 {
        return Err(DecisionError::TooLarge);
    }
    for (id, question) in questions {
        if !label(id) {
            return Err(DecisionError::Malformed);
        }
        let valid = match question {
            Question::Noul { instructions } => instruction(instructions),
            Question::Choice {
                instructions,
                criteria,
            } => {
                instruction(instructions)
                    && !criteria.is_empty()
                    && criteria.len() <= 255
                    && criteria.iter().all(|(k, v)| label(k) && v.len() <= 16384)
            }
            Question::Score {
                instructions,
                criteria,
            } => {
                instruction(instructions)
                    && !criteria.is_empty()
                    && criteria.len() <= 255
                    && criteria.iter().all(|v| v.len() <= 16384)
            }
        };
        if !valid {
            return Err(DecisionError::Malformed);
        }
    }
    let messages = state.get("messages").unwrap_or(state);
    if let Some(messages) = messages.as_array() {
        for message in messages {
            if let Some(parts) = message.get("content").and_then(serde_json::Value::as_array)
                && parts.iter().any(|part| {
                    matches!(
                        part.get("type").and_then(serde_json::Value::as_str),
                        Some("image_url" | "input_audio")
                    )
                })
            {
                return Err(DecisionError::Malformed);
            }
        }
    }
    // Count serialization before cloning any state into the bounded actor.
    #[derive(serde::Serialize)]
    struct Payload<'a> {
        state: &'a serde_json::Value,
        questions: &'a BTreeMap<String, Question>,
    }
    serde_json::to_writer(Budget(MAX_FRAME - 1024), &Payload { state, questions })
        .map_err(|_| DecisionError::TooLarge)
}

pub(super) async fn frame(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Vec<u8>, DecisionError> {
    let mut line = Vec::new();
    loop {
        let bytes = reader
            .fill_buf()
            .await
            .map_err(|_| DecisionError::Unavailable)?;
        if bytes.is_empty() {
            return Err(if line.is_empty() {
                DecisionError::Unavailable
            } else {
                DecisionError::InvalidAnswer
            });
        }
        let end = bytes.iter().position(|&c| c == b'\n');
        let n = end.unwrap_or(bytes.len());
        if n > MAX_FRAME - line.len() {
            return Err(DecisionError::InvalidAnswer);
        }
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(line);
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    protocol: u32,
    owner: String,
    generation: String,
    status: String,
    runtime_revision: String,
    model_sha256: String,
    device: String,
    context_tokens: u32,
}

pub(super) fn ready(line: &[u8], owner: &str, config: &ClefConfig) -> Result<(), DecisionError> {
    let r: Ready = serde_json::from_slice(line).map_err(|_| DecisionError::InvalidAnswer)?;
    if r.protocol != 1
        || r.owner != owner
        || r.generation != config.generation
        || r.status != "ready"
        || r.runtime_revision != RUNTIME
        || r.model_sha256 != config.model_sha256
        || r.device != config.device.as_str()
        || r.context_tokens != config.context_tokens
    {
        return Err(DecisionError::InvalidAnswer);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    protocol: u32,
    owner: String,
    generation: String,
    request_id: u64,
    status: String,
    decision: Option<NativeDecision>,
    code: Option<u16>,
}

impl Reply {
    fn parse(
        line: &[u8],
        owner: &str,
        generation: &str,
        request_id: u64,
    ) -> Result<Self, DecisionError> {
        let r: Self = serde_json::from_slice(line).map_err(|_| DecisionError::InvalidAnswer)?;
        if r.protocol != 1
            || r.owner != owner
            || r.generation != generation
            || r.request_id != request_id
        {
            return Err(DecisionError::InvalidAnswer);
        }
        Ok(r)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeDecision {
    model: String,
    #[serde(deserialize_with = "unique_map")]
    answers: BTreeMap<String, NativeAnswer>,
    usage: Usage,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum NativeAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(deserialize_with = "unique_map")]
        probabilities: BTreeMap<String, f64>,
        #[serde(deserialize_with = "unique_map")]
        legend: BTreeMap<String, String>,
        confidence: Option<f64>,
    },
}

fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("unique named values")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((k, v)) = map.next_entry::<String, T>()? {
                if result.insert(k, v).is_some() {
                    return Err(A::Error::custom("duplicate key"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

pub(super) fn decision(
    line: &[u8],
    owner: &str,
    generation: &str,
    id: u64,
    questions: &BTreeMap<String, Question>,
) -> Result<Decision, DecisionError> {
    let reply = Reply::parse(line, owner, generation, id)?;
    if reply.status == "refused" && reply.decision.is_none() {
        return Err(match reply.code {
            Some(413) => DecisionError::TooLarge,
            Some(400 | 422) => DecisionError::Malformed,
            Some(_) => DecisionError::Unavailable,
            None => DecisionError::InvalidAnswer,
        });
    }
    if reply.status == "outcome_unknown" && reply.decision.is_none() && reply.code.is_none() {
        return Err(DecisionError::Unavailable);
    }
    if reply.status != "ok" || reply.code.is_some() {
        return Err(DecisionError::InvalidAnswer);
    }
    let native = reply.decision.ok_or(DecisionError::InvalidAnswer)?;
    if !native
        .model
        .strip_prefix("/dev/fd/")
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        || native.usage.input_tokens == 0
        || native.usage.output_tokens != 0
    {
        return Err(DecisionError::InvalidAnswer);
    }
    let mut answers = BTreeMap::new();
    for (key, value) in native.answers {
        let question = questions.get(&key).ok_or(DecisionError::InvalidAnswer)?;
        let answer = match (value, question) {
            (NativeAnswer::Noul { noul }, Question::Noul { .. }) => Answer {
                noul: Some(noul),
                ..Default::default()
            },
            (
                NativeAnswer::Choice {
                    choice,
                    probabilities,
                    confidence,
                },
                Question::Choice { .. },
            ) => Answer {
                choice: Some(choice),
                probabilities,
                confidence,
                ..Default::default()
            },
            (
                NativeAnswer::Score {
                    score,
                    probabilities,
                    legend,
                    confidence,
                },
                Question::Score { criteria, .. },
            ) => {
                if legend
                    != criteria
                        .iter()
                        .enumerate()
                        .map(|(i, s)| (i.to_string(), s.clone()))
                        .collect()
                    || (score
                        - probabilities
                            .iter()
                            .map(|(k, p)| k.parse::<usize>().map_or(f64::NAN, |i| i as f64 * p))
                            .sum::<f64>())
                    .abs()
                        > 0.01
                {
                    return Err(DecisionError::InvalidAnswer);
                }
                Answer {
                    score: Some(score),
                    probabilities,
                    legend,
                    confidence,
                    ..Default::default()
                }
            }
            _ => return Err(DecisionError::InvalidAnswer),
        };
        answers.insert(key, answer);
    }
    let result = Decision {
        answers,
        input_tokens: Some(native.usage.input_tokens),
        output_tokens: Some(native.usage.output_tokens),
    };
    checked(&result, questions)?;
    Ok(result)
}

pub(super) fn closed(
    line: &[u8],
    owner: &str,
    generation: &str,
    id: u64,
) -> Result<(), DecisionError> {
    let reply = Reply::parse(line, owner, generation, id)?;
    if reply.status != "closed" || reply.decision.is_some() || reply.code.is_some() {
        return Err(DecisionError::InvalidAnswer);
    }
    Ok(())
}
