//! skill tool. Returns the body on an exact name match, otherwise returns a
//! list of candidates.

use polaris_skills::Skill;

mod bm25;
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

/// Byte cap on the description preview shown per candidate in a search
/// result (see `description_preview`). Chosen from measurement against a
/// real 831-skill corpus and its evaluation queries: a first-sentence
/// preview capped at this length still carries the query-matching term
/// that caused BM25 to rank the item in 97.6% of measured hit cases, and
/// still carries a near-universal skill's full trigger clause in 98.8% of
/// cases, while cutting the corpus's total candidate-list render cost by
/// roughly half (43,777 -> 20,389 tokens on the full 831-item corpus,
/// o200k_base). Not part of the Agent Skills spec — a rendering choice
/// local to this file, independent of `MAX_BODY_BYTES` and `MAX_LIST_BYTES`.
const MAX_PREVIEW_BYTES: usize = 200;

/// Renders a candidate-list preview of a skill's description: the first
/// sentence, capped at `MAX_PREVIEW_BYTES`.
///
/// Full descriptions are the dominant cost of a search result (a
/// `MAX_RESULTS`-sized candidate list of full descriptions ran roughly
/// 1,000+ tokens against a real corpus) — most of that text describes
/// detail beyond what's needed to recognize whether a candidate is worth
/// reading further. Skill descriptions in this ecosystem are
/// conventionally written with the selecting information (what the skill
/// does, or the "use when X" trigger condition) front-loaded into the
/// first sentence, so cutting there measures as safe rather than assumed
/// safe (see `MAX_PREVIEW_BYTES`'s doc comment for the measurement).
///
/// This only affects the search-result preview. An exact name match still
/// returns the full, untruncated body via a completely separate path.
fn description_preview(description: &str) -> String {
    let trimmed = description.trim();
    let sentence_end = trimmed
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && trimmed[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(i, c)| i + c.len_utf8());
    let sentence = match sentence_end {
        Some(end) => trimmed[..end].trim(),
        None => trimmed,
    };
    let (capped, truncated) = cap_bytes(sentence, MAX_PREVIEW_BYTES);
    if !truncated {
        return capped.to_string();
    }
    // Back off to the last word boundary so the cut doesn't land
    // mid-word. Text with no space in the capped range (a single very
    // long token, or a script like Japanese that doesn't delimit words
    // with spaces) has no boundary to back off to, so the byte-safe cut
    // from `cap_bytes` is used as-is.
    let word_boundary = capped.rfind(' ').unwrap_or(capped.len());
    format!("{}…", &capped[..word_boundary])
}

