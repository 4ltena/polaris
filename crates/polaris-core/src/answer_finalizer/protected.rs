//! Deterministic protected-span extraction and restoration.
//!
//! The v1 scanner deliberately rejects Markdown constructs that this bounded
//! parser cannot assign unambiguous byte ranges to.  Failing closed is safer
//! than passing a partially protected answer to a translator.

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use thiserror::Error;

const SENTINEL_PREFIX: &str = "⟦POLARIS_PROTECTED_";
const SENTINEL_SUFFIX: &str = "⟧";
const MAX_SPANS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedSpanKindV1 {
    FencedCode,
    InlineCode,
    LinkDestination,
    AutolinkTarget,
    Citation,
    ToolEvidence,
    Command,
    Path,
    Url,
    Email,
    Identifier,
    Version,
    NumericUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedSpanV1 {
    pub index: usize,
    pub kind: ProtectedSpanKindV1,
    pub start: usize,
    pub end: usize,
    pub exact_bytes: Vec<u8>,
    pub sha256: String,
    pub sentinel: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedDocumentV1 {
    pub source_sha256: String,
    pub protected_text: String,
    pub spans: Vec<ProtectedSpanV1>,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    kind: ProtectedSpanKindV1,
    start: usize,
    end: usize,
    priority: u8,
}

pub fn scan_protected_spans(
    source: &str,
    quoted_tool_evidence: &[&str],
) -> Result<Vec<ProtectedSpanV1>, ProtectionError> {
    reject_unsupported_markdown(source)?;
    if source.contains(SENTINEL_PREFIX) {
        return Err(ProtectionError::SentinelCollision);
    }

    let mut candidates = Vec::new();
    scan_code(source, &mut candidates)?;
    scan_links_and_citations(source, &mut candidates);
    for evidence in quoted_tool_evidence {
        if evidence.is_empty() {
            continue;
        }
        for (start, _) in source.match_indices(evidence) {
            candidates.push(Candidate {
                kind: ProtectedSpanKindV1::ToolEvidence,
                start,
                end: start + evidence.len(),
                priority: 2,
            });
        }
    }
    scan_regex_categories(source, &mut candidates);

    candidates.retain(|candidate| {
        candidate.start < candidate.end
            && source.is_char_boundary(candidate.start)
            && source.is_char_boundary(candidate.end)
    });
    candidates.sort_by_key(|candidate| {
        (
            candidate.priority,
            candidate.start,
            usize::MAX - candidate.end,
        )
    });

    let mut accepted: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if accepted
            .iter()
            .any(|existing| overlaps(*existing, candidate))
        {
            continue;
        }
        accepted.push(candidate);
    }
    accepted.sort_by_key(|candidate| candidate.start);
    if accepted.len() > MAX_SPANS {
        return Err(ProtectionError::TooManySpans);
    }

    Ok(accepted
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            let bytes = source.as_bytes()[candidate.start..candidate.end].to_vec();
            let digest = digest(&bytes);
            ProtectedSpanV1 {
                index,
                kind: candidate.kind,
                start: candidate.start,
                end: candidate.end,
                sentinel: sentinel(index, &digest),
                exact_bytes: bytes,
                sha256: digest,
            }
        })
        .collect())
}

pub fn protect_translation_input(
    source: &str,
    quoted_tool_evidence: &[&str],
) -> Result<ProtectedDocumentV1, ProtectionError> {
    let spans = scan_protected_spans(source, quoted_tool_evidence)?;
    let mut protected = String::with_capacity(source.len());
    let mut cursor = 0;
    for span in &spans {
        protected.push_str(&source[cursor..span.start]);
        protected.push_str(&span.sentinel);
        cursor = span.end;
    }
    protected.push_str(&source[cursor..]);
    Ok(ProtectedDocumentV1 {
        source_sha256: digest(source.as_bytes()),
        protected_text: protected,
        spans,
    })
}

