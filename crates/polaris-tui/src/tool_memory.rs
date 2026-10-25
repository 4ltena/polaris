//! Immutable, scoped tool output storage behind the reserved memory read path.
use crate::memory::lock_memory;
use futures_util::future::BoxFuture;
use polaris_core::{
    session::Session,
    tool_memory::{RetentionMode, SavedToolResult, ToolMemory, ToolMemoryBackend},
};
use polaris_memory::{Embedding, MemoryStore, Record, SearchMode, SearchRequest};
use polaris_provider::ToolCall;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

const RAW: &str = "tool-result-raw";
const CHUNK: &str = "tool-result-chunk";
const PAYLOAD: usize = 3500;

pub fn configure_tool_memory(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    session_id: &str,
    mode: RetentionMode,
    embedding: Option<(String, String)>,
) -> io::Result<()> {
    configure_tool_memory_with_origins(
        session,
        project,
        state_dir,
        session_id,
        &[],
        mode,
        embedding,
    )
}

/// Ancestors are read-only; all new records belong to the current session.
#[allow(clippy::too_many_arguments)]
pub fn configure_tool_memory_with_origins(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    current_session_id: &str,
    origins: &[String],
    mode: RetentionMode,
    embedding: Option<(String, String)>,
) -> io::Result<()> {
    validate_session_ids(current_session_id, origins)?;
    configure_tool_memory_inner(
        session,
        project,
        state_dir,
        current_session_id,
        origins,
        None,
        mode,
        embedding,
    )
}

/// Configures a rewind branch whose origin records are restricted to a durable
/// raw-object checkpoint. New records in the branch's own session stay visible.
#[allow(clippy::too_many_arguments)]
pub fn configure_tool_memory_with_origin_allowlist(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    current_session_id: &str,
    origins: &[String],
    visible_origin_raw_ids: &[String],
    mode: RetentionMode,
    embedding: Option<(String, String)>,
) -> io::Result<()> {
    validate_session_ids(current_session_id, origins)?;
    configure_tool_memory_inner(
        session,
        project,
        state_dir,
        current_session_id,
        origins,
        Some(validate_raw_ids(visible_origin_raw_ids)?),
        mode,
        embedding,
    )
}

#[allow(clippy::too_many_arguments)]
fn configure_tool_memory_inner(
    session: &mut Session,
    project: &Path,
    state_dir: &Path,
    current_session_id: &str,
    origins: &[String],
    visible_origin_raw_ids: Option<BTreeSet<String>>,
    mode: RetentionMode,
    embedding: Option<(String, String)>,
) -> io::Result<()> {
    let project = polaris_core::project::resolve_root(project);
    let backend = Backend {
        project: crate::persist::project_identity(&project)?,
        session: current_session_id.into(),
        origins: origins.to_vec(),
        visible_origin_raw_ids,
        state: state_dir.into(),
        embedding: embedding
            .map(|(url, model)| Encoder::new(&url, model))
            .transpose()?,
    };
    check_paths(state_dir, true)?;
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(state_dir)?;
    check_paths(state_dir, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Chmod the checked directory handle, never a potentially substituted pathname.
        let directory = std::fs::File::open(state_dir)?;
        let opened = directory.metadata()?;
        let current = std::fs::symlink_metadata(state_dir)?;
        if !opened.is_dir()
            || current.file_type().is_symlink()
            || opened.ino() != current.ino()
            || opened.dev() != current.dev()
            || opened.uid() != effective_uid()
            || opened.permissions().mode() & 0o022 != 0
        {
            return Err(io::Error::other("Memory storage owner or state changed"));
        }
        if opened.permissions().mode() & 0o077 != 0 {
            directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
    }
    validate_paths(state_dir)?;
    session.tool_memory = Some(ToolMemory {
        backend: Arc::new(backend),
        mode,
        threshold_bytes: 8192,
    });
    Ok(())
}

/// Lists only raw objects that this configuration can actually retrieve.
///
/// Callers persist this result in a rewind checkpoint; malformed IDs and rows
/// outside the configured project/session provenance are excluded.
#[allow(clippy::too_many_arguments)]
pub fn visible_tool_memory_raw_object_ids(
    project: &Path,
    state_dir: &Path,
    current_session_id: &str,
    origins: &[String],
    visible_origin_raw_ids: Option<&[String]>,
) -> io::Result<Vec<String>> {
    validate_session_ids(current_session_id, origins)?;
    let backend = Backend {
        project: crate::persist::project_identity(&polaris_core::project::resolve_root(project))?,
        session: current_session_id.into(),
        origins: origins.to_vec(),
        visible_origin_raw_ids: visible_origin_raw_ids.map(validate_raw_ids).transpose()?,
        state: state_dir.into(),
        embedding: None,
    };
    backend.visible_raw_object_ids()
}

fn validate_session_ids(current_session_id: &str, origins: &[String]) -> io::Result<()> {
    for id in std::iter::once(current_session_id).chain(origins.iter().map(String::as_str)) {
        if id.is_empty()
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(io::Error::other("Invalid memory session ID"));
        }
    }
    Ok(())
}

fn validate_raw_ids(ids: &[String]) -> io::Result<BTreeSet<String>> {
    if ids.iter().any(|id| !is_raw_id(id)) {
        return Err(io::Error::other("Invalid memory source ID"));
    }
    Ok(ids.iter().cloned().collect())
}

