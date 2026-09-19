//! The ranking stage, against a deterministic embedder.
//!
//! Nothing here starts the real helper. What is being checked is the
//! ordering, the refresh accounting and the refusals — the model's own
//! quality is the measurement's job, not a unit test's.

use super::*;
use crate::conversation_memory::{EmbeddingModel, StrictFuture};
use std::sync::Mutex;

/// Maps text to a unit vector by counting the marker letters in it, so a
/// description and a query that share letters come out close. Counts every
/// call, which is how the refresh tests tell a re-embed from a reuse.
struct Counting {
    metadata: EmbeddingModel,
    passages: Mutex<Vec<String>>,
    queries: Mutex<Vec<String>>,
    /// Chunk a query into one vector per line.
    per_line: bool,
    fail: bool,
}

impl Counting {
    fn new(dimension: i64) -> Self {
        Self {
            metadata: EmbeddingModel {
                model: "fixture".into(),
                revision: "r".into(),
                dimension,
            },
            passages: Mutex::new(Vec::new()),
            queries: Mutex::new(Vec::new()),
            per_line: false,
            fail: false,
        }
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        let dimension = self.metadata.dimension.max(1) as usize;
        let mut raw = vec![0f32; dimension];
        for byte in text.bytes() {
            if byte.is_ascii_lowercase() {
                raw[(byte - b'a') as usize % dimension] += 1.0;
            }
        }
        let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 {
            // A uniform unit vector, so an empty-ish text is equidistant
            // rather than producing NaN.
            let value = 1.0 / (dimension as f32).sqrt();
            return vec![value; dimension];
        }
        raw.into_iter().map(|v| v / norm).collect()
    }
}

impl StrictEmbedder for Counting {
    fn metadata(&self) -> &EmbeddingModel {
        &self.metadata
    }
    fn embed_passage<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async move {
            self.passages.lock().unwrap().push(text.to_string());
            if self.fail {
                return Err(io::Error::other("埋め込み失敗"));
            }
            Ok(self.vector(text))
        })
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async move {
            self.queries.lock().unwrap().push(text.to_string());
            if self.fail {
                return Err(io::Error::other("埋め込み失敗"));
            }
            if self.per_line {
                return Ok(text.lines().map(|line| self.vector(line)).collect());
            }
            Ok(vec![self.vector(text)])
        })
    }
}

fn candidates(pairs: &[(&str, &str)]) -> Vec<Candidate> {
    pairs
        .iter()
        .map(|(id, text)| Candidate {
            id: (*id).to_string(),
            text: (*text).to_string(),
        })
        .collect()
}

#[tokio::test]
async fn the_closest_candidate_comes_first_and_the_limit_is_honoured() {
    let embedder = Counting::new(26);
    let mut index = Index::default();
    index
        .refresh(
            &embedder,
            &candidates(&[
                ("resize", "resize an image to given dimensions"),
                ("email", "send an email to a mailing list"),
                ("deploy", "deploy the service to production"),
            ]),
        )
        .await
        .expect("refresh");

    let ranked = index
        .rank(&embedder, "send an email to a mailing list", 2)
        .await
        .expect("rank");
    assert_eq!(ranked.len(), 2, "{ranked:?}");
    assert_eq!(ranked[0].id, "email", "{ranked:?}");
    assert!(
        ranked[0].similarity >= ranked[1].similarity,
        "not sorted: {ranked:?}"
    );
}

/// Re-embedding an unchanged catalogue on every request would make the
/// cheap route the expensive one: 162 descriptions against one query.
#[tokio::test]
async fn an_unchanged_description_is_not_embedded_twice_and_a_changed_one_is() {
    let embedder = Counting::new(26);
    let mut index = Index::default();
    let first = candidates(&[("a", "alpha tool"), ("b", "beta tool")]);
    index.refresh(&embedder, &first).await.expect("refresh");
    assert_eq!(embedder.passages.lock().unwrap().len(), 2);

    index.refresh(&embedder, &first).await.expect("refresh");
    assert_eq!(
        embedder.passages.lock().unwrap().len(),
        2,
        "an unchanged catalogue was embedded again"
    );

    let changed = candidates(&[("a", "alpha tool"), ("b", "beta tool, now different")]);
    index.refresh(&embedder, &changed).await.expect("refresh");
    assert_eq!(
        embedder.passages.lock().unwrap().len(),
        3,
        "a changed description kept its old vector"
    );
}

