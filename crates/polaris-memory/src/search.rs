//! Ranks legacy memory records into bounded, provenance-preserving excerpts.
//!
//! Connection setup, record decoding, validation, and tombstone ownership
//! remain in the parent module.

use rusqlite::params;

use super::{
    Embedding, Error, MAX_EXCERPT_BYTES, MAX_QUERY_BYTES, MAX_RESULTS, MemoryStore, Result,
    SearchHit, SearchMode, SearchRequest, read_record, validate_embedding, validate_metadata,
};

impl MemoryStore {
    /// Ranks inside the requested scope and keeps only a bounded set of excerpts.
    /// Exact score ties prefer newer timestamps: numeric Unix milliseconds for
    /// native records, lexical order for consistently formatted imported dates.
    /// Mixed timestamp formats are deterministic, not chronologically normalized.
    /// Lexical terms are literal: SQL/FTS syntax, `%`, `_`, etc. have no special role.
    pub fn search(&self, request: &SearchRequest<'_>) -> Result<Vec<SearchHit>> {
        if let SearchMode::Hybrid { query, embedding } = request.mode {
            return self.search_hybrid(request, query, embedding);
        }
        self.search_candidates(request, None)
    }

    fn search_candidates(
        &self,
        request: &SearchRequest<'_>,
        excerpt_query: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        validate_metadata(request.project_id)?;
        if let Some(session) = request.session_id {
            validate_metadata(session)?;
        }
        let terms = match request.mode {
            SearchMode::Lexical(query) => {
                if query.len() > MAX_QUERY_BYTES || query.trim().is_empty() {
                    return Err(Error::InvalidInput(
                        "query must be nonempty and at most 4096 bytes",
                    ));
                }
                query_terms(query)
            }
            SearchMode::Semantic(embedding) => {
                validate_embedding(embedding)?;
                excerpt_query.map(query_terms).unwrap_or_default()
            }
            SearchMode::Hybrid { .. } => unreachable!("hybrid handled above"),
        };
        let limit = request.limit.min(MAX_RESULTS);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let excerpt_bytes = request.excerpt_bytes.min(MAX_EXCERPT_BYTES);
        let (sql, model, dimension) = match request.mode {
            SearchMode::Lexical(_) => (
                "SELECT r.project_id, r.session_id, r.id, r.role, r.timestamp, r.source, r.text, NULL
                 FROM memory_records r WHERE r.project_id=?1 AND (?2 IS NULL OR r.session_id=?2)
                 AND ?3 IS NULL AND ?4 IS NULL", None, None,
            ),
            SearchMode::Semantic(embedding) => (
                "SELECT r.project_id, r.session_id, r.id, r.role, r.timestamp, r.source, r.text, v.embedding
                 FROM memory_vectors v JOIN memory_records r USING(project_id, session_id, id)
                 WHERE r.project_id=?1 AND (?2 IS NULL OR r.session_id=?2)
                 AND v.model=?3 AND v.dimension=?4", Some(embedding.model.as_str()), Some(embedding.values.len() as i64),
            ),
            SearchMode::Hybrid { .. } => unreachable!("hybrid handled above"),
        };
        let mut statement = self.connection.prepare(sql)?;
        let mut rows = statement.query(params![
            request.project_id,
            request.session_id,
            model,
            dimension
        ])?;
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            let record = read_record(row)?;
            let folded = if terms.is_empty() {
                String::new()
            } else {
                record.text.to_lowercase()
            };
            let score = match request.mode {
                SearchMode::Lexical(_) => {
                    if !terms.iter().all(|term| folded.contains(term)) {
                        continue;
                    }
                    terms
                        .iter()
                        .map(|term| folded.matches(term.as_str()).count())
                        .sum::<usize>() as f64
                }
                SearchMode::Semantic(query) => {
                    let bytes: Vec<u8> = row.get(7)?;
                    cosine(&query.values, &bytes)?
                }
                SearchMode::Hybrid { .. } => unreachable!("hybrid handled above"),
            };
            let hit = SearchHit {
                excerpt: relevant_excerpt(&record.text, &folded, &terms, excerpt_bytes),
                project_id: record.project_id,
                session_id: record.session_id,
                id: record.id,
                role: record.role,
                timestamp: record.timestamp,
                source: record.source,
                score,
            };
            hits.push(hit);
            hits.sort_by(compare_hits);
            hits.truncate(limit);
        }
        Ok(hits)
    }

    fn search_hybrid(
        &self,
        request: &SearchRequest<'_>,
        query: &str,
        embedding: &Embedding,
    ) -> Result<Vec<SearchHit>> {
        let mut candidates = std::collections::BTreeMap::<(String, String), SearchHit>::new();
        // Both candidate lists are bounded and filtered before any ranking. The
        // lexical excerpt wins when a record appears in both lists.
        for mode in [SearchMode::Lexical(query), SearchMode::Semantic(embedding)] {
            let candidate_request = SearchRequest {
                project_id: request.project_id,
                session_id: request.session_id,
                mode,
                limit: if request.limit == 0 { 0 } else { MAX_RESULTS },
                excerpt_bytes: request.excerpt_bytes,
            };
            for (rank, mut hit) in self
                .search_candidates(&candidate_request, Some(query))?
                .into_iter()
                .enumerate()
            {
                hit.score = 1.0 / (60.0 + rank as f64 + 1.0);
                let key = (hit.session_id.clone(), hit.id.clone());
                if let Some(existing) = candidates.get_mut(&key) {
                    existing.score += hit.score;
                } else {
                    candidates.insert(key, hit);
                }
            }
        }
        let mut hits: Vec<_> = candidates.into_values().collect();
        hits.sort_by(compare_hits);
        hits.truncate(request.limit.min(MAX_RESULTS));
        Ok(hits)
    }
}