pub fn restore_translation_output(
    document: &ProtectedDocumentV1,
    translated: &str,
) -> Result<String, ProtectionError> {
    validate_document(document)?;
    let observed = collect_sentinels(translated)?;
    let expected = document
        .spans
        .iter()
        .map(|span| span.sentinel.as_str())
        .collect::<Vec<_>>();
    if observed.len() != expected.len() {
        return Err(ProtectionError::SentinelCount);
    }
    if observed != expected {
        let observed_set = observed.iter().copied().collect::<BTreeSet<_>>();
        if observed_set.len() != observed.len() {
            return Err(ProtectionError::DuplicateSentinel);
        }
        return Err(ProtectionError::SentinelOrderOrIdentity);
    }

    let mut restored = String::with_capacity(translated.len());
    let mut cursor = 0;
    for span in &document.spans {
        let relative = translated[cursor..]
            .find(&span.sentinel)
            .ok_or(ProtectionError::MissingSentinel)?;
        let start = cursor + relative;
        restored.push_str(&translated[cursor..start]);
        let exact =
            std::str::from_utf8(&span.exact_bytes).map_err(|_| ProtectionError::CorruptSpanMap)?;
        restored.push_str(exact);
        cursor = start + span.sentinel.len();
    }
    restored.push_str(&translated[cursor..]);

    // Re-scan the output. Every original protected byte string must still be
    // recognized with the same structural kind and in the same order. This
    // catches removed link delimiters, changed fence structure, and similar
    // mutations outside the sentinel itself.
    let rescanned = scan_protected_spans(&restored, &[])?;
    let mut search_from = 0;
    for expected_span in document.spans.iter().filter(|span| {
        matches!(
            span.kind,
            ProtectedSpanKindV1::FencedCode
                | ProtectedSpanKindV1::InlineCode
                | ProtectedSpanKindV1::LinkDestination
                | ProtectedSpanKindV1::AutolinkTarget
                | ProtectedSpanKindV1::Citation
        )
    }) {
        let found = rescanned[search_from..].iter().position(|candidate| {
            candidate.kind == expected_span.kind
                && candidate.exact_bytes == expected_span.exact_bytes
                && candidate.sha256 == expected_span.sha256
        });
        let Some(relative) = found else {
            return Err(ProtectionError::StructuralIntegrity);
        };
        search_from += relative + 1;
    }
    Ok(restored)
}

fn validate_document(document: &ProtectedDocumentV1) -> Result<(), ProtectionError> {
    let mut previous_end = 0;
    for (index, span) in document.spans.iter().enumerate() {
        if span.index != index
            || span.start < previous_end
            || span.start >= span.end
            || span.sha256 != digest(&span.exact_bytes)
            || span.sentinel != sentinel(index, &span.sha256)
        {
            return Err(ProtectionError::CorruptSpanMap);
        }
        previous_end = span.end;
    }
    Ok(())
}

fn collect_sentinels(text: &str) -> Result<Vec<&str>, ProtectionError> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(SENTINEL_PREFIX) {
        let start = cursor + relative;
        let rest = &text[start..];
        let end = rest
            .find(SENTINEL_SUFFIX)
            .map(|end| start + end + SENTINEL_SUFFIX.len())
            .ok_or(ProtectionError::MutatedSentinel)?;
        found.push(&text[start..end]);
        cursor = end;
    }
    Ok(found)
}

