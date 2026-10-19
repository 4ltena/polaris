//! M7c-3c: which `.gitignore` rules a setting command may add.
//!
//! Only additions, and only ones that cannot change what an existing line
//! already decided. Existing lines, their order, and negations are left
//! exactly as they are: a rule that would re-ignore something a negation
//! deliberately un-ignores is withheld rather than appended and explained
//! away afterwards.
/// A rule that was not added, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub rule: String,
    pub reason: WithheldReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithheldReason {
    /// The same line is already there.
    AlreadyPresent,
    /// A negation would stop meaning what it means today. The line is kept
    /// so a person can see what the collision was.
    ContradictsNegation { line: String },
}

/// What to append and what to leave out. `existing` is the file as it
/// stands; `wanted` is the fixed list from the recipe's profile, in the
/// order the recipe names them.
pub fn plan_additions<'a>(existing: &str, wanted: &[&'a str]) -> (Vec<&'a str>, Vec<Withheld>) {
    let lines: Vec<&str> = existing.lines().map(str::trim).collect();
    // A comment cannot also be a negation: a line starting with `#` never
    // starts with `!`, so stripping the `!` is the whole check.
    let negations: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix('!'))
        .filter(|body| !body.is_empty())
        .collect();
    let mut add = Vec::new();
    let mut withheld = Vec::new();
    for rule in wanted {
        if lines.contains(rule) {
            withheld.push(Withheld {
                rule: (*rule).to_owned(),
                reason: WithheldReason::AlreadyPresent,
            });
            continue;
        }
        match negations.iter().find(|body| covers(rule, body)) {
            Some(body) => withheld.push(Withheld {
                rule: (*rule).to_owned(),
                reason: WithheldReason::ContradictsNegation {
                    line: format!("!{body}"),
                },
            }),
            None => add.push(*rule),
        }
    }
    (add, withheld)
}

/// Whether adding `rule` would re-ignore what `negated` un-ignores.
///
/// Deliberately generous about what counts as a collision. Working out
/// exactly what a gitignore pattern matches means implementing gitignore;
/// getting that subtly wrong would silently re-ignore a file someone asked
/// to keep. Withholding a rule costs an untidy ignore file, which a person
/// can see and fix.
fn covers(rule: &str, negated: &str) -> bool {
    let trim = |held: &str| {
        held.trim_start_matches('/')
            .trim_end_matches('/')
            .to_owned()
    };
    let (rule, negated) = (trim(rule), trim(negated));
    if rule == negated {
        return true;
    }
    // `dist` against `dist/keep.txt`: the directory the rule names contains
    // the path the negation names.
    if negated.starts_with(&format!("{rule}/")) {
        return true;
    }
    // `**/coverage` against `coverage/report`, and the reverse nesting.
    match rule.rsplit('/').next() {
        Some(last) if !last.is_empty() && last != rule => {
            negated == last || negated.starts_with(&format!("{last}/"))
        }
        _ => false,
    }
}

/// Appends the rules to the project's `.gitignore`, in the order given.
///
/// Only appends: existing lines keep their text and their order, because
/// their order is what decides which rule wins. A file that did not end in
/// a newline gets one first, so the last existing rule is not merged with
/// the first new one.
pub fn append(project_root: &std::path::Path, rules: &[&str]) -> std::io::Result<()> {
    #[cfg(not(unix))]
    use std::io::Write;
    if rules.is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let existing = read(project_root)?;
        append_checked(
            project_root,
            rules,
            &crate::conversation_state::content_hash(existing.as_bytes()),
            None,
        )
        .map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let path = project_root.join(".gitignore");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        if !existing.is_empty() && !existing.ends_with('\n') {
            writeln!(file)?;
        }
        for rule in rules {
            writeln!(file, "{rule}")?;
        }
        file.sync_all()
    }
}

#[cfg(test)]
mod tests;

#[cfg(unix)]
pub fn read(project_root: &std::path::Path) -> std::io::Result<String> {
    use std::{
        fs::File,
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::MetadataExt,
        },
    };
    let root = crate::project_mutation::open_directory_nofollow(project_root)?;
    polaris_auth::protection::with_protected_paths(|registry| {
        let path = project_root.join(".gitignore");
        let denied = || {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "protected ignore file",
            )
        };
        if registry.paths().iter().any(|p| path.starts_with(p)) {
            return Err(denied());
        }
        let name = std::ffi::CString::new(".gitignore").unwrap();
        let fd = unsafe {
            libc::openat(
                root.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::NotFound {
                Ok(String::new())
            } else {
                Err(error)
            };
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() > 1024 * 1024
            || registry
                .identities()
                .iter()
                .any(|p| p.dev == metadata.dev() && p.ino == metadata.ino())
        {
            return Err(denied());
        }
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(denied());
        }
        String::from_utf8(bytes).map_err(|_| denied())
    })
    .map_err(|_| std::io::Error::other("authentication protection unavailable"))?
}

#[cfg(unix)]
pub fn append_checked(
    project_root: &std::path::Path,
    rules: &[&str],
    expected: &str,
    expected_source: Option<(u64, u64)>,
) -> std::io::Result<String> {
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::MetadataExt,
        },
    };
    let root = crate::project_mutation::open_directory_nofollow(project_root)?;
    let root_identity = root.metadata()?;
    if expected_source
        .is_some_and(|identity| identity != (root_identity.dev(), root_identity.ino()))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "source changed",
        ));
    }
    polaris_auth::protection::with_protected_paths(|registry| {
        let path = project_root.join(".gitignore");
        let denied = || {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe or stale ignore file",
            )
        };
        if registry.paths().iter().any(|p| path.starts_with(p)) {
            return Err(denied());
        }
        let name = std::ffi::CString::new(".gitignore").unwrap();
        let fd = unsafe {
            libc::openat(
                root.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_APPEND
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() > 1024 * 1024
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7022 != 0
            || registry
                .identities()
                .iter()
                .any(|p| p.dev == metadata.dev() && p.ino == metadata.ino())
        {
            return Err(denied());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut bytes = Vec::new();
        (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 || crate::conversation_state::content_hash(&bytes) != expected
        {
            return Err(denied());
        }
        let existing = String::from_utf8(bytes).map_err(|_| denied())?;
        let mut addition = String::new();
        if !rules.is_empty() && !existing.is_empty() && !existing.ends_with('\n') {
            addition.push('\n');
        }
        for rule in rules {
            if rule.is_empty() || rule.len() > 4096 || rule.chars().any(char::is_control) {
                return Err(denied());
            }
            addition.push_str(rule);
            addition.push('\n');
        }
        if existing.len().saturating_add(addition.len()) > 1024 * 1024 {
            return Err(denied());
        }
        let check = || -> std::io::Result<()> {
            let named_root = std::fs::symlink_metadata(project_root)?;
            let named = std::fs::symlink_metadata(&path)?;
            if !named_root.is_dir()
                || !named.is_file()
                || named.nlink() != 1
                || (named_root.dev(), named_root.ino())
                    != (root_identity.dev(), root_identity.ino())
                || (named.dev(), named.ino()) != (metadata.dev(), metadata.ino())
            {
                return Err(denied());
            }
            Ok(())
        };
        check()?;
        file.write_all(addition.as_bytes())?;
        file.sync_all()?;
        root.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let mut after = String::new();
        (&mut file)
            .take(1024 * 1024 + 1)
            .read_to_string(&mut after)?;
        if after != format!("{existing}{addition}") {
            return Err(denied());
        }
        check()?;
        Ok(after)
    })
    .map_err(|_| std::io::Error::other("authentication protection unavailable"))?
}
