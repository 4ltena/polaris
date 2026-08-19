//! The part of the always-on context that the harness does not own. The
//! full text of AGENTS.md is never loaded. Because it's truncated at a
//! ceiling, no matter how large AGENTS.md gets, the budget cannot be broken.

use std::path::Path;

use crate::budget::count_tokens;

/// The ceiling on the number of tokens allowed for the constitution block.
pub const CONSTITUTION_LIMIT: usize = 150;

/// The ceiling on the number of tokens allowed for the environment block.
///
/// `environment_block` only carries the cwd and the branch name, but both
/// are strings dictated by the disk / Git — the harness does not control
/// their length. The real-world value in this actual repository is around
/// 24 tokens for cwd + branch combined, but without an explicit ceiling,
/// the promise that "there is no path to exceeding the budget no matter how
/// far the skill count, the size of AGENTS.md, or the length of environment
/// info grows" would break against an abnormally long cwd or a huge branch
/// name. We chose 200 as a value that keeps more than 8x headroom over the
/// measured real-world cwd + branch value (24 tokens) while landing in
/// roughly the same order of magnitude as the constitution ceiling
/// (`CONSTITUTION_LIMIT` = 150), and that leaves the always-on total enough
/// headroom against 990 even when both are simultaneously packed to their
/// ceiling. Saturating both at once, and also passing 100 skills, measures
/// 626 tokens (skills add not a single token) — this is exactly the input
/// `absurdly_long_cwd_cannot_push_the_assembled_system_over_budget` builds
/// (it is the only test that packs all 3 inputs named by acceptance
/// criterion 1 to their maximum simultaneously;
/// `full_always_on_context_stays_within_budget` measures the case where the
/// environment block is at its real-world length). This number moves if the
/// tool count or schema changes — the 525 once written here had gone stale,
/// left over from before the skill tool existed, unrefreshed: exactly what
/// this file's own thesis warns about — a number with no living test behind
/// it rots.
pub const ENVIRONMENT_LIMIT: usize = 200;

const BEGIN: &str = "<!-- polaris:always-on -->";
const END: &str = "<!-- /polaris:always-on -->";

/// Pulls out only the always-on portion from AGENTS.md.
///
/// If it's wrapped in markers, returns what's inside them. If there are no
/// markers, returns the `## Always on` heading's section, up to just before
/// the next `## `. If neither is present, returns an empty string. There is
/// no path that returns the full text.
pub fn extract_always_on(markdown: &str) -> String {
    if let Some(start) = markdown.find(BEGIN) {
        let after = start + BEGIN.len();
        if let Some(rel) = markdown[after..].find(END) {
            return markdown[after..after + rel].trim().to_string();
        }
    }

    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in markdown.lines() {
        if inside {
            if line.starts_with("## ") {
                break;
            }
            out.push(line);
        } else if line.starts_with("## ") && line[3..].trim() == "Always on" {
            inside = true;
        }
    }
    out.join("\n").trim().to_string()
}

/// Truncates line by line when over the ceiling. Never drops everything.
///
/// Made visible for calls from within the crate so that `build_system`
/// stays safe even if it's misconfigured and handed un-truncated
/// constitution text.
pub(crate) fn cap(text: &str, limit: usize) -> String {
    if count_tokens(text) <= limit {
        return text.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let mut trial = kept.clone();
        trial.push(line);
        if count_tokens(&trial.join("\n")) > limit {
            break;
        }
        kept.push(line);
    }
    if kept.is_empty() {
        // If the ceiling is exceeded by the first line alone, binary-search
        // for the largest prefix that fits while staying on a char boundary.
        return cap_by_char_boundary(text.lines().next().unwrap_or_default(), limit);
    }
    kept.join("\n")
}

