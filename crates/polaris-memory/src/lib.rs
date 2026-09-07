//! Local, explicitly populated memory. Retrieved text is quoted historical evidence,
//! never a current instruction or authorization. No model or network calls are made.
//!
//! Callers own opt-in, project identity, timestamps, stable record IDs and import
//! policy. IDs are unique within (project_id, session_id). Search scans only the
//! selected project (and optionally session); exact cosine is intended for local
//! corpora, not an approximate nearest-neighbor service.

use rusqlite::{Connection, OptionalExtension, Row, params};
pub mod conversation;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

pub const MAX_RESULTS: usize = 100;
pub const MAX_EXCERPT_BYTES: usize = 4096;
pub const MAX_METADATA_BYTES: usize = 1024;
pub const MAX_EMBEDDING_DIMENSIONS: usize = 65_536;
pub const MAX_QUERY_BYTES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid memory input: {0}")]
    InvalidInput(&'static str),
    #[error("invalid stored embedding")]
    InvalidEmbedding,
    #[error("session was forgotten; importing it again is disabled")]
    SessionForgotten,
    #[error("record ID occurs in multiple sessions; specify a session")]
    AmbiguousRecordId,
    #[error("record does not exist in the requested project and session")]
    RecordNotFound,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Full historical record. `timestamp` and `source` are caller-supplied provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub project_id: String,
    pub session_id: String,
    pub id: String,
    pub role: String,
    pub timestamp: String,
    pub source: String,
    pub text: String,
}

