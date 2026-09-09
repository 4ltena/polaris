//! read tool. Line numbers are attached to the output so the model can
//! point at a location as path:line.

use std::path::Path;

use crate::{ToolError, path_policy};

/// Maximum number of bytes a read is allowed. Generous enough to be more
/// than sufficient for any real source file (even the largest `.rs` file in
/// this repository is under 35 KB), while staying three orders of magnitude
/// below the scale at which an accident (a device file or a huge file
/// pointed to by mistake) crashes the harness (observed: 2.26 GB in 4
/// seconds for `/dev/zero`).
pub const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

/// Default number of lines used when the model omits `limit`.
///
/// If the caller (`polaris_core::agent::dispatch`) hardcoded 2000 on the
/// spot, the place that decides how many lines to stop at would be
/// separated from the place that announces the stop. Putting the default
/// here keeps it handled in the same file as the truncation notice below.
pub const DEFAULT_LIMIT: usize = 2000;

/// Maximum number of bytes the rendered output is allowed to reach,
/// independent of `limit`.
///
/// `MAX_READ_BYTES` bounds the file this is willing to open at all, but
/// says nothing about how many of `limit`'s requested lines actually make
/// it into the response — a model that passes an unusually large `limit`
/// against a file close to that 5 MiB ceiling would otherwise get the
/// whole thing back in one call, unbounded by anything but the file's own
/// size. `bash` already holds its output to a byte cap
/// (`crate::bash::MAX_OUTPUT_BYTES`) for the same reason; this is that
/// same shape applied here. Set well above `DEFAULT_LIMIT`'s typical
/// output (this repository's own source files run at most tens of KB) so
/// ordinary reads are never affected, and well below `MAX_READ_BYTES` so
/// the worst case (an explicit huge `limit` against a file near the size
/// ceiling) is actually bounded rather than merely less likely.
pub const MAX_READ_OUTPUT_BYTES: usize = 1024 * 1024;

/// `offset` is a 0-based line number; `limit` is the number of lines to
/// return. The line numbers in the output are 1-based.
pub fn read(path: &Path, offset: usize, limit: usize) -> Result<String, ToolError> {
    let budget = std::env::var("POLARIS_READ_OUTPUT_BYTES")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|n| (1024..=MAX_READ_OUTPUT_BYTES).contains(n))
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "POLARIS_READ_OUTPUT_BYTES must be between 1024 and 1048576",
                    )
                })
        })
        .transpose()?;
    read_with_budget(path, offset, limit, budget)
}

fn read_with_budget(
    path: &Path,
    offset: usize,
    limit: usize,
    budget: Option<usize>,
) -> Result<String, ToolError> {
    read_impl(path, offset, limit, budget, false)
}

/// 隔離helper専用。本文は開いたFDの型と実読取上限でも制限する。
pub(crate) fn read_isolated(
    path: &Path,
    offset: usize,
    limit: usize,
    budget: usize,
) -> Result<String, ToolError> {
    read_impl(path, offset, limit, Some(budget), true)
}

