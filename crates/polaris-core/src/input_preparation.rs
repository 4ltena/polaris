//! Reversible, host-only projection of repeated paragraphs in a new root input.
//!
//! The caller owns persistence, source identity, slot matching and root eligibility.
//! This module neither changes history nor calls a provider.

use crate::{budget::count_tokens, conversation_state::content_hash};
use serde::{Deserialize, Serialize};
use std::time::Instant;

const MIN_INPUT_TOKENS: usize = 4096;
const MAX_INPUT_TOKENS: usize = 32_000;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const MAX_PARAGRAPHS: usize = 128;
// JSON can escape one source byte as six bytes. Leave bounded room for framing.
const MAX_ENCODED_BYTES: usize = MAX_INPUT_BYTES * 6 + 4096;
const PREAMBLE: &str = "[Polaris input encoding v1]\nThe original input is the concatenation of chunks[order[i]] in order; repeated indices repeat those exact bytes.\n";

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputPreparationMode {
    #[default]
    Off,
    RepeatRefs,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreparationReason {
    Off,
    ByteLimit,
    TooShort,
    TokenLimit,
    ProtectedSyntax,
    ParagraphLimit,
    InsufficientSavings,
    InvalidProjection,
    Applied,
}

impl PreparationReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ByteLimit => "byte_limit",
            Self::TooShort => "too_short",
            Self::TokenLimit => "token_limit",
            Self::ProtectedSyntax => "protected_syntax",
            Self::ParagraphLimit => "paragraph_limit",
            Self::InsufficientSavings => "insufficient_savings",
            Self::InvalidProjection => "invalid_projection",
            Self::Applied => "applied",
        }
    }
}

/// Content-free observations. Unmeasured token counts stay absent, never zero.
#[derive(Debug, Clone, Serialize)]
pub struct PreparationMetrics {
    pub reason: PreparationReason,
    pub input_bytes: usize,
    pub input_tokens: Option<usize>,
    pub output_bytes: Option<usize>,
    pub output_tokens: Option<usize>,
    pub elapsed_micros: u64,
}

pub struct Preparation {
    pub projection: Option<Projection>,
    pub metrics: PreparationMetrics,
}

/// Ephemeral request content, not a replacement for the persisted source.
#[derive(Clone)]
pub struct Projection {
    pub encoded: String,
    pub source_hash: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
}

