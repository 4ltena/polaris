//! What this project has already settled, so it is not settled twice.
//!
//! An agent re-opens a question it closed last week, because nothing in its
//! context says it was closed. The person watching remembers, and has to
//! say so again. The cost is not only the tokens spent re-deriving an
//! answer — it is that the second answer may differ from the first, and
//! then neither is trusted.
//!
//! The ledger is one line per settled question, with the time it was
//! settled. Before a run starts, the judge is asked, for each line,
//! whether the request in hand is that same question. Anything it adopts
//! is put in front of the model.
//!
//! **This advises; it never blocks.** The two errors are not symmetric: a
//! false "already decided" would stop work that should happen, while a
//! false negative only leaves things as they are today. So the model is
//! told what looks settled and decides for itself, and a wrong entry costs
//! a sentence rather than a task.
//!
//! The bands in `decision_bands` were calibrated on 「役に立つか」 and do
//! not transfer to this question. Only the adopted band is read, and
//! nothing is refused on the strength of it. Until the wording here has
//! its own measurement, the edge divides the answers without anyone
//! knowing where it ought to sit — which is survivable precisely because
//! the output is advice.

use std::path::Path;

use crate::decision_bands::{Bands, Refused};

/// The most entries named in the injected section.
///
/// A run that matches half the ledger has not found a settled question; it
/// has found a ledger written too vaguely. Showing three keeps the signal
/// readable and makes that failure visible instead of burying it.
pub const MAX_SHOWN: usize = 3;

/// Token ceiling for the injected section.
pub const SECTION_LIMIT: usize = 192;

/// Where the ledger lives, relative to the project root.
pub const PATH: &str = ".polaris/decisions.csv";

/// The heading the matches are put under.
pub const HEADING: &str = "## 既に決めたかもしれないこと";

/// One settled question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// When it was settled. Free text so a hand-written row is not
    /// rejected; nothing parses it back into a date.
    pub at: String,
    /// The question and its answer, in one line.
    pub what: String,
}

/// Reads the ledger. A missing file is an empty ledger, not an error — a
/// project that has settled nothing yet is the ordinary case.
pub fn load(root: &Path) -> Vec<Entry> {
    std::fs::read_to_string(root.join(PATH))
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Parses the two-column CSV.
///
/// Hand-editing is expected, so a malformed row is skipped rather than
/// failing the read: one bad line must not take the whole ledger with it.
/// The header is optional and recognised by its first field alone.
pub fn parse(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for record in records(text) {
        let [at, what] = &record[..] else { continue };
        if at.is_empty() || what.is_empty() || at == "at" {
            continue;
        }
        out.push(Entry {
            at: at.clone(),
            what: what.clone(),
        });
    }
    out
}

/// Splits RFC 4180 records. A quoted field may hold commas, quotes and
/// newlines, so this cannot be done by splitting on lines first.
fn records(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut started = false;

    while let Some(c) = chars.next() {
        started = true;
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (true, _) => field.push(c),
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, _) => field.push(c),
        }
    }
    if started && (!field.is_empty() || !row.is_empty()) {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Quotes a field only when it has to be quoted.
fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Appends one settled question.
///
/// Append-only on purpose. A ledger whose rows can be rewritten is a
/// ledger that cannot be trusted to say what was decided *then*, which is
/// the only thing it is for. Correcting a past entry means adding the
/// correction as its own line.
pub fn append(root: &Path, at: &str, what: &str) -> std::io::Result<()> {
    use std::io::Write;
    let what = what.trim();
    if what.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "決めたことが空です",
        ));
    }
    let path = root.join(PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fresh = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if fresh {
        writeln!(file, "at,what")?;
    }
    writeln!(file, "{},{}", escape(at), escape(what))
}

/// The section put in front of the model, or `None` when nothing matched.
pub fn section(matched: &[&Entry]) -> Option<String> {
    let mut body = String::new();
    let mut held = 0usize;
    for entry in matched.iter().take(MAX_SHOWN) {
        let line = format!("- {} — {}\n", entry.at, entry.what);
        if crate::budget::count_tokens(&(body.clone() + &line)) > SECTION_LIMIT {
            break;
        }
        body.push_str(&line);
        held += 1;
    }
    (held > 0).then_some(body)
}

/// The question asked of each entry.
///
/// Deliberately not `decision_bands::question`. That one asks 「役に立つか」
/// and its two edges were calibrated on that wording; the doc comment there
/// says changing the wording invalidates the numbers. This asks something
/// else, so it gets its own wording and reads only the adopted band.
fn question(query: &str, entry: &Entry) -> polaris_provider::decision::Question {
    polaris_provider::decision::Question::Noul {
        instructions: format!(
            "いま始めようとしている検討は、過去に決めたこの件と同じ問いか。\
             いま: {query} / 過去（{}）: {}",
            entry.at, entry.what
        ),
    }
}

fn state(query: &str) -> serde_json::Value {
    serde_json::json!({ "task": query })
}

/// How many entries one request may carry, measured from the real
/// questions rather than fixed, so a growing ledger cannot start
/// overflowing in silence.
fn chunk_size(entries: &[Entry], query: &str) -> usize {
    let budget = polaris_provider::decision::MAX_STATE_TOKENS * 3 / 4;
    let mut used = crate::budget::count_tokens(&state(query).to_string());
    let mut held = 0usize;
    for entry in entries {
        let cost = match question(query, entry) {
            polaris_provider::decision::Question::Noul { instructions } => {
                crate::budget::count_tokens(&instructions)
            }
            _ => unreachable!("an entry is always asked about with a Noul"),
        };
        if used + cost > budget {
            break;
        }
        used += cost;
        held += 1;
    }
    held.max(1)
}

/// Asks the judge which settled questions this request repeats.
///
/// Newest first: a ledger accumulates, and the entry most likely to be the
/// one being repeated is the one settled most recently. When more match
/// than `section` will show, that ordering decides which survive.
pub async fn route<'a>(
    decider: &dyn polaris_provider::decision::Decider,
    entries: &'a [Entry],
    query: &str,
) -> Result<Vec<&'a Entry>, Refused> {
    if entries.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let size = chunk_size(entries, query);
    let state = state(query);
    let mut matched: Vec<&Entry> = Vec::new();
    let mut answered = 0usize;
    let mut refusal = None;
    for chunk in entries.chunks(size) {
        match judge(decider, &state, query, chunk).await {
            Ok(Bands { adopted, .. }) => {
                answered += 1;
                matched.extend(adopted.into_iter().filter_map(|index| chunk.get(index)));
            }
            Err(Refused::Cancelled) => return Err(Refused::Cancelled),
            Err(failure) => refusal = Some(failure),
        }
    }
    match (answered, refusal) {
        (0, Some(failure)) => Err(failure),
        _ => Ok(matched),
    }
}

/// Wraps `decision_bands::judge` with this module's own question text.
async fn judge(
    decider: &dyn polaris_provider::decision::Decider,
    state: &serde_json::Value,
    query: &str,
    chunk: &[Entry],
) -> Result<Bands, Refused> {
    crate::decision_bands::judge_with(
        decider,
        state,
        chunk.iter().map(|entry| question(query, entry)).collect(),
        std::future::pending(),
    )
    .await
}

#[cfg(test)]
mod tests;