fn cosine(query: &[f32], bytes: &[u8]) -> Result<f64> {
    if bytes.len() != query.len() * 4 {
        return Err(Error::InvalidEmbedding);
    }
    let (mut dot, mut query_norm, mut stored_norm) = (0.0, 0.0, 0.0);
    for (q, chunk) in query.iter().zip(bytes.chunks_exact(4)) {
        let v = f32::from_le_bytes(chunk.try_into().expect("four-byte chunk")) as f64;
        if !v.is_finite() {
            return Err(Error::InvalidEmbedding);
        }
        let q = *q as f64;
        dot += q * v;
        query_norm += q * q;
        stored_norm += v * v;
    }
    if stored_norm == 0.0 {
        return Err(Error::InvalidEmbedding);
    }
    Ok((dot / (query_norm.sqrt() * stored_norm.sqrt())).clamp(-1.0, 1.0))
}

// Native archives use integer Unix milliseconds. Imported nonnumeric timestamps
// sort lexicographically and must use one consistent format/timezone for freshness.
// Mixed formats have deterministic ordering, not inferred chronological meaning.
// Only exact relevance ties reach this comparison.
fn compare_hits(a: &SearchHit, b: &SearchHit) -> std::cmp::Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| freshness_key(&b.timestamp).cmp(&freshness_key(&a.timestamp)))
        .then_with(|| a.session_id.cmp(&b.session_id))
        .then_with(|| a.id.cmp(&b.id))
}

fn freshness_key(timestamp: &str) -> (u8, u128, &str) {
    if timestamp.bytes().all(|b| b.is_ascii_digit())
        && let Ok(millis) = timestamp.parse::<u128>()
    {
        (1, millis, "")
    } else {
        (0, 0, timestamp)
    }
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    terms.sort();
    terms.dedup();
    terms
}

pub(super) fn floor_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn relevant_excerpt(text: &str, folded: &str, terms: &[String], budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    // Whole-record embeddings cannot locate a relevant passage on their own.
    if terms.is_empty() || text.len() <= budget {
        return text[..floor_boundary(text, text.len().min(budget))].to_owned();
    }
    // Map case-folded matches once, including expanding lowercase characters.
    let mut folded_bytes = 0;
    let mut boundaries = Vec::new();
    for (offset, character) in text.char_indices() {
        boundaries.push((folded_bytes, offset));
        folded_bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
    }
    boundaries.push((folded_bytes, text.len()));
    let mut matches = Vec::new();
    for term in terms {
        for (offset, _) in folded.match_indices(term.as_str()) {
            let start = boundaries[boundaries.partition_point(|&(f, _)| f <= offset) - 1].1;
            let end = boundaries[boundaries.partition_point(|&(f, _)| f < offset + term.len())].1;
            matches.push((start, end));
        }
    }
    matches.sort_unstable();
    let mut best_start = 0;
    let mut best_coverage = 0;
    // Prefer a window containing more distinct query terms, not repetition of
    // one term. Equal coverage keeps the earliest passage, deterministically.
    'windows: for &(offset, match_end) in &matches {
        let context = (budget / 4).min(budget.saturating_sub(match_end - offset));
        // Also try without leading context so it cannot crowd out another term.
        for padding in [context, 0] {
            let mut start = offset.saturating_sub(padding);
            while !text.is_char_boundary(start) {
                start += 1;
            }
            let end = floor_boundary(text, start.saturating_add(budget).min(text.len()));
            let window = text[start..end].to_lowercase();
            let coverage = terms
                .iter()
                .filter(|term| window.contains(term.as_str()))
                .count();
            if coverage > best_coverage {
                best_start = start;
                best_coverage = coverage;
                if coverage == terms.len() {
                    break 'windows;
                }
            }
        }
    }
    let end = floor_boundary(text, best_start.saturating_add(budget).min(text.len()));
    text[best_start..end].to_owned()
}
