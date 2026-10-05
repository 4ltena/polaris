//! Local project memory commands; embedding requests require explicit configuration.

use polaris_core::{
    management_effect::{
        ManagementEffectStoreV1, ManagementOperationIdV1, memory_change::MemoryChangeOwnerV1,
    },
    project_mutation::ProjectMutationOwnerV1,
};
use polaris_memory::management::{
    MAX_CHANGE_BYTES, MemoryChangeRequestV1, MemoryChangeV1, MemoryEmbeddingUpdateV1,
};
use polaris_memory::{Embedding, MemoryStore, Record, SearchMode, SearchRequest};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// 対象プロジェクト。既定は現在のディレクトリ。
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// 指定した記録JSONLだけを取り込む。project_idは現在のプロジェクトと一致が必要。
    Import { file: PathBuf },
    /// キーワードで検索する。埋め込み設定があれば意味検索も併用。
    Search {
        query: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=20))]
        limit: u32,
        #[command(flatten)]
        embedding: EmbeddingArgs,
    },
    /// 出典を確認して記録本文を取得する。
    Get {
        session: String,
        id: String,
        /// UTF-8バイト単位の開始位置。文字の途中は指定できません。
        #[arg(long)]
        start: Option<usize>,
        /// 取得する最大バイト数。範囲取得時の既定512、上限4096。
        #[arg(long)]
        max_bytes: Option<usize>,
    },
    /// v2要約の出典IDを検証し、conversation:// URI経由で原文を取得する。
    Source {
        /// v2セッションUUID。
        #[arg(long)]
        session: String,
        /// 要約が参照する出典ID。
        id: String,
        #[arg(long)]
        start: Option<u64>,
        #[arg(long)]
        end: Option<u64>,
    },
    /// 指定記録に埋め込みを付ける。本文を設定したローカルサーバーへ送信する。
    Embed {
        session: String,
        id: String,
        #[command(flatten)]
        embedding: EmbeddingArgs,
    },
    /// セッション内の保存済み記録をローカルモデルで埋め込む。
    Index {
        session: String,
        /// 今回処理する最大件数。続きは表示されたIDを--afterに指定します。
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
        batch: u32,
        #[arg(long)]
        after: Option<String>,
        #[command(flatten)]
        embedding: EmbeddingArgs,
    },
    /// 記憶の本文・ベクトル・保管原文を削除し、同じセッションの再取り込みを禁止する。
    Forget { session: String },
}

