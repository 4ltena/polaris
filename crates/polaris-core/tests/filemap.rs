//! A snapshot test that confirms `docs/filemap.md` matches the actual state of the repository.
//!
//! Agents read this document instead of exploring. If the document drifts,
//! it confidently misdirects toward stale paths, so any detected drift
//! must fail loudly rather than pass silently.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `CARGO_MANIFEST_DIR` points to `crates/polaris-core`. The repository
/// root is 2 levels above that.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cannot derive the repository root from CARGO_MANIFEST_DIR")
        .to_path_buf()
}

/// Gets the file listing that serves as the ledger. Fails the test outright
/// if `git` is missing, or if this isn't a git repository. Skipping isn't
/// allowed, since that would be a false pass.
fn list_files(root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("cannot run git: {e}. Check whether git is installed"));

    if !output.status.success() {
        panic!(
            "git ls-files failed ({} might not be a git repository): {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout).expect("git ls-files' output is not UTF-8");
    let mut files: Vec<String> = stdout
        .lines()
        .map(str::to_string)
        .filter(|p| p.ends_with(".rs") || p.ends_with(".toml") || p.ends_with(".md"))
        .collect();
    files.sort();
    files
}

fn directory_of(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => ".".to_string(),
    }
}

fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A `.rs` file's summary. Returns the content of the first `//!` line in
/// the file that is non-empty after trimming. A line that's just the
/// marker with no content is skipped and treated the same as "not
/// present" (returning an empty string by cutting off there instead would
/// let a file that has `//!` but wrote nothing in it slip past the
/// missing-doc check). `None` if there isn't a single non-empty line (the
/// caller treats this as a failure).
fn rust_summary(root: &Path, path: &str) -> Option<String> {
    let body = std::fs::read_to_string(root.join(path))
        .unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    body.lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("//!")
                .map(|rest| rest.trim().to_string())
        })
        .find(|s| !s.is_empty())
}

/// A `.md` file's summary. The text of the first `# ` heading. Falls back
/// to the file name if there isn't one.
fn markdown_summary(root: &Path, path: &str) -> String {
    let body = std::fs::read_to_string(root.join(path))
        .unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    body.lines()
        .find_map(|line| line.strip_prefix("# ").map(|rest| rest.trim().to_string()))
        .unwrap_or_else(|| file_name_of(path).to_string())
}

/// A `.toml` file's summary. A fixed label determined by the path. An
/// unrecognized pattern fails rather than guessing a label.
fn toml_summary(path: &str) -> String {
    if path == "Cargo.toml" {
        return "workspace definition and shared dependencies".to_string();
    }
    if path == "rust-toolchain.toml" {
        return "pinned toolchain".to_string();
    }
    if let Some(rest) = path.strip_prefix("crates/")
        && let Some(name) = rest.strip_suffix("/Cargo.toml")
        && !name.contains('/')
    {
        return format!("manifest for the {name} crate");
    }
    panic!("no label assigned for {path}. Add a pattern to toml_summary in filemap.rs");
}