fn is_raw_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Refuse aliases and shared writable state before opening SQLite or the shared lock.
/// The private directory is the trust boundary; these checks are not an openat API.
#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // POSIX geteuid has no arguments, pointer access, or failure case.
    unsafe { geteuid() }
}
fn validate_paths(state: &Path) -> io::Result<()> {
    check_paths(state, false)
}
fn check_paths(state: &Path, allow_readable_directory: bool) -> io::Result<()> {
    if state
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::other(
            "Storage paths cannot contain parent-directory references",
        ));
    }
    let absolute = if state.is_absolute() {
        state.to_path_buf()
    } else {
        std::env::current_dir()?.join(state)
    };
    for path in absolute.ancestors() {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(io::Error::other(
                    "The memory storage path contains a link or non-directory",
                ));
            }
            Ok(meta) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    let mode = meta.permissions().mode();
                    if mode & 0o022 != 0 && !(mode & 0o1000 != 0 && meta.uid() == 0) {
                        return Err(io::Error::other(
                            "The memory storage path is writable by other users",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if let Ok(meta) = std::fs::symlink_metadata(&absolute) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let forbidden = if allow_readable_directory {
                0o022
            } else {
                0o077
            };
            if meta.uid() != effective_uid() || meta.permissions().mode() & forbidden != 0 {
                return Err(io::Error::other(
                    "Memory storage must be an owner-only directory (permissions are not changed)",
                ));
            }
        }
    }
    for name in [
        "memory.sqlite3",
        "memory.lock",
        "memory.sqlite3-wal",
        "memory.sqlite3-shm",
        "memory.sqlite3-journal",
    ] {
        match std::fs::symlink_metadata(absolute.join(name)) {
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err(io::Error::other(
                        "A memory file is a link or non-regular file",
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    if meta.uid() != effective_uid()
                        || meta.nlink() != 1
                        || meta.permissions().mode() & 0o077 != 0
                    {
                        return Err(io::Error::other("Cannot open a shared memory file"));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct Backend {
    project: String,
    session: String,
    origins: Vec<String>,
    /// `None` preserves the ordinary resume/fork visibility contract.
    /// `Some` gates records stored in ancestor sessions by raw object ID.
    visible_origin_raw_ids: Option<BTreeSet<String>>,
    state: PathBuf,
    embedding: Option<Encoder>,
}
impl Backend {
    fn lock(&self) -> io::Result<std::fs::File> {
        validate_paths(&self.state)?;
        let lock = lock_memory(&self.state)?;
        validate_paths(&self.state)?;
        Ok(lock)
    }
    fn store(&self) -> io::Result<MemoryStore> {
        validate_paths(&self.state)?;
        let store =
            MemoryStore::open(self.state.join("memory.sqlite3")).map_err(io::Error::other)?;
        for session in self.sessions() {
            if store
                .is_session_forgotten(&self.project, session)
                .map_err(io::Error::other)?
            {
                return Err(io::Error::other(
                    "Memory from a deleted session is unavailable",
                ));
            }
        }
        Ok(store)
    }
    fn sessions(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.session.as_str()).chain(self.origins.iter().map(String::as_str))
    }
    fn raw_id(record: &Record) -> Option<&str> {
        match record.role.as_str() {
            RAW if is_raw_id(&record.id) => Some(&record.id),
            CHUNK => record
                .id
                .split_once("-c")
                .filter(|(raw_id, suffix)| {
                    is_raw_id(raw_id)
                        && suffix.len() == 8
                        && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
                .map(|(raw_id, _)| raw_id),
            _ => None,
        }
    }
    fn record_is_visible(&self, record: &Record) -> bool {
        if record.project_id != self.project {
            return false;
        }
        Self::raw_id(record)
            .is_some_and(|raw_id| self.raw_id_is_visible_for_session(&record.session_id, raw_id))
    }
    fn raw_id_is_visible_for_session(&self, session: &str, raw_id: &str) -> bool {
        if session == self.session {
            return true;
        }
        self.origins.iter().any(|origin| origin == session)
            && self
                .visible_origin_raw_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(raw_id))
    }
    fn visible_raw_object_ids(&self) -> io::Result<Vec<String>> {
        let _lock = self.lock()?;
        let store = self.store()?;
        let mut ids = BTreeSet::new();
        for session in self.sessions() {
            let mut cursor = None;
            loop {
                let page = store
                    .records_page(&self.project, session, cursor.as_deref(), 100)
                    .map_err(io::Error::other)?;
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map(|record| record.id.clone());
                ids.extend(
                    page.into_iter()
                        .filter(|record| {
                            record.role == RAW
                                && is_raw_id(&record.id)
                                && self.record_is_visible(record)
                        })
                        .map(|record| record.id),
                );
            }
        }
        Ok(ids.into_iter().collect())
    }
    fn get(&self, store: &MemoryStore, id: &str, role: &str) -> io::Result<Record> {
        for session in self.sessions() {
            if let Some(record) = store
                .get(&self.project, session, id)
                .map_err(io::Error::other)?
                .filter(|record| record.role == role && self.record_is_visible(record))
            {
                return Ok(record);
            }
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "Memory not found"))
    }
    fn insert(&self, store: &mut MemoryStore, record: &Record) -> io::Result<bool> {
        if record.project_id != self.project || record.session_id != self.session {
            return Err(io::Error::other(
                "The memory write destination is not the current session",
            ));
        }
        if let Some(old) = store
            .get(&self.project, &self.session, &record.id)
            .map_err(io::Error::other)?
        {
            if old.text != record.text || old.role != record.role || old.source != record.source {
                return Err(io::Error::other("Different content for the same memory ID"));
            }
            return Ok(false);
        }
        store.upsert(record, None).map_err(io::Error::other)?;
        Ok(true)
    }
    fn persist(&self, call: &ToolCall, text: &str) -> io::Result<(String, Vec<Record>)> {
        let provenance = serde_json::to_vec(&(
            &self.project,
            &self.session,
            &call.id,
            &call.name,
            &call.arguments,
            text,
        ))
        .map_err(io::Error::other)?;
        let id = format!("{:x}", Sha256::digest(provenance));
        // Only a bounded tool name and opaque call digest enter searchable metadata.
        let tool: String = call
            .name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        let source = format!("tool={tool};call={:x}", Sha256::digest(call.id.as_bytes()));
        let raw = Record {
            project_id: self.project.clone(),
            session_id: self.session.clone(),
            id: id.clone(),
            role: RAW.into(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_millis()
                .to_string(),
            source,
            text: text.into(),
        };
        let _lock = self.lock()?;
        let mut store = self.store()?;
        self.insert(&mut store, &raw)?;
        let boundaries: Vec<_> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut added = Vec::new();
        for (number, start) in (0..boundaries.len() - 1).step_by(2000).enumerate() {
            let a = boundaries[start];
            let b = boundaries[(start + 2000).min(boundaries.len() - 1)];
            let line = text[..a].bytes().filter(|&b| b == b'\n').count();
            let chunk = Record {
                id: format!("{id}-c{number:08}"),
                role: CHUNK.into(),
                source: format!("memory://{id}/bytes/{a};offset={line};end={b}"),
                text: text[a..b].into(),
                ..raw.clone()
            };
            let inserted = self.insert(&mut store, &chunk)?;
            let pending = match &self.embedding {
                Some(encoder) => !store
                    .has_embedding(&self.project, &self.session, &chunk.id, &encoder.model)
                    .map_err(io::Error::other)?,
                None => inserted,
            };
            if pending {
                added.push(chunk);
            }
        }
        if self.get(&store, &id, RAW)?.text != text {
            return Err(io::Error::other("Memory source reread verification failed"));
        }
        Ok((id, added))
    }
    fn search(
        &self,
        query: &str,
        vector: Option<&Embedding>,
        diagnostic: &str,
    ) -> io::Result<String> {
        let _lock = self.lock()?;
        let store = self.store()?;
        // Scan immutable raw text so lexical matches can span vector chunk boundaries.
        let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut lexical = Vec::new();
        for session in self.sessions() {
            let mut cursor = None;
            loop {
                let page = store
                    .records_page(&self.project, session, cursor.as_deref(), 100)
                    .map_err(io::Error::other)?;
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map(|r| r.id.clone());
                for record in page {
                    if record.role != RAW
                        || !is_raw_id(&record.id)
                        || !self.record_is_visible(&record)
                    {
                        continue;
                    }
                    let lower = record.text.to_lowercase();
                    if !terms.is_empty() && terms.iter().all(|term| lower.contains(term)) {
                        let score: usize =
                            terms.iter().map(|term| lower.matches(term).count()).sum();
                        lexical.push((record, score));
                    }
                }
            }
        }
        lexical.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        let mut ranked: std::collections::HashMap<String, (Record, f64)> = lexical
            .into_iter()
            .enumerate()
            .map(|(rank, (record, _))| (record.id.clone(), (record, 1.0 / (61 + rank) as f64)))
            .collect();
        let mut semantic_count = 0;
        if let Some(vector) = vector {
            for session in self.sessions() {
                let request = SearchRequest {
                    project_id: &self.project,
                    session_id: Some(session),
                    mode: SearchMode::Semantic(vector),
                    limit: 100,
                    excerpt_bytes: 320,
                };
                for (rank, hit) in store
                    .search(&request)
                    .map_err(io::Error::other)?
                    .into_iter()
                    .filter(|hit| {
                        hit.role == CHUNK
                            && hit.id.split_once("-c").is_some_and(|(raw_id, suffix)| {
                                is_raw_id(raw_id)
                                    && suffix.len() == 8
                                    && suffix.bytes().all(|byte| byte.is_ascii_digit())
                                    && self.raw_id_is_visible_for_session(session, raw_id)
                            })
                    })
                    .enumerate()
                {
                    semantic_count += 1;
                    let record = self.get(&store, &hit.id, CHUNK)?;
                    let Some((raw_id, _)) = hit.id.split_once("-c") else {
                        continue;
                    };
                    ranked.entry(raw_id.to_owned()).or_insert((record, 0.0)).1 +=
                        1.0 / (61 + rank) as f64;
                }
            }
        }
        let mut hits: Vec<_> = ranked.into_values().collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        let mut output =
            format!("{diagnostic}\nExcerpts of matches (not all matches or full text)\n");
        if vector.is_some() && semantic_count == 0 {
            output.push_str("No usable semantic candidates: falling back to keyword search\n");
        }
        let mut seen = std::collections::HashSet::new();
        for (hit, _) in hits {
            let (raw_id, base) = if hit.role == RAW {
                (hit.id.as_str(), 0)
            } else {
                let Some((raw_id, _)) = hit.id.split_once("-c") else {
                    continue;
                };
                let base = hit
                    .source
                    .split("/bytes/")
                    .nth(1)
                    .and_then(|s| s.split(';').next())
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                (raw_id, base)
            };
            if !seen.insert(raw_id.to_owned()) {
                continue;
            }
            let raw = self.get(&store, raw_id, RAW)?;
            let ranges = excerpt_ranges(&raw.text, &terms, base);
            for (start, end) in ranges {
                let first = raw.text[..start].bytes().filter(|&b| b == b'\n').count() + 1;
                let last = first + raw.text[start..end].bytes().filter(|&b| b == b'\n').count();
                let row = format!(
                    "memory://{raw_id}/bytes/{start}\nSaved body lines {first}–{last} (excerpt)\n{}\n",
                    &raw.text[start..end]
                );
                if output.len() + row.len() > 4000 {
                    output.push_str(
                        "Additional candidates omitted. Retrieve the original text if needed.\n",
                    );
                    return Ok(output);
                }
                output.push_str(&row);
            }
        }
        Ok(output)
    }
    fn search_scoped(&self, id: &str, query: &str) -> io::Result<String> {
        let _lock = self.lock()?;
        let store = self.store()?;
        // Resolve provenance before matching. A missing, foreign, or forgotten ID
        // must never broaden into the global search path.
        let raw = self.get(&store, id, RAW).map_err(|_| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "The specified memory ID is not in the current project or an allowed ancestor",
            )
        })?;
        let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        let lower = raw.text.to_lowercase();
        let mut output = format!(
            "Record-scoped keyword search\nmemory://{id}\nExcerpts of matches (not all matches or full text)\n"
        );
        if terms.is_empty() || !terms.iter().all(|term| lower.contains(term)) {
            return Ok(output);
        }
        for (start, end) in excerpt_ranges(&raw.text, &terms, 0) {
            let first = raw.text[..start].bytes().filter(|&b| b == b'\n').count() + 1;
            let last = first + raw.text[start..end].bytes().filter(|&b| b == b'\n').count();
            let row = format!(
                "memory://{id}/bytes/{start}\nSaved body lines {first}–{last} (excerpt)\n{}\n",
                &raw.text[start..end]
            );
            if output.len() + row.len() > 4000 {
                output.push_str(
                    "Additional candidates omitted. Retrieve the original text if needed.\n",
                );
                break;
            }
            output.push_str(&row);
        }
        Ok(output)
    }
}
impl ToolMemoryBackend for Backend {
    fn save<'a>(
        &'a self,
        call: &'a ToolCall,
        text: &'a str,
    ) -> BoxFuture<'a, io::Result<SavedToolResult>> {
        Box::pin(async move {
            let (id, chunks) = self.persist(call, text)?;
            if let Some(encoder) = &self.embedding {
                for chunk in chunks {
                    let vector = encoder.encode(&chunk.text).await.map_err(|_| {
                        io::Error::other(
                            "Original text saved, but embedding failed. Full text is retained",
                        )
                    })?;
                    let _lock = self.lock()?;
                    let mut store = self.store()?;
                    self.get(&store, &chunk.id, CHUNK)?;
                    store
                        .set_embedding(&self.project, &self.session, &chunk.id, &vector)
                        .map_err(io::Error::other)?;
                }
            }
            // Forget may have run while awaiting an embedding response.
            {
                let _lock = self.lock()?;
                let store = self.store()?;
                if self.get(&store, &id, RAW)?.text != text {
                    return Err(io::Error::other("Memory source mismatch"));
                }
            }
            Ok(SavedToolResult {
                id,
                preview: text.chars().take(128).collect(),
                bytes: text.len(),
            })
        })
    }
    fn read<'a>(
        &'a self,
        path: &'a str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<'a, io::Result<String>> {
        Box::pin(async move {
            let target = path
                .strip_prefix("memory://")
                .ok_or_else(|| io::Error::other("Invalid memory URI"))?;
            if let Some(query) = target.strip_prefix("search/") {
                if query.is_empty() || query.len() > 4096 {
                    return Err(io::Error::other("Specify a search query of 1–4096 bytes"));
                }
                // Check tombstones before sending any query to the encoder, too.
                {
                    let _lock = self.lock()?;
                    self.store()?;
                }
                let vector = match &self.embedding {
                    Some(e) => e.encode(query).await,
                    None => Err(io::Error::other("not configured")),
                };
                let label = if vector.is_ok() {
                    "hybrid search"
                } else if self.embedding.is_some() {
                    "embedding failed: falling back to keyword search"
                } else {
                    "keyword search (embedding not configured)"
                };
                return self.search(query, vector.as_ref().ok(), label);
            }
            if let Some((id, query)) = target.split_once("/search/") {
                if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(io::Error::other("Invalid source ID"));
                }
                if query.trim().is_empty() || query.len() > 4096 {
                    return Err(io::Error::other(
                        "Specify a record-scoped search query of 1–4096 bytes",
                    ));
                }
                return self.search_scoped(id, query);
            }
            let (id, byte) = match target.split_once("/bytes/") {
                Some((id, byte)) => (id, byte.parse::<usize>().map_err(io::Error::other)?),
                None => (target, 0),
            };
            if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::other("Invalid source ID"));
            }
            let _lock = self.lock()?;
            let store = self.store()?;
            let record = self.get(&store, id, RAW)?;
            if target.contains("/bytes/") {
                bounded_read(&record.text, id, 0, usize::MAX, byte)
            } else {
                bounded_read(&record.text, id, offset, limit, 0)
            }
        })
    }
}
// Keep separate evidence locations, including late corrections, within a fixed budget.
fn excerpt_ranges(text: &str, terms: &[String], fallback: usize) -> Vec<(usize, usize)> {
    let mut points = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let Some(relative) = match_offset(&text[cursor..], terms) else {
            break;
        };
        let at = cursor + relative;
        points.push(at);
        cursor = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    if points.is_empty() {
        points.push(floor(text, fallback));
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for at in points {
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        let line_end = text[at..].find('\n').map_or(text.len(), |n| at + n);
        let (mut start, mut end) = if line_end - line_start <= 768 {
            (line_start, line_end)
        } else {
            let start = floor(text, at.saturating_sub(80));
            (start, floor(text, start + 768))
        };
        // Expand to paragraph boundaries only while complete lines fit.
        while start > 0 && text.as_bytes()[start - 1] == b'\n' {
            let previous_end = start - 1;
            let previous_start = text[..previous_end].rfind('\n').map_or(0, |n| n + 1);
            if text[previous_start..previous_end].trim().is_empty() || end - previous_start > 768 {
                break;
            }
            start = previous_start;
        }
        while end < text.len() && text.as_bytes()[end] == b'\n' {
            let next_start = end + 1;
            let next_end = text[next_start..]
                .find('\n')
                .map_or(text.len(), |n| next_start + n);
            if text[next_start..next_end].trim().is_empty() || next_end - start > 768 {
                break;
            }
            end = next_end;
        }
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 && end.max(last.1) - last.0 <= 900 {
                last.1 = last.1.max(end);
                continue;
            }
            // Avoid repeating overlapping bytes even when a merged range is too large.
            start = start.max(last.1);
        }
        if start < end {
            ranges.push((start, end));
        }
    }
    if ranges.len() > 3 {
        let last = *ranges.last().unwrap();
        ranges.truncate(2);
        ranges.push(last);
    }
    ranges
}

