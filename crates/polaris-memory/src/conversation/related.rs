//! Bounded candidate generation, separate from structured-memory classification.
use super::*;

const MAX_IDS: usize = 16_384;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_MATCH_BYTES: usize = 64 * 1024 * 1024;
const MAX_CANDIDATES: usize = 3;

/// Optional search work limits, distinct from malformed or unverified evidence.
/// An owner may record omission of this lane; it must not report a no-hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum RelatedSummaryBudget {
    #[error("query_terms")]
    QueryTerms,
    #[error("grants")]
    Grants,
    #[error("serialized_rows")]
    SerializedRows,
    #[error("matching_work")]
    MatchingWork,
    #[error("vector_work")]
    VectorWork,
    #[error("raw_proof_work")]
    RawProofWork,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelatedSummaryCandidate {
    /// The owner must compare this with its saved response and resolve the raw
    /// source before packing it. A matching index hash alone is not authority.
    pub pending: PendingSummary,
    pub similarity: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelatedSummaryCandidates {
    pub keyword: Vec<RelatedSummaryCandidate>,
    /// Uncalibrated suggestions, never a successful entity/current-value lookup.
    /// Callers must not use these to fill a structured no-hit or missing state.
    pub semantic: Vec<RelatedSummaryCandidate>,
}

impl MemoryStore {
    /// Check the bounded index scan before requesting query embeddings. This
    /// does not prove owner authority, rank candidates, or validate future query
    /// vectors. The owner must recheck its snapshot and perform the checked
    /// candidate scan after embedding, in its verified read transaction.
    pub fn preflight_related_summary_scan(
        &self,
        published: &PublishedView,
        keywords: &str,
    ) -> Result<()> {
        self.summary_candidates_checked(published, keywords, None, |_| Ok(()), false)
            .map(|_| ())
    }

    /// Exact marker IDs only, with complete validated evidence and at most three
    /// candidates in each lane. This does not choose or inject memory, or alter
    /// a structured retrieval classification. It can join the owner's verified
    /// read transaction, keeping candidate and raw-source checks consistent.
    pub fn related_summary_candidates(
        &self,
        query: ConversationQuery<'_>,
    ) -> Result<RelatedSummaryCandidates> {
        self.related_summary_candidates_checked(query, |_| Ok(()))
    }

    /// Check every visible row against owner-held evidence before matching or
    /// truncating. Checking only returned hits would let a modified index hide
    /// an authoritative summary by changing its text or embedding metadata.
    pub fn related_summary_candidates_checked(
        &self,
        query: ConversationQuery<'_>,
        verify_saved: impl FnMut(&PendingSummary) -> Result<()>,
    ) -> Result<RelatedSummaryCandidates> {
        self.summary_candidates_checked(
            query.published,
            query.keywords,
            Some(std::slice::from_ref(&query.embedding)),
            verify_saved,
            true,
        )
    }

    /// All tokenizer chunks share one scan and owner verification callback.
    /// Each row is ranked by its strongest chunk, never by the first alone.
    pub fn related_summary_chunks_checked(
        &self,
        published: &PublishedView,
        keywords: &str,
        embeddings: &[QueryEmbedding],
        verify_saved: impl FnMut(&PendingSummary) -> Result<()>,
    ) -> Result<RelatedSummaryCandidates> {
        self.summary_candidates_checked(published, keywords, Some(embeddings), verify_saved, true)
    }

    /// Literal supplements do not need a query embedding. All visible rows are
    /// still checked before filtering. Similarity is unused (zero) in this lane.
    pub fn keyword_summary_candidates_checked(
        &self,
        published: &PublishedView,
        keywords: &str,
        verify_saved: impl FnMut(&PendingSummary) -> Result<()>,
    ) -> Result<Vec<RelatedSummaryCandidate>> {
        Ok(self
            .summary_candidates_checked(published, keywords, None, verify_saved, true)?
            .keyword)
    }

    fn summary_candidates_checked(
        &self,
        published: &PublishedView,
        keywords: &str,
        embeddings: Option<&[QueryEmbedding]>,
        mut verify_saved: impl FnMut(&PendingSummary) -> Result<()>,
        rank_candidates: bool,
    ) -> Result<RelatedSummaryCandidates> {
        let count = published
            .ancestors
            .iter()
            .try_fold(published.visible_ids.len(), |count, ancestor| {
                count.checked_add(ancestor.visible_ids.len())
            });
        if count.is_none_or(|count| count > MAX_IDS) || published.ancestors.len() > MAX_IDS {
            return Err(Error::RelatedSummaryBudgetExceeded(
                RelatedSummaryBudget::Grants,
            ));
        }
        if keywords.len() > 64 * 1024
            || keywords.trim().is_empty()
            || embeddings.is_some_and(|parts| parts.is_empty() || parts.len() > 32)
        {
            return Err(Error::InvalidInput("related summary query exceeds bounds"));
        }
        validate_view(published)?;
        if let Some(parts) = embeddings {
            let first = &parts[0];
            for part in parts {
                validate_query_embedding(part)?;
                if part.dimension > 4096
                    || part.model != first.model
                    || part.revision != first.revision
                    || part.dimension != first.dimension
                {
                    return Err(Error::InvalidInput("related query chunks disagree"));
                }
            }
        }
        let terms: Vec<_> = keywords
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if terms.len() > 128 {
            return Err(Error::RelatedSummaryBudgetExceeded(
                RelatedSummaryBudget::QueryTerms,
            ));
        }
        let owned = self.structured_read_transaction()?;
        let conn = &self.connection;
        reject_forgotten(conn, &published.scope)?;
        let mut scopes = vec![(published.scope.clone(), &published.visible_ids)];
        for ancestor in &published.ancestors {
            let scope = Scope {
                project_id: published.scope.project_id.clone(),
                session_id: ancestor.session_id.clone(),
                epoch: ancestor.epoch,
                generation: ancestor.generation,
            };
            validate_scope(&scope)?;
            if !is_forgotten(conn, &scope)? {
                scopes.push((scope, &ancestor.visible_ids));
            }
        }
        // Session generations are independent counters. Preserve the owner's
        // current/nearest-ancestor ordering before comparing them within scope.
        let scope_rank: HashMap<_, _> = scopes
            .iter()
            .enumerate()
            .map(|(rank, (scope, _))| (scope.session_id.clone(), rank))
            .collect();
        let mut bytes = 0usize;
        let mut match_bytes = 0usize;
        let mut vector_values = 0usize;
        let mut result = RelatedSummaryCandidates::default();
        for (scope, ids) in scopes {
            for id in ids {
                let pending = read_summary_receipt(conn, &scope, id)?;
                verify_saved(&pending)?;
                let size = serde_json::to_vec(&pending)
                    .map_err(|_| Error::InvalidInput("related summary serialization failed"))?
                    .len();
                bytes = bytes
                    .checked_add(size)
                    .ok_or(Error::RelatedSummaryBudgetExceeded(
                        RelatedSummaryBudget::SerializedRows,
                    ))?;
                if bytes > MAX_BYTES {
                    return Err(Error::RelatedSummaryBudgetExceeded(
                        RelatedSummaryBudget::SerializedRows,
                    ));
                }
                if embeddings.is_some_and(|parts| {
                    let embedding = &parts[0];
                    pending.embedding.model != embedding.model
                        || pending.embedding.revision != embedding.revision
                        || pending.embedding.dimension != embedding.dimension
                }) {
                    continue;
                }
                // Match only summary content, not JSON field names or metadata.
                let document: SummaryDocument = serde_json::from_str(&pending.summary)
                    .map_err(|_| Error::InvalidInput("related summary document is invalid"))?;
                let text = document
                    .facts
                    .iter()
                    .chain(&document.decisions)
                    .chain(&document.constraints)
                    .chain(&document.corrections)
                    .chain(&document.open_items)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .to_lowercase();
                match_bytes = match_bytes
                    .checked_add(text.len().saturating_mul(terms.len()))
                    .ok_or(Error::RelatedSummaryBudgetExceeded(
                        RelatedSummaryBudget::MatchingWork,
                    ))?;
                if match_bytes > MAX_MATCH_BYTES {
                    return Err(Error::RelatedSummaryBudgetExceeded(
                        RelatedSummaryBudget::MatchingWork,
                    ));
                }
                if !rank_candidates {
                    continue;
                }
                let keyword = terms.iter().all(|term| text.contains(term));
                if !keyword && embeddings.is_none() {
                    continue;
                }
                let similarity = if let Some(parts) = embeddings {
                    vector_values = vector_values
                        .checked_add(parts.len().saturating_mul(pending.embedding.values.len()))
                        .filter(|count| *count <= 64 * 1024 * 1024)
                        .ok_or(Error::RelatedSummaryBudgetExceeded(
                            RelatedSummaryBudget::VectorWork,
                        ))?;
                    let mut strongest = f64::NEG_INFINITY;
                    for part in parts {
                        strongest = strongest.max(cosine(&part.values, &pending.embedding.values)?);
                    }
                    strongest
                } else {
                    0.0
                };
                let lane = if keyword {
                    &mut result.keyword
                } else {
                    &mut result.semantic
                };
                lane.push(RelatedSummaryCandidate {
                    pending,
                    similarity,
                });
                lane.sort_by(|a, b| {
                    (if keyword {
                        std::cmp::Ordering::Equal
                    } else {
                        b.similarity.total_cmp(&a.similarity)
                    })
                    .then_with(|| {
                        scope_rank[&a.pending.scope.session_id]
                            .cmp(&scope_rank[&b.pending.scope.session_id])
                    })
                    .then_with(|| b.pending.scope.generation.cmp(&a.pending.scope.generation))
                    .then_with(|| a.pending.scope.session_id.cmp(&b.pending.scope.session_id))
                    .then_with(|| a.pending.scope.epoch.cmp(&b.pending.scope.epoch))
                    .then_with(|| a.pending.id.cmp(&b.pending.id))
                });
                lane.truncate(MAX_CANDIDATES);
            }
        }
        if let Some(transaction) = owned {
            transaction.commit()?;
        }
        Ok(result)
    }
}
