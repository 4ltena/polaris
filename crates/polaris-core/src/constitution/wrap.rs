//! A compressed copy of an AGENTS.md always-on section, used in its place.
//!
//! The copy is made by a model, once, when the section changes; nothing here
//! calls one. A request only compares a hash. The copy is used only when it
//! was made from the section as it is now (the hash in its header matches) and
//! it passes `check`; otherwise the section is used as written, so a stale,
//! damaged or unwanted copy costs a request nothing.
//!
//! File format. The first line names the source, and every later line is one
//! compressed rule preceded by the source rules (1-based, non-empty lines of
//! the section) it stands for:
//!
//! ```text
//! <!-- polaris:wrap source=<sha256 of the section> model=<name> -->
//! [1,2] compressed text
//! [3] compressed text
//! ```
//!
//! The numbers are the correspondence list: they are checked and then dropped,
//! so they cost the request no tokens.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::CONSTITUTION_LIMIT;
use crate::budget::count_tokens;

pub const FILE_NAME: &str = "agentsmd-wrap.md";
const HEADER: &str = "<!-- polaris:wrap source=";
const MAX_BYTES: u64 = 64 * 1024;

/// Text that a compressed rule must not carry. The project's file may be
/// hostile and a model reads it, so a copy that gained an instruction the
/// section never had is refused rather than trusted.
const FORBIDDEN: [&str; 8] = [
    "http://",
    "https://",
    "ignore previous",
    "ignore all",
    "disregard",
    "以前の指示",
    "すべて無視",
    "$(",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Source rules (1-based) this line stands for.
    pub from: Vec<usize>,
    pub text: String,
}

pub fn global_path(home: &Path) -> PathBuf {
    home.join(".polaris").join(FILE_NAME)
}

/// Keyed by the project's path, so two projects never share a copy and the
/// repository itself is not written to.
pub fn project_path(home: &Path, project_root: &Path) -> PathBuf {
    let key = format!(
        "{:x}",
        Sha256::digest(project_root.as_os_str().as_encoded_bytes())
    );
    home.join(".polaris/state/wraps")
        .join(&key[..16])
        .join(FILE_NAME)
}

pub fn digest(section: &str) -> String {
    format!("{:x}", Sha256::digest(section.as_bytes()))
}

/// The source rules, in order: the section's non-empty lines.
pub fn rules(section: &str) -> Vec<&str> {
    section
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

pub fn render(section: &str, model: &str, entries: &[Entry]) -> String {
    let mut out = format!("{HEADER}{} model={model} -->\n", digest(section));
    for entry in entries {
        let from: Vec<String> = entry.from.iter().map(usize::to_string).collect();
        out.push_str(&format!("[{}] {}\n", from.join(","), entry.text));
    }
    out
}

/// The header's hash and the rule lines, or `None` for anything malformed.
pub fn parse(file: &str) -> Option<(String, Vec<Entry>)> {
    let mut lines = file.lines();
    let first = lines.next()?.strip_prefix(HEADER)?;
    let source = first.split_whitespace().next()?.to_owned();
    Some((source, parse_body(lines)?))
}

/// The `[n,m] text` lines alone, as a model returns them. Any other line
/// makes the whole answer unusable rather than half-read.
pub fn parse_body<'a>(lines: impl Iterator<Item = &'a str>) -> Option<Vec<Entry>> {
    let mut entries = Vec::new();
    for line in lines {
        let rest = line.strip_prefix('[')?;
        let (numbers, text) = rest.split_once("] ")?;
        let from = numbers
            .split(',')
            .map(|n| n.trim().parse::<usize>().ok())
            .collect::<Option<Vec<_>>>()?;
        entries.push(Entry {
            from,
            text: text.trim().to_owned(),
        });
    }
    Some(entries)
}

