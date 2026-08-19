//! The mutation operation executed inside the confined child.
//!
//! It crosses from parent to child as a single JSON payload. No shell is
//! interposed, so as not to create a path that breaks on quotes and
//! newlines that might appear in the path or content.

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Mutation {
    Write {
        path: PathBuf,
        content: String,
    },
    Edit {
        path: PathBuf,
        old: String,
        new: String,
    },
}

/// The category of reason `apply` failed for.
///
/// They're kept separate because the words that should go back to the model
/// are opposite. "The replacement target was not found" is an entirely
/// ordinary outcome, and the model just needs to pick a different marker.
/// "The OS refused it" is a policy matter, and the model has no choice but
/// to pick a different place to write or ask for approval. Returning both
/// as the same denial would make the model start hunting for a permissions
/// problem in a case that a corrected marker alone would have fixed,
/// throwing away a round trip.
///
/// The category is decided here because this is the only place the material
/// exists. `apply` runs inside the confined child, and all that can cross to
/// the parent is the exit status and stdout/stderr — errno does not cross
/// the boundary. By the time `e.to_string()` is built, `ErrorKind` has
/// already been discarded, and trying to reconstruct it from the string on
/// the parent side would mean relying on the OS's wording (which is also
/// locale-dependent). The judgment is made here, where errno is still
/// available, and only the conclusion crosses to the parent, in the shape of
/// [`ApplyError::to_wire`].
#[derive(Debug)]
pub enum ApplyError {
    /// A problem with the request itself. The helper ran, but couldn't
    /// carry out the request as given (zero or multiple matches for the
    /// replacement target, the target file is missing, etc.).
    Request(String),
    /// The OS refused it. A sandbox policy violation lands here.
    ///
    /// "An ordinary permission error that has nothing to do with
    /// confinement" (e.g. a read-only file inside the root) also lands
    /// here. errno alone cannot distinguish the two. The spec itself states
    /// that classification by errno doesn't hold up. Falling on the side of
    /// denial here matches `run_mutation`'s previous handling (treat every
    /// nonzero as a denial), the same conservative direction, and doesn't
    /// create a new blind spot.
    Refused(String),
}

/// The marker. Used only to convey, across the confined child's single
/// stream of stderr to the parent, that this is a request problem. It adds
/// nothing to the exit code (the child still returns only 0 or nonzero, as
/// before).
const REQUEST_PROBLEM_MARKER: &str = "polaris-helper[request problem]: ";

impl ApplyError {
    /// Builds the single line the child writes to stderr. Only
    /// [`ApplyError::Request`] gets the marker. Output without the marker
    /// is still treated as a "denial" on the parent side as before, so the
    /// handling doesn't change for a helper that failed to launch, or one
    /// that crashed through an unanticipated path.
    pub fn to_wire(&self) -> String {
        match self {
            ApplyError::Request(msg) => format!("{REQUEST_PROBLEM_MARKER}{msg}"),
            ApplyError::Refused(msg) => msg.clone(),
        }
    }

    /// The body of the reason. Does not include the marker.
    pub fn message(&self) -> &str {
        match self {
            ApplyError::Request(msg) | ApplyError::Refused(msg) => msg,
        }
    }
}

/// Sorts an I/O error into a category.
///
/// `ErrorKind::PermissionDenied` is the portable name std gives to both
/// `EPERM` and `EACCES`. Seatbelt (macOS) returns `EPERM`, and landlock
/// (Linux) returns `EACCES`, so a sandbox denial always lands here.
/// Everything else (the target is missing, the target is a directory, etc.)
/// is "can't carry out the request as given" rather than a denial, and the
/// model just needs to fix the request.
fn classify_io(e: std::io::Error) -> ApplyError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        ApplyError::Refused(e.to_string())
    } else {
        ApplyError::Request(e.to_string())
    }
}

/// Pulls the reason out of the child's stderr as a request problem. `None`
/// if the marker is absent (treated as a denial).
///
/// The parent (`polaris_tools::write::run_mutation`) sees the marker only
/// through this function. Copying the marker string to both sides would
/// mean nobody notices if only one of them changes.
pub fn request_problem(stderr: &str) -> Option<&str> {
    stderr.trim().strip_prefix(REQUEST_PROBLEM_MARKER)
}

