//! M7b-2: the synchronization repository. Git only ever sees the staging
//! copy, never the project directory, and every call names its own git dir,
//! work tree and index. Hooks, user configuration and prompts are disabled,
//! so nothing a participant sends can run here.
use super::share::{ShareManifest, SyncPaths};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

/// Bounds for one staging pass. Exceeding one refuses the pass instead of
/// sharing a partial tree.
pub const MAX_FILES: usize = 20_000;
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Staged {
    pub files: usize,
    pub bytes: u64,
    /// Present but not shared: a symlink, a name that is not UTF-8, or a
    /// file over the per-file bound. Never silently dropped.
    pub skipped: Vec<PathBuf>,
}

pub struct SyncRepository {
    paths: SyncPaths,
}

impl SyncRepository {
    /// Creates the repository if it is missing. The project is not touched.
    pub fn open(paths: SyncPaths) -> io::Result<Self> {
        for directory in [&paths.git_dir, &paths.work_tree] {
            fs::create_dir_all(directory)?;
        }
        let repository = Self { paths };
        if !repository.paths.git_dir.join("HEAD").exists() {
            repository.git(&["init", "--quiet", "--initial-branch=polaris"])?;
        }
        Ok(repository)
    }

    /// One bounded git call against this repository only. `pub(super)` so
    /// the exchange can use the same hardened invocation.
    pub(super) fn git_public(&self, arguments: &[&str]) -> io::Result<String> {
        self.git(arguments)
    }

    /// One bounded git call against this repository only.
    fn git(&self, arguments: &[&str]) -> io::Result<String> {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.paths.git_dir)
            .arg("--work-tree")
            .arg(&self.paths.work_tree)
            // Nothing a participant can send may run, and no user or system
            // configuration reaches this repository.
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["-c", "core.fsmonitor=false"])
            .args(["-c", "core.symlinks=false"])
            .args(arguments)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_INDEX_FILE", &self.paths.index)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_AUTHOR_NAME", "Polaris")
            .env("GIT_AUTHOR_EMAIL", "polaris@localhost")
            .env("GIT_COMMITTER_NAME", "Polaris")
            .env("GIT_COMMITTER_EMAIL", "polaris@localhost");
        let output = command.output()?;
        if !output.status.success() {
            // The message can name paths, so it stays a fixed category here.
            return Err(io::Error::other(format!(
                "git {} failed",
                arguments.first().copied().unwrap_or("call")
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Rebuilds the staging copy from the project, sharing only what the
    /// manifest allows. Rebuilding is what keeps a removal in the project
    /// from lingering in the shared tree.
    pub fn stage_from(&self, project_root: &Path, manifest: &ShareManifest) -> io::Result<Staged> {
        let work_tree = &self.paths.work_tree;
        for entry in fs::read_dir(work_tree)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        let mut staged = Staged::default();
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let source = project_root.join(&relative);
            for entry in fs::read_dir(&source)? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    staged.skipped.push(relative.join(entry.file_name()));
                    continue;
                };
                let child = relative.join(&name);
                if !manifest.shares(&child) {
                    continue;
                }
                // Symlinks are never followed and never shared: their target
                // means something different on the other device.
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    staged.skipped.push(child);
                    continue;
                }
                if kind.is_dir() {
                    fs::create_dir_all(work_tree.join(&child))?;
                    pending.push(child);
                    continue;
                }
                let bytes = entry.metadata()?.len();
                if bytes > MAX_FILE_BYTES {
                    staged.skipped.push(child);
                    continue;
                }
                staged.files += 1;
                staged.bytes += bytes;
                if staged.files > MAX_FILES || staged.bytes > MAX_TOTAL_BYTES {
                    return Err(io::Error::other("shared tree over its bounds"));
                }
                fs::copy(project_root.join(&child), work_tree.join(&child))?;
            }
        }
        Ok(staged)
    }

    /// Records the staged tree. Returns the commit, or `None` when nothing
    /// changed since the last one.
    pub fn commit(&self, message: &str) -> io::Result<Option<String>> {
        self.git(&["add", "--all", "."])?;
        if self.git(&["status", "--porcelain"])?.is_empty() {
            // Nothing staged differs from what is recorded, including the
            // case where this device has nothing shared yet.
            return Ok(None);
        }
        self.git(&["commit", "--quiet", "--allow-empty-message", "-m", message])?;
        self.git(&["rev-parse", "HEAD"]).map(Some)
    }

    pub fn head(&self) -> Option<String> {
        self.git(&["rev-parse", "HEAD"]).ok()
    }

    /// Puts the staging copy back to exactly what one commit holds. Applying
    /// reads the staging copy, so it must be the agreed commit and not
    /// whatever a previous command happened to leave there.
    pub fn restore(&self, commit: &str) -> io::Result<()> {
        self.git(&["reset", "--hard", "--quiet", commit]).map(drop)
    }

    /// What one agreed state changed against the last one this device put
    /// into the project, as paths to write and paths to remove. `from` is
    /// `None` before anything has been applied, and then nothing is removed:
    /// a file the shared history never held belongs to this device alone.
    pub fn changed_paths(
        &self,
        from: Option<&str>,
        to: &str,
    ) -> io::Result<(Vec<String>, Vec<String>)> {
        let Some(from) = from else {
            return Ok((self.files_at(to)?, Vec::new()));
        };
        // A rename is taken as a removal and an addition, so the pair needs no
        // separate reading, and `-z` keeps unusual names intact.
        let output = self.git(&["diff", "--name-status", "--no-renames", "-z", from, to])?;
        let mut written = Vec::new();
        let mut removed = Vec::new();
        let mut fields = output.split('\0').filter(|field| !field.is_empty());
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            if status.starts_with('D') {
                removed.push(path.to_owned());
            } else {
                written.push(path.to_owned());
            }
        }
        Ok((written, removed))
    }

    /// The shared paths in one commit, for checking what was shared.
    pub fn files_at(&self, commit: &str) -> io::Result<Vec<String>> {
        Ok(self
            .git(&["ls-tree", "-r", "--name-only", commit])?
            .lines()
            .map(str::to_owned)
            .collect())
    }
}

#[cfg(test)]
mod tests;