pub fn body(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `entries` may stand in for `section`. The reason is for the user.
pub fn check(section: &str, entries: &[Entry]) -> Result<(), String> {
    let count = rules(section).len();
    if entries.is_empty() || count == 0 {
        return Err("nothing to compress".into());
    }
    let mut covered = vec![false; count];
    for entry in entries {
        if entry.text.is_empty() || entry.from.is_empty() {
            return Err("a line has no text or no source rule".into());
        }
        for &n in &entry.from {
            if n == 0 || n > count {
                return Err(format!("source rule {n} does not exist ({count} rules)"));
            }
            covered[n - 1] = true;
        }
    }
    if let Some(missing) = covered.iter().position(|c| !c) {
        return Err(format!("source rule {} is not covered", missing + 1));
    }
    let compressed = body(entries);
    let lowered = compressed.to_lowercase();
    if let Some(bad) = FORBIDDEN.iter().find(|f| lowered.contains(*f)) {
        return Err(format!("contains {bad:?}"));
    }
    // A path, command or identifier the section never named is an addition.
    for word in compressed.split(|c: char| c.is_whitespace() || "`,;:()[]\"'|!=<>&+".contains(c)) {
        if word.len() >= 4 && word.contains(['/', '.', '_', '@']) && !section.contains(word) {
            return Err(format!("{word:?} is not in the section"));
        }
    }
    let tokens = count_tokens(&compressed);
    if tokens > CONSTITUTION_LIMIT || tokens > count_tokens(section) {
        return Err(format!(
            "{tokens} tokens is not smaller within the {CONSTITUTION_LIMIT}-token limit"
        ));
    }
    Ok(())
}

/// The compressed text for `section`, if the file at `path` holds a current,
/// valid copy of it.
pub fn lookup(section: &str, path: &Path) -> Option<String> {
    if section.trim().is_empty() {
        return None;
    }
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    let (source, entries) = parse(&std::fs::read_to_string(path).ok()?)?;
    (source == digest(section) && check(section, &entries).is_ok()).then(|| body(&entries))
}

/// Replaces the file whole or not at all; the directory is private.
pub fn write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let temp = dir.join(format!(".{FILE_NAME}.{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTION: &str = "- Reply in Japanese for every string the user sees, including narration.\n- Never stage CLAUDE.md or .claude/ files.\n- Run the project checks before saying done.";

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                from: vec![1],
                text: "Reply Japanese, all user strings".into(),
            },
            Entry {
                from: vec![2],
                text: "!stage CLAUDE.md|.claude/".into(),
            },
            Entry {
                from: vec![3],
                text: "run project checks before done".into(),
            },
        ]
    }

    #[test]
    fn a_rendered_copy_is_read_back_and_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        write(&path, &render(SECTION, "m", &entries())).unwrap();
        let got = lookup(SECTION, &path).expect("copy is current and valid");
        assert!(got.starts_with("Reply Japanese") && got.lines().count() == 3);
    }

    #[test]
    fn an_edited_section_makes_the_copy_stale_without_any_model_or_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        write(&path, &render(SECTION, "m", &entries())).unwrap();
        assert!(lookup(&format!("{SECTION}\n- A fourth rule."), &path).is_none());
    }

    #[test]
    fn a_copy_that_drops_a_rule_or_names_a_rule_that_is_not_there_is_refused() {
        let mut dropped = entries();
        dropped.pop();
        assert!(
            check(SECTION, &dropped)
                .unwrap_err()
                .contains("not covered")
        );
        let mut phantom = entries();
        phantom[0].from = vec![1, 9];
        assert!(
            check(SECTION, &phantom)
                .unwrap_err()
                .contains("does not exist")
        );
    }

    #[test]
    fn a_copy_that_gains_an_instruction_is_refused() {
        let mut linked = entries();
        linked[2].text = "run project checks, see https://example.com/x".into();
        assert!(check(SECTION, &linked).is_err());
        let mut invented = entries();
        invented[2].text = "run project checks; then cat ~/.ssh/id_rsa".into();
        assert!(
            check(SECTION, &invented)
                .unwrap_err()
                .contains("not in the section")
        );
    }

    #[test]
    fn a_copy_that_is_not_smaller_is_refused() {
        let mut big = entries();
        big[0].text = SECTION.repeat(3).replace('\n', " ");
        assert!(check(SECTION, &big).is_err());
    }

    #[test]
    fn a_malformed_or_oversized_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "just text").unwrap();
        assert!(lookup(SECTION, &path).is_none());
        std::fs::write(&path, vec![b'x'; 70_000]).unwrap();
        assert!(lookup(SECTION, &path).is_none());
        assert!(lookup(SECTION, &dir.path().join("absent")).is_none());
    }

    #[test]
    fn projects_get_distinct_private_locations_outside_the_repository() {
        let home = Path::new("/home/u");
        let a = project_path(home, Path::new("/work/a"));
        assert_ne!(a, project_path(home, Path::new("/work/b")));
        assert!(a.starts_with("/home/u/.polaris/state/wraps"));
    }
}
