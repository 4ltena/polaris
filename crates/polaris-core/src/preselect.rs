//! Ranking candidates by embedding similarity — M10's second route.
//!
//! polaris keeps two preselection routes and neither is a fallback for the
//! other. One asks the typed judge; this one asks nobody, and ranks the
//! catalogue by how close each entry's description sits to the request in
//! multilingual-e5-base's space. Measured on 162 tools and 5 tasks, the two
//! have opposite characters: the judge takes a few too many and is
//! sometimes wrong, this takes fewer and is not
//! ([measurement](../../../.polaris/notes/checks/2026-09-18-preselection-cost/result.json)).
//! The one that is not wrong is the one placed where no judge exists, so a
//! run that loses the judge loses recall, never correctness.
//!
//! **There is no threshold here and there must not be one.** Cosine
//! similarity is not a probability, so `decision_bands`' 0.8 and 0.4 have
//! no meaning against it — that was decided in M10 and is the reason this
//! module returns an *order* and a count, never a verdict. Anything that
//! wants "is this one good enough" has to ask the judge.
//!
//! The lexical stage above this one decides order, not eligibility: a thin
//! BM25 result is padded up to the candidate count rather than obeyed. One
//! match must not replace the whole catalogue with one unrelated entry.

use std::io;

use crate::conversation_memory::StrictEmbedder;

/// One thing that can be selected, and the text that describes it.
///
/// `id` is whatever the caller uses to name it afterwards — a tool name, a
/// path, a skill name. This module never interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub text: String,
}

/// An embedded catalogue.
///
/// Built once per catalogue and reused across requests: embedding 162
/// descriptions costs far more than the one query embedding a ranking
/// needs, so rebuilding per request would make the cheap route the
/// expensive one. `refresh` re-embeds only what changed.
#[derive(Debug, Default)]
pub struct Index {
    entries: Vec<Entry>,
    /// Which model's space these vectors live in, as
    /// `model@revision/dimension`. Ranking with a different one compares
    /// coordinates from two spaces: the arithmetic succeeds and the
    /// numbers mean nothing, which is the failure that never gets noticed.
    space: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: String,
    /// The description this vector was made from. Held so `refresh` can
    /// tell a changed description from an unchanged one without keeping a
    /// separate hash that could disagree with it.
    text: String,
    vector: Vec<f32>,
}

/// How a candidate scored, highest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub id: String,
    /// Cosine similarity. **Not a probability and not comparable across
    /// models.** Carried so a caller can log or measure it, never so it
    /// can be compared against a constant.
    pub similarity: f32,
}

impl Index {
    /// The ids currently held, in insertion order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.id.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The model this index was built with, once it has been built.
    pub fn space(&self) -> Option<&str> {
        self.space.as_deref()
    }

    /// Brings the index in line with `candidates`.
    ///
    /// Entries whose description is unchanged keep their vector; the rest
    /// are embedded. Entries no longer in `candidates` are dropped, so an
    /// index cannot go on ranking a tool that was removed from the
    /// catalogue. The result is ordered exactly like `candidates` so two
    /// runs over the same catalogue produce the same index.
    ///
    /// A single embedding failure fails the whole refresh. A partial index
    /// would rank the entries that happened to embed above the ones that
    /// did not, which reads as a judgement about the catalogue rather than
    /// as the failure it is.
    pub async fn refresh(
        &mut self,
        embedder: &dyn StrictEmbedder,
        candidates: &[Candidate],
    ) -> io::Result<()> {
        let space = space_of(embedder);
        if self.space.as_deref().is_some_and(|held| held != space) {
            // Not an error — a caller may legitimately rebuild an index
            // against another model — but every held vector belongs to the
            // old space, so none of them may be reused.
            self.entries.clear();
        }
        let mut next = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if candidate.id.is_empty() || candidate.text.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "選定候補にidか説明が無いものがあります",
                ));
            }
            match self
                .entries
                .iter()
                .find(|held| held.id == candidate.id && held.text == candidate.text)
            {
                Some(held) => next.push(held.clone()),
                None => {
                    let vector = embedder.embed_passage(&candidate.text).await?;
                    check_dimension(embedder, &vector)?;
                    next.push(Entry {
                        id: candidate.id.clone(),
                        text: candidate.text.clone(),
                        vector,
                    });
                }
            }
        }
        self.entries = next;
        self.space = Some(space);
        Ok(())
    }

    /// The `limit` closest candidates to `query`, closest first.
    ///
    /// A query longer than the model's window comes back as several
    /// chunks; a candidate scores as its best chunk. Averaging instead
    /// would let a long request's unrelated second half pull down the
    /// entry that answers its first.
    ///
    /// Ties keep index order, which is catalogue order, so the result is
    /// reproducible rather than dependent on the sort's internals.
    pub async fn rank(
        &self,
        embedder: &dyn StrictEmbedder,
        query: &str,
        limit: usize,
    ) -> io::Result<Vec<Ranked>> {
        if self.entries.is_empty() || limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let space = space_of(embedder);
        if self.space.as_deref() != Some(space.as_str()) {
            return Err(io::Error::other(format!(
                "索引は{}で作られており、{space}では比較できません",
                self.space.as_deref().unwrap_or("(未構築)")
            )));
        }
        let chunks = embedder.embed_query(query).await?;
        if chunks.is_empty() {
            return Err(io::Error::other("問い合わせの埋め込みが空です"));
        }
        for chunk in &chunks {
            check_dimension(embedder, chunk)?;
        }
        let mut scored: Vec<Ranked> = self
            .entries
            .iter()
            .map(|entry| Ranked {
                id: entry.id.clone(),
                similarity: chunks
                    .iter()
                    .map(|chunk| dot(chunk, &entry.vector))
                    .fold(f32::NEG_INFINITY, f32::max),
            })
            .collect();
        // `sort_by` is stable, so equal scores stay in catalogue order.
        scored.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        Ok(scored)
    }
}

/// The identity of an embedding space. Two different models, or two
/// revisions of one, put the same word in different places, so a vector
/// from one is not comparable with a vector from the other.
fn space_of(embedder: &dyn StrictEmbedder) -> String {
    let metadata = embedder.metadata();
    format!(
        "{}@{}/{}",
        metadata.model, metadata.revision, metadata.dimension
    )
}

/// The helper L2-normalises every vector, so the dot product *is* the
/// cosine. Normalising again here would hide a helper that stopped doing
/// it, and the scores would stay plausible while meaning something else.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn check_dimension(embedder: &dyn StrictEmbedder, vector: &[f32]) -> io::Result<()> {
    let expected = embedder.metadata().dimension;
    if expected <= 0 || vector.len() as i64 != expected {
        return Err(io::Error::other(format!(
            "埋め込みの次元が{}ではなく{}です",
            expected,
            vector.len()
        )));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(io::Error::other("埋め込みに有限でない値があります"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
