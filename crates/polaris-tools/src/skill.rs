//! skill tool. Returns the body on an exact name match, otherwise returns a
//! list of candidates.

use polaris_skills::Skill;

// Not yet called from `lookup` -- a later task wires ranking in. Its own
// unit tests are the only caller today, and `cargo clippy --all-targets`
// still compiles the plain `lib` target (without `cfg(test)`) where none of
// that applies, so without this the module reads as entirely dead code.
#[allow(dead_code)]
mod bm25;

// Not yet called from `lookup` -- a later task wires selection in. Its own
// unit tests are the only caller today, and `cargo clippy --all-targets`
// still compiles the plain `lib` target (without `cfg(test)`) where none of
// that applies, so without this the module reads as entirely dead code.
#[allow(dead_code)]
mod near_universal;

pub use near_universal::{MAX_NEAR_UNIVERSAL, near_universal};

/// Byte cap applied when returning a SKILL.md body. The Agent Skills spec
/// recommends keeping the body under roughly 5,000 tokens and pushing
/// detail out to reference files. There's no tokenizer here, so this
/// approximates using byte count instead. For a body that mixes in
/// Japanese, each token tends to run roughly 2 to 4 bytes, so this treats
/// 5,000 tokens not as a strict floor but as a "roughly this scale"
/// guideline, and sets the cap at 32 KiB with some margin — clearly looser
/// than the spec's recommendation, but enough to stop a runaway body from
/// pushing conversation cost up without bound. This mirrors the role
/// `MAX_READ_BYTES` plays for `read`.
pub const MAX_BODY_BYTES: usize = 32 * 1024;

/// Cap on the number of candidates returned at once as a search result.
/// Because the conversation history is resent in full on every turn, an
/// unbounded count would make the per-turn cost grow without limit as
/// skills accumulate — the same kind of cost a 990-token budget prevents
/// elsewhere in the system would slip straight through this path. 20 was
/// chosen as a guideline that leaves a scale that can still be surveyed at
/// a glance, while making the truncation explicit and nudging toward
/// narrowing the query when it kicks in.
const MAX_RESULTS: usize = 20;

/// Cap on the total byte size allowed for the whole candidate-list output.
///
/// `MAX_RESULTS` only bounds the count. The spec allows `description` up to
/// 1,024 characters, and for Japanese that can reach 3 KB for a single
/// entry, so 20 of them together come to about 61 KB — twice the
/// `MAX_BODY_BYTES` (32 KiB) that this same file imposes on a single
/// body's content, let through on flimsier grounds. A candidate list is a
/// "table of contents for choosing what to read", not something meant to
/// be read in full, so the table of contents should never exceed the cap
/// on the body itself. Set the cap at 8 KiB, a quarter of the body cap, and
/// when it's exceeded, state that it was truncated using the same wording
/// as the count cap.
const MAX_LIST_BYTES: usize = 8 * 1024;

/// Cap on the query echoed back into the message when nothing matches.
///
/// The query is an arbitrary-length string written by the model, and this
/// wording stays in the conversation history as the tool result, resent
/// every subsequent turn. This was the one input in this file with no cap.
/// Knowing what was searched for is enough, so it can be kept short.
const MAX_ECHOED_QUERY_BYTES: usize = 120;