/// Carries out the mutation. On success, returns a single line readable by
/// both the human and the model.
pub fn apply(m: &Mutation) -> Result<String, ApplyError> {
    match m {
        Mutation::Write { path, content } => {
            // Even if path is a symlink pointing outside the writable
            // roots, this doesn't verify that here. That's deliberate.
            // apply itself performs no path verification at all; what
            // actually stops it is entirely delegated to the OS sandbox
            // (being launched as a confined child). Adding verification
            // here would only duplicate that claim, and if it ever drifted
            // out of sync it would produce false reassurance.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(classify_io)?;
            }
            std::fs::write(path, content).map_err(classify_io)?;
            Ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                path.display()
            ))
        }
        Mutation::Edit { path, old, new } => {
            let body = std::fs::read_to_string(path).map_err(classify_io)?;
            let hits = count_overlapping(&body, old);
            match hits {
                0 => Err(ApplyError::Request(format!(
                    "the replacement target was not found in {}",
                    path.display()
                ))),
                1 => {
                    let out = body.replace(old.as_str(), new.as_str());
                    std::fs::write(path, out).map_err(classify_io)?;
                    Ok(format!("replaced 1 occurrence in {}", path.display()))
                }
                // A replacement where the caller can't tell which
                // occurrence got replaced is never returned as a success.
                // Silently replacing only the first one would be the worst
                // outcome: it succeeds, yet the result left behind doesn't
                // match what was intended.
                n => Err(ApplyError::Request(format!(
                    "{} has {n} occurrences of the replacement target. Pass a string that resolves to exactly one occurrence",
                    path.display()
                ))),
            }
        }
    }
}

