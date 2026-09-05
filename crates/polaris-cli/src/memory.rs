//! Local project memory commands; embedding requests require explicit configuration.

use polaris_memory::{Embedding, MemoryStore, Record, SearchMode, SearchRequest};
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
    Get { session: String, id: String },
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
    async fn encode(&self, text: &str) -> Result<Option<Embedding>, Box<dyn std::error::Error>> {
        let Some(base) = &self.embedding_url else {
            return Ok(None);
        };
        let url = url::Url::parse(base)?;
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
        {
            return Err(
                "埋め込みURLは認証情報を含まないローカルのhttp/https URLにしてください".into(),
            );
        }
        let model = self
            .embedding_model
            .as_deref()
            .ok_or("埋め込みモデルが必要です")?;
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()?
            .post(url)
            .json(&serde_json::json!({"model":model,"input":text}))
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = response.json().await?;
        let values: Vec<f32> = serde_json::from_value(
            body.pointer("/data/0/embedding")
                .ok_or("埋め込みが応答にありません")?
                .clone(),
        )?;
        Ok(Some(Embedding {
            model: model.into(),
            values,
        }))
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
    let state = PathBuf::from(home)
        .join(".polaris/state")
        .join(polaris_core::project::project_id(&project));
    let database = state.join("memory.sqlite3");
    let writes = matches!(
        &args.command,
        Command::Import { .. }
            | Command::Embed { .. }
            | Command::Index { .. }
            | Command::Forget { .. }
    );
    let mut store = if writes {
        std::fs::create_dir_all(&state)?;
        MemoryStore::open(&database)?
    } else {
        MemoryStore::open_read_only(&database)?
    };
    match args.command {
        Command::Import { file } => {
            let contents = std::fs::read_to_string(file)?;
            let records = contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(serde_json::from_str::<Record>)
                .collect::<Result<Vec<_>, _>>()?;
            for record in &records {
                if record.project_id != project_id {
                    return Err("別プロジェクトの記録は取り込めません".into());
                }
                validate_session(&record.session_id)?;
                if store.is_session_forgotten(&project_id, &record.session_id)? {
                    return Err("削除済みセッションは取り込めません".into());
                }
            }
            for record in &records {
                store.upsert(record, None)?;
            }
            println!("{}件を取り込みました", records.len());
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
        Command::Get { session, id } => {
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
        Command::Embed {
            session,
            id,
            embedding,
        } => {
            let record = store
                .get(&project_id, &session, &id)?
                .ok_or("記録がありません")?;
            let vector = embedding
                .encode(&record.text)
                .await?
                .ok_or("--embedding-urlと--embedding-modelが必要です")?;
            store.set_embedding(&project_id, &session, &id, &vector)?;
            println!("埋め込みを保存しました");
        }
        Command::Index { session, embedding } => {
            if embedding.embedding_url.is_none() {
                return Err("--embedding-urlと--embedding-modelが必要です".into());
            }
            let mut after: Option<String> = None;
            let mut count = 0usize;
            loop {
                let records = store.records_page(&project_id, &session, after.as_deref(), 50)?;
                if records.is_empty() {
                    break;
                }
                for record in &records {
                    let vector = embedding
                        .encode(&record.text)
                        .await?
                        .ok_or("埋め込み設定がありません")?;
                    store.set_embedding(&project_id, &session, &record.id, &vector)?;
                    count += 1;
                }
                after = records.last().map(|r| r.id.clone());
            }
            println!("{count}件の埋め込みを保存しました");
        }
        Command::Forget { session } => {
            validate_session(&session)?;
            let _lock = polaris_tui::lock_memory(&state)?;
            let count = store.delete_session(&project_id, &session)?;
            let directory = state
                .join("memory-archives")
                .join(format!("{session}.archive"));
            if directory.exists() {
                std::fs::remove_dir_all(directory)?;
            }
            println!("{count}件と保管原文を削除しました。再取り込みは禁止されます。");
        }
    }
    Ok(())
}

fn validate_session(session: &str) -> Result<(), Box<dyn std::error::Error>> {
    if session.is_empty()
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("セッションIDが不正です".into());
    }
    Ok(())
}
