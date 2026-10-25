//! Routing a request to a handful of files, instead of sending the whole map.
//!
//! `docs/filemap.md` is one generated line per documented file. The system
//! prompt tells the model to read it before searching the tree, and `read`
//! has no cap that a 1,054-line file reaches, so that instruction costs
//! 15,857 tokens in a single tool result — which then stays in the history
//! and is re-sent on every later iteration of the run, because tool memory
//! is off by default and compaction does not fire until 200,000.
//!
//! The map is not compressible: 69% of it is the descriptions, and the
//! descriptions are the whole value. What is compressible is *sending all
//! of it*. The judge answers one independent question per entry in a single
//! request, at an input price two orders of magnitude below the main
//! model's, and only the entries it adopts are put in front of the model.
//!
//! Lexical ranking is deliberately not used here. `rank_by_bm25` tokenises
//! on `[a-z0-9]+`, so a Japanese request produces no terms at all, and 438
//! of this repository's 528 descriptions are English. Narrowing by shared
//! vocabulary before judging meaning would drop the right file for exactly
//! the requests this exists to serve — the same failure
//! `mcp::preselection::fitting` documents for tools.

use crate::decision_bands::Refused;

/// The heading the routed files are put under.
///
/// It says where they came from, because the system prompt still tells the
/// model to read the map before searching the tree — correct when there is
/// no judge, and a 15,857-token repeat of work already done when there is.
/// Nineteen tokens an iteration to say the map has been consulted is the
/// cheap side of that trade by three orders of magnitude.
pub const HEADING: &str =
    "## Relevant files (already selected from docs/filemap.md; no need to reread)";

/// The most files named in the injected section.
///
/// The adopted band is not bounded by the judge, and a broad request can
/// adopt dozens. Eight is what fits the token ceiling below at the observed
/// path-and-summary length, and a list longer than that stops reading as a
/// starting point and starts reading as a second map.
pub const MAX_PICKS: usize = 8;

/// Token ceiling for the injected section, measured the same way every
/// other budgeted section is.
///
/// Held separately from `MAX_PICKS` because the two bound different
/// failures: the count bounds how much the model is asked to consider, and
/// this bounds what the run pays for it on every iteration.
pub const SECTION_LIMIT: usize = 256;

/// One documented file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Repository-relative path, rebuilt from the section heading and the
    /// bare file name the line carries.
    pub path: String,
    pub summary: String,
}

/// Rebuilds `(path, summary)` pairs from a generated file map.
///
/// The generator writes a `## ` heading holding a directory in backticks,
/// then one `- ` line per file holding a *bare file name* in backticks, an
/// em dash, and a one-line summary. The path is not on the line, so it only
/// exists once the heading it sits under is known; a line before any
/// heading has no path and is dropped rather than guessed at.
///
/// Anything that does not match is skipped, not repaired. This parses a
/// generated file, and a line the generator did not produce is a signal
/// that the two have diverged — inventing an entry for it would hide that.
pub fn parse(markdown: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut directory: Option<&str> = None;
    for line in markdown.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            directory = backticked(rest.trim());
            continue;
        }
        let Some(directory) = directory else { continue };
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        let Some(name) = backticked(rest) else {
            continue;
        };
        // The summary is what follows the first em dash after the name.
        // Splitting on the first one keeps a summary that contains another
        // em dash intact, which several of them do.
        let Some((_, summary)) = rest.split_once(" — ") else {
            continue;
        };
        let summary = summary.trim();
        if name.is_empty() || summary.is_empty() {
            continue;
        }
        entries.push(Entry {
            path: if directory == "." {
                name.to_owned()
            } else {
                format!("{directory}/{name}")
            },
            summary: summary.to_owned(),
        });
    }
    entries
}

/// The contents of the first pair of backticks, if the text opens with one.
fn backticked(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('`')?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// The section put in front of the model, or `None` when nothing was
/// adopted.
///
/// Truncation is by whole entries. A half-written path is worse than a
/// shorter list: the model cannot tell the two apart, and `read` on a
/// truncated path fails in a way that looks like the file is missing.
pub fn section(picks: &[&Entry]) -> Option<String> {
    let mut body = String::new();
    let mut held = 0usize;
    for entry in picks.iter().take(MAX_PICKS) {
        let line = format!("- `{}` — {}\n", entry.path, entry.summary);
        if crate::budget::count_tokens(&(body.clone() + &line)) > SECTION_LIMIT {
            break;
        }
        body.push_str(&line);
        held += 1;
    }
    (held > 0).then_some(body)
}

/// How many entries one request may carry.
///
/// The judge's limit is on the whole request, and every entry adds its own
/// question, so the count that fits depends on how long the entries are.
/// Measuring the real questions is what keeps this correct as the map
/// grows; a fixed count would silently start overflowing.
fn chunk_size(entries: &[Entry], query: &str, phase: &str) -> usize {
    let budget = polaris_provider::decision::MAX_STATE_TOKENS * 3 / 4;
    let state = crate::budget::count_tokens(&state(query, phase).to_string());
    let mut used = state;
    let mut held = 0usize;
    for entry in entries {
        let question =
            match crate::decision_bands::question(query, "file", &entry.path, &entry.summary) {
                polaris_provider::decision::Question::Noul { instructions } => {
                    crate::budget::count_tokens(&instructions)
                }
                _ => unreachable!("an entry is always asked about with a Noul"),
            };
        if used + question > budget {
            break;
        }
        used += question;
        held += 1;
    }
    held.max(1)
}

fn state(query: &str, phase: &str) -> serde_json::Value {
    serde_json::json!({ "task": query, "phase": phase })
}

/// Asks the judge about every entry and returns the adopted ones, best
/// first.
///
/// The map does not fit in one request, so it is asked about in chunks.
/// Chunking is not filtering: every entry is asked about, which is the one
/// property that makes this usable for a request whose wording shares
/// nothing with the descriptions. A chunk the judge could not answer is
/// dropped rather than failing the whole route — a partial list of real
/// files is still worth more than the 15,857-token read it replaces.
pub async fn route<'a>(
    decider: &dyn polaris_provider::decision::Decider,
    entries: &'a [Entry],
    query: &str,
    phase: &str,
) -> Result<Vec<&'a Entry>, Refused> {
    if entries.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let size = chunk_size(entries, query, phase);
    let state = state(query, phase);
    let mut adopted: Vec<&Entry> = Vec::new();
    let mut answered = 0usize;
    let mut refusal = None;
    for chunk in entries.chunks(size) {
        let candidates: Vec<(&str, &str)> = chunk
            .iter()
            .map(|entry| (entry.path.as_str(), entry.summary.as_str()))
            .collect();
        match crate::decision_bands::judge(
            decider,
            &state,
            query,
            "file",
            &candidates,
            std::future::pending(),
        )
        .await
        {
            Ok(bands) => {
                answered += 1;
                adopted.extend(
                    bands
                        .context_candidates()
                        .into_iter()
                        .filter_map(|index| chunk.get(index)),
                );
            }
            // A stop is the person's, and is not partial progress.
            Err(Refused::Cancelled) => return Err(Refused::Cancelled),
            Err(failure) => refusal = Some(failure),
        }
    }
    match (answered, refusal) {
        (0, Some(failure)) => Err(failure),
        _ => Ok(adopted),
    }
}

#[cfg(test)]
mod tests;