fn read_impl(
    path: &Path,
    offset: usize,
    limit: usize,
    budget: Option<usize>,
    bounded: bool,
) -> Result<String, ToolError> {
    let output_cap = budget.unwrap_or(MAX_READ_OUTPUT_BYTES);
    if path_policy::is_denied(path) {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }
    // A symlink slips past `is_denied`'s string comparison. Resolve where
    // it points and run the same judgment on that too, to close the gap. A
    // nonexistent path fails to normalize, but that's treated as an
    // ordinary I/O error, not a "denial". Misreporting a typo'd path as
    // "can't read because it's a secret file" would send the model in the
    // wrong direction.
    if let Ok(real) = std::fs::canonicalize(path)
        && path_policy::is_denied(&real)
    {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }

    // `read_to_string` loads everything into memory without checking size
    // or type. If pointed at a device file (`/dev/zero`, etc.) or a FIFO,
    // the read never finishes and resident memory grows without bound.
    // Check the type and size from the metadata first, and deny even a
    // regular file without reading any of its content if it's over the cap.
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(ToolError::NotAFile(path.display().to_string()));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(ToolError::TooLarge {
            path: path.display().to_string(),
            limit: MAX_READ_BYTES,
            actual: meta.len(),
        });
    }

    let body = if bounded {
        crate::isolated_read::bounded_text(path, MAX_READ_BYTES)?
    } else {
        std::fs::read_to_string(path)?
    };

    // An empty file, limit=0, and an offset past the total line count are
    // all alike in that "there are zero target lines", but they mean
    // completely different things to the model. Collapsing all three into
    // `Ok("")` would make a call that simply moved offset past EOF look
    // like "the file was empty" — the shape this milestone repeated the
    // most, out of all the failures dressed up as success.
    if body.is_empty() {
        return Ok("(empty file)".to_string());
    }

    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();

    if limit == 0 {
        return Ok("(0 lines: limit is 0)".to_string());
    }
    if offset >= total {
        return Ok(format!(
            "(0 lines: offset {offset} is past the total {total} lines)"
        ));
    }

    let mut out = String::new();
    // Tracks the 1-based index of the last line actually written, so the
    // notice below can report the true stopping point regardless of
    // whether `limit` or `MAX_READ_OUTPUT_BYTES` is what stopped it —
    // `end` used to be derived purely from `offset`/`limit` and would have
    // claimed lines were shown that the byte cap had actually left out.
    let mut last_included = offset;
    for (i, line) in lines.iter().enumerate().skip(offset).take(limit) {
        let rendered = format!("{}\t{}\n", i + 1, line);
        // Always emit at least one line even if it alone exceeds the cap.
        // Returning "truncated" without showing any content leaves the
        // model with no next move at all — the same principle `bash` and
        // the skill tool's candidate list already apply to their own caps.
        if last_included > offset && out.len() + rendered.len() > output_cap {
            break;
        }
        if budget.is_some() && last_included == offset && rendered.len() > output_cap {
            let mut end = output_cap;
            while !rendered.is_char_boundary(end) {
                end -= 1;
            }
            out.push_str(&rendered[..end]);
            out.push_str(&format!(
                "\n(line {} was truncated within the line by the experimental byte budget. Use bash with a bounded extraction for the remainder; read offset only advances whole lines.)\n", i + 1
            ));
            last_included = i + 1;
            break;
        }
        out.push_str(&rendered);
        last_included = i + 1;
    }

    // Output that happens to return exactly `limit` lines can't be
    // distinguished from output where the file simply ends there versus
    // output where this stopped partway through. The default `limit` takes
    // effect even when the model doesn't specify one, so the model can end
    // up believing it "read everything" without even knowing it was
    // truncated — an unmarked partial answer is a wrong answer presented as
    // a complete one. Apply the same standard here that the skill body and
    // search results already hold to.
    if last_included < total {
        out.push_str(&format!(
            "(showed lines {}-{last_included} of {total} total. continue with offset={last_included}.)\n",
            offset + 1
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("can't create temp file");
        for l in lines {
            writeln!(f, "{l}").expect("can't write");
        }
        f.flush().expect("can't flush");
        f
    }

    #[test]
    fn harness_credentials_are_denied_directly_and_through_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join(".polaris");
        std::fs::create_dir(&store).unwrap();
        for name in [
            "auth.json",
            "api_key.json",
            "auth.json.tmp",
            "api_key.json.tmp",
        ] {
            let path = store.join(name);
            std::fs::write(&path, "DUMMY_SECRET_SENTINEL").unwrap();
            assert!(matches!(
                read(&path, 0, 20),
                Err(crate::ToolError::PathDenied(_))
            ));
            #[cfg(unix)]
            {
                let alias = root.path().join(format!("alias-{name}"));
                std::os::unix::fs::symlink(&path, &alias).unwrap();
                assert!(matches!(
                    read(&alias, 0, 20),
                    Err(crate::ToolError::PathDenied(_))
                ));
            }
        }
        let ordinary = root.path().join("auth.json");
        std::fs::write(&ordinary, "ordinary config").unwrap();
        assert!(read(&ordinary, 0, 20).unwrap().contains("ordinary config"));
    }

    #[test]
    fn numbers_lines_from_one() {
        let f = fixture(&["alpha", "beta"]);
        let out = read(f.path(), 0, 100).expect("can't read");
        assert_eq!(out, "1\talpha\n2\tbeta\n");
    }

    #[test]
    fn honors_offset_and_limit() {
        let f = fixture(&["a", "b", "c", "d"]);
        let out = read(f.path(), 1, 2).expect("can't read");
        assert_eq!(
            out,
            "2\tb\n3\tc\n(showed lines 2-3 of 4 total. continue with offset=3.)\n"
        );
    }

    #[test]
    fn a_truncated_read_says_where_it_stopped_and_how_to_continue() {
        // Output stopped by limit and output where the file simply ends
        // there must not come back in the same shape. The stopped case
        // states that it stopped and how to continue.
        let f = fixture(&["a", "b", "c", "d", "e"]);
        let out = read(f.path(), 0, 2).expect("can't read");
        assert_eq!(
            out,
            "1\ta\n2\tb\n(showed lines 1-2 of 5 total. continue with offset=2.)\n"
        );
    }

    #[test]
    fn a_read_that_reaches_the_end_says_nothing_extra() {
        // If output that reached the last line got a truncation notice
        // attached, that would become a wrong answer saying "there's more
        // to come". Pin down both sides of the boundary.
        let f = fixture(&["a", "b", "c"]);
        assert_eq!(
            read(f.path(), 0, 3).expect("can't read"),
            "1\ta\n2\tb\n3\tc\n"
        );
        assert_eq!(read(f.path(), 2, 1).expect("can't read"), "3\tc\n");
        assert_eq!(
            read(f.path(), 0, 100).expect("can't read"),
            "1\ta\n2\tb\n3\tc\n"
        );
    }

    #[test]
    fn the_default_limit_truncates_and_says_so() {
        // The default that takes effect when the model omits limit. Since
        // it wasn't specified, this wording is the model's only way to
        // learn it was truncated.
        let lines: Vec<String> = (0..DEFAULT_LIMIT + 5)
            .map(|i| format!("line {i}"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let f = fixture(&refs);

        let out = read(f.path(), 0, DEFAULT_LIMIT).expect("can't read");
        assert!(
            out.ends_with(&format!(
                "(showed lines 1-{DEFAULT_LIMIT} of {} total. continue with offset={DEFAULT_LIMIT}.)\n",
                DEFAULT_LIMIT + 5
            )),
            "doesn't say it was truncated by the default limit: {}",
            &out[out.len().saturating_sub(160)..]
        );
    }

    #[test]
    fn refuses_denied_paths() {
        let err = read(std::path::Path::new("/home/u/.ssh/id_rsa"), 0, 100)
            .expect_err("should be denied");
        assert!(matches!(err, ToolError::PathDenied(_)));
    }

    #[test]
    fn refuses_symlink_to_denied_target() {
        // Create an entity that mimics a denied directory, and put a
        // symlink to it somewhere else (under a filename that looks
        // harmless). `is_denied` only looks at the path string, so unless
        // the link is followed to resolve the real entity before judging,
        // it slips through.
        let dir = tempfile::tempdir().expect("can't create temp dir");
        let ssh_dir = dir.path().join(".ssh");
        std::fs::create_dir(&ssh_dir).expect("can't create directory");
        let target = ssh_dir.join("id_rsa");
        std::fs::write(&target, "secret").expect("can't write");

        let link = dir.path().join("notes.txt");
        std::os::unix::fs::symlink(&target, &link).expect("can't create symlink");

        let err = read(&link, 0, 100).expect_err("should be denied");
        assert!(matches!(err, ToolError::PathDenied(_)));
    }

    #[test]
    fn missing_file_is_io_error_not_denied() {
        // A nonexistent file fails to normalize (canonicalize).
        // Misreporting that as a denial would make the model mistake a
        // typo for "can't read because it's a secret file".
        let dir = tempfile::tempdir().expect("can't create temp dir");
        let missing = dir.path().join("does-not-exist.txt");

        let err = read(&missing, 0, 100).expect_err("should be an error");
        assert!(matches!(err, ToolError::Io(_)));
    }

    #[test]
    fn refuses_non_regular_files_like_directories() {
        // A directory has `is_file()` false, the same "not a regular file"
        // path as a device file like `/dev/zero`. This reproduces
        // regardless of OS, so make it the primary regression test.
        let dir = tempfile::tempdir().expect("can't create temp dir");
        let err = read(dir.path(), 0, 100).expect_err("should be denied");
        assert!(matches!(err, ToolError::NotAFile(_)));
    }

    #[test]
    #[cfg(unix)]
    fn refuses_device_files() {
        // The actual path that made the review's process consume 2.26 GB
        // of memory. Since this is rejected at the metadata stage, this
        // call returns an error immediately without reading any content at
        // all.
        let err = read(std::path::Path::new("/dev/zero"), 0, 100).expect_err("should be denied");
        assert!(matches!(err, ToolError::NotAFile(_)));
    }

    #[test]
    fn a_huge_limit_is_still_bounded_by_the_output_byte_cap() {
        // A file well under MAX_READ_BYTES, but with enough lines that an
        // unbounded `limit` would render past MAX_READ_OUTPUT_BYTES. Before
        // this cap existed, nothing stopped this from coming back whole.
        let line = "x".repeat(200);
        let line_count = (MAX_READ_OUTPUT_BYTES / (line.len() + 10)) * 2;
        let lines: Vec<String> = (0..line_count).map(|_| line.clone()).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let f = fixture(&refs);

        let out = read(f.path(), 0, usize::MAX).expect("can't read");

        assert!(
            out.len() <= MAX_READ_OUTPUT_BYTES + line.len() + 256,
            "output exceeds the byte cap: {} bytes",
            out.len()
        );
        assert!(
            out.contains("continue with offset="),
            "missing wording indicating the output was truncated: {}",
            &out[out.len().saturating_sub(160)..]
        );
    }

    #[test]
    fn a_single_line_over_the_output_byte_cap_is_still_returned() {
        // Returning "truncated" without showing even one line leaves the
        // model with no next move at all. Always emit the first line.
        let huge_line = "x".repeat(MAX_READ_OUTPUT_BYTES + 10);
        let f = fixture(&[&huge_line]);

        let out = read(f.path(), 0, 100).expect("can't read");
        assert!(
            out.starts_with("1\t"),
            "the first line was not shown: {}",
            &out[..out.len().min(80)]
        );
        assert!(
            !out.contains("continue with offset="),
            "says it was truncated even though the only line was shown"
        );
    }

    #[test]
    fn experimental_budget_preserves_line_continuation_and_unicode() {
        let f = fixture(&[&"日本語".repeat(300), "last"]);
        let out = read_with_budget(f.path(), 0, 100, Some(1024)).unwrap();
        assert!(out.starts_with("1\t日本語"));
        assert!(out.contains("truncated within the line"));
        assert!(out.contains("continue with offset=1"));
        assert!(out.len() < 1400);
        assert_eq!(
            read_with_budget(f.path(), 1, 100, Some(1024)).unwrap(),
            "2\tlast\n"
        );
    }

    #[test]
    fn experimental_budget_leaves_small_reads_unchanged() {
        let f = fixture(&["alpha", "beta"]);
        assert_eq!(
            read_with_budget(f.path(), 0, 100, Some(1024)).unwrap(),
            read_with_budget(f.path(), 0, 100, None).unwrap()
        );
    }

    #[test]
    fn refuses_files_over_the_size_ceiling() {
        let dir = tempfile::tempdir().expect("can't create temp dir");
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).expect("can't create");
        f.set_len(MAX_READ_BYTES + 1).expect("can't set size");

        let err = read(&path, 0, 100).expect_err("should be denied");
        match err {
            ToolError::TooLarge {
                limit,
                actual,
                path: p,
            } => {
                assert_eq!(limit, MAX_READ_BYTES);
                assert_eq!(actual, MAX_READ_BYTES + 1);
                assert!(p.contains("huge.txt"));
            }
            other => panic!("expected TooLarge but got {other:?}"),
        }
    }

    #[test]
    fn files_at_the_size_ceiling_are_still_allowed() {
        let dir = tempfile::tempdir().expect("can't create temp dir");
        let path = dir.path().join("at_limit.txt");
        std::fs::write(&path, "just one line\n").expect("can't write");

        let out = read(&path, 0, 100).expect("should be readable since it's under the cap");
        assert_eq!(out, "1\tjust one line\n");
    }

    #[test]
    fn empty_file_says_so_instead_of_looking_like_a_match() {
        let f = tempfile::NamedTempFile::new().expect("can't create temp file");
        let out = read(f.path(), 0, 100).expect("should be readable");
        assert_eq!(out, "(empty file)");
    }

    #[test]
    fn limit_zero_says_so_instead_of_looking_empty() {
        let f = fixture(&["a", "b", "c"]);
        let out = read(f.path(), 0, 0).expect("should be readable");
        assert_eq!(out, "(0 lines: limit is 0)");
    }

    #[test]
    fn offset_past_end_of_file_says_so_instead_of_looking_empty() {
        let f = fixture(&(0..10).map(|_| "line").collect::<Vec<_>>());
        let out = read(f.path(), 5000, 100).expect("should be readable");
        assert_eq!(out, "(0 lines: offset 5000 is past the total 10 lines)");
    }

    #[test]
    fn offset_exactly_at_line_count_is_also_past_end() {
        // offset is 0-based. An offset exactly equal to the line count is
        // "just past the last line"; getting the boundary off by one
        // either drops the last line or lets one extra out-of-range line
        // through.
        let f = fixture(&["a", "b", "c"]);
        let out = read(f.path(), 3, 100).expect("should be readable");
        assert_eq!(out, "(0 lines: offset 3 is past the total 3 lines)");
    }
}
