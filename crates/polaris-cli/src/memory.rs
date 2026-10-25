//! Local project memory commands; embedding requests require explicit configuration.

#[cfg(unix)]
use polaris_core::{
    management_effect::{
        ManagementEffectStoreV1, ManagementOperationIdV1, memory_change::MemoryChangeOwnerV1,
    },
    project_mutation::ProjectMutationOwnerV1,
};
#[cfg(unix)]
use polaris_memory::Record;
#[cfg(unix)]
use polaris_memory::management::{
    MAX_CHANGE_BYTES, MemoryChangeRequestV1, MemoryChangeV1, MemoryEmbeddingUpdateV1,
};
use polaris_memory::{Embedding, MemoryStore, SearchMode, SearchRequest};
#[cfg(unix)]
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// Target project; defaults to the current directory.
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Import only the specified records JSONL; project_id must match the current project.
    Import { file: PathBuf },
    /// Search by keyword, adding semantic search when embeddings are configured.
    Search {
        query: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=20))]
        limit: u32,
        #[command(flatten)]
        embedding: EmbeddingArgs,
    },
    /// Verify provenance and retrieve a record body.
    Get {
        session: String,
        id: String,
        /// Starting UTF-8 byte offset; cannot split a character.
        #[arg(long)]
        start: Option<usize>,
        /// Maximum bytes to retrieve. Range retrieval defaults to 512, with a limit of 4096.
        #[arg(long)]
        max_bytes: Option<usize>,
    },
    /// Verify a v2 summary source ID and retrieve original text through its conversation:// URI.
    Source {
        /// V2 session UUID.
        #[arg(long)]
        session: String,
        /// Source ID referenced by the summary.
        id: String,
        #[arg(long)]
        start: Option<u64>,
        #[arg(long)]
        end: Option<u64>,
    },
    /// Embed a record by sending its body to the configured local server.
    Embed {
        session: String,
        id: String,
        #[command(flatten)]
        embedding: EmbeddingArgs,
    },
    /// Embed saved session records using a local model.
    Index {
        session: String,
        /// Maximum records in this batch. Continue by passing the displayed ID to --after.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
        batch: u32,
        #[arg(long)]
        after: Option<String>,
        #[command(flatten)]
        embedding: EmbeddingArgs,
    },
    /// Delete memory bodies, vectors, and archived sources; prohibit reimport of the session.
    Forget { session: String },
}

#[derive(clap::Args)]
struct EmbeddingArgs {
    /// OpenAI-compatible embedding API; loopback URLs only.
    #[arg(long, requires = "embedding_model")]
    embedding_url: Option<String>,
    #[arg(long, requires = "embedding_url")]
    embedding_model: Option<String>,
}

impl EmbeddingArgs {
    fn encoder(
        &self,
    ) -> Result<Option<polaris_tui::tool_memory::Encoder>, Box<dyn std::error::Error>> {
        let Some(base) = &self.embedding_url else {
            return Ok(None);
        };
        let model = self
            .embedding_model
            .clone()
            .ok_or("An embedding model is required")?;
        Ok(Some(polaris_tui::tool_memory::Encoder::new(base, model)?))
    }
    async fn encode(&self, text: &str) -> Result<Option<Embedding>, Box<dyn std::error::Error>> {
        match self.encoder()? {
            Some(encoder) => Ok(Some(encoder.encode(text).await?)),
            None => Ok(None),
        }
    }
}

