//! M7a-2: which registered project folders look like one another, and what
//! that suggests. The comparison is over file names and sizes only — never
//! file contents — so it is cheap, bounded, and reveals nothing about what a
//! project holds. A suggestion is a reason to look, never a move.
use serde::Serialize;
use std::collections::BTreeMap;

/// Folders beyond this are not compared: the graph is for a person to read.
pub const MAX_FOLDERS: usize = 64;
/// Below this, two folders are not called similar at all.
pub const MIN_SIMILARITY: u32 = 30;
/// At or above this, the same work looks like it lives in two places.
pub const SUGGEST_AT: u32 = 70;

/// One folder as the host sampled it: the shared-file names it holds with
/// their sizes. Names only; no contents are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    /// Relative path to size. Bounded by the caller.
    pub entries: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Edge {
    pub left: String,
    pub right: String,
    /// 0–100. Shared names weighted by how close their sizes are.
    pub similarity: u32,
    /// The names both folders hold, for the person to check. Bounded.
    pub shared: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suggestion {
    /// Nearly the same content: one may be a stale copy of the other.
    SameWorkInTwoPlaces,
    /// A large shared part: a common piece could be extracted.
    SharedPart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Proposal {
    pub left: String,
    pub right: String,
    pub suggestion: Suggestion,
    /// Why, in the same terms the graph was built from.
    pub because: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Graph {
    pub edges: Vec<Edge>,
    pub proposals: Vec<Proposal>,
    /// Folders left out because the graph is bounded.
    pub omitted: usize,
}

/// Builds the graph. Deterministic: the same folders give the same edges in
/// the same order.
pub fn graph(folders: &[Folder]) -> Graph {
    let omitted = folders.len().saturating_sub(MAX_FOLDERS);
    let folders = &folders[..folders.len().min(MAX_FOLDERS)];
    let mut edges = Vec::new();
    for (index, left) in folders.iter().enumerate() {
        for right in &folders[index + 1..] {
            let (similarity, shared) = compare(left, right);
            if similarity >= MIN_SIMILARITY {
                edges.push(Edge {
                    left: left.id.clone(),
                    right: right.id.clone(),
                    similarity,
                    shared,
                });
            }
        }
    }
    edges.sort_by(|a, b| {
        b.similarity
            .cmp(&a.similarity)
            .then_with(|| (&a.left, &a.right).cmp(&(&b.left, &b.right)))
    });
    let proposals = edges
        .iter()
        .filter(|edge| edge.similarity >= SUGGEST_AT)
        .map(|edge| Proposal {
            left: edge.left.clone(),
            right: edge.right.clone(),
            suggestion: if edge.similarity >= 90 {
                Suggestion::SameWorkInTwoPlaces
            } else {
                Suggestion::SharedPart
            },
            because: format!(
                "{} shared filenames, {}% similarity",
                edge.shared.len(),
                edge.similarity
            ),
        })
        .collect();
    Graph {
        edges,
        proposals,
        omitted,
    }
}

/// Jaccard over names, with each shared name weighted by how close the two
/// sizes are. Two folders with the same names but very different sizes are
/// less similar than two that match closely.
fn compare(left: &Folder, right: &Folder) -> (u32, Vec<String>) {
    let mut shared = Vec::new();
    let mut weight = 0.0_f64;
    for (name, left_size) in &left.entries {
        if let Some(right_size) = right.entries.get(name) {
            let (small, large) = if left_size <= right_size {
                (*left_size, *right_size)
            } else {
                (*right_size, *left_size)
            };
            weight += if large == 0 {
                1.0
            } else {
                small as f64 / large as f64
            };
            shared.push(name.clone());
        }
    }
    let union = left.entries.len() + right.entries.len() - shared.len();
    if union == 0 {
        return (0, shared);
    }
    let similarity = ((weight / union as f64) * 100.0).round() as u32;
    shared.truncate(16);
    (similarity.min(100), shared)
}

pub mod sample;

#[cfg(test)]
mod tests;