/// An actual embedding supplied explicitly by the caller; never synthesized here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    pub model: String,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, Copy)]
pub enum SearchMode<'a> {
    /// All whitespace-separated terms must occur as case-insensitive substrings.
    /// This preserves Japanese phrases and code identifiers without a tokenizer.
    Lexical(&'a str),
    /// Only stored embeddings with the same model and dimension are candidates.
    Semantic(&'a Embedding),
    /// Reciprocal rank fusion of up to MAX_RESULTS candidates per mode, k=60.
    /// This combines rankings; it does not imply calibrated relevance.
    Hybrid {
        query: &'a str,
        embedding: &'a Embedding,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct SearchRequest<'a> {
    pub project_id: &'a str,
    pub session_id: Option<&'a str>,
    pub mode: SearchMode<'a>,
    /// Clamped to MAX_RESULTS. Zero returns no candidates.
    pub limit: usize,
    /// Per-hit UTF-8 byte budget, clamped to MAX_EXCERPT_BYTES.
    pub excerpt_bytes: usize,
}

impl<'a> SearchRequest<'a> {
    pub fn lexical(project_id: &'a str, query: &'a str) -> Self {
        Self {
            project_id,
            session_id: None,
            mode: SearchMode::Lexical(query),
            limit: 10,
            excerpt_bytes: 512,
        }
    }
}

/// Bounded excerpt and provenance, deliberately excluding the full record text.
/// Treat this as a quotation, not as permission to perform any operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub project_id: String,
    pub session_id: String,
    pub id: String,
    pub role: String,
    pub timestamp: String,
    pub source: String,
    pub excerpt: String,
    /// Lexical occurrence count or cosine similarity, depending on search mode.
    pub score: f64,
}

/// A bounded UTF-8 byte range. Offsets refer to the original record, and `end`
/// is exclusive. The record retains provenance but contains only the selected text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordRange {
    pub record: Record,
    pub start: usize,
    pub end: usize,
    pub total_bytes: usize,
}

pub struct MemoryStore {
    connection: Connection,
}

impl MemoryStore {
    /// Opens or creates a database. The caller must create its parent directory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        #[cfg(unix)]
        if path.as_ref() != Path::new(":memory:") {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path.as_ref())?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_records (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                id TEXT NOT NULL,
                role TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                source TEXT NOT NULL,
                text TEXT NOT NULL,
                PRIMARY KEY (project_id, session_id, id)
            );
            CREATE TABLE IF NOT EXISTS memory_vectors (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                id TEXT NOT NULL,
                model TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                PRIMARY KEY (project_id, session_id, id),
                FOREIGN KEY (project_id, session_id, id)
                    REFERENCES memory_records(project_id, session_id, id)
                    ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS memory_vector_model
                ON memory_vectors(project_id, model, dimension);
            CREATE TABLE IF NOT EXISTS memory_forgotten_sessions (
                project_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                PRIMARY KEY (project_id, session_id)
            );",
        )?;
        transaction.commit()?;
        conversation::initialize(&connection)?;
        Ok(Self { connection })
    }

    /// Opens an existing database without creation, schema initialization or
    /// permission changes. Mutations on this handle fail with a SQLite error.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { connection })
    }

    /// Atomically inserts or replaces one stable ID and its optional embedding.
    /// `None` clears any old vector, so updated text cannot retain stale embeddings.
    pub fn upsert(&mut self, record: &Record, embedding: Option<&Embedding>) -> Result<()> {
        for value in [
            &record.project_id,
            &record.session_id,
            &record.id,
            &record.role,
            &record.timestamp,
            &record.source,
        ] {
            validate_metadata(value)?;
        }
        if let Some(embedding) = embedding {
            validate_embedding(embedding)?;
        }
        // Acquire the writer lock before checking the tombstone, including across
        // independent connections racing an automatic import against forgetting.
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let forgotten: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_forgotten_sessions WHERE project_id=?1 AND session_id=?2)",
            params![record.project_id, record.session_id], |row| row.get(0),
        )?;
        if forgotten {
            return Err(Error::SessionForgotten);
        }
        transaction.execute(
            "INSERT INTO memory_records VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(project_id, session_id, id) DO UPDATE SET
             role=excluded.role, timestamp=excluded.timestamp,
             source=excluded.source, text=excluded.text",
            params![
                record.project_id,
                record.session_id,
                record.id,
                record.role,
                record.timestamp,
                record.source,
                record.text
            ],
        )?;
        transaction.execute(
            "DELETE FROM memory_vectors WHERE project_id=?1 AND session_id=?2 AND id=?3",
            params![record.project_id, record.session_id, record.id],
        )?;
        if let Some(embedding) = embedding {
            let bytes: Vec<u8> = embedding
                .values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            transaction.execute(
                "INSERT INTO memory_vectors VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.project_id,
                    record.session_id,
                    record.id,
                    embedding.model,
                    embedding.values.len() as i64,
                    bytes
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Fetches full text with an explicit project/session scope; never truncates it.
    pub fn get(&self, project_id: &str, session_id: &str, id: &str) -> Result<Option<Record>> {
        Ok(self
            .connection
            .query_row(
                "SELECT project_id, session_id, id, role, timestamp, source, text
             FROM memory_records WHERE project_id=?1 AND session_id=?2 AND id=?3",
                params![project_id, session_id, id],
                read_record,
            )
            .optional()?)
    }

    /// Fetches at most MAX_EXCERPT_BYTES bytes starting at an exact UTF-8
    /// boundary. Out-of-bounds/interior-byte starts are errors; EOF is valid.
    /// The end rounds down to a character boundary. Zero budgets are valid.
    pub fn get_range(
        &self,
        project_id: &str,
        session_id: &str,
        id: &str,
        start: usize,
        max_bytes: usize,
    ) -> Result<Option<RecordRange>> {
        let Some(mut record) = self.get(project_id, session_id, id)? else {
            return Ok(None);
        };
        let total_bytes = record.text.len();
        if !record.text.is_char_boundary(start) {
            return Err(Error::InvalidInput(
                "start must be a UTF-8 boundary within the record",
            ));
        }
        let end = floor_boundary(
            &record.text,
            start
                .saturating_add(max_bytes.min(MAX_EXCERPT_BYTES))
                .min(total_bytes),
        );
        record.text = record.text[start..end].to_owned();
        Ok(Some(RecordRange {
            record,
            start,
            end,
            total_bytes,
        }))
    }

    /// Fetches a project-scoped ID, rejecting ambiguous IDs across sessions.
    pub fn get_by_id(&self, project_id: &str, id: &str) -> Result<Option<Record>> {
        let mut statement = self.connection.prepare(
            "SELECT project_id, session_id, id, role, timestamp, source, text
             FROM memory_records WHERE project_id=?1 AND id=?2 LIMIT 2",
        )?;
        let mut rows = statement.query_map(params![project_id, id], read_record)?;
        let record = rows.next().transpose()?;
        if rows.next().transpose()?.is_some() {
            return Err(Error::AmbiguousRecordId);
        }
        Ok(record)
    }

    /// Pages full records in ID order for explicit batch indexing. Pass the last
    /// ID as `after_id` for the next page. At most MAX_RESULTS records are returned;
    /// their full text is intentionally not byte-bounded. Concurrent inserts before
    /// the cursor require a subsequent indexing pass.
    pub fn records_page(
        &self,
        project_id: &str,
        session_id: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let mut statement = self.connection.prepare(
            "SELECT project_id, session_id, id, role, timestamp, source, text
             FROM memory_records WHERE project_id=?1 AND session_id=?2
             AND (?3 IS NULL OR id > ?3) ORDER BY id LIMIT ?4",
        )?;
        let records = statement
            .query_map(
                params![
                    project_id,
                    session_id,
                    after_id,
                    limit.min(MAX_RESULTS) as i64
                ],
                read_record,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Whether this scoped record already has an embedding for the requested model.
    pub fn has_embedding(
        &self,
        project_id: &str,
        session_id: &str,
        id: &str,
        model: &str,
    ) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_vectors WHERE project_id=?1 AND session_id=?2 AND id=?3 AND model=?4)",
            params![project_id, session_id, id, model], |row| row.get(0),
        )?)
    }

    /// Attaches/replaces a supplied embedding without modifying text or metadata.
    pub fn set_embedding(
        &mut self,
        project_id: &str,
        session_id: &str,
        id: &str,
        embedding: &Embedding,
    ) -> Result<()> {
        validate_embedding(embedding)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let forgotten: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_forgotten_sessions WHERE project_id=?1 AND session_id=?2)",
            params![project_id, session_id], |row| row.get(0),
        )?;
        if forgotten {
            return Err(Error::SessionForgotten);
        }
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_records WHERE project_id=?1 AND session_id=?2 AND id=?3)",
            params![project_id, session_id, id], |row| row.get(0),
        )?;
        if !exists {
            return Err(Error::RecordNotFound);
        }
        let bytes: Vec<u8> = embedding
            .values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        transaction.execute(
            "INSERT INTO memory_vectors VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(project_id, session_id, id) DO UPDATE SET
             model=excluded.model, dimension=excluded.dimension, embedding=excluded.embedding",
            params![
                project_id,
                session_id,
                id,
                embedding.model,
                embedding.values.len() as i64,
                bytes
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns a persistent tombstone state. Read errors must not be treated as false.
    /// This advisory check is not an atomic guard for external archive-file writes;
    /// callers must coordinate such writes with their own forget operation.
    pub fn is_session_forgotten(&self, project_id: &str, session_id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_forgotten_sessions WHERE project_id=?1 AND session_id=?2)",
            params![project_id, session_id], |row| row.get(0),
        )?)
    }

    /// Deletes records/vectors and persists a tombstone atomically, preventing
    /// future imports even if the session had no records. Returns records deleted.
    /// This is logical deletion, not a promise of forensic disk erasure.
    pub fn delete_session(&mut self, project_id: &str, session_id: &str) -> Result<usize> {
        validate_metadata(project_id)?;
        validate_metadata(session_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO memory_forgotten_sessions VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![project_id, session_id],
        )?;
        let count = transaction.execute(
            "DELETE FROM memory_records WHERE project_id=?1 AND session_id=?2",
            params![project_id, session_id],
        )?;
        transaction.commit()?;
        Ok(count)
    }

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

fn read_record(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        project_id: row.get(0)?,
        session_id: row.get(1)?,
        id: row.get(2)?,
        role: row.get(3)?,
        timestamp: row.get(4)?,
        source: row.get(5)?,
        text: row.get(6)?,
    })
}

fn validate_metadata(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > MAX_METADATA_BYTES {
        return Err(Error::InvalidInput(
            "metadata must be nonempty and at most 1024 bytes",
        ));
    }
    Ok(())
}

fn validate_embedding(embedding: &Embedding) -> Result<()> {
    validate_metadata(&embedding.model)?;
    if embedding.values.is_empty()
        || embedding.values.len() > MAX_EMBEDDING_DIMENSIONS
        || !embedding.values.iter().all(|v| v.is_finite())
        || embedding.values.iter().all(|&v| v == 0.0)
    {
        return Err(Error::InvalidInput(
            "embedding must have 1..=65536 finite values and nonzero norm",
        ));
    }
    Ok(())
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

fn floor_boundary(text: &str, mut offset: usize) -> usize {
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
