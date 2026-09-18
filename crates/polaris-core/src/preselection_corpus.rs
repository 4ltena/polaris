//! The fixed corpus every preselection comparison is measured against, and
//! the shape a comparison row takes.
//!
//! **Why this is a file and not a fixture inside one test.** The point of
//! the comparison is that a number measured today can be put beside a
//! number measured after the next change. That only holds if the candidates
//! and the correct answers are the same both times, so the corpus is data,
//! loaded and hashed, rather than a literal somebody edits while tuning.
//!
//! The labels are a design-time judgement, not an observation of real work.
//! They say which tools a person would reasonably reach for, including the
//! ones that are a means to the end rather than the end itself — which is
//! exactly the distinction the question's wording turns on.
use std::collections::BTreeMap;

use polaris_mcp::catalog::index::{IndexedTool, ServerIndex};
use serde::Deserialize;

/// The corpus as it sits on disk.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub schema_version: u32,
    #[allow(dead_code)]
    pub note: String,
    pub servers: BTreeMap<String, Vec<CorpusTool>>,
    pub tasks: Vec<CorpusTask>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusTool {
    pub key: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusTask {
    pub id: String,
    pub phase: String,
    pub query: String,
    /// The tools a person would reasonably reach for. Everything else in
    /// the catalogue is, for this task, not useful.
    pub useful: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub note: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("コーパスを読めません: {0}")]
    Io(#[from] std::io::Error),
    #[error("コーパスの形が違います: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("コーパスのschema_versionが{0}です。1しか読めません")]
    UnsupportedVersion(u32),
    #[error("課題{task}の正解{tool}がカタログにありません")]
    UnknownLabel { task: String, tool: String },
}

impl Corpus {
    pub fn parse(bytes: &[u8]) -> Result<Self, CorpusError> {
        let held: Self = serde_json::from_slice(bytes)?;
        if held.schema_version != 1 {
            return Err(CorpusError::UnsupportedVersion(held.schema_version));
        }
        // A label naming a tool the catalogue does not hold would count as
        // a miss forever, and nothing would say why. Caught on load.
        let known: std::collections::BTreeSet<&str> = held
            .servers
            .values()
            .flatten()
            .map(|tool| tool.key.as_str())
            .collect();
        for task in &held.tasks {
            for tool in &task.useful {
                if !known.contains(tool.as_str()) {
                    return Err(CorpusError::UnknownLabel {
                        task: task.id.clone(),
                        tool: tool.clone(),
                    });
                }
            }
        }
        Ok(held)
    }

    /// The index a run would have been given, built from the corpus.
    ///
    /// The hashes are derived from the corpus itself rather than invented,
    /// so two builds of the same corpus produce the same index and a
    /// changed description changes the hash.
    pub fn index(&self) -> BTreeMap<String, ServerIndex> {
        self.servers
            .iter()
            .map(|(server_id, tools)| {
                (
                    server_id.clone(),
                    ServerIndex {
                        destination_hash: stable_hash(&format!("destination:{server_id}")),
                        partial: false,
                        tools: tools
                            .iter()
                            .map(|tool| IndexedTool {
                                key: tool.key.clone(),
                                name: tool.name.clone(),
                                description: tool.description.clone(),
                                summary_hash: stable_hash(&format!(
                                    "summary:{server_id}:{}:{}",
                                    tool.key, tool.description
                                )),
                                schema_hash: stable_hash(&format!(
                                    "schema:{server_id}:{}",
                                    tool.key
                                )),
                            })
                            .collect(),
                    },
                )
            })
            .collect()
    }

    pub fn tool_count(&self) -> usize {
        self.servers.values().map(Vec::len).sum()
    }

    /// Identifies which corpus a measurement was taken against. A row
    /// without this cannot be compared with anything.
    pub fn digest(&self) -> String {
        let mut material = format!("v{}", self.schema_version);
        for (server_id, tools) in &self.servers {
            material.push_str(server_id);
            for tool in tools {
                material.push_str(&tool.key);
                material.push_str(&tool.description);
            }
        }
        for task in &self.tasks {
            material.push_str(&task.id);
            material.push_str(&task.query);
            for tool in &task.useful {
                material.push_str(tool);
            }
        }
        stable_hash(&material)
    }
}

fn stable_hash(material: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(material.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// One measured arm against one task.
///
/// `missed` and `wrong` are kept apart because they are different failures:
/// a missed tool means the run cannot do the work, and a wrong one means it
/// paid for schema it did not need. A single accuracy number hides which.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Row {
    pub corpus_digest: String,
    pub arm: String,
    pub task: String,
    pub status: String,
    /// What the arm chose, in the order it chose them.
    pub picked: Vec<String>,
    /// Labelled useful and not picked.
    pub missed: Vec<String>,
    /// Picked and not labelled useful.
    pub wrong: Vec<String>,
    /// Absent for an arm that has no band structure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bands: Option<[usize; 3]>,
    /// False when the arm could not be measured at all, so an empty
    /// `picked` is not read as "it chose nothing".
    pub measured: bool,
}

impl Row {
    pub fn score(
        corpus: &Corpus,
        arm: &str,
        task: &CorpusTask,
        status: &str,
        picked: Vec<String>,
        bands: Option<[usize; 3]>,
        measured: bool,
    ) -> Self {
        let useful: std::collections::BTreeSet<&str> =
            task.useful.iter().map(String::as_str).collect();
        let chosen: std::collections::BTreeSet<&str> = picked.iter().map(String::as_str).collect();
        Self {
            corpus_digest: corpus.digest(),
            arm: arm.to_owned(),
            task: task.id.clone(),
            status: status.to_owned(),
            missed: useful
                .difference(&chosen)
                .map(|held| (*held).to_owned())
                .collect(),
            wrong: chosen
                .difference(&useful)
                .map(|held| (*held).to_owned())
                .collect(),
            picked,
            bands,
            measured,
        }
    }
}

#[cfg(test)]
#[path = "preselection_corpus/tests.rs"]
mod tests;