/// Formats the candidate list up to `max_count` entries and
/// `MAX_LIST_BYTES` bytes. If either one causes a cutoff, state explicitly
/// that it was cut off — silently returning only part of the list would let
/// the model mistake it for the full set.
///
/// Each entry's description is rendered as `description_preview`'s short
/// preview, not the full text — see that function's doc comment. This
/// means `MAX_LIST_BYTES` is no longer primarily a guard against
/// description bloat (a preview is bounded by `MAX_PREVIEW_BYTES`
/// regardless of the source description's length); it now mainly backstops
/// unbounded skill names and a large `max_count`.
fn list_candidates(items: &[&Skill], header: &str, max_count: usize) -> String {
    let mut out = String::from(header);
    let mut shown = 0usize;
    for s in items.iter().take(max_count) {
        let line = format!("- {}: {}\n", s.name, description_preview(&s.description));
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
/// A search ranks by BM25 over skill names and descriptions, with stemming
/// and synonym expansion applied to both the query and the corpus. Because
/// BM25's tokenizer only recognizes ASCII tokens, a query that scores every
/// skill at 0 (Japanese and other non-Latin queries, or one sharing no
/// vocabulary with any skill) falls back to the old substring-containment
/// match instead, so CJK and short substring/prefix queries stay reachable.
/// Whatever the search finds is always joined with the small set of
/// "near-universal" skills, which are included regardless of query
/// relevance.
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
            MAX_RESULTS,
        );
    }

    let index = bm25::Bm25::new(skills);
    let mut ranked = index.rank(q, MAX_RESULTS);

    // BM25's tokenizer only extracts ASCII [a-z0-9]+ tokens, so a query
    // written in Japanese (or any non-Latin script), or one with no token
    // overlapping any skill's vocabulary, scores every skill at 0. The
    // previous substring-containment search had no such limitation.
    // Falling back to it here when BM25 finds nothing keeps
    // Japanese-described skills and short substring/prefix queries
    // reachable, matching the spec's requirement not to regress CJK
    // search behavior.
    if ranked.is_empty() {
        let needle = q.to_lowercase();
        ranked = skills
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&needle)
                    || s.description.to_lowercase().contains(&needle)
            })
            .take(MAX_RESULTS)
            .collect();
    }

    // Whether the real content search found anything, independent of
    // near_universal -- drives which message header is shown below, so a
    // query that matched nothing doesn't read as if it succeeded just
    // because a near-universal skill is always present.
    let content_found = !ranked.is_empty();

    // near_universal items go first: `list_candidates`'s byte cap
    // truncates in slice order, so appending them after `ranked` would
    // let a corpus of large (e.g. Japanese) descriptions push them past
    // the cap and silently defeat the "always included" guarantee they
    // exist for. Putting them first means the content search results are
    // what get trimmed under byte pressure, not the always-included set.
    // Dedup relies on `polaris-skills`'s discovery guaranteeing unique
    // names within a single loaded skill set (first name wins on a
    // collision) -- `lookup` itself does not re-enforce that here.
    let mut combined: Vec<&Skill> = near_universal::near_universal(skills);
    for r in ranked.iter() {
        if !combined.iter().any(|s| s.name == r.name) {
            combined.push(r);
        }
    }

    if combined.is_empty() {
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
        return list_candidates(&all, &header, MAX_RESULTS);
    }

    let header = if content_found {
        "candidates. pass the name as-is if you need the body.\n"
    } else {
        "no direct match for the search; showing skills that apply almost always instead.\n"
    };
    list_candidates(
        &combined,
        header,
        MAX_RESULTS + near_universal::MAX_NEAR_UNIVERSAL,
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
    fn description_preview_stops_at_the_first_sentence() {
        let preview = description_preview(
            "Use when implementing any feature. This second sentence should not appear.",
        );
        assert_eq!(preview, "Use when implementing any feature.");
    }

    #[test]
    fn description_preview_returns_the_whole_text_when_it_has_no_sentence_end() {
        let preview = description_preview("A short description with no terminal punctuation");
        assert_eq!(preview, "A short description with no terminal punctuation");
    }

    #[test]
    fn description_preview_caps_a_long_first_sentence_at_a_word_boundary() {
        let long_sentence = format!("Use when {}.", "word ".repeat(60).trim());
        assert!(long_sentence.len() > MAX_PREVIEW_BYTES);
        let preview = description_preview(&long_sentence);
        assert!(
            preview.len() <= MAX_PREVIEW_BYTES + "…".len(),
            "preview exceeds the cap: {} bytes",
            preview.len()
        );
        assert!(
            preview.ends_with('…'),
            "truncation was not marked: {preview}"
        );
        assert!(
            !preview.contains("  "),
            "cut mid-word instead of at a word boundary: {preview}"
        );
    }

    #[test]
    fn description_preview_does_not_panic_on_multibyte_utf8_at_the_cap() {
        // "あ" is 3 bytes in UTF-8. A run long enough to cross
        // MAX_PREVIEW_BYTES with no ASCII space anywhere exercises the
        // word-boundary backoff's fallback (no space found) together with
        // `cap_bytes`'s char-boundary safety, on a script that doesn't
        // delimit words with spaces at all.
        let long_japanese = format!("使用時{}。", "あ".repeat(200));
        let preview = description_preview(&long_japanese);
        assert!(preview.len() <= MAX_PREVIEW_BYTES + "…".len());
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
        // `combined.is_empty()` path and produce output without this
        // wording, which is what lets this difference be detected.
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
        // Descriptions are rendered as a short preview now (see
        // `description_preview`), so a single entry's byte footprint is
        // bounded by `MAX_PREVIEW_BYTES` regardless of the source
        // description's length — an oversized description can no longer
        // be the thing that exceeds `MAX_LIST_BYTES`. A skill name has no
        // such cap, so this test drives the byte cap through an oversized
        // name instead, to keep exercising the same property: a cap that
        // only looked at count would let this slip through.
        let big_name_prefix = "fat-".to_string() + &"x".repeat(1024);
        let skills: Vec<Skill> = (0..MAX_RESULTS)
            .map(|i| Skill {
                name: format!("{big_name_prefix}-{i:02}"),
                description: "A description for testing.".into(),
                body: "body".into(),
                path: format!("/x/fat-{i:02}/SKILL.md").into(),
            })
            .collect();
        let one_entry = format!("- {big_name_prefix}-00: A description for testing.\n").len();

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
        // emit the first entry. Drives the cap through an oversized name,
        // not description, for the same reason as the test above.
        let skills = vec![Skill {
            name: format!("huge-{}", "x".repeat(MAX_LIST_BYTES)),
            description: "A description for testing.".into(),
            body: "body".into(),
            path: "/x/huge/SKILL.md".into(),
        }];

        let out = lookup(&skills, "");
        assert!(out.contains("- huge-"), "not even one candidate was shown");
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

    #[test]
    fn a_multi_word_query_finds_a_skill_the_old_substring_match_never_could() {
        let skills = vec![
            Skill {
                name: "git-commit".into(),
                description: "Creates a commit. Used for commit or git topics.".into(),
                body: "Body A".into(),
                path: "/x/git-commit/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "Body B".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        let out = lookup(&skills, "how do I deploy this to production");
        assert!(
            out.contains("deploy-tool"),
            "multi-word query did not find deploy-tool: {out}"
        );
    }

    #[test]
    fn a_synonym_query_finds_a_skill_that_never_uses_the_query_word() {
        let skills = vec![Skill {
            name: "secret-scanner".into(),
            description: "Scans for leaked secrets before commit.".into(),
            body: "body".into(),
            path: "/x/secret-scanner/SKILL.md".into(),
        }];
        let out = lookup(&skills, "credential rotation policy");
        assert!(
            out.contains("secret-scanner"),
            "synonym expansion did not surface secret-scanner: {out}"
        );
    }

    #[test]
    fn a_stemmed_query_finds_a_skill_using_a_different_word_form() {
        let skills = vec![Skill {
            name: "deploy-tool".into(),
            description: "Handles deployment to production servers.".into(),
            body: "body".into(),
            path: "/x/deploy-tool/SKILL.md".into(),
        }];
        let out = lookup(&skills, "deploying to prod");
        assert!(out.contains("deploy-tool"));
    }

    #[test]
    fn a_near_universal_skill_is_always_included_regardless_of_query_relevance() {
        let skills = vec![
            Skill {
                name: "test-driven-development".into(),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "body".into(),
                path: "/x/test-driven-development/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "body".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        // "production servers" matches deploy-tool by BM25, and has no
        // lexical relationship to test-driven-development at all -- the
        // near-universal skill must still show up.
        let out = lookup(&skills, "production servers");
        assert!(out.contains("deploy-tool"));
        assert!(
            out.contains("test-driven-development"),
            "near-universal skill was not included despite zero query relevance: {out}"
        );
    }

    #[test]
    fn a_near_universal_skill_does_not_appear_on_an_exact_name_match() {
        let skills = vec![
            Skill {
                name: "test-driven-development".into(),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "TDD body".into(),
                path: "/x/test-driven-development/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "deploy body".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        let out = lookup(&skills, "deploy-tool");
        assert!(out.contains("deploy body"));
        assert!(
            !out.contains("test-driven-development"),
            "near-universal skill leaked into an exact-name-match result: {out}"
        );
    }

    #[test]
    fn a_near_universal_skill_does_not_appear_on_an_empty_query_beyond_its_natural_listing() {
        // An empty query already lists everything, so a near-universal
        // skill appears there too -- but it must not be duplicated or
        // specially annotated, just present once like any other skill.
        let skills = vec![Skill {
            name: "test-driven-development".into(),
            description:
                "Use when implementing any feature or bugfix, before writing implementation code"
                    .into(),
            body: "body".into(),
            path: "/x/test-driven-development/SKILL.md".into(),
        }];
        let out = lookup(&skills, "");
        let occurrences = out.matches("test-driven-development").count();
        assert_eq!(occurrences, 1, "listed more than once: {out}");
    }

    #[test]
    fn a_corpus_with_no_near_universal_skill_behaves_exactly_as_before() {
        let skills = vec![Skill {
            name: "deploy-tool".into(),
            description: "Handles deployment to production servers.".into(),
            body: "body".into(),
            path: "/x/deploy-tool/SKILL.md".into(),
        }];
        let out = lookup(&skills, "deploying to prod");
        assert!(out.contains("deploy-tool"));
        assert!(out.contains("candidates. pass the name as-is if you need the body."));
        assert_eq!(out.matches("deploy-tool").count(), 1);
    }

    #[test]
    fn a_search_result_shows_only_the_first_sentence_of_a_matched_description() {
        let skills = vec![Skill {
            name: "deploy-tool".into(),
            description: "Handles deployment to production servers. Covers rollback, health checks, and canary releases in extensive detail that a candidate list should never have to carry in full.".into(),
            body: "body".into(),
            path: "/x/deploy-tool/SKILL.md".into(),
        }];
        let out = lookup(&skills, "deploying to prod");
        assert!(
            out.contains("Handles deployment to production servers."),
            "first sentence missing from the search result: {out}"
        );
        assert!(
            !out.contains("canary releases"),
            "full description leaked into the search result instead of a preview: {out}"
        );
    }

    #[test]
    fn near_universal_survives_when_content_search_alone_would_fill_the_count_cap() {
        let mut skills: Vec<Skill> = (0..(MAX_RESULTS + 10))
            .map(|i| Skill {
                name: format!("deploy-tool-{i:02}"),
                description: "Handles deployment to production servers.".into(),
                body: "body".into(),
                path: format!("/x/deploy-tool-{i:02}/SKILL.md").into(),
            })
            .collect();
        skills.push(Skill {
            name: "test-driven-development".into(),
            description:
                "Use when implementing any feature or bugfix, before writing implementation code"
                    .into(),
            body: "body".into(),
            path: "/x/test-driven-development/SKILL.md".into(),
        });
        let out = lookup(&skills, "deploying to production servers");
        assert!(
            out.contains("test-driven-development"),
            "near-universal skill was truncated away despite the count cap: {out}"
        );
        let deploy_count = (0..(MAX_RESULTS + 10))
            .filter(|i| out.contains(&format!("deploy-tool-{i:02}")))
            .count();
        assert_eq!(
            deploy_count, MAX_RESULTS,
            "expected exactly MAX_RESULTS content-search hits, got {deploy_count}: {out}"
        );
    }

    #[test]
    fn a_skill_that_is_both_near_universal_and_a_strong_content_match_appears_once() {
        let skills = vec![Skill {
            name: "test-driven-development".into(),
            description:
                "Use when implementing any feature or bugfix, before writing implementation code"
                    .into(),
            body: "body".into(),
            path: "/x/test-driven-development/SKILL.md".into(),
        }];
        let out = lookup(&skills, "implementing a feature or bugfix");
        assert_eq!(out.matches("test-driven-development").count(), 1);
    }

    #[test]
    fn a_query_matching_nothing_but_near_universal_present_gets_a_distinct_header() {
        let skills = vec![
            Skill {
                name: "test-driven-development".into(),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "body".into(),
                path: "/x/test-driven-development/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "body".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        let out = lookup(&skills, "a completely unrelated term");
        assert!(out.contains("no direct match"));
        assert!(out.contains("test-driven-development"));
        assert!(!out.contains("deploy-tool"));
    }

    #[test]
    fn a_japanese_query_still_finds_a_skill_via_the_substring_fallback() {
        let skills = vec![Skill {
            name: "profile-generator".into(),
            description: "日本語の紹介文を生成する。".into(),
            body: "body".into(),
            path: "/x/profile-generator/SKILL.md".into(),
        }];
        let out = lookup(&skills, "紹介文");
        assert!(out.contains("profile-generator"));
    }
}