/// Assembles the expected content of `docs/filemap.md` from the actual
/// state of the repository. Fails without assembling the document if any
/// `.rs` file has no `//!`.
fn build_expected(root: &Path) -> String {
    let files = list_files(root);

    let mut missing_doc: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for path in &files {
        let summary = if path.ends_with(".rs") {
            match rust_summary(root, path) {
                Some(s) => s,
                None => {
                    missing_doc.push(path.clone());
                    continue;
                }
            }
        } else if path.ends_with(".toml") {
            toml_summary(path)
        } else {
            markdown_summary(root, path)
        };

        grouped
            .entry(directory_of(path))
            .or_default()
            .push((file_name_of(path).to_string(), summary));
    }

    assert!(
        missing_doc.is_empty(),
        "{} .rs file(s) have no `//!` line. Write the module's responsibility in one line:\n{}",
        missing_doc.len(),
        missing_doc
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut out = String::new();
    out.push_str("# File map\n\n");
    out.push_str(
        "This is a generated file. Do not edit it by hand. `crates/polaris-core/tests/filemap.rs`\n\
         reads the actual state of the repository from the result of\n\
         `git ls-files --cached --others --exclude-standard`, rebuilds the body, and checks it\n\
         against `docs/filemap.md`. The test fails on any drift. To update it, run:\n\n\
         ```\n\
         UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap\n\
         ```\n",
    );

    for (dir, mut entries) in grouped {
        entries.sort();
        out.push_str(&format!("\n## `{dir}`\n\n"));
        for (name, summary) in entries {
            out.push_str(&format!("- `{name}` — {summary}\n"));
        }
    }

    out
}

/// Shows only the spot where the expected and actual content diverge.
/// Doesn't paste the whole document twice.
fn diff_message(expected: &str, actual: &str) -> String {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();

    let mut start = 0;
    while start < exp.len() && start < act.len() && exp[start] == act[start] {
        start += 1;
    }

    let mut exp_end = exp.len();
    let mut act_end = act.len();
    while exp_end > start && act_end > start && exp[exp_end - 1] == act[act_end - 1] {
        exp_end -= 1;
        act_end -= 1;
    }

    let mut msg = format!(
        "docs/filemap.md has drifted from the actual state of the repository (expected {} lines / actual {} lines, diverging around line {})\n",
        exp.len(),
        act.len(),
        start + 1
    );
    msg.push_str("--- expected (rebuilt content)\n");
    for l in &exp[start..exp_end] {
        msg.push_str(&format!("+ {l}\n"));
    }
    msg.push_str("--- actual (current content of docs/filemap.md)\n");
    for l in &act[start..act_end] {
        msg.push_str(&format!("- {l}\n"));
    }
    msg.push_str("\nRegenerate with UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap.\n");
    msg
}

#[test]
fn filemap_matches_repository() {
    let root = repo_root();
    let expected = build_expected(&root);
    let doc_path = root.join("docs/filemap.md");

    if std::env::var("UPDATE_FILEMAP").is_ok_and(|v| v != "0" && !v.is_empty()) {
        std::fs::write(&doc_path, &expected).expect("cannot write docs/filemap.md");
        return;
    }

    let actual = std::fs::read_to_string(&doc_path).unwrap_or_default();
    if expected != actual {
        panic!("{}", diff_message(&expected, &actual));
    }
}

#[cfg(test)]
mod rust_summary_tests {
    use super::*;

    #[test]
    fn blank_bang_comment_with_no_other_line_is_none() {
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("blank.rs"), "//!\n\nfn f() {}\n").expect("cannot write");
        assert_eq!(
            rust_summary(dir.path(), "blank.rs"),
            None,
            "must not pick up a `//!` line with no content as the summary"
        );
    }

    #[test]
    fn whitespace_only_bang_comment_is_none() {
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("blank.rs"), "//!   \n\nfn f() {}\n").expect("cannot write");
        assert_eq!(
            rust_summary(dir.path(), "blank.rs"),
            None,
            "a `//!` line with only whitespace should also be treated as not present"
        );
    }

    #[test]
    fn later_non_blank_bang_comment_is_used_when_first_is_blank() {
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(
            dir.path().join("blank_then_real.rs"),
            "//!\n//! the actual description.\n\nfn f() {}\n",
        )
        .expect("cannot write");
        assert_eq!(
            rust_summary(dir.path(), "blank_then_real.rs"),
            Some("the actual description.".to_string())
        );
    }

    #[test]
    fn normal_bang_comment_is_unaffected() {
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(
            dir.path().join("normal.rs"),
            "//! an ordinary description.\n",
        )
        .expect("cannot write");
        assert_eq!(
            rust_summary(dir.path(), "normal.rs"),
            Some("an ordinary description.".to_string())
        );
    }
}