fn scan_code(source: &str, candidates: &mut Vec<Candidate>) -> Result<(), ProtectionError> {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let line_start = cursor == 0 || bytes[cursor - 1] == b'\n';
        if line_start {
            let indent = bytes[cursor..]
                .iter()
                .take_while(|byte| **byte == b' ')
                .count();
            if indent <= 3 {
                let fence_start = cursor + indent;
                let marker = bytes.get(fence_start).copied();
                if matches!(marker, Some(b'`' | b'~')) {
                    let count = bytes[fence_start..]
                        .iter()
                        .take_while(|byte| Some(**byte) == marker)
                        .count();
                    if count >= 3 {
                        let mut line_end = source[fence_start..]
                            .find('\n')
                            .map(|value| fence_start + value + 1)
                            .unwrap_or(source.len());
                        let mut closing_end = None;
                        while line_end < source.len() {
                            let next_end = source[line_end..]
                                .find('\n')
                                .map(|value| line_end + value + 1)
                                .unwrap_or(source.len());
                            let line = source[line_end..next_end]
                                .trim_end_matches(['\r', '\n'])
                                .trim_start_matches(' ');
                            let closing_count = line
                                .as_bytes()
                                .iter()
                                .take_while(|byte| Some(**byte) == marker)
                                .count();
                            if closing_count >= count && line[closing_count..].trim().is_empty() {
                                closing_end = Some(next_end);
                                break;
                            }
                            line_end = next_end;
                        }
                        let end = closing_end.ok_or(ProtectionError::UnclosedFence)?;
                        candidates.push(Candidate {
                            kind: ProtectedSpanKindV1::FencedCode,
                            start: cursor,
                            end,
                            priority: 0,
                        });
                        cursor = end;
                        continue;
                    }
                }
            }
        }
        if bytes[cursor] == b'`' {
            let count = bytes[cursor..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            let marker = "`".repeat(count);
            if let Some(relative) = source[cursor + count..].find(&marker) {
                let end = cursor + count + relative + count;
                candidates.push(Candidate {
                    kind: ProtectedSpanKindV1::InlineCode,
                    start: cursor,
                    end,
                    priority: 0,
                });
                cursor = end;
                continue;
            }
            return Err(ProtectionError::UnclosedInlineCode);
        }
        cursor += source[cursor..]
            .chars()
            .next()
            .expect("in range")
            .len_utf8();
    }
    Ok(())
}

fn scan_links_and_citations(source: &str, candidates: &mut Vec<Candidate>) {
    let citation = Regex::new(r"\[\^(?:[A-Za-z0-9_.:-]+)\]|\[citation:[A-Za-z0-9_.:-]+\]")
        .expect("static citation regex");
    add_matches(
        &citation,
        source,
        ProtectedSpanKindV1::Citation,
        1,
        candidates,
    );

    let destination = Regex::new(r"\]\((?P<dest>[^\s()]+)\)").expect("static link regex");
    for capture in destination.captures_iter(source) {
        if let Some(value) = capture.name("dest") {
            candidates.push(Candidate {
                kind: ProtectedSpanKindV1::LinkDestination,
                start: value.start(),
                end: value.end(),
                priority: 1,
            });
        }
    }
    let autolink =
        Regex::new(r"<(?P<target>(?:https?://|mailto:)[^<>\s]+)>").expect("static autolink regex");
    for capture in autolink.captures_iter(source) {
        let value = capture.name("target").expect("named group");
        candidates.push(Candidate {
            kind: ProtectedSpanKindV1::AutolinkTarget,
            start: value.start(),
            end: value.end(),
            priority: 1,
        });
    }
}