/// Truncates `text` to at most `limit` bytes. Backs off the cut point so it
/// never crosses a UTF-8 character boundary. Returns whether truncation
/// actually happened, as a bool.
fn cap_bytes(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// Formats the candidate list up to `MAX_RESULTS` entries and
/// `MAX_LIST_BYTES` bytes. If either one causes a cutoff, state explicitly
/// that it was cut off — silently returning only part of the list would let
/// the model mistake it for the full set.
fn list_candidates(items: &[&Skill], header: &str) -> String {
    let mut out = String::from(header);
    let mut shown = 0usize;
    for s in items.iter().take(MAX_RESULTS) {
        let line = format!("- {}: {}\n", s.name, s.description);
        // Always emit the first entry even if it exceeds the cap.
        // Returning "truncated" without showing even one entry leaves the
        // model with no next move at all. So the output fits within, at
        // most, `MAX_LIST_BYTES` + header + one candidate's worth.
        if shown > 0 && out.len() + line.len() > MAX_LIST_BYTES {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < items.len() {
        out.push_str(&format!(
            "(showing {shown} of {} total; narrow the query or pass a name directly.)\n",
            items.len()
        ));
    }
    out
}

/// Looks up the given term as a skill name; if it doesn't match, searches
/// names and descriptions instead.
///
/// The reason a search doesn't return the body is progressive disclosure:
/// it lets a candidate be seen before deciding whether to read it. If every
/// body were returned, searching would be pointless.
pub fn lookup(skills: &[Skill], q: &str) -> String {
    if skills.is_empty() {
        return "no skill was found at all. there is no SKILL.md at the search location."
            .to_string();
    }

    // Strip leading/trailing whitespace exactly once, and use that same
    // value for every judgment that follows: the empty check, the
    // exact-match check, and the partial-match check. Using this trimmed
    // value in one place and an untrimmed `q` elsewhere would let an
    // exact-match query with surrounding whitespace slip past the match
    // check.
    let q = q.trim();

    if let Some(s) = skills.iter().find(|s| s.name == q) {
        let (body, truncated) = cap_bytes(&s.body, MAX_BODY_BYTES);
        let mut out = format!("# {}\n\n{}\n", s.name, body);
        if truncated {
            out.push_str(&format!(
                "\n(the body was truncated at {MAX_BODY_BYTES} bytes. read {} directly for the full text.)\n",
                s.path.display()
            ));
        }
        return out;
    }

    // Treat an empty or whitespace-only q as a query that deliberately
    // narrows nothing down. Rust's `str::contains` always returns true
    // against an empty needle, so letting it pass straight through to the
    // search below would effectively become "return every skill". That's
    // itself a reasonable reading of "show me what's available", so rather
    // than silently streaming everything back, state that this
    // interpretation was made and apply the same count cap as any other
    // result.
    if q.is_empty() {
        let all: Vec<&Skill> = skills.iter().collect();
        return list_candidates(
            &all,
            "q is empty, so listing the skills that exist. pass a name or term to narrow it down.\n",
        );
    }

    let needle = q.to_lowercase();
    let hits: Vec<&Skill> = skills
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&needle)
                || s.description.to_lowercase().contains(&needle)
        })
        .collect();

    if hits.is_empty() {
        let all: Vec<&Skill> = skills.iter().collect();
        // q is an arbitrary-length string written by the model. Echoing it
        // back as-is would turn an uncapped input into uncapped output, and
        // it stays in history to be resent every turn. Cut it to a length
        // that still conveys what was searched for, and say that it was
        // cut.
        let (echoed, truncated) = cap_bytes(q, MAX_ECHOED_QUERY_BYTES);
        let header = if truncated {
            format!(
                "{echoed}… (query is long, showing only the first {MAX_ECHOED_QUERY_BYTES} bytes) matched no skill. what's available is listed below.\n"
            )
        } else {
            format!("{echoed} matched no skill. what's available is listed below.\n")
        };
        return list_candidates(&all, &header);
    }

    list_candidates(
        &hits,
        "candidates. pass the name as-is if you need the body.\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<polaris_skills::Skill> {
        vec![
            polaris_skills::Skill {
                name: "git-commit".into(),
                description: "Creates a commit. Used for commit or git topics.".into(),
                body: "Body A".into(),
                path: "/x/git-commit/SKILL.md".into(),
            },
            polaris_skills::Skill {
                name: "writing-style".into(),
                description: "Polishes Japanese prose.".into(),
                body: "Body B".into(),
                path: "/x/writing-style/SKILL.md".into(),
            },
        ]
    }

    #[test]
    fn an_exact_name_returns_the_body() {
        let out = lookup(&fixtures(), "git-commit");
        assert!(out.contains("Body A"), "the body was not returned: {out}");
        assert!(!out.contains("Body B"));
    }

    #[test]
    fn a_query_returns_names_and_descriptions_not_bodies() {
        let out = lookup(&fixtures(), "commit");
        assert!(out.contains("git-commit"));
        assert!(
            !out.contains("Body A"),
            "a search must not also return the body: {out}"
        );
    }

    #[test]
    fn a_query_matching_nothing_says_so_and_lists_what_exists() {
        let out = lookup(&fixtures(), "a completely unrelated term");
        assert!(out.contains("git-commit") && out.contains("writing-style"));
    }

    #[test]
    fn an_empty_skill_set_is_distinguishable_from_a_query_matching_nothing() {
        // Looking only at whether both return a non-empty string wouldn't
        // distinguish them. Pin down that the distinct wording for "not a
        // single skill exists" never shows up in output from a search that
        // simply found no match. If the early return for `skills.is_empty()`
        // were removed, a query against an empty set would fall into the
        // `hits.is_empty()` path and produce output without this wording,
        // which is what lets this difference be detected.
        let empty_set = lookup(&[], "something");
        assert!(
            empty_set.contains("no skill was found at all"),
            "missing the wording that says not a single skill exists: {empty_set}"
        );

        let no_match = lookup(&fixtures(), "a completely unrelated term");
        assert!(
            !no_match.contains("no skill was found at all"),
            "says \"not a single one exists\" when this was merely zero hits: {no_match}"
        );
    }

    #[test]
    fn an_empty_query_lists_what_exists_instead_of_matching_everything_silently() {
        // Rust's `"anything".contains("")` is always true, so letting an
        // empty string pass straight through to the search below would
        // make everything "hit". Returning that as a candidate list is
        // reasonable on its own, but pin down that it doesn't include the
        // bodies, and that it's discernible that this interpretation was
        // made.
        //
        // "names are included but bodies aren't" alone wouldn't distinguish
        // this from removing the deliberate branch and letting it fall
        // through to the search (since needle is an empty string,
        // everything lands in hits and gets formatted by the same
        // list_candidates, so the two are indistinguishable). Pin down the
        // wording unique to the deliberate branch itself, so that branch
        // actually running can be detected.
        let out = lookup(&fixtures(), "");
        assert!(out.contains("git-commit") && out.contains("writing-style"));
        assert!(!out.contains("Body A") && !out.contains("Body B"));
        assert!(
            out.contains("is empty, so listing"),
            "missing wording indicating the empty string was treated as a deliberate query: {out}"
        );
    }

    #[test]
    fn a_padded_query_finds_the_same_skill_as_the_unpadded_query() {
        // The empty check was done with q.trim(), but the exact-match check
        // right before it used the untrimmed q as-is. An exact-match query
        // padded with whitespace would fail the match check, also miss the
        // empty check, and fall straight through into the search, turning
        // into a "no match" candidate list. Pin down that trim happens
        // exactly once and the same value is used for both the empty check
        // and the match check.
        let unpadded = lookup(&fixtures(), "git-commit");
        let padded = lookup(&fixtures(), " git-commit ");
        assert_eq!(
            padded, unpadded,
            "matching without trimming leading/trailing whitespace: {padded}"
        );
    }

    #[test]
    fn a_whitespace_only_query_is_treated_the_same_as_empty() {
        // A whitespace-only query would still produce a list even by
        // falling through to the "no candidates" path (hits is empty), so
        // for the same reason as above, pin down the distinctive wording
        // too.
        let out = lookup(&fixtures(), "   ");
        assert!(out.contains("git-commit") && out.contains("writing-style"));
        assert!(
            out.contains("is empty, so listing"),
            "missing wording indicating whitespace-only was treated as a deliberate query: {out}"
        );
    }

    #[test]
    fn search_results_are_capped_and_say_so_when_truncated() {
        let skills: Vec<Skill> = (0..(MAX_RESULTS + 10))
            .map(|i| Skill {
                name: format!("skill-{i:02}"),
                description: "A description for testing.".into(),
                body: "body".into(),
                path: format!("/x/skill-{i:02}/SKILL.md").into(),
            })
            .collect();
        let total = skills.len();

        let out = lookup(&skills, "");
        let shown = out.lines().filter(|l| l.starts_with("- skill-")).count();
        assert_eq!(
            shown, MAX_RESULTS,
            "should show only the cap of {MAX_RESULTS} entries: {shown} shown"
        );
        assert!(
            out.contains(&total.to_string()),
            "missing wording indicating only some of the {total} total were shown: {out}"
        );
    }

    #[test]
    fn search_results_are_capped_by_bytes_not_only_by_count() {
        // The spec allows `description` up to 1,024 characters. For
        // Japanese that comes to about 3 KB per entry, and 20 of them
        // (`MAX_RESULTS`) together come to about 61 KB — twice the 32 KiB
        // that this same file imposes on a single body, slipping through
        // the gap left by a cap that only looks at count.
        let description = "€".repeat(1024);
        let skills: Vec<Skill> = (0..MAX_RESULTS)
            .map(|i| Skill {
                name: format!("fat-{i:02}"),
                description: description.clone(),
                body: "body".into(),
                path: format!("/x/fat-{i:02}/SKILL.md").into(),
            })
            .collect();
        let one_entry = format!("- fat-00: {description}\n").len();

        let out = lookup(&skills, "");
        let shown = out.lines().filter(|l| l.starts_with("- fat-")).count();

        assert!(
            shown < skills.len(),
            "the byte cap has no effect: displayed all {shown} entries"
        );
        assert!(
            out.len() <= MAX_LIST_BYTES + one_entry + 256,
            "candidate list exceeds the cap: {} bytes",
            out.len()
        );
        assert!(
            out.len() < MAX_BODY_BYTES,
            "candidate list is bigger than the cap allowed for a single body: {} bytes",
            out.len()
        );
        assert!(
            out.contains("narrow the query or pass a name directly"),
            "missing wording indicating only some were shown: {out}"
        );
    }

    #[test]
    fn a_single_candidate_over_the_byte_cap_is_still_returned() {
        // Returning "truncated" for cap reasons without showing even a
        // single entry leaves the model with no next move at all. Always
        // emit the first entry.
        let skills = vec![Skill {
            name: "huge".into(),
            description: "€".repeat(MAX_LIST_BYTES),
            body: "body".into(),
            path: "/x/huge/SKILL.md".into(),
        }];

        let out = lookup(&skills, "");
        assert!(out.contains("- huge:"), "not even one candidate was shown");
        assert!(
            !out.contains("narrow the query or pass a name directly"),
            "says it was truncated even though everything was shown: {}",
            &out[..out.len().min(200)]
        );
    }

    #[test]
    fn a_no_match_message_does_not_echo_the_query_back_unbounded() {
        // q is an arbitrary-length string written by the model, and this
        // wording stays in history to be resent every subsequent turn.
        // This was the one input in this file with no cap.
        let q = "a term that won't be found".repeat(2000);
        let out = lookup(&fixtures(), &q);

        assert!(
            out.len() < 1024,
            "echoing the query straight back: output {} bytes, query {} bytes",
            out.len(),
            q.len()
        );
        assert!(
            out.contains("query is long, showing only the first"),
            "missing wording indicating the query was truncated: {out}"
        );
        assert!(
            out.contains("git-commit") && out.contains("writing-style"),
            "candidate list is not shown: {out}"
        );
    }

    #[test]
    fn an_oversized_body_is_truncated_and_says_so() {
        let big_body = "€".repeat(MAX_BODY_BYTES);
        let skills = vec![Skill {
            name: "big".into(),
            description: "a huge skill".into(),
            body: big_body.clone(),
            path: "/x/big/SKILL.md".into(),
        }];

        let out = lookup(&skills, "big");
        assert!(
            out.len() < big_body.len(),
            "the body was not truncated: output {} bytes, body {} bytes",
            out.len(),
            big_body.len()
        );
        assert!(
            out.contains("truncated"),
            "missing wording indicating truncation: {}",
            &out[out.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn a_body_within_the_cap_is_returned_whole_and_unmarked() {
        let body = "body".repeat(10);
        let skills = vec![Skill {
            name: "small".into(),
            description: "a small skill".into(),
            body: body.clone(),
            path: "/x/small/SKILL.md".into(),
        }];

        let out = lookup(&skills, "small");
        assert!(out.contains(&body));
        assert!(!out.contains("truncated"));
    }
}
