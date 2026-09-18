//! Which part of a long tool result is worth quoting back.
//!
//! A build log or a test run is mostly noise. Its first 128 characters are a
//! banner, the lines that say what went wrong sit somewhere in the middle, and
//! the verdict is on the last line. This picks those regions out with a plain
//! scan for failure markers, keeps a few lines of context around each one,
//! reserves part of the budget for the tail whatever else happens, and marks
//! every gap it leaves so the excerpt cannot be mistaken for the whole output.

/// Lines kept either side of a marker. A log puts the detail *after* the
/// marker line — the `-->` of a Rust error, the frames of a stack trace — so
/// the window reaches further down than up.
const BEFORE: usize = 2;
const AFTER: usize = 5;

/// Case-insensitive substrings that make a line a failure. `error` also covers
/// `error[E0308]`, and `failed` covers `FAILED`.
const STRONG: &[&str] = &[
    "error",
    "panic",
    "failed",
    "assertion",
    "traceback",
    "fatal",
    "exception",
];

/// Only consulted when nothing stronger matched anywhere in the text: in a log
/// that never says it failed, a warning is the most interesting line there is.
const WEAK: &[&str] = &["warning"];

/// The interesting regions of `text`, at most `budget_bytes` long, with every
/// omission spelled out. Text that already fits comes back untouched.
///
/// The marker scan decides what is interesting. [`select_judged`] answers the
/// same question with a judge when the scan finds nothing to aim at.
pub fn select(text: &str, budget_bytes: usize) -> String {
    if text.len() <= budget_bytes {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let found = markers(&lines);
    assemble(&lines, found, budget_bytes)
}

/// How many lines one judged request may ask about. A log is unbounded and a
/// question per line is not, so past this the marker scan answers alone. The
/// bound is the same order as the tool catalogue the judge already handles in
/// one request (162 measured on 2026-09-18, ~40 reference tokens each).
const MAX_JUDGED_LINES: usize = 200;

/// The same excerpt, with the judge saying which lines matter when the marker
/// scan cannot tell.
///
/// **It only runs where the scan admits it does not know.** A text with a real
/// failure marker in it is already answered, by a rule that costs nothing and
/// was measured; asking a model to re-decide that would spend a request to
/// agree. So the judge is asked only when no strong marker was found at all —
/// the case E-2's first stage left open — and only while the text is short
/// enough to ask about line by line.
///
/// Without a judge, with one that cannot answer, or past the line bound, this
/// is exactly [`select`].
pub async fn select_judged(
    judge: Option<&dyn polaris_provider::decision::Decider>,
    query: &str,
    text: &str,
    budget_bytes: usize,
) -> String {
    if text.len() <= budget_bytes {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let found = markers(&lines);
    // `markers` falls back to the weak class when nothing stronger matched, so
    // "no strong marker" is what an empty *or* weak-only result means. Only the
    // first is a case the scan could not answer.
    let strong = found.iter().any(|index| is_strong(lines[*index]));
    let Some(judge) = judge.filter(|_| !strong && lines.len() <= MAX_JUDGED_LINES) else {
        return select(text, budget_bytes);
    };
    let named: Vec<(&str, &str)> = lines.iter().map(|line| ("行", clamp_line(line))).collect();
    let Ok(bands) = crate::decision_bands::judge(
        judge,
        &serde_json::json!({ "task": query }),
        query,
        "行",
        &named,
        std::future::pending(),
    )
    .await
    else {
        return select(text, budget_bytes);
    };
    // Adopted and undecided both become something to aim at: the excerpt keeps
    // context around them and reports every line it left out, so an undecided
    // line costs a little room rather than being lost.
    let mut aimed = bands.adopted;
    aimed.extend(bands.middle);
    aimed.sort_unstable();
    if aimed.is_empty() {
        // Every line excluded. The judge answered, but not with somewhere to
        // aim, so the scan's head-and-tail answer is still the better one.
        return select(text, budget_bytes);
    }
    assemble(&lines, aimed, budget_bytes)
}

/// One line as the judge sees it. A single very long line would otherwise make
/// one question larger than the whole request is allowed to be.
fn clamp_line(line: &str) -> &str {
    const LINE_BYTES: usize = 512;
    let mut end = line.len().min(LINE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Builds the excerpt around `found`, whatever decided it.
fn assemble(lines: &[&str], found: Vec<usize>, budget_bytes: usize) -> String {
    let Some(last_line) = lines.len().checked_sub(1) else {
        return String::new(); // Unreachable: an empty text fits any budget.
    };
    let cost = |index: usize| lines[index].len() + 1;
    // An upper bound on one omission marker, so the marker that will precede
    // the first kept region is paid for before any region is admitted.
    let reserve = omission(lines.len()).len() + 1;

    // The verdict is at the end, so the tail is not negotiable: the last line
    // is kept whatever it costs, and earlier lines join it while a third of
    // the budget lasts.
    let tail_budget = budget_bytes / 3;
    let mut tail = last_line;
    let mut tail_bytes = cost(tail);
    while tail > 0 && tail_bytes + cost(tail - 1) <= tail_budget {
        tail -= 1;
        tail_bytes += cost(tail);
    }

    let mut regions: Vec<(usize, usize)> = found
        .into_iter()
        .map(|index| {
            (
                index.saturating_sub(BEFORE),
                (index + AFTER + 1).min(lines.len()),
            )
        })
        .collect();
    if regions.is_empty() {
        // Nothing to aim at: show where the output starts and where it ends.
        // Two markers are paid for here: the gap this leaves in the middle,
        // and the leading one the loop below reserves and a head starting at
        // line 0 never spends.
        let head_budget = budget_bytes.saturating_sub(tail_bytes + 2 * reserve);
        let mut head = 0;
        let mut head_bytes = 0;
        while head < tail && head_bytes + cost(head) <= head_budget {
            head_bytes += cost(head);
            head += 1;
        }
        if head > 0 {
            regions.push((0, head));
        }
    }
    regions.push((tail, lines.len()));
    let regions = merge(regions);

    // The last failure is the one that matters, so what goes when the budget
    // runs out is the earlier regions, and the front of a region too long to
    // fit whole. The gap markers say how many lines went with them.
    let mut used = reserve;
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for (position, &region) in regions.iter().enumerate().rev() {
        let mandatory = position + 1 == regions.len();
        let gap_cost = match kept.last() {
            Some(next) => omission(next.0 - region.1).len() + 1,
            None => 0,
        };
        let room = budget_bytes.saturating_sub(used + gap_cost);
        let fitted = fit_front(lines, region, room);
        let bytes = region_bytes(lines, fitted);
        if bytes > room && !mandatory {
            break;
        }
        used += bytes + gap_cost;
        kept.push(fitted);
        if fitted.0 != region.0 {
            break; // The shrink used up what was left.
        }
    }
    kept.reverse();

    let mut out = String::new();
    let mut previous = 0;
    for (start, end) in kept {
        if start > previous {
            out.push_str(&omission(start - previous));
            out.push('\n');
        }
        for line in &lines[start..end] {
            out.push_str(line);
            out.push('\n');
        }
        previous = end;
    }
    out.pop();
    clamp(out, budget_bytes)
}

/// Marker lines, strongest class first. A weak marker is worth nothing beside
/// a real failure, so the two classes are never mixed.
fn markers(lines: &[&str]) -> Vec<usize> {
    let strong: Vec<usize> = (0..lines.len()).filter(|&i| is_strong(lines[i])).collect();
    if strong.is_empty() {
        return (0..lines.len())
            .filter(|&i| contains_any(lines[i], WEAK))
            .collect();
    }
    strong
}

fn is_strong(line: &str) -> bool {
    // `E/` is logcat's severity column and only means that at the start of a
    // line; as a substring it matches every path whose directory ends in `e`.
    line.starts_with("E/") || contains_any(line, STRONG)
}

fn contains_any(line: &str, needles: &[&str]) -> bool {
    let lower = line.to_ascii_lowercase();
    needles.iter().any(|needle| lower.contains(needle))
}

/// Overlapping or touching windows become one region, so no line is emitted
/// twice and no gap marker claims an omission of zero lines.
fn merge(mut regions: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    regions.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in regions {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn region_bytes(lines: &[&str], (start, end): (usize, usize)) -> usize {
    lines[start..end].iter().map(|line| line.len() + 1).sum()
}

/// Drop lines from the front of a region until it fits, never below one line.
/// A region is dropped from the front because its newest lines are the ones
/// worth keeping, the same reason the earlier regions go first.
fn fit_front(lines: &[&str], (start, end): (usize, usize), room: usize) -> (usize, usize) {
    let mut start = start;
    let mut bytes = region_bytes(lines, (start, end));
    while start + 1 < end && bytes > room {
        bytes -= lines[start].len() + 1;
        start += 1;
    }
    (start, end)
}

/// Read by the model, so it is written in the language the model is answering
/// in, and it states the count: an unqualified `...` would let a truncated log
/// pass for a complete one.
fn omission(lines: usize) -> String {
    format!("… {} 行省略 …", grouped(lines))
}

fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The budget is a hard byte limit even when a single line overruns it on its
/// own, and a cut lands on a character boundary: half of a multi-byte
/// character would be worse than the omission it saves.
fn clamp(text: String, budget_bytes: usize) -> String {
    if text.len() <= budget_bytes {
        return text;
    }
    const MARK: &str = "…\n";
    if budget_bytes <= MARK.len() {
        return keep_last(&text, budget_bytes).to_owned();
    }
    format!("{MARK}{}", keep_last(&text, budget_bytes - MARK.len()))
}

fn keep_last(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = text.len() - max_bytes;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}

#[cfg(test)]
#[path = "error_region/tests.rs"]
mod tests;