fn scan_regex_categories(source: &str, candidates: &mut Vec<Candidate>) {
    let patterns = [
        (
            ProtectedSpanKindV1::Command,
            r"(?m)^[ \t]*[$>][ \t]+[A-Za-z][^\r\n]*|(?m)\b(?:cargo|git|npm|pnpm|yarn|python3?|swift|make|cmake|docker|kubectl)\s+[A-Za-z0-9][^\r\n]*",
            3,
        ),
        (ProtectedSpanKindV1::Url, r"https?://[^\s<>\]\)]+", 3),
        (
            ProtectedSpanKindV1::Email,
            r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
            3,
        ),
        (
            ProtectedSpanKindV1::Path,
            r#"(?:~|\.{1,2})?/[A-Za-z0-9_.~+@%/-]+|\b[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.~+@%-]+)+|[A-Za-z]:\\(?:[^\\\s:*?\"<>|]+\\)*[^\\\s:*?\"<>|]*"#,
            3,
        ),
        (
            ProtectedSpanKindV1::Identifier,
            r"(?:\b[0-9a-fA-F]{40,64}\b|\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-5][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}\b|\b[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+\b|\b[A-Z][A-Z0-9_]{2,}\b|\b[A-Za-z_][A-Za-z0-9_]*\(\)|--[a-z0-9][a-z0-9-]*|-[A-Za-z]\b|\$[A-Z_][A-Z0-9_]*)",
            3,
        ),
        (
            ProtectedSpanKindV1::Version,
            r"\bv?\d+\.\d+(?:\.\d+)?(?:[-+][0-9A-Za-z.-]+)?\b",
            4,
        ),
        (
            ProtectedSpanKindV1::NumericUnit,
            r"(?x)(?:[$€£¥]\s?\d+(?:[.,]\d+)*)|(?:\b\d{4}-\d{2}-\d{2}\b)|(?:\b\d{1,2}:\d{2}(?::\d{2})?\b)|(?:\b\d+(?:[.,]\d+)?\s?(?:ms|s|sec|min|h|B|KB|MB|GB|KiB|MiB|GiB|Hz|kHz|MHz|GHz|px|pt|em|rem|°C|°F|mA|A|V|W|tokens?)\b)|(?:\b\d+(?:[.,]\d+)?\s?%)|(?:\b\d+(?:[.,]\d+)*\b)",
            4,
        ),
    ];
    for (kind, pattern, priority) in patterns {
        let regex = Regex::new(pattern).expect("static protected-span regex");
        add_matches(&regex, source, kind, priority, candidates);
    }
}

fn add_matches(
    regex: &Regex,
    source: &str,
    kind: ProtectedSpanKindV1,
    priority: u8,
    candidates: &mut Vec<Candidate>,
) {
    for found in regex.find_iter(source) {
        candidates.push(Candidate {
            kind,
            start: found.start(),
            end: found.end(),
            priority,
        });
    }
}

fn reject_unsupported_markdown(source: &str) -> Result<(), ProtectionError> {
    let unsupported = [
        r"(?m)^\s*\[\^[^\]]+\]:",
        r"\[[^\]\n]+\]\[[^\]\n]*\]",
        r"(?m)^\s*\|?.+\|.+\r?\n\s*\|?\s*:?-{3,}",
        r"(?s)\$\$.+?\$\$|\\\(.+?\\\)|\\\[.+?\\\]",
        r"(?i)<\/?[a-z][a-z0-9-]*(?:\s[^<>]*)?/?>",
    ];
    if unsupported.iter().any(|pattern| {
        Regex::new(pattern)
            .expect("static unsupported-markdown regex")
            .is_match(source)
    }) {
        return Err(ProtectionError::UnsupportedMarkdown);
    }
    Ok(())
}

fn overlaps(left: Candidate, right: Candidate) -> bool {
    left.start < right.end && right.start < left.end
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sentinel(index: usize, digest: &str) -> String {
    format!(
        "{SENTINEL_PREFIX}{index:04}_{}{SENTINEL_SUFFIX}",
        &digest[..12]
    )
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtectionError {
    #[error("source collides with the reserved sentinel namespace")]
    SentinelCollision,
    #[error("unsupported Markdown cannot be translated safely")]
    UnsupportedMarkdown,
    #[error("fenced code block is not closed")]
    UnclosedFence,
    #[error("inline code span is not closed")]
    UnclosedInlineCode,
    #[error("protected span limit exceeded")]
    TooManySpans,
    #[error("protected span map is corrupt")]
    CorruptSpanMap,
    #[error("translated sentinel is missing")]
    MissingSentinel,
    #[error("translated sentinel is malformed or mutated")]
    MutatedSentinel,
    #[error("translated sentinel count differs")]
    SentinelCount,
    #[error("translated sentinel is duplicated")]
    DuplicateSentinel,
    #[error("translated sentinel order or identity differs")]
    SentinelOrderOrIdentity,
    #[error("restored Markdown structure does not preserve protected spans")]
    StructuralIntegrity,
}