// Map byte offsets in lowercase text back to the original UTF-8 character.
fn match_offset(text: &str, terms: &[String]) -> Option<usize> {
    let lower = text.to_lowercase();
    let target = terms.iter().filter_map(|term| lower.find(term)).min()?;
    let mut offset = 0;
    for (original, ch) in text.char_indices() {
        offset += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
        if offset > target {
            return Some(original);
        }
    }
    None
}
fn floor(text: &str, max: usize) -> usize {
    let mut n = max.min(text.len());
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    n
}
fn bounded_read(
    text: &str,
    id: &str,
    offset: usize,
    limit: usize,
    byte: usize,
) -> io::Result<String> {
    if byte > text.len() || !text.is_char_boundary(byte) {
        return Err(io::Error::other("Invalid UTF-8 retrieval offset"));
    }
    if limit == 0 {
        return Ok(String::new());
    }
    let start = text
        .split_inclusive('\n')
        .take(offset)
        .map(str::len)
        .sum::<usize>()
        .max(byte);
    let end_lines = start
        + text[start..]
            .split_inclusive('\n')
            .take(limit)
            .map(str::len)
            .sum::<usize>();
    let end = floor(text, end_lines.min(start.saturating_add(PAYLOAD)));
    let mut result = text[start..end].to_owned();
    if end < text.len() {
        result.push_str(&format!("\n[Continue: memory://{id}/bytes/{end}]\n"));
    }
    Ok(result)
}
#[cfg(test)]
type TestEncoder = Arc<dyn Fn(&str) -> io::Result<Embedding> + Send + Sync>;