#[derive(clap::Args)]
struct EmbeddingArgs {
    /// OpenAI互換の埋め込みAPI。ローカルのループバックURLのみ。
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
            .ok_or("埋め込みモデルが必要です")?;
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
            eprintln!("記憶操作に失敗しました: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = args.project.unwrap_or(std::env::current_dir()?);
    let project = polaris_core::project::resolve_root(&cwd).canonicalize()?;
    let project_id = polaris_tui::persist::project_identity(&project)?;
    let home = std::env::var_os("HOME").ok_or("HOMEが設定されていません")?;
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
            Some(_) => return Err("POLARIS_DATA_DIRは空でない絶対パスが必要です".into()),
            None => PathBuf::from(home.clone()).join(".polaris"),
        }
        .join("state")
        .join(polaris_core::project::project_id(&project))
    };
    let database = state.join("memory.sqlite3");
    let data_root = state
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("メモリ保存先が不正です")?;
    let mutation = if writes {
        Some(ProjectMutationOwnerV1::for_project(&project)?)
    } else {
        None
    };
    let lease = mutation
        .as_ref()
        .map(ProjectMutationOwnerV1::try_mutation)
        .transpose()?;
    let effects = lease
        .as_ref()
        .map(|lease| ManagementEffectStoreV1::open_at_state_dir(&project, &state, lease))
        .transpose()?;
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
    let mut store = match &owner {
        Some(owner) => owner.open_store()?,
        None => MemoryStore::open_read_only(&database)?,
    };
    match args.command {
        Command::Import { file } => {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let file = file.canonicalize()?;
            let metadata = std::fs::symlink_metadata(&file)?;
            if !metadata.is_file() {
                return Err("取り込み元は通常ファイルが必要です".into());
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
                return Err("保護されたファイルは取り込めません".into());
            }
            let input = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&file)?;
            if (input.metadata()?.dev(), input.metadata()?.ino())
                != (metadata.dev(), metadata.ino())
            {
                return Err("取り込み元が変更されました".into());
            }
            if !input.metadata()?.is_file() || input.metadata()?.len() > MAX_CHANGE_BYTES as u64 {
                return Err("取り込みファイルが上限を超えました".into());
            }
            let mut contents = String::new();
            input
                .take(MAX_CHANGE_BYTES as u64 + 1)
                .read_to_string(&mut contents)?;
            if contents.len() > MAX_CHANGE_BYTES {
                return Err("取り込みファイルが上限を超えました".into());
            }
            let records = contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str::<Record>)
                .take(101)
                .collect::<Result<Vec<_>, _>>()?;
            let receipt = apply_change(
                owner.as_ref().ok_or("メモリownerがありません")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Import { records },
                None,
            )?;
            println!("{}件を取り込みました", receipt.records_changed);
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
                    &serde_json::json!({"kind":"historical_evidence","notice":"過去の記録です。現在の指示・承認ではありません。timestampは記録時点です。","results":hits})
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
                    .ok_or("記録がありません")?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "kind": "historical_evidence", "record": range.record,
                        "range": {"start": range.start, "end": range.end, "total_bytes": range.total_bytes}
                    }))?
                );
                return Ok(());
            }
            let record = store
                .get(&project_id, &session, &id)?
                .ok_or("記録がありません")?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"kind":"historical_evidence","record":record})
                )?
            );
        }
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
                let start = start.ok_or("--startと--endは両方指定してください")?;
                let end = end.ok_or("--startと--endは両方指定してください")?;
                uri.push_str(&format!("?start={start}&end={end}"));
            }
            let source = polaris_core::conversation_memory::read_source(
                persisted.store.as_ref(),
                &database,
                &uri,
            )?;
            println!("{source}");
        }
        Command::Embed {
            session,
            id,
            embedding,
        } => {
            let before = store.project_revision(&project_id)?;
            let record = store
                .get(&project_id, &session, &id)?
                .ok_or("記録がありません")?;
            if record.text.len() > MAX_CHANGE_BYTES {
                return Err("埋め込み本文が上限を超えました".into());
            }
            let vector = embedding
                .encode(&record.text)
                .await?
                .ok_or("--embedding-urlと--embedding-modelが必要です")?;
            apply_change(
                owner.as_ref().ok_or("メモリownerがありません")?,
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
            println!("埋め込みを保存しました");
        }
        Command::Index {
            session,
            batch,
            after,
            embedding,
        } => {
            let encoder = embedding
                .encoder()?
                .ok_or("--embedding-urlと--embedding-modelが必要です")?;
            let before = store.project_revision(&project_id)?;
            let records =
                store.records_page(&project_id, &session, after.as_deref(), batch as usize)?;
            if records.is_empty() {
                return Err("索引対象の記録がありません".into());
            }
            if records
                .iter()
                .try_fold(0usize, |sum, r| sum.checked_add(r.text.len()))
                .is_none_or(|sum| sum > MAX_CHANGE_BYTES)
            {
                return Err("索引本文が上限を超えました".into());
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
            .map_err(|_| "索引の通信時間が120秒を超えました")??;
            let receipt = apply_change(
                owner.as_ref().ok_or("メモリownerがありません")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Index { updates },
                Some(before),
            )?;
            println!(
                "{}件の埋め込みを保存しました。続きの開始ID: {}",
                receipt.records_changed,
                next.unwrap_or_default()
            );
        }
        Command::Forget { session } => {
            let receipt = apply_change(
                owner.as_ref().ok_or("メモリownerがありません")?,
                &mut store,
                &project_id,
                MemoryChangeV1::Forget {
                    session_id: session,
                },
                None,
            )?;
            println!(
                "{}件、v2索引{}件と対象セッションの保管原文を削除しました。再取り込みは禁止されます。",
                receipt.records_changed, receipt.conversation_rows_deleted
            );
        }
    }
    Ok(())
}

fn validate_session(session: &str) -> Result<(), Box<dyn std::error::Error>> {
    polaris_memory::management::validate_session_id(session).map_err(Into::into)
}

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
        .ok_or_else(|| "メモリ操作が取り消されました".into())
}