/// An entry dropped from the catalogue must stop being offered. Keeping it
/// would let the ranking name a tool that no longer exists, and the caller
/// would fail at dispatch with no idea why.
#[tokio::test]
async fn a_candidate_removed_from_the_catalogue_leaves_the_index() {
    let embedder = Counting::new(26);
    let mut index = Index::default();
    index
        .refresh(&embedder, &candidates(&[("a", "alpha"), ("b", "beta")]))
        .await
        .expect("refresh");
    index
        .refresh(&embedder, &candidates(&[("a", "alpha")]))
        .await
        .expect("refresh");
    assert_eq!(index.ids().collect::<Vec<_>>(), vec!["a"]);
}

/// A partial index ranks whatever embedded above whatever did not, which
/// looks like a judgement about the catalogue instead of a failure.
#[tokio::test]
async fn one_failed_embedding_fails_the_whole_refresh_and_leaves_the_index_alone() {
    let mut embedder = Counting::new(26);
    let mut index = Index::default();
    index
        .refresh(&embedder, &candidates(&[("a", "alpha")]))
        .await
        .expect("refresh");

    embedder.fail = true;
    let result = index
        .refresh(&embedder, &candidates(&[("a", "alpha"), ("b", "beta")]))
        .await;
    assert!(result.is_err(), "a failed embedding was absorbed");
    assert_eq!(
        index.ids().collect::<Vec<_>>(),
        vec!["a"],
        "the index was left half-written"
    );
}

/// A long request comes back as chunks. A candidate that answers one of
/// them must not be pulled down by the rest.
#[tokio::test]
async fn a_chunked_query_scores_each_candidate_at_its_best_chunk() {
    let mut embedder = Counting::new(26);
    embedder.per_line = true;
    let mut index = Index::default();
    index
        .refresh(
            &embedder,
            &candidates(&[("zzz", "zzzz zzzz zzzz"), ("aaa", "aaaa aaaa aaaa")]),
        )
        .await
        .expect("refresh");

    let ranked = index
        .rank(&embedder, "aaaa aaaa aaaa\nqqqq qqqq qqqq", 2)
        .await
        .expect("rank");
    assert_eq!(ranked[0].id, "aaa", "{ranked:?}");
    // Its best chunk is an exact match, so averaging with the unrelated
    // second line would have shown up as a score well below 1.
    assert!(ranked[0].similarity > 0.99, "{ranked:?}");
}

#[tokio::test]
async fn an_empty_index_or_query_ranks_nothing_rather_than_guessing() {
    let embedder = Counting::new(26);
    let empty = Index::default();
    assert!(
        empty
            .rank(&embedder, "anything", 3)
            .await
            .expect("rank")
            .is_empty()
    );

    let mut index = Index::default();
    index
        .refresh(&embedder, &candidates(&[("a", "alpha")]))
        .await
        .expect("refresh");
    assert!(
        index
            .rank(&embedder, "   ", 3)
            .await
            .expect("rank")
            .is_empty()
    );
    assert!(
        index
            .rank(&embedder, "alpha", 0)
            .await
            .expect("rank")
            .is_empty()
    );
    assert!(
        !embedder
            .queries
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.trim().is_empty()),
        "an empty query was sent to the model anyway"
    );
}

/// Vectors from two models are two coordinate systems. `dot` would happily
/// multiply them pairwise and return a plausible number, so the mismatch
/// has to be refused before any arithmetic happens — the first version of
/// this module checked only the query's own length and let exactly this
/// through.
#[tokio::test]
async fn an_index_built_with_one_model_will_not_rank_against_another() {
    let small = Counting::new(8);
    let large = Counting::new(16);
    let mut index = Index::default();
    index
        .refresh(&small, &candidates(&[("a", "alpha")]))
        .await
        .expect("refresh");
    assert!(
        index.rank(&large, "alpha", 1).await.is_err(),
        "a 16-dimension query ranked an 8-dimension index"
    );

    // Same size, different model: the dimension alone would not catch it.
    let mut other = Counting::new(8);
    other.metadata.model = "another".into();
    assert!(
        index.rank(&other, "alpha", 1).await.is_err(),
        "two 8-dimension models were treated as one space"
    );

    // Rebuilding against the new model is allowed, and discards the old
    // vectors rather than keeping them alongside.
    index
        .refresh(&other, &candidates(&[("a", "alpha")]))
        .await
        .expect("refresh");
    assert_eq!(index.len(), 1);
    assert!(index.rank(&other, "alpha", 1).await.is_ok());
    assert!(index.rank(&small, "alpha", 1).await.is_err());
}

#[tokio::test]
async fn a_candidate_without_an_id_or_a_description_is_refused() {
    let embedder = Counting::new(26);
    let mut index = Index::default();
    for bad in [("", "text"), ("id", ""), ("id", "   ")] {
        assert!(
            index.refresh(&embedder, &candidates(&[bad])).await.is_err(),
            "{bad:?}"
        );
    }
}