pub struct Encoder {
    client: reqwest::Client,
    url: url::Url,
    model: String,
    #[cfg(test)]
    fake: Option<TestEncoder>,
}
impl Encoder {
    pub fn new(base: &str, model: String) -> io::Result<Self> {
        let mut url =
            url::Url::parse(base).map_err(|_| io::Error::other("Invalid embedding URL"))?;
        let local = match url.host_str() {
            Some("localhost") => true,
            Some(host) => host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback()),
            None => false,
        };
        if !local
            || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || model.trim().is_empty()
            || model.len() > 1024
        {
            return Err(io::Error::other(
                "Specify a credential-free loopback URL and model for embeddings",
            ));
        }
        // Pin localhost to a literal address; never delegate its resolution to DNS.
        if url.host_str() == Some("localhost") {
            url.set_host(Some("127.0.0.1")).map_err(io::Error::other)?;
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            url,
            model,
            #[cfg(test)]
            fake: None,
        })
    }
    pub async fn encode(&self, text: &str) -> io::Result<Embedding> {
        #[cfg(test)]
        if let Some(fake) = &self.fake {
            return fake(text);
        }
        let mut attempt = EmbeddingAttempt {
            input_tokens: None,
            ok: false,
        };
        let mut response = self
            .client
            .post(self.url.clone())
            .json(&serde_json::json!({"model": self.model, "input": text}))
            .send()
            .await
            .map_err(io::Error::other)?;
        if !response.status().is_success() {
            return Err(io::Error::other("Embedding HTTP request failed"));
        }
        const MAX: usize = 1024 * 1024;
        if response.content_length().is_some_and(|n| n > MAX as u64) {
            return Err(io::Error::other("Embedding response exceeds the limit"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
            if bytes.len() + chunk.len() > MAX {
                return Err(io::Error::other("Embedding response exceeds the limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        attempt.input_tokens = body
            .pointer("/usage/input_tokens")
            .or_else(|| body.pointer("/usage/prompt_tokens"))
            .and_then(serde_json::Value::as_u64);
        let embedding = decode_embedding(&bytes, &self.model)?;
        attempt.ok = true;
        Ok(embedding)
    }
}
// Drop also records cancellation after an actual request future was polled.
struct EmbeddingAttempt {
    input_tokens: Option<u64>,
    ok: bool,
}
impl Drop for EmbeddingAttempt {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr().lock(),
            "tool-memory-embedding: {}",
            serde_json::json!({"input_tokens": self.input_tokens, "ok": self.ok})
        );
    }
}
fn decode_embedding(bytes: &[u8], model: &str) -> io::Result<Embedding> {
    let body: serde_json::Value = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    let values: Vec<f32> = serde_json::from_value(
        body.pointer("/data/0/embedding")
            .ok_or_else(|| io::Error::other("No embedding response"))?
            .clone(),
    )
    .map_err(io::Error::other)?;
    if values.is_empty()
        || values.len() > 65536
        || values.iter().any(|v| !v.is_finite())
        || values.iter().all(|v| *v == 0.0)
    {
        return Err(io::Error::other("Invalid embedding vector"));
    }
    Ok(Embedding {
        model: model.into(),
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn private_temp() -> tempfile::TempDir {
        let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }
    fn backend(state: &Path) -> Backend {
        Backend {
            project: "project".into(),
            session: "session".into(),
            origins: Vec::new(),
            visible_origin_raw_ids: None,
            state: state.into(),
            embedding: None,
        }
    }
    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path":"original", "private":"do-not-index"}),
        }
    }

    #[tokio::test]
    async fn immutable_raw_survives_source_change_and_chunks_are_deduplicated() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let text = format!("{}\nneedle\n", "日本語".repeat(2400));
        let source = dir.path().join("original");
        std::fs::write(&source, &text).unwrap();
        let saved = backend
            .save(&call(), &std::fs::read_to_string(&source).unwrap())
            .await
            .unwrap();
        std::fs::write(&source, "changed").unwrap();
        let store = backend.store().unwrap();
        assert_eq!(backend.get(&store, &saved.id, RAW).unwrap().text, text);
        let records = store.records_page("project", "session", None, 100).unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records
                .iter()
                .filter(|r| r.role == CHUNK)
                .map(|r| r.text.as_str())
                .collect::<String>(),
            text
        );
        assert!(
            records
                .iter()
                .all(|r| r.source.len() <= 1024 && !r.source.contains("do-not-index"))
        );
        assert!(backend.persist(&call(), &text).unwrap().1.is_empty());
        let read = backend
            .read(&format!("memory://{}", saved.id), 0, 100)
            .await
            .unwrap();
        assert!(read.len() <= 4096 && read.contains("[Continue:"));
        let search = backend.read("memory://search/needle", 0, 20).await.unwrap();
        assert!(search.contains(&saved.id) && search.contains("needle"));
    }

    #[tokio::test]
    async fn scoped_search_reopens_the_saved_snapshot_after_source_changes() {
        let dir = private_temp();
        let source = dir.path().join("original");
        std::fs::write(&source, "日本語 needle saved").unwrap();
        let saved = backend(dir.path())
            .save(&call(), &std::fs::read_to_string(&source).unwrap())
            .await
            .unwrap();
        std::fs::write(&source, "needle changed on disk").unwrap();

        let resumed = backend(dir.path());
        let result = resumed
            .read(&format!("memory://{}/search/needle", saved.id), 0, 10)
            .await
            .unwrap();
        assert!(result.contains("日本語 needle saved"));
        assert!(!result.contains("changed on disk"));
        assert!(
            result
                .lines()
                .any(|line| line == format!("memory://{}", saved.id))
        );
    }

    #[tokio::test]
    async fn byte_uri_roundtrip_recovers_giant_line_through_public_backend() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let text = format!("{}\nend", "日本🦀".repeat(2000));
        let saved = backend.save(&call(), &text).await.unwrap();
        let mut uri = format!("memory://{}", saved.id);
        let mut restored = String::new();
        loop {
            let page = backend.read(&uri, 0, 1).await.unwrap();
            assert!(page.len() <= 4096);
            if let Some((body, trailer)) = page.rsplit_once("\n[Continue: ") {
                restored.push_str(body);
                uri = trailer.strip_suffix("]\n").unwrap().to_owned();
                assert!(uri.starts_with(&format!("memory://{}/bytes/", saved.id)));
            } else {
                restored.push_str(&page);
                break;
            }
        }
        assert_eq!(restored, text);
        assert!(
            backend
                .read(&format!("memory://{}/bytes/1", saved.id), 0, 1)
                .await
                .is_err()
        );
    }

    #[test]
    fn utf8_continuation_restores_long_lines_exactly() {
        let text = format!("{}\nlast\n", "🦀日本語".repeat(2000));
        let mut restored = String::new();
        let mut byte = 0;
        loop {
            let page = bounded_read(&text, &"a".repeat(64), 0, 100, byte).unwrap();
            assert!(page.len() <= 4096);
            if let Some((body, continuation)) = page.rsplit_once("\n[Continue: ") {
                restored.push_str(body);
                let next = continuation
                    .split("/bytes/")
                    .nth(1)
                    .unwrap()
                    .split(']')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                assert!(next > byte);
                byte = next;
            } else {
                restored.push_str(&page);
                break;
            }
        }
        assert_eq!(restored, text);
        assert_eq!(
            bounded_read("one\ntwo\nthree", "id", 1, 1, 0).unwrap(),
            "two\n\n[Continue: memory://id/bytes/8]\n"
        );
        assert!(bounded_read("日本", "id", 0, 1, 1).is_err());
        assert_eq!(bounded_read("one", "id", usize::MAX, 1, 0).unwrap(), "");
    }

    #[tokio::test]
    async fn scope_record_types_and_forget_are_enforced() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let saved = backend.save(&call(), "needle original").await.unwrap();
        let mut store = backend.store().unwrap();
        let mut foreign = backend.get(&store, &saved.id, RAW).unwrap();
        foreign.id = "b".repeat(64);
        foreign.role = "user".into();
        store.upsert(&foreign, None).unwrap();
        assert!(
            backend
                .read(&format!("memory://{}", foreign.id), 0, 10)
                .await
                .is_err()
        );
        for (project, session) in [("other", "session"), ("project", "other")] {
            let other = Backend {
                project: project.into(),
                session: session.into(),
                ..self::backend(dir.path())
            };
            assert!(
                other
                    .read(&format!("memory://{}", saved.id), 0, 10)
                    .await
                    .is_err()
            );
            assert!(
                !other
                    .read("memory://search/needle", 0, 10)
                    .await
                    .unwrap()
                    .contains("original")
            );
        }
        store.delete_session("project", "session").unwrap();
        assert!(backend.save(&call(), "needle original").await.is_err());
        assert!(
            backend
                .read(&format!("memory://{}", saved.id), 0, 10)
                .await
                .is_err()
        );
        assert!(backend.read("memory://search/needle", 0, 10).await.is_err());
    }

    #[tokio::test]
    async fn scoped_search_never_falls_back_outside_its_provenance() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let selected = backend
            .save(
                &call(),
                &format!(
                    "{}\n{} selected needle",
                    "日本🦀".repeat(700),
                    "日本🦀".repeat(30)
                ),
            )
            .await
            .unwrap();
        let distractor = backend.save(&call(), "distractor needle").await.unwrap();
        let uri = format!("memory://{}/search/needle", selected.id);
        let result = backend.read(&uri, 0, 10).await.unwrap();
        assert!(result.len() <= 4000);
        assert!(result.contains("日本🦀") && result.contains("selected needle"));
        assert!(!result.contains("distractor"));
        assert!(
            result
                .lines()
                .any(|line| line == format!("memory://{}", selected.id))
        );
        let byte_uri = result
            .lines()
            .find(|line| line.starts_with(&format!("memory://{}/bytes/", selected.id)))
            .unwrap();
        assert!(
            backend
                .read(byte_uri, 0, 1)
                .await
                .unwrap()
                .contains("needle")
        );

        let no_hit = backend
            .read(&format!("memory://{}/search/absent", selected.id), 0, 10)
            .await
            .unwrap();
        assert!(!no_hit.contains("distractor") && !no_hit.contains("/bytes/"));
        assert!(
            backend
                .read("memory://bad/search/needle", 0, 10)
                .await
                .is_err()
        );

        let foreign = Backend {
            project: "foreign".into(),
            session: "session".into(),
            origins: Vec::new(),
            visible_origin_raw_ids: None,
            state: dir.path().into(),
            embedding: None,
        };
        assert!(foreign.read(&uri, 0, 10).await.is_err());
        let mut store = backend.store().unwrap();
        store.delete_session("project", "session").unwrap();
        assert!(backend.read(&uri, 0, 10).await.is_err());
        assert!(
            backend
                .read(&format!("memory://{}/search/needle", distractor.id), 0, 10)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn partial_failure_preserves_raw_and_never_overwrites_collision() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let (id, _) = backend.persist(&call(), "original").unwrap();
        let mut store = backend.store().unwrap();
        let mut chunk = backend
            .get(&store, &format!("{id}-c00000000"), CHUNK)
            .unwrap();
        chunk.text = "mismatch".into();
        store.upsert(&chunk, None).unwrap();
        assert!(backend.save(&call(), "original").await.is_err());
        assert_eq!(backend.get(&store, &id, RAW).unwrap().text, "original");
        assert_eq!(
            backend.get(&store, &chunk.id, CHUNK).unwrap().text,
            "mismatch"
        );
        let bad = Backend {
            state: dir.path().join("missing"),
            ..self::backend(dir.path())
        };
        assert!(bad.save(&call(), "original").await.is_err());
    }

    #[tokio::test]
    async fn raw_records_do_not_crowd_chunks_and_hits_retrieve_original_offset() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let text = format!("{}needle", "line\n".repeat(800));
        let saved = backend.save(&call(), &text).await.unwrap();
        assert!(saved.preview.chars().count() <= 128);
        let mut store = backend.store().unwrap();
        let raw = backend.get(&store, &saved.id, RAW).unwrap();
        for n in 0..120 {
            store
                .upsert(
                    &Record {
                        id: format!("other{n}"),
                        text: "needle ".repeat(20),
                        ..raw.clone()
                    },
                    None,
                )
                .unwrap();
        }
        let search = backend.read("memory://search/needle", 0, 10).await.unwrap();
        let result_line = search
            .lines()
            .find(|line| line.starts_with("memory://"))
            .unwrap();
        let uri = result_line.split(' ').next().unwrap();
        let restored = backend.read(uri, 0, 100).await.unwrap();
        assert!(restored.contains("needle"));
        assert_eq!(search.matches(&saved.id).count(), 1);
    }

    #[test]
    fn hybrid_uses_explicit_test_vectors_and_duplicate_save_preserves_them() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let (id, chunks) = backend.persist(&call(), "意味で取得する原文").unwrap();
        let vector = Embedding {
            model: "test-fixture".into(),
            values: vec![1.0, 0.5],
        };
        let mut store = backend.store().unwrap();
        store
            .set_embedding("project", "session", &chunks[0].id, &vector)
            .unwrap();
        assert!(
            backend
                .persist(&call(), "意味で取得する原文")
                .unwrap()
                .1
                .is_empty()
        );
        let result = backend
            .search("unmatched", Some(&vector), "hybrid search")
            .unwrap();
        assert!(result.contains(&id) && result.contains("意味で取得する原文"));
        assert!(!result.contains("falling back"));
        let unknown = Embedding {
            model: "unknown".into(),
            values: vec![1.0],
        };
        assert!(
            backend
                .search("原文", Some(&unknown), "hybrid search")
                .unwrap()
                .contains("falling back")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_shared_permissions_and_hardlinks_without_modifying_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = private_temp();
        let foreign = private_temp();
        let target = foreign.path().join("untouched");
        std::fs::write(&target, "original").unwrap();
        for name in ["memory.sqlite3", "memory.lock", "memory.sqlite3-wal"] {
            let link = dir.path().join(name);
            symlink(&target, &link).unwrap();
            assert!(backend(dir.path()).save(&call(), "new").await.is_err());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
            std::fs::remove_file(link).unwrap();
        }
        let alias = dir.path().join("alias");
        symlink(foreign.path(), &alias).unwrap();
        assert!(backend(&alias).save(&call(), "new").await.is_err());
        assert!(validate_paths(&alias.join("child")).is_err());
        let hard = dir.path().join("memory.sqlite3");
        std::fs::hard_link(&target, &hard).unwrap();
        assert!(backend(dir.path()).save(&call(), "new").await.is_err());
        std::fs::remove_file(hard).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(backend(dir.path()).save(&call(), "new").await.is_err());
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn configure_tightens_only_owned_readable_state_and_refuses_shared_state() {
        use std::os::unix::fs::PermissionsExt;
        let project = private_temp();
        let state = private_temp();
        let child = state.path().join("unrelated");
        std::fs::write(&child, "untouched").unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut session = Session::default();
        configure_tool_memory(
            &mut session,
            project.path(),
            state.path(),
            "session",
            RetentionMode::History,
            None,
        )
        .unwrap();
        assert!(session.tool_memory.is_some());
        assert_eq!(
            std::fs::metadata(state.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&child).unwrap().permissions().mode() & 0o777,
            0o644
        );
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            configure_tool_memory(
                &mut session,
                project.path(),
                state.path(),
                "session",
                RetentionMode::History,
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::metadata(state.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o775
        );
    }

    #[tokio::test]
    async fn ancestors_are_read_only_and_forget_is_scoped() {
        let dir = private_temp();
        let parent = backend(dir.path());
        let original = parent.save(&call(), "ancestor needle").await.unwrap();
        let child = Backend {
            session: "child".into(),
            origins: vec![parent.session.clone()],
            visible_origin_raw_ids: None,
            ..backend(dir.path())
        };
        let uri = format!("memory://{}", original.id);
        assert_eq!(child.read(&uri, 0, 10).await.unwrap(), "ancestor needle");
        assert!(
            child
                .read("memory://search/needle", 0, 10)
                .await
                .unwrap()
                .contains(&original.id)
        );
        assert!(
            child
                .read(&format!("memory://{}/search/needle", original.id), 0, 10)
                .await
                .unwrap()
                .contains("ancestor needle")
        );
        let own = child.save(&call(), "child needle").await.unwrap();
        assert_ne!(
            child.save(&call(), "ancestor needle").await.unwrap().id,
            original.id
        );
        assert!(
            parent
                .read(&format!("memory://{}", own.id), 0, 10)
                .await
                .is_err()
        );
        assert!(
            !parent
                .read("memory://search/child", 0, 10)
                .await
                .unwrap()
                .contains(&own.id)
        );
        let foreign = Backend {
            project: "foreign".into(),
            ..child
        };
        assert!(foreign.read(&uri, 0, 10).await.is_err());
        let child = Backend {
            project: parent.project.clone(),
            ..foreign
        };
        let mut store = parent.store().unwrap();
        store
            .delete_session(&parent.project, &child.session)
            .unwrap();
        assert!(child.save(&call(), "new").await.is_err());
        assert!(child.read(&uri, 0, 10).await.is_err());
        assert_eq!(parent.read(&uri, 0, 10).await.unwrap(), "ancestor needle");
        let descendant = Backend {
            session: "descendant".into(),
            origins: vec![parent.session.clone()],
            visible_origin_raw_ids: None,
            ..backend(dir.path())
        };
        descendant.save(&call(), "own data").await.unwrap();
        store
            .delete_session(&parent.project, &parent.session)
            .unwrap();
        assert!(descendant.save(&call(), "new").await.is_err());
        assert!(
            descendant
                .read("memory://search/data", 0, 10)
                .await
                .is_err()
        );
        assert!(
            !store
                .records_page(&parent.project, &descendant.session, None, 100)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn ordinary_origin_configuration_keeps_all_ancestor_records_visible() {
        let project = private_temp();
        let state = project.path().to_path_buf();
        let parent = Backend {
            project: crate::persist::project_identity(&polaris_core::project::resolve_root(
                project.path(),
            ))
            .unwrap(),
            state: state.clone(),
            ..backend(&state)
        };
        let before = parent.save(&call(), "first ancestor needle").await.unwrap();
        let after = parent.save(&call(), "later ancestor needle").await.unwrap();
        let mut session = Session::default();
        configure_tool_memory_with_origins(
            &mut session,
            project.path(),
            &state,
            "ordinary-child",
            std::slice::from_ref(&parent.session),
            RetentionMode::History,
            None,
        )
        .unwrap();
        let backend = session.tool_memory.unwrap().backend;
        assert!(
            backend
                .read(&format!("memory://{}", before.id), 0, 10)
                .await
                .unwrap()
                .contains("first ancestor")
        );
        assert!(
            backend
                .read(&format!("memory://{}", after.id), 0, 10)
                .await
                .unwrap()
                .contains("later ancestor")
        );
        let visible = visible_tool_memory_raw_object_ids(
            project.path(),
            &state,
            "ordinary-child",
            std::slice::from_ref(&parent.session),
            None,
        )
        .unwrap();
        assert!(visible.contains(&before.id) && visible.contains(&after.id));
    }

    #[tokio::test]
    async fn rewind_allowlist_blocks_later_origin_records_on_every_read_path_after_restart() {
        let dir = private_temp();
        let parent = Backend {
            project: crate::persist::project_identity(&polaris_core::project::resolve_root(
                dir.path(),
            ))
            .unwrap(),
            ..backend(dir.path())
        };
        let before = parent.save(&call(), "before rewind needle").await.unwrap();
        let after = parent.save(&call(), "later origin needle").await.unwrap();
        let vector = Embedding {
            model: "rewind-fixture".into(),
            values: vec![1.0, 0.5],
        };
        let mut store = parent.store().unwrap();
        for raw_id in [&before.id, &after.id] {
            store
                .set_embedding(
                    &parent.project,
                    &parent.session,
                    &format!("{raw_id}-c00000000"),
                    &vector,
                )
                .unwrap();
        }
        drop(store);

        let allowed = vec![before.id.clone()];
        let child = Backend {
            project: parent.project.clone(),
            session: "rewind-child".into(),
            origins: vec![parent.session.clone()],
            visible_origin_raw_ids: Some(allowed.iter().cloned().collect()),
            ..backend(dir.path())
        };
        let own = child.save(&call(), "branch-owned needle").await.unwrap();
        assert!(
            child
                .read(&format!("memory://{}", before.id), 0, 10)
                .await
                .unwrap()
                .contains("before rewind")
        );
        assert!(
            child
                .read(&format!("memory://{}", own.id), 0, 10)
                .await
                .unwrap()
                .contains("branch-owned")
        );
        assert!(
            child
                .read(&format!("memory://{}", after.id), 0, 10)
                .await
                .is_err()
        );
        assert!(
            child
                .read(&format!("memory://{}/search/needle", after.id), 0, 10)
                .await
                .is_err()
        );

        let lexical = child.read("memory://search/needle", 0, 10).await.unwrap();
        assert!(lexical.contains(&before.id) && lexical.contains(&own.id));
        assert!(!lexical.contains(&after.id) && !lexical.contains("later origin"));
        let semantic = child
            .search("unmatched", Some(&vector), "hybrid search")
            .unwrap();
        assert!(semantic.contains(&before.id));
        assert!(!semantic.contains(&after.id) && !semantic.contains("later origin"));

        let visible = visible_tool_memory_raw_object_ids(
            dir.path(),
            dir.path(),
            "rewind-child",
            std::slice::from_ref(&parent.session),
            Some(&allowed),
        )
        .unwrap();
        assert!(visible.contains(&before.id) && visible.contains(&own.id));
        assert!(!visible.contains(&after.id));

        // Rebuild the backend from the persisted checkpoint inputs, as a
        // resumed rewind branch does after process restart.
        let restarted = Backend {
            project: parent.project.clone(),
            session: "rewind-child".into(),
            origins: vec![parent.session.clone()],
            visible_origin_raw_ids: Some(allowed.iter().cloned().collect()),
            ..backend(dir.path())
        };
        assert!(
            restarted
                .read(&format!("memory://{}", after.id), 0, 10)
                .await
                .is_err()
        );
        let restarted_lexical = restarted
            .read("memory://search/needle", 0, 10)
            .await
            .unwrap();
        assert!(restarted_lexical.contains(&before.id));
        assert!(!restarted_lexical.contains(&after.id));
        let restarted_semantic = restarted
            .search("unmatched", Some(&vector), "hybrid search")
            .unwrap();
        assert!(restarted_semantic.contains(&before.id));
        assert!(!restarted_semantic.contains(&after.id));
    }

    #[test]
    fn excerpts_keep_separate_correction_and_merge_overlaps() {
        let text = "needle approved\nvalue 37\n\nbackground\n\nneedle revoked\nvalue 41";
        let ranges = excerpt_ranges(text, &["needle".into()], 0);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[0].0..ranges[0].1], "needle approved\nvalue 37");
        assert_eq!(&text[ranges[1].0..ranges[1].1], "needle revoked\nvalue 41");
        let nearby = "needle approved\nneedle corrected\nvalue 41";
        assert_eq!(
            excerpt_ranges(nearby, &["needle".into()], 0),
            vec![(0, nearby.len())]
        );
    }

    #[tokio::test]
    async fn search_returns_bounded_multiple_locations_and_late_correction() {
        let dir = private_temp();
        let backend = backend(dir.path());
        let text = (0..8)
            .map(|n| format!("needle decision {n}\n{}\n\n", "説明".repeat(180)))
            .collect::<String>()
            + "needle revoked final";
        let saved = backend.save(&call(), &text).await.unwrap();
        let result = backend.read("memory://search/needle", 0, 10).await.unwrap();
        assert!(result.len() <= 4096);
        assert!(result.contains("needle decision 0"));
        assert!(result.contains("needle revoked final"));
        assert_eq!(result.matches(&saved.id).count(), 3);
        for uri in result.lines().filter(|l| l.starts_with("memory://")) {
            let restored = backend.read(uri, 0, 10).await.unwrap();
            assert!(restored.contains("needle"));
        }
    }

    #[tokio::test]
    async fn lexical_boundary_and_lowercase_offsets_return_real_matches() {
        for prefix in ["x".repeat(1997), "İ".repeat(1500)] {
            let dir = private_temp();
            let backend = backend(dir.path());
            let text = format!("{prefix}needle{}", "z".repeat(3000));
            let saved = backend.save(&call(), &text).await.unwrap();
            let result = backend.read("memory://search/NEEDLE", 0, 10).await.unwrap();
            assert!(result.contains("needle"), "{result}");
            let uri = result.lines().find(|l| l.starts_with("memory://")).unwrap();
            assert!(uri.contains(&saved.id));
            assert!(backend.read(uri, 0, 1).await.unwrap().contains("needle"));
        }
    }

    #[tokio::test]
    async fn duplicate_save_retries_only_missing_model_embeddings_offline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = private_temp();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut encoder = Encoder::new("http://127.0.0.1/embed", "fixture".into()).unwrap();
        encoder.fake = Some(Arc::new(move |_| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                return Err(io::Error::other("injected failure"));
            }
            Ok(Embedding {
                model: "fixture".into(),
                values: vec![1.0, 0.5],
            })
        }));
        let backend = Backend {
            embedding: Some(encoder),
            ..backend(dir.path())
        };
        let text = "x".repeat(4500);
        let (_, pending) = backend.persist(&call(), &text).unwrap();
        assert_eq!(pending.len(), 3);
        let store = backend.store().unwrap();
        assert!(
            !store
                .has_embedding("project", "session", &pending[0].id, "fixture")
                .unwrap()
        );
        assert!(backend.save(&call(), &text).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            store
                .has_embedding("project", "session", &pending[0].id, "fixture")
                .unwrap()
        );
        assert!(
            !store
                .has_embedding("project", "session", &pending[0].id, "different")
                .unwrap()
        );
        assert!(
            !store
                .has_embedding("foreign", "session", &pending[0].id, "fixture")
                .unwrap()
        );
        assert!(
            !store
                .has_embedding("project", "foreign", &pending[0].id, "fixture")
                .unwrap()
        );
        assert_eq!(backend.persist(&call(), &text).unwrap().1.len(), 2);
        backend.save(&call(), &text).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(backend.persist(&call(), &text).unwrap().1.is_empty());
        backend.save(&call(), &text).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let mut store = backend.store().unwrap();
        store.delete_session("project", "session").unwrap();
        assert!(
            !store
                .has_embedding("project", "session", &pending[0].id, "fixture")
                .unwrap()
        );
    }

    #[test]
    fn configure_validates_all_origin_ids_before_creating_state() {
        let project = private_temp();
        for id in ["", "../bad", "日本語", "bad/id"] {
            let mut session = Session::default();
            let path = project.path().join("absent");
            assert!(
                configure_tool_memory_with_origins(
                    &mut session,
                    project.path(),
                    &path,
                    "child",
                    &[id.into()],
                    RetentionMode::History,
                    None
                )
                .is_err()
            );
            assert!(!path.exists());
        }
        let mut session = Session::default();
        let path = project.path().join("absent-raw-id");
        assert!(
            configure_tool_memory_with_origin_allowlist(
                &mut session,
                project.path(),
                &path,
                "child",
                &["parent".into()],
                &["a".repeat(63)],
                RetentionMode::History,
                None,
            )
            .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn embedding_validation_without_network() {
        for url in [
            "https://example.com",
            "http://localhost@evil.test",
            "file:///tmp/x",
            "http://user@127.0.0.1",
            "http://127.0.0.1?key=x",
        ] {
            assert!(Encoder::new(url, "model".into()).is_err());
        }
        for url in [
            "http://localhost:1234/embed",
            "http://127.0.0.1/embed",
            "http://[::1]/embed",
        ] {
            assert!(Encoder::new(url, "model".into()).is_ok());
        }
        assert!(decode_embedding(br#"{"data":[{"embedding":[0,0]}]}"#, "m").is_err());
        assert!(decode_embedding(br#"{"data":[]}"#, "m").is_err());
        assert_eq!(
            decode_embedding(br#"{"data":[{"embedding":[0.5,1]}]}"#, "m")
                .unwrap()
                .values,
            vec![0.5, 1.0]
        );
    }
}
