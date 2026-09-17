//! M7c-3c: what git looks like at a project, and what `git_setting` may do
//! about it. Reading only; nothing here writes, initializes or commits.
//!
//! The point of the distinctions below is that a directory with no commits
//! is not the same as a directory with no repository, and neither is the
//! same as a directory that belongs to a repository further up. Treating
//! them alike is how a tool ends up running `git init` inside someone's
//! monorepo.
use super::super::project_git::{GitReadError, read_only_git};
use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

/// One detection pass may take this long in total.
const BUDGET: Duration = Duration::from_secs(3);

/// What the project directory actually is, as far as git is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// No repository at or above this directory.
    None,
    /// An ordinary repository whose top level is this directory.
    Own { head: Head },
    /// A linked worktree. `.git` is a file pointing at the real git dir.
    Worktree { head: Head },
    /// This directory sits inside a repository that starts higher up.
    /// Initializing here would nest one repository inside another.
    Inside { top_level: String },
    /// A bare repository has no work tree to set up.
    Bare,
}

/// Whether the repository has anything committed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    /// At least one commit, on a branch or detached.
    Committed,
    /// A repository exists but nothing is committed. **Not** a reason to
    /// initialize: the repository is already there.
    Unborn,
}

/// What `git_setting` may do here, decided from the placement alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitPlan {
    /// Only the additive `.gitignore` tidying applies.
    TidyOnly,
    /// Nothing applies, with the reason kept.
    NotApplicable { reason: GitRefusal },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRefusal {
    /// No repository. Creating one is the person's decision, not a step
    /// this command takes on its own.
    NoRepository,
    /// A repository owns this directory from higher up.
    InsideAnother { top_level: String },
    /// No work tree to configure.
    BareRepository,
    /// git could not be read at all, which is not the same as "no
    /// repository" and never leads to initializing one.
    Unreadable,
}

/// Reads the placement. `root` is an open directory; every call is bound to
/// it, so a path swapped underneath is not followed.
pub fn placement(root: &File) -> Result<Placement, GitReadError> {
    let deadline = Instant::now() + BUDGET;
    let text = |args: &[&str]| -> Result<String, GitReadError> {
        let bytes = read_only_git(root, args, deadline)?;
        String::from_utf8(bytes)
            .map(|held| held.trim().to_owned())
            .map_err(|_| GitReadError)
    };
    // Outside a repository this fails, which is the one answer that means
    // "none". Every other failure is left as unreadable by the caller.
    let Ok(inside) = text(&["rev-parse", "--is-inside-work-tree"]) else {
        return Ok(Placement::None);
    };
    if inside != "true" {
        // Inside a repository but with no work tree: a bare one.
        return Ok(Placement::Bare);
    }
    let top_level = text(&["rev-parse", "--show-toplevel"])?;
    // An empty prefix means this directory is the top level. A non-empty one
    // means the repository starts above, so nothing may be created here.
    if !text(&["rev-parse", "--show-prefix"])?.is_empty() {
        return Ok(Placement::Inside { top_level });
    }
    let head = if text(&["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        Head::Committed
    } else {
        Head::Unborn
    };
    // `.git` as a file is the linked-worktree layout. Its git dir lives
    // elsewhere, so configuration written here would land somewhere else.
    if Path::new(&top_level).join(".git").is_file() {
        return Ok(Placement::Worktree { head });
    }
    Ok(Placement::Own { head })
}

/// What may be done, given what was found. An unreadable git is refused
/// rather than read as an empty directory.
pub fn plan(placement: Result<Placement, GitReadError>) -> GitPlan {
    let refuse = |reason| GitPlan::NotApplicable { reason };
    match placement {
        Err(GitReadError) => refuse(GitRefusal::Unreadable),
        // An unborn HEAD is still a repository; it is never initialized again.
        Ok(Placement::Own { .. } | Placement::Worktree { .. }) => GitPlan::TidyOnly,
        Ok(Placement::None) => refuse(GitRefusal::NoRepository),
        Ok(Placement::Inside { top_level }) => refuse(GitRefusal::InsideAnother { top_level }),
        Ok(Placement::Bare) => refuse(GitRefusal::BareRepository),
    }
}

#[cfg(test)]
mod tests;