impl Projection {
    /// Decode bounded input and verify its original-source hash before returning it.
    pub fn reconstruct(&self) -> Option<String> {
        reconstruct(&self.encoded, &self.source_hash)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Encoding {
    v: u8,
    chunks: Vec<String>,
    order: Vec<usize>,
}

/// Prepare only content eligible by size and syntax. Host eligibility is separate.
pub fn prepare(original: &str, mode: InputPreparationMode) -> Preparation {
    let started = Instant::now();
    let mut metrics = PreparationMetrics {
        reason: PreparationReason::Off,
        input_bytes: original.len(),
        input_tokens: None,
        output_bytes: None,
        output_tokens: None,
        elapsed_micros: 0,
    };
    let projection = prepare_inner(original, mode, &mut metrics);
    metrics.elapsed_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    Preparation {
        projection,
        metrics,
    }
}

fn prepare_inner(
    original: &str,
    mode: InputPreparationMode,
    metrics: &mut PreparationMetrics,
) -> Option<Projection> {
    if mode == InputPreparationMode::Off {
        return None;
    }
    if original.len() > MAX_INPUT_BYTES {
        metrics.reason = PreparationReason::ByteLimit;
        return None;
    }
    let input_tokens = count_tokens(original);
    metrics.input_tokens = Some(input_tokens);
    if let Some(reason) = token_rejection(input_tokens) {
        metrics.reason = reason;
        return None;
    }
    if protected_syntax(original) {
        metrics.reason = PreparationReason::ProtectedSyntax;
        return None;
    }
    let Some(paragraphs) = paragraphs(original) else {
        metrics.reason = PreparationReason::ParagraphLimit;
        return None;
    };
    metrics.reason = PreparationReason::InvalidProjection;
    let projection = encode(original, paragraphs, input_tokens)?;
    metrics.output_bytes = Some(projection.encoded.len());
    metrics.output_tokens = Some(projection.output_tokens);
    if !worthwhile(input_tokens, projection.output_tokens) {
        metrics.reason = PreparationReason::InsufficientSavings;
        return None;
    }
    metrics.reason = PreparationReason::Applied;
    Some(projection)
}

fn token_rejection(tokens: usize) -> Option<PreparationReason> {
    if tokens < MIN_INPUT_TOKENS {
        Some(PreparationReason::TooShort)
    } else if tokens > MAX_INPUT_TOKENS {
        Some(PreparationReason::TokenLimit)
    } else {
        None
    }
}

fn worthwhile(input_tokens: usize, output_tokens: usize) -> bool {
    let Some(saved) = input_tokens.checked_sub(output_tokens) else {
        return false;
    };
    saved >= 512 && saved >= input_tokens.div_ceil(5)
}

fn protected_syntax(original: &str) -> bool {
    original.split('\n').any(|line| {
        if line.starts_with('\t') {
            return true;
        }
        let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
        if spaces >= 4 {
            return true;
        }
        let rest = &line[spaces..];
        rest.starts_with('>') || rest.starts_with("```") || rest.starts_with("~~~")
    })
}

fn paragraphs(original: &str) -> Option<Vec<&str>> {
    let bytes = original.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let separator = if bytes[cursor..].starts_with(b"\n\n") {
            2
        } else if bytes[cursor..].starts_with(b"\r\n\r\n") {
            4
        } else {
            cursor += 1;
            continue;
        };
        cursor += separator;
        spans.push(&original[start..cursor]);
        if spans.len() > MAX_PARAGRAPHS {
            return None;
        }
        start = cursor;
    }
    if start < original.len() {
        spans.push(&original[start..]);
    }
    (spans.len() <= MAX_PARAGRAPHS).then_some(spans)
}

fn encode(original: &str, paragraphs: Vec<&str>, input_tokens: usize) -> Option<Projection> {
    let mut wire = Encoding {
        v: 1,
        chunks: Vec::new(),
        order: Vec::with_capacity(paragraphs.len()),
    };
    for paragraph in paragraphs {
        let id = match wire.chunks.iter().position(|chunk| chunk == paragraph) {
            Some(id) => id,
            None => {
                wire.chunks.push(paragraph.to_owned());
                wire.chunks.len() - 1
            }
        };
        wire.order.push(id);
    }
    let encoded = format!("{PREAMBLE}{}", serde_json::to_string(&wire).ok()?);
    let source_hash = content_hash(original.as_bytes());
    if reconstruct(&encoded, &source_hash)?.as_bytes() != original.as_bytes() {
        return None;
    }
    let output_tokens = count_tokens(&encoded);
    Some(Projection {
        encoded,
        source_hash,
        input_tokens,
        output_tokens,
    })
}

fn reconstruct(encoded: &str, expected_hash: &str) -> Option<String> {
    if encoded.len() > MAX_ENCODED_BYTES {
        return None;
    }
    let wire: Encoding = serde_json::from_str(encoded.strip_prefix(PREAMBLE)?).ok()?;
    if wire.v != 1
        || wire.chunks.is_empty()
        || wire.chunks.len() > MAX_PARAGRAPHS
        || wire.order.is_empty()
        || wire.order.len() > MAX_PARAGRAPHS
        || wire
            .chunks
            .iter()
            .enumerate()
            .any(|(id, chunk)| chunk.is_empty() || wire.chunks[..id].contains(chunk))
    {
        return None;
    }
    let mut original = String::new();
    let mut next_id = 0;
    for id in wire.order {
        // A new ID must be the next first-occurrence ID, never a forward reference.
        if id > next_id {
            return None;
        }
        if id == next_id {
            next_id += 1;
        }
        let chunk = wire.chunks.get(id)?;
        if original.len().checked_add(chunk.len())? > MAX_INPUT_BYTES {
            return None;
        }
        original.push_str(chunk);
    }
    if next_id != wire.chunks.len() || content_hash(original.as_bytes()) != expected_hash {
        return None;
    }
    Some(original)
}

#[cfg(test)]
mod tests;
