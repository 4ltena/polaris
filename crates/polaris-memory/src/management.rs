//! Bounded atomic management batches, durable project revisions and receipts.

use crate::{Embedding, Error, MemoryStore, Record, Result, validate_embedding, validate_metadata};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MAX_CHANGE_RECORDS: usize = 100;
pub const MAX_CHANGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryChangeKindV1 {
    Import,
    Embed,
    Index,
    Forget,
}

impl MemoryChangeKindV1 {
    pub fn inventory(self) -> &'static str {
        match self {
            Self::Import => "C14",
            Self::Embed => "C18",
            Self::Index => "C19",
            Self::Forget => "C20",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEmbeddingUpdateV1 {
    pub session_id: String,
    pub id: String,
    pub embedding: Embedding,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryChangeV1 {
    Import {
        records: Vec<Record>,
    },
    Embed {
        updates: Vec<MemoryEmbeddingUpdateV1>,
    },
    Index {
        updates: Vec<MemoryEmbeddingUpdateV1>,
    },
    Forget {
        session_id: String,
    },
}

impl MemoryChangeV1 {
    pub fn kind(&self) -> MemoryChangeKindV1 {
        match self {
            Self::Import { .. } => MemoryChangeKindV1::Import,
            Self::Embed { .. } => MemoryChangeKindV1::Embed,
            Self::Index { .. } => MemoryChangeKindV1::Index,
            Self::Forget { .. } => MemoryChangeKindV1::Forget,
        }
    }
    pub fn count(&self) -> usize {
        match self {
            Self::Import { records } => records.len(),
            Self::Embed { updates } | Self::Index { updates } => updates.len(),
            Self::Forget { .. } => 1,
        }
    }
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Forget { session_id } => Some(session_id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryChangeRequestV1 {
    pub operation_id: String,
    pub project_id: String,
    pub expected_revision: u64,
    pub change: MemoryChangeV1,
}

impl MemoryChangeRequestV1 {
    /// Hash only after checking the actual bounded payload. No body is persisted.
    pub fn request_hash(&self) -> Result<String> {
        validate_metadata(&self.project_id)?;
        if self.operation_id.len() != 32
            || !self
                .operation_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::InvalidInput("invalid memory operation ID"));
        }
        if self.expected_revision > i64::MAX as u64
            || self.change.count() == 0
            || self.change.count() > MAX_CHANGE_RECORDS
        {
            return Err(Error::InvalidInput(
                "memory batch must contain 1..=100 records",
            ));
        }
        let mut keys = BTreeSet::new();
        match &self.change {
            MemoryChangeV1::Import { records } => {
                for record in records {
                    if record.project_id != self.project_id {
                        return Err(Error::WrongProject);
                    }
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
                    validate_session_id(&record.session_id)?;
                    if record.text.len() > MAX_CHANGE_BYTES
                        || !keys.insert((&record.session_id, &record.id))
                    {
                        return Err(Error::InvalidInput("duplicate or oversized memory record"));
                    }
                }
            }
            MemoryChangeV1::Embed { updates } | MemoryChangeV1::Index { updates } => {
                if matches!(&self.change, MemoryChangeV1::Embed { .. }) && updates.len() != 1 {
                    return Err(Error::InvalidInput("embed requires exactly one record"));
                }
                for update in updates {
                    validate_session_id(&update.session_id)?;
                    validate_metadata(&update.id)?;
                    validate_embedding(&update.embedding)?;
                    if !keys.insert((&update.session_id, &update.id)) {
                        return Err(Error::InvalidInput("duplicate embedding target"));
                    }
                }
            }
            MemoryChangeV1::Forget { session_id } => validate_session_id(session_id)?,
        }
        let mut bounded = BoundedRequestBytes(Vec::new());
        serde_json::to_writer(&mut bounded, self)
            .map_err(|_| Error::InvalidInput("memory batch exceeds byte limit"))?;
        Ok(format!("{:x}", Sha256::digest(bounded.0)))
    }
}

struct BoundedRequestBytes(Vec<u8>);
impl std::io::Write for BoundedRequestBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_CHANGE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("memory request exceeds byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn validate_session_id(session: &str) -> Result<()> {
    validate_metadata(session)?;
    if !session
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::InvalidInput("invalid memory session ID"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryChangeReceiptV1 {
    pub operation_id: String,
    pub request_hash: String,
    pub kind: MemoryChangeKindV1,
    pub session_id: Option<String>,
    pub before_revision: u64,
    pub after_revision: u64,
    pub records_changed: usize,
    pub conversation_rows_deleted: usize,
}

pub(crate) fn initialize(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_project_revisions (
        project_id TEXT PRIMARY KEY NOT NULL,
        revision INTEGER NOT NULL CHECK(typeof(revision)='integer' AND revision>=0)
    ); CREATE TABLE IF NOT EXISTS memory_operation_receipts (
        project_id TEXT NOT NULL, operation_id TEXT NOT NULL, request_hash TEXT NOT NULL,
        receipt TEXT NOT NULL, PRIMARY KEY(project_id, operation_id)
    );",
    )?;
    for table in [
        "memory_records",
        "memory_vectors",
        "memory_forgotten_sessions",
        "conversation_sources_v2",
        "conversation_summaries_v2",
        "conversation_vectors_v2",
    ] {
        for (event, reference) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
            connection.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS {table}_management_{event} AFTER {event} ON {table} BEGIN
                INSERT INTO memory_project_revisions(project_id,revision) VALUES({reference}.project_id,1)
                ON CONFLICT(project_id) DO UPDATE SET revision=revision+1;
                END;"))?;
        }
    }
    Ok(())
}

fn revision(connection: &Connection, project: &str) -> Result<u64> {
    let value: Option<i64> = connection
        .query_row(
            "SELECT revision FROM memory_project_revisions WHERE project_id=?1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    u64::try_from(value.unwrap_or(0))
        .map_err(|_| Error::InvalidInput("invalid stored memory revision"))
}

fn receipt(
    connection: &Connection,
    project: &str,
    operation: &str,
) -> Result<Option<MemoryChangeReceiptV1>> {
    let value: Option<(String, String)> = connection.query_row("SELECT request_hash,receipt FROM memory_operation_receipts WHERE project_id=?1 AND operation_id=?2", params![project, operation], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    value
        .map(|(hash, json)| {
            let receipt: MemoryChangeReceiptV1 = serde_json::from_str(&json)
                .map_err(|_| Error::InvalidInput("invalid stored memory receipt"))?;
            if receipt.operation_id != operation
                || receipt.request_hash != hash
                || receipt.after_revision < receipt.before_revision
            {
                return Err(Error::InvalidInput("memory receipt binding mismatch"));
            }
            Ok(receipt)
        })
        .transpose()
}

fn reject_forgotten(connection: &Connection, project: &str, session: &str) -> Result<()> {
    let forgotten: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_forgotten_sessions WHERE project_id=?1 AND session_id=?2)", params![project, session], |r| r.get(0))?;
    if forgotten {
        Err(Error::SessionForgotten)
    } else {
        Ok(())
    }
}

impl MemoryStore {
    pub fn database_path(&self) -> Option<&str> {
        self.connection.path()
    }
    pub fn project_revision(&self, project: &str) -> Result<u64> {
        validate_metadata(project)?;
        revision(&self.connection, project)
    }
    /// Explicit receipt lookup: missing and read errors remain distinct.
    pub fn change_receipt(
        &self,
        project: &str,
        operation: &str,
    ) -> Result<Option<MemoryChangeReceiptV1>> {
        receipt(&self.connection, project, operation)
    }
    pub fn validate_change(
        &self,
        project: &str,
        request: &MemoryChangeRequestV1,
    ) -> Result<String> {
        if request.project_id != project {
            return Err(Error::WrongProject);
        }
        let hash = request.request_hash()?;
        if let Some(prior) = receipt(&self.connection, project, &request.operation_id)? {
            return if prior.request_hash == hash {
                Ok(hash)
            } else {
                Err(Error::OperationConflict)
            };
        }
        if revision(&self.connection, project)? != request.expected_revision {
            return Err(Error::StaleRevision);
        }
        validate_targets(&self.connection, project, &request.change)?;
        Ok(hash)
    }
    /// Mutations and receipt share one immediate SQLite transaction. Replay
    /// returns the original receipt before stale/tombstone checks, without writes.
    pub fn apply_change(
        &mut self,
        project: &str,
        request: &MemoryChangeRequestV1,
    ) -> Result<MemoryChangeReceiptV1> {
        if request.project_id != project {
            return Err(Error::WrongProject);
        }
        let hash = request.request_hash()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(prior) = receipt(&tx, project, &request.operation_id)? {
            return if prior.request_hash == hash {
                Ok(prior)
            } else {
                Err(Error::OperationConflict)
            };
        }
        let before = revision(&tx, project)?;
        if before != request.expected_revision {
            return Err(Error::StaleRevision);
        }
        validate_targets(&tx, project, &request.change)?;
        let mut changed = 0;
        let mut cleaned = 0;
        match &request.change {
            MemoryChangeV1::Import { records } => {
                for r in records {
                    tx.execute("INSERT INTO memory_records VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(project_id,session_id,id) DO UPDATE SET role=excluded.role,timestamp=excluded.timestamp,source=excluded.source,text=excluded.text", params![r.project_id,r.session_id,r.id,r.role,r.timestamp,r.source,r.text])?;
                    tx.execute("DELETE FROM memory_vectors WHERE project_id=?1 AND session_id=?2 AND id=?3", params![project,r.session_id,r.id])?;
                    changed += 1;
                }
            }
            MemoryChangeV1::Embed { updates } | MemoryChangeV1::Index { updates } => {
                for u in updates {
                    let bytes: Vec<u8> = u
                        .embedding
                        .values
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect();
                    tx.execute("INSERT INTO memory_vectors VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(project_id,session_id,id) DO UPDATE SET model=excluded.model,dimension=excluded.dimension,embedding=excluded.embedding", params![project,u.session_id,u.id,u.embedding.model,u.embedding.values.len() as i64,bytes])?;
                    changed += 1;
                }
            }
            MemoryChangeV1::Forget { session_id } => {
                tx.execute(
                    "INSERT INTO memory_forgotten_sessions VALUES(?1,?2)",
                    params![project, session_id],
                )?;
                changed = tx.execute(
                    "DELETE FROM memory_records WHERE project_id=?1 AND session_id=?2",
                    params![project, session_id],
                )?;
                cleaned = tx.execute(
                    "DELETE FROM conversation_sources_v2 WHERE project_id=?1 AND session_id=?2",
                    params![project, session_id],
                )?;
            }
        }
        let receipt = MemoryChangeReceiptV1 {
            operation_id: request.operation_id.clone(),
            request_hash: hash.clone(),
            kind: request.change.kind(),
            session_id: request.change.session_id().map(str::to_owned),
            before_revision: before,
            after_revision: revision(&tx, project)?,
            records_changed: changed,
            conversation_rows_deleted: cleaned,
        };
        let json = serde_json::to_string(&receipt)
            .map_err(|_| Error::InvalidInput("invalid memory receipt"))?;
        tx.execute(
            "INSERT INTO memory_operation_receipts VALUES(?1,?2,?3,?4)",
            params![project, request.operation_id, hash, json],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
}

fn validate_targets(connection: &Connection, project: &str, change: &MemoryChangeV1) -> Result<()> {
    match change {
        MemoryChangeV1::Import { records } => {
            for r in records {
                reject_forgotten(connection, project, &r.session_id)?;
            }
        }
        MemoryChangeV1::Embed { updates } | MemoryChangeV1::Index { updates } => {
            for u in updates {
                reject_forgotten(connection, project, &u.session_id)?;
                let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_records WHERE project_id=?1 AND session_id=?2 AND id=?3)", params![project,u.session_id,u.id], |r|r.get(0))?;
                if !exists {
                    return Err(Error::RecordNotFound);
                }
            }
        }
        MemoryChangeV1::Forget { session_id } => reject_forgotten(connection, project, session_id)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests;