/// Counts how many times `needle` appears in `haystack`, allowing overlap.
///
/// `str::matches` only counts non-overlapping occurrences. Counting
/// `needle = "aa"` against `haystack = "aaa"` gives 1 under a
/// non-overlapping count, but it actually occurs at position 0 and
/// position 1 — two places — and the caller can't tell which one got
/// replaced. This is the same kind of situation the multi-match rejection
/// mechanism was built to prevent in the first place, and missing it
/// produces the worst outcome: "it succeeded, yet the result left behind
/// doesn't match what was intended".
fn count_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while let Some(rel) = haystack[offset..].find(needle) {
        count += 1;
        let hit_start = offset + rel;
        // Advancing by needle's full length would miss overlaps. The next
        // search starts from the character boundary right after this
        // match's first character.
        let advance = haystack[hit_start..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        offset = hit_start + advance;
        if offset > haystack.len() {
            break;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_the_file_and_its_parents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("a/b/c.txt");
        let m = Mutation::Write {
            path: target.clone(),
            content: "body text".into(),
        };
        apply(&m).expect("failed");
        assert_eq!(
            std::fs::read_to_string(&target).expect("can't read"),
            "body text"
        );
    }

    #[test]
    fn edit_replaces_exactly_one_occurrence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "before xxx after").expect("can't write");

        apply(&Mutation::Edit {
            path: target.clone(),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect("failed");

        assert_eq!(
            std::fs::read_to_string(&target).expect("can't read"),
            "before yyy after"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_appears_more_than_once() {
        // A replacement where the model can't tell which occurrence got
        // replaced must never be returned as a success. Silently replacing
        // only the first one would be the worst outcome: it succeeds, yet
        // the result left behind doesn't match what was intended.
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "xxx and xxx").expect("can't write");

        let err = apply(&Mutation::Edit {
            path: target.clone(),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("multiple matches went through");
        assert!(
            matches!(err, ApplyError::Request(_)),
            "multiple matches are a request problem, not a denial: {err:?}"
        );
        assert!(
            err.message().contains("2"),
            "the count wasn't conveyed: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("can't read"),
            "xxx and xxx",
            "rewritten despite being refused"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_overlaps_itself() {
        // "aa" looks like it appears exactly once in "aaa" under a
        // non-overlapping count, but it actually occurs at position 0 and
        // position 1 — two places. str::matches misses this and would
        // silently replace only the first occurrence (the leading 2
        // characters).
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "aaa").expect("can't write");

        let err = apply(&Mutation::Edit {
            path: target.clone(),
            old: "aa".into(),
            new: "b".into(),
        })
        .expect_err("an overlapping match went through");
        assert!(
            matches!(err, ApplyError::Request(_)),
            "an overlapping match is a request problem, not a denial: {err:?}"
        );
        assert!(
            err.message().contains("2"),
            "the count wasn't conveyed: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("can't read"),
            "aaa",
            "rewritten despite being refused"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_is_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "nothing here").expect("can't write");

        let err = apply(&Mutation::Edit {
            path: target,
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("a mismatch went through");
        assert!(
            matches!(err, ApplyError::Request(_)),
            "a mismatch is a request problem, not a denial: {err:?}"
        );
        assert!(err.message().contains("not found"), "{err:?}");
    }

    #[test]
    fn an_edit_of_a_file_that_does_not_exist_is_a_request_problem_not_a_refusal() {
        // A missing target is not "confinement refused it". The model just
        // needs to create it first or point at a different path, with no
        // need to doubt the policy or the writable roots.
        let dir = tempfile::tempdir().expect("temp dir");
        let err = apply(&Mutation::Edit {
            path: dir.path().join("no-such-file.txt"),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("editing a nonexistent file went through");
        assert!(
            matches!(err, ApplyError::Request(_)),
            "a missing target is classified as a denial: {err:?}"
        );
    }

    #[test]
    fn only_permission_errors_are_classified_as_a_refusal() {
        // Pins down that errno is the only material that distinguishes a
        // denial from a request problem. EPERM(1) comes from Seatbelt,
        // EACCES(13) comes from landlock, and std maps both to
        // ErrorKind::PermissionDenied. ENOENT(2) is an ordinary failure
        // that is neither, and must not be treated as a denial.
        //
        // This doesn't construct the case with real file permissions,
        // because when a container test runs as root, even 0444 can still
        // be written to, which would make the test meaningless depending
        // on the environment.
        assert!(
            matches!(
                classify_io(std::io::Error::from_raw_os_error(1)),
                ApplyError::Refused(_)
            ),
            "EPERM was not classified as a refusal"
        );
        assert!(
            matches!(
                classify_io(std::io::Error::from_raw_os_error(13)),
                ApplyError::Refused(_)
            ),
            "EACCES was not classified as a refusal"
        );
        assert!(
            matches!(
                classify_io(std::io::Error::from_raw_os_error(2)),
                ApplyError::Request(_)
            ),
            "ENOENT was classified as a refusal"
        );
    }

    #[test]
    fn the_marker_survives_the_trip_to_the_parent_and_only_marks_request_problems() {
        // What the parent (run_mutation) sees is only the child's stderr
        // string. Round-trip it with the marker attached, and pin down both
        // that the category truly gets conveyed, and that a denial carries
        // no marker (i.e. without a marker it's still treated as a denial
        // as before).
        let req = ApplyError::Request("replacement target not found".to_string());
        let wire = req.to_wire();
        assert_eq!(
            request_problem(&wire),
            Some("replacement target not found"),
            "the marked reason can't be extracted on the parent side: {wire}"
        );

        let refused = ApplyError::Refused("Operation not permitted (os error 1)".to_string());
        let wire = refused.to_wire();
        assert_eq!(
            request_problem(&wire),
            None,
            "the denial carries a marker (it would be treated as a request problem): {wire}"
        );

        // The child writes it out with a trailing newline. The parent trims
        // before reading.
        assert_eq!(
            request_problem(&format!("{}\n", ApplyError::Request("x".into()).to_wire())),
            Some("x")
        );
    }

    #[test]
    fn a_mutation_round_trips_through_json() {
        // It crosses to the helper as a single line of JSON. If the round
        // trip breaks, the mutation operation can't happen.
        let m = Mutation::Write {
            path: "/a/b".into(),
            content: "newline\nand \"quotes\"".into(),
        };
        let s = serde_json::to_string(&m).expect("serialize");
        let back: Mutation = serde_json::from_str(&s).expect("restore");
        match back {
            Mutation::Write { path, content } => {
                assert_eq!(path, std::path::PathBuf::from("/a/b"));
                assert_eq!(content, "newline\nand \"quotes\"");
            }
            other => panic!("became a different variant: {other:?}"),
        }
    }
}