/// Returns the largest prefix of `text`, starting from the front, whose
/// token count is at most `limit`.
///
/// Trimming one character at a time would call `count_tokens` a number of
/// times proportional to the line's length, which is slow regardless of how
/// cheap any single call to `count_tokens` is (tens of thousands of calls
/// for one 20KB line). Binary-searching over only the character boundaries
/// `char_indices` returns keeps the call count logarithmic in the number of
/// candidates. It never cuts across a multi-byte character. An empty string
/// is always at most `limit`, so `lo` satisfies the loop invariant
/// continuously from its initial value onward.
fn cap_by_char_boundary(text: &str, limit: usize) -> String {
    if count_tokens(text) <= limit {
        return text.to_string();
    }

    let mut boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    boundaries.push(text.len());

    let mut lo = 0usize;
    let mut hi = boundaries.len() - 1;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if count_tokens(&text[..boundaries[mid]]) <= limit {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    text[..boundaries[lo]].to_string()
}

/// Distinguishes between a file not existing and any other read failure
/// (permissions, invalid UTF-8, etc.). "Doesn't exist" is `Ok(None)` — a
/// normal state that may be silently ignored. Anything else is `Err` — the
/// caller is responsible for letting the user know.
fn try_read_block(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(Some(extract_always_on(&body))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Silently returns an empty string when the file doesn't exist. Any other
/// failure (no permission, the encoding isn't UTF-8, etc.) warns to stderr
/// that the rules could not be read, then returns an empty string. Neither
/// case halts startup — a constitution that can't be read isn't fatal, but
/// there's a real difference between silently "acting as if there are no
/// rules" and, after telling the user, "not applying the rules this time
/// because they couldn't be read." Without the latter, the user has no way
/// to notice that their rules have never once taken effect.
fn read_block(path: &Path) -> String {
    match try_read_block(path) {
        Ok(Some(block)) => block,
        Ok(None) => String::new(),
        Err(e) => {
            eprintln!(
                "warning: cannot read {} ({e}). These rules will not be applied this time.",
                path.display()
            );
            String::new()
        }
    }
}

/// Reads the global rules and the project rules in that order, combines
/// them, and truncates to the ceiling. It's not a failure if either one is
/// missing. The global path is taken as an argument so tests can pin the path.
///
/// Because `cap()` keeps a prefix, naively concatenating first and then
/// truncating the whole thing would wipe out the more specific project-side
/// rules (always placed after) entirely whenever the global side alone
/// reaches the ceiling. When both sources are present, we truncate the
/// global side at `CONSTITUTION_LIMIT / 2` first, then hand the remaining
/// budget to the project, so that both survive. When only one is present,
/// that one alone may use the full ceiling.
pub fn load_from(global_agents: Option<&Path>, project_root: &Path) -> String {
    let global = global_agents.map(read_block).unwrap_or_default();
    let project = read_block(&project_root.join("AGENTS.md"));

    match (global.is_empty(), project.is_empty()) {
        (true, true) => String::new(),
        (true, false) => cap(&project, CONSTITUTION_LIMIT),
        (false, true) => cap(&global, CONSTITUTION_LIMIT),
        (false, false) => {
            let global_capped = cap(&global, CONSTITUTION_LIMIT / 2);
            let global_tokens = count_tokens(&global_capped);
            // Also deduct the token cost of the newline used to join them.
            // With a word-boundary-sensitive tokenizer like o200k_base, a
            // newline becomes its own independent chunk, so the measured
            // token count after joining matches the plain sum of the
            // measured counts of each piece.
            let separator_tokens = count_tokens("\n");
            let project_limit = CONSTITUTION_LIMIT
                .saturating_sub(global_tokens)
                .saturating_sub(separator_tokens);
            let project_capped = cap(&project, project_limit);
            format!("{global_capped}\n{project_capped}")
        }
    }
}

/// Resolves `~/.polaris/AGENTS.md` as the global rules and reads it.
pub fn load(project_root: &Path) -> String {
    let global = std::env::var_os("HOME").map(|h| Path::new(&h).join(".polaris").join("AGENTS.md"));
    load_from(global.as_deref(), project_root)
}

/// Environment info. Hands over up front the facts the model would otherwise spend a turn discovering.
pub fn environment_block(cwd: &Path, branch: Option<&str>) -> String {
    let mut s = format!("cwd: {}", cwd.display());
    if let Some(b) = branch {
        s.push_str(&format!("\ngit branch: {b}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const MARKED: &str = "\
# AGENTS

Preamble. Not loaded.

<!-- polaris:always-on -->
Never push directly to main.
<!-- /polaris:always-on -->

## Details
Long procedure. Not loaded either.
";

    const HEADING: &str = "\
# AGENTS

## Always on

Never push directly to main.

## Skill routing

Long procedure. Not loaded.
";

    #[test]
    fn extracts_marked_block_only() {
        let got = extract_always_on(MARKED);
        assert_eq!(got, "Never push directly to main.");
    }

    #[test]
    fn falls_back_to_always_on_heading_and_stops_at_next_section() {
        let got = extract_always_on(HEADING);
        assert_eq!(got, "Never push directly to main.");
        assert!(!got.contains("Skill routing"));
    }

    #[test]
    fn returns_empty_without_marker_or_heading() {
        assert_eq!(extract_always_on("# AGENTS\n\nBody only.\n"), "");
    }

    #[test]
    fn merges_global_then_project_rules() {
        let g = tempfile::tempdir().expect("temp directory");
        let pj = tempfile::tempdir().expect("temp directory");
        std::fs::write(
            g.path().join("AGENTS.md"),
            "## Always on\n\nRespond in Japanese.\n",
        )
        .expect("cannot write");
        std::fs::write(
            pj.path().join("AGENTS.md"),
            "## Always on\n\nNever push directly to main.\n",
        )
        .expect("cannot write");

        let got = load_from(Some(&g.path().join("AGENTS.md")), pj.path());
        assert!(
            got.contains("Respond in Japanese."),
            "the global rules were dropped"
        );
        assert!(
            got.contains("Never push directly to main."),
            "the project rules were dropped"
        );
        let gi = got.find("Respond").expect("global is missing");
        let pi = got.find("Never push").expect("project is missing");
        assert!(gi < pi, "global does not come first");
    }

    #[test]
    fn caps_oversized_constitution() {
        let mut body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            body.push_str(&format!("rule {i}: a long line strung together here.\n"));
        }
        body.push_str("<!-- /polaris:always-on -->\n");

        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("AGENTS.md"), &body).expect("cannot write");

        let got = load(dir.path());
        assert!(
            count_tokens(&got) <= CONSTITUTION_LIMIT,
            "not truncated: {} tokens",
            count_tokens(&got)
        );
        assert!(!got.is_empty(), "must not discard everything");
    }

    #[test]
    fn caps_single_oversized_line_at_a_char_boundary() {
        // A single line made of full-width characters (3 bytes each in
        // UTF-8), with no line break in the middle, so it goes straight into
        // cap()'s binary-search fallback. Truncating naively at the byte
        // level would create a case where the cut crosses a character
        // boundary and produces invalid UTF-8.
        let line = "あ".repeat(3000);
        let got = cap(&line, CONSTITUTION_LIMIT);

        assert!(
            !got.is_empty(),
            "must not return an empty string from non-empty input"
        );
        assert!(
            count_tokens(&got) <= CONSTITUTION_LIMIT,
            "not truncated: {} tokens",
            count_tokens(&got)
        );
        // If the cut crossed a character boundary, at this point it would no
        // longer be a valid prefix of the original line and the character
        // would look broken. This also doubles as confirmation that the cut
        // lands on a valid char boundary.
        assert!(line.starts_with(&got), "not a prefix of the original line");
        assert!(got.chars().all(|c| c == 'あ'), "a character is broken");
    }

    #[test]
    fn both_sources_present_when_global_alone_is_oversized() {
        // Even when the global side alone is huge enough to reach the
        // ceiling on its own, it must not wipe out the more specific
        // project-side rules. Confirm both strings survive and that the
        // total stays within the ceiling.
        let g = tempfile::tempdir().expect("temp directory");
        let pj = tempfile::tempdir().expect("temp directory");

        let mut global_body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            global_body.push_str(&format!(
                "global rule {i}: a long line strung together here.\n"
            ));
        }
        global_body.push_str("<!-- /polaris:always-on -->\n");
        std::fs::write(g.path().join("AGENTS.md"), &global_body).expect("cannot write");

        std::fs::write(
            pj.path().join("AGENTS.md"),
            "## Always on\n\nNever push directly to main.\n",
        )
        .expect("cannot write");

        let got = load_from(Some(&g.path().join("AGENTS.md")), pj.path());
        assert!(
            got.contains("Never push directly to main."),
            "the project rules disappeared: {got}"
        );
        assert!(
            got.contains("global rule 0"),
            "no global rules survived: {got}"
        );
        let n = count_tokens(&got);
        assert!(n <= CONSTITUTION_LIMIT, "exceeds the ceiling: {n}");
    }

    #[test]
    fn single_source_uses_the_full_limit() {
        // When there is no global side, the project side alone may use the
        // full amount of CONSTITUTION_LIMIT. Confirm it isn't reserved down
        // to half by checking that the actual remainder exceeds half the
        // ceiling.
        let mut body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            body.push_str(&format!("rule {i}: a long line strung together here.\n"));
        }
        body.push_str("<!-- /polaris:always-on -->\n");

        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("AGENTS.md"), &body).expect("cannot write");

        let got = load_from(None, dir.path());
        let n = count_tokens(&got);
        assert!(n <= CONSTITUTION_LIMIT, "exceeds the ceiling: {n}");
        assert!(
            n > CONSTITUTION_LIMIT / 2,
            "only half is used despite being a single source: {n}"
        );
    }

    #[test]
    fn try_read_block_returns_none_for_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp directory");
        let missing = dir.path().join("AGENTS.md");
        assert_eq!(
            try_read_block(&missing).expect("must not be an error"),
            None
        );
    }

    #[test]
    fn try_read_block_returns_err_for_invalid_utf8() {
        // An AGENTS.md saved as non-UTF-8 (e.g. Shift-JIS) must not be
        // collapsed into the same case as "doesn't exist". Distinguish it as
        // Err so the caller can know the user's rules could not be read.
        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, [0x82, 0xa0, 0x82, 0xa2]).expect("cannot write"); // part of a Shift-JIS sequence
        let err = try_read_block(&path).expect_err("invalid UTF-8 should be an error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(unix)]
    fn try_read_block_returns_err_for_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "## Always on\n\nshould be unreadable\n").expect("cannot write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("cannot change permissions");

        // Independently confirm this test's premise (that this process is
        // actually running in an environment that enforces permissions)
        // through a raw read that bypasses `try_read_block`. Deferring that
        // judgment to this ground truth via `result` matters: if
        // `try_read_block` regressed into always returning `Ok`, that would
        // otherwise get papered over as "must have been running as root",
        // and the test would pass regardless of whether the restriction was
        // actually enforced.
        let ground_truth = std::fs::read_to_string(&path);
        let result = try_read_block(&path);

        // Cleanup: restore permissions so tempdir's Drop can delete it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("cannot restore permissions");

        match ground_truth {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let err = result.expect_err("should be an error since there's no permission");
                assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            }
            // In an environment where permissions aren't enforced (e.g.
            // running as root), this test's premise doesn't hold, so skip
            // the judgment.
            _ => eprintln!(
                "file permissions are not enforced in this environment (running as root?). Skipping the judgment."
            ),
        }
    }

    #[test]
    fn returns_empty_when_neither_file_exists() {
        let dir = tempfile::tempdir().expect("temp directory");
        assert_eq!(load_from(None, dir.path()), "");
    }

    #[test]
    fn environment_block_carries_cwd_and_branch() {
        let got = environment_block(Path::new("/w/polaris"), Some("feat/x"));
        assert!(got.contains("/w/polaris"));
        assert!(got.contains("feat/x"));
    }

    /// A set of skills fed to the budget tests. Their name, description, and
    /// body are all given a length that would be visible if it leaked into
    /// the always-on context. They don't go through disk, because what we
    /// want to see here is not "can it be discovered" but "does it get
    /// piled in".
    fn probe_skills(n: usize) -> Vec<polaris_skills::Skill> {
        (0..n)
            .map(|i| polaris_skills::Skill {
                name: format!("budget-probe-{i:03}"),
                description: format!(
                    "marker {i:03} for checking it hasn't leaked into the always-on context."
                ),
                body: format!("body {i}"),
                path: format!("/x/budget-probe-{i:03}/SKILL.md").into(),
            })
            .collect()
    }

    #[test]
    fn full_always_on_context_stays_within_budget() {
        let constitution = "a".repeat(2000);
        let capped = cap(&constitution, CONSTITUTION_LIMIT);
        let env = environment_block(Path::new("/w/polaris"), Some("feat/m1-headless-loop"));
        let always_on = crate::prompt::assemble_always_on(&capped, &env, &probe_skills(100));

        let n = always_on.tokens();
        assert!(
            n <= crate::budget::BUDGET_LIMIT,
            "the always-on context including the constitution and environment is {n} tokens, exceeding the ceiling"
        );
    }

    #[test]
    fn absurdly_long_cwd_cannot_push_the_assembled_system_over_budget() {
        // Reproduces the review's repro condition (filling the constitution
        // to its ceiling, then adding an absurdly long cwd, exceeds 990)
        // using a deeply nested working directory. A naive repetition of a
        // single identical character compresses down to very few tokens
        // under BPE and never exceeds the ceiling, so this uses the shape of
        // "many directory levels" close to a real path instead. This length
        // (7,690 bytes) far exceeds macOS's `PATH_MAX` and could never occur
        // in reality, but `build_system`'s promise must not be "it only
        // breaks on inputs that could realistically occur" — unless the
        // environment block goes through `cap()`, this input alone pushed
        // the assertion's target sum past 990 (measured at 1,538 tokens
        // uncapped).
        //
        // This is the only test that simultaneously piles all 3 inputs named
        // by acceptance criterion 1 (skill count, AGENTS.md size, and length
        // of environment info) up to their maximum at once, so it also hands
        // over 100 skills. This is the true simultaneous worst case, and the
        // ceiling number we report comes from this test.
        let huge_cwd: String = (0..600).map(|i| format!("/component{i}")).collect();
        let oversized_constitution = "a".repeat(2000);
        let capped_constitution = cap(&oversized_constitution, CONSTITUTION_LIMIT);
        let env = environment_block(Path::new(&huge_cwd), Some("feat/m1-headless-loop"));
        let always_on =
            crate::prompt::assemble_always_on(&capped_constitution, &env, &probe_skills(100));

        let n = always_on.tokens();
        assert!(
            n <= crate::budget::BUDGET_LIMIT,
            "the always-on context is {n} tokens even including a huge cwd, exceeding the ceiling"
        );
    }
}