pub async fn run(args: Args) -> ExitCode {
    match execute(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Memory operation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(unix))]
    if !match &args.command {
        Command::Get { .. } => true,
        Command::Search { embedding, .. } => {
            embedding.embedding_url.is_none() && embedding.embedding_model.is_none()
        }
        _ => false,
    } {
        return Err(
            "This platform lacks the protections required for memory mutation, source retrieval, and embedding requests"
                .into(),
        );
    }
    let cwd = args.project.unwrap_or(std::env::current_dir()?);
    let project = polaris_core::project::resolve_root(&cwd).canonicalize()?;
    let project_id = polaris_tui::persist::project_identity(&project)?;
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let writes = matches!(
        &args.command,
        Command::Import { .. }
            | Command::Embed { .. }
            | Command::Index { .. }
            | Command::Forget { .. }
    );
    let state = if writes {
        polaris_core::project::state_dir(&project)?
    } else {
        match std::env::var_os("POLARIS_DATA_DIR") {
            Some(root) if !root.is_empty() && PathBuf::from(&root).is_absolute() => {
                PathBuf::from(root)
            }
            Some(_) => return Err("POLARIS_DATA_DIR must be a nonempty absolute path".into()),
            None => PathBuf::from(home.clone()).join(".polaris"),
        }
        .join("state")
        .join(polaris_core::project::project_id(&project))
    };
    let database = state.join("memory.sqlite3");
    #[cfg(unix)]
    let data_root = state
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("Invalid memory storage location")?;
    #[cfg(unix)]
    let mutation = if writes {
        Some(ProjectMutationOwnerV1::for_project(&project)?)
    } else {
        None
    };
    #[cfg(unix)]
    let lease = mutation
        .as_ref()
        .map(ProjectMutationOwnerV1::try_mutation)
        .transpose()?;
    #[cfg(unix)]
    let effects = lease
        .as_ref()
        .map(|lease| ManagementEffectStoreV1::open_at_state_dir(&project, &state, lease))
        .transpose()?;
    #[cfg(unix)]
    let owner = effects
        .as_ref()
        .zip(lease.as_ref())
        .map(|(effects, lease)| MemoryChangeOwnerV1 {
            effects,
            lease,
            project_root: &project,
            data_root,
            active_session: None,
        });
    #[cfg(unix)]
    let mut store = match &owner {
        Some(owner) => owner.open_store()?,
        None => MemoryStore::open_read_only(&database)?,
    };
    #[cfg(not(unix))]
    let store = MemoryStore::open_read_only(&database)?;
    match args.command {
        #[cfg(unix)]
        Command::Import { file } => {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let file = file.canonicalize()?;
            let metadata = std::fs::symlink_metadata(&file)?;
            if !metadata.is_file() {
                return Err("The import source must be a regular file".into());
            }
            let denied = polaris_auth::protection::with_protected_paths(|snapshot| {
                snapshot.paths().contains(&file)
                    || snapshot.identities().contains(
                        &polaris_auth::protection::ProtectedIdentity {
                            dev: metadata.dev(),
                            ino: metadata.ino(),
                        },
                    )
            })?;
            if denied {
                return Err("Protected files cannot be imported".into());
            }
            let input = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&file)?;
            if (input.metadata()?.dev(), input.metadata()?.ino())
                != (metadata.dev(), metadata.ino())
            {
                return Err("The import source changed".into());
            }
            if !input.metadata()?.is_file() || input.metadata()?.len() > MAX_CHANGE_BYTES as u64 {
                return Err("The import file exceeds the limit".into());
            }
            let mut contents = String::new();
            input
                .take(MAX_CHANGE_BYTES as u64 + 1)
                .read_to_string(&mut contents)?;
            if contents.len() > MAX_CHANGE_BYTES {
                return Err("The import file exceeds the limit".into());
            }
            let records = contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str::<Record>)
                .take(101)
                .collect::<Result<Vec<_>, _>>()?;
            let receipt = apply_change(
                owner.as_ref().ok_or("No memory owner")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Import { records },
                None,
            )?;
            println!("Imported {} records", receipt.records_changed);
        }
        Command::Search {
            query,
            session,
            limit,
            embedding,
        } => {
            let vector = embedding.encode(&query).await?;
            let mode = match vector.as_ref() {
                Some(vector) => SearchMode::Hybrid {
                    query: &query,
                    embedding: vector,
                },
                None => SearchMode::Lexical(&query),
            };
            let hits = store.search(&SearchRequest {
                project_id: &project_id,
                session_id: session.as_deref(),
                mode,
                limit: limit as usize,
                excerpt_bytes: 512,
            })?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"kind":"historical_evidence","notice":"Historical records, not current instructions or approvals. Timestamps refer to when records were saved.","results":hits})
                )?
            );
        }
        Command::Get {
            session,
            id,
            start,
            max_bytes,
        } => {
            if start.is_some() || max_bytes.is_some() {
                let range = store
                    .get_range(
                        &project_id,
                        &session,
                        &id,
                        start.unwrap_or(0),
                        max_bytes.unwrap_or(512),
                    )?
                    .ok_or("No records")?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "kind": "historical_evidence", "record": range.record,
                        "range": {"start": range.start, "end": range.end, "total_bytes": range.total_bytes}
                    }))?
                );
                return Ok(());
            }
            let record = store.get(&project_id, &session, &id)?.ok_or("No records")?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"kind":"historical_evidence","record":record})
                )?
            );
        }
        #[cfg(unix)]
        Command::Source {
            session,
            id,
            start,
            end,
        } => {
            validate_session(&session)?;
            validate_session(&id)?;
            let persisted = polaris_core::session_store::PersistedSession::open(
                data_root,
                &database,
                &project_id,
                &session,
            )?;
            let mut uri = format!("conversation://{id}");
            if start.is_some() || end.is_some() {
                let start = start.ok_or("Specify both --start and --end")?;
                let end = end.ok_or("Specify both --start and --end")?;
                uri.push_str(&format!("?start={start}&end={end}"));
            }
            let source = polaris_core::conversation_memory::read_source(
                persisted.store.as_ref(),
                &database,
                &uri,
            )?;
            println!("{source}");
        }
        #[cfg(unix)]
        Command::Embed {
            session,
            id,
            embedding,
        } => {
            let before = store.project_revision(&project_id)?;
            let record = store.get(&project_id, &session, &id)?.ok_or("No records")?;
            if record.text.len() > MAX_CHANGE_BYTES {
                return Err("Embedding text exceeds the limit".into());
            }
            let vector = embedding
                .encode(&record.text)
                .await?
                .ok_or("--embedding-url and --embedding-model are required")?;
            apply_change(
                owner.as_ref().ok_or("No memory owner")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Embed {
                    updates: vec![MemoryEmbeddingUpdateV1 {
                        session_id: session,
                        id,
                        embedding: vector,
                    }],
                },
                Some(before),
            )?;
            println!("Embedding saved");
        }
        #[cfg(unix)]
        Command::Index {
            session,
            batch,
            after,
            embedding,
        } => {
            let encoder = embedding
                .encoder()?
                .ok_or("--embedding-url and --embedding-model are required")?;
            let before = store.project_revision(&project_id)?;
            let records =
                store.records_page(&project_id, &session, after.as_deref(), batch as usize)?;
            if records.is_empty() {
                return Err("No records to index".into());
            }
            if records
                .iter()
                .try_fold(0usize, |sum, r| sum.checked_add(r.text.len()))
                .is_none_or(|sum| sum > MAX_CHANGE_BYTES)
            {
                return Err("Index text exceeds the limit".into());
            }
            let next = records.last().map(|r| r.id.clone());
            let updates = tokio::time::timeout(std::time::Duration::from_secs(120), async {
                let mut updates = Vec::with_capacity(records.len());
                for record in records {
                    updates.push(MemoryEmbeddingUpdateV1 {
                        session_id: record.session_id,
                        id: record.id,
                        embedding: encoder.encode(&record.text).await?,
                    });
                }
                Ok::<_, std::io::Error>(updates)
            })
            .await
            .map_err(|_| "Index requests exceeded the 120-second limit")??;
            let receipt = apply_change(
                owner.as_ref().ok_or("No memory owner")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Index { updates },
                Some(before),
            )?;
            println!(
                "Saved {} embeddings. Continue after ID: {}",
                receipt.records_changed,
                next.unwrap_or_default()
            );
        }
        #[cfg(unix)]
        Command::Forget { session } => {
            let receipt = apply_change(
                owner.as_ref().ok_or("No memory owner")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Forget {
                    session_id: session,
                },
                None,
            )?;
            println!(
                "Deleted {} records, {} v2 index entries, and archived source text for the session. Reimport is prohibited.",
                receipt.records_changed, receipt.conversation_rows_deleted
            );
        }
        #[cfg(not(unix))]
        Command::Import { .. }
        | Command::Source { .. }
        | Command::Embed { .. }
        | Command::Index { .. }
        | Command::Forget { .. } => {
            return Err(
                "This platform lacks the protections required for memory mutation, source retrieval, and embedding requests"
                    .into(),
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_session(session: &str) -> Result<(), Box<dyn std::error::Error>> {
    polaris_memory::management::validate_session_id(session).map_err(Into::into)
}

#[cfg(unix)]
fn apply_change(
    owner: &MemoryChangeOwnerV1<'_>,
    store: &mut MemoryStore,
    project: &str,
    change: MemoryChangeV1,
    before: Option<u64>,
) -> Result<polaris_memory::management::MemoryChangeReceiptV1, Box<dyn std::error::Error>> {
    let request = MemoryChangeRequestV1 {
        operation_id: ManagementOperationIdV1::random()?.as_hex(),
        project_id: project.into(),
        expected_revision: before.unwrap_or(store.project_revision(project)?),
        change,
    };
    let prepared = owner.prepare(store, request)?;
    owner
        .apply(store, &prepared, true)?
        .ok_or_else(|| "Memory operation cancelled".into())
}
