//! Reading folders into the shape the comparison takes.
//!
//! Names and sizes only. No file is opened, so nothing about what a project
//! holds is read, and a folder full of secrets samples the same as one full
//! of source. Symlinks are not followed: a link into someone else's tree
//! would otherwise be sampled as if it belonged here.
use super::Folder;
use std::collections::BTreeMap;
use std::path::Path;

/// Entries per folder. A folder larger than this is sampled up to the bound
/// and said to be truncated, rather than quietly compared on a slice.
pub const MAX_ENTRIES: usize = 2_000;
/// How deep below a folder the sampling goes.
pub const MAX_DEPTH: usize = 6;

/// Directory names never sampled: what a build or a package manager put
/// there is the same everywhere, so including them makes every project look
/// like every other one.
const SKIPPED: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".next",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".polaris",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sampled {
    pub folder: Folder,
    /// The folder held more than `MAX_ENTRIES`; what came back is a part of
    /// it, so a low similarity against it means less than it looks.
    pub truncated: bool,
}

/// Samples one folder. `id` is what the graph will call it.
pub fn folder(id: &str, root: &Path) -> std::io::Result<Sampled> {
    let mut entries = BTreeMap::new();
    let mut truncated = false;
    walk(root, root, 0, &mut entries, &mut truncated)?;
    Ok(Sampled {
        folder: Folder {
            id: id.to_owned(),
            entries,
        },
        truncated,
    })
}

fn walk(
    root: &Path,
    at: &Path,
    depth: usize,
    entries: &mut BTreeMap<String, u64>,
    truncated: &mut bool,
) -> std::io::Result<()> {
    if depth > MAX_DEPTH || *truncated {
        return Ok(());
    }
    // An unreadable child can be left out, but an unreadable root is not a
    // successful empty sample of the requested project.
    let read = match std::fs::read_dir(at) {
        Ok(read) => read,
        Err(error) if depth == 0 => return Err(error),
        Err(_) => return Ok(()),
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if SKIPPED.contains(&name) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            walk(root, &path, depth + 1, entries, truncated)?;
            if *truncated {
                return Ok(());
            }
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        if entries.len() >= MAX_ENTRIES {
            *truncated = true;
            return Ok(());
        }
        entries.insert(relative.to_owned(), metadata.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
