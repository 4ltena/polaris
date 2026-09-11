//! v3記憶処理のworker/owner境界。Writerはserviceの所有者だけが操作する。
use crate::{
    conversation_memory::StrictHistory,
    desktop_store::{MemoryResources, MemorySnapshot, SummaryWork, Writer},
};
use polaris_desktop_protocol::request::RunTarget;
use polaris_memory::{MemoryStore, conversation::PendingSummary};
use polaris_provider::Message;
use std::{io, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

type Reply<T> = oneshot::Sender<io::Result<T>>;
enum Request {
    Snapshot(Reply<MemorySnapshot>),
    Begin(Box<MemorySnapshot>, u64, Reply<SummaryWork>),
    Save(Box<SummaryWork>, String, Reply<()>),
    Publish(Box<SummaryWork>, Box<PendingSummary>, Reply<()>),
    Check(Box<MemorySnapshot>, Reply<()>),
    Ready(Box<MemorySnapshot>, Vec<String>, u64, Reply<()>),
    Read(String, Reply<String>),
}
impl Request {
    fn closed(&self) -> bool {
        match self {
            Self::Snapshot(r) => r.is_closed(),
            Self::Begin(_, _, r) => r.is_closed(),
            Self::Save(_, _, r)
            | Self::Publish(_, _, r)
            | Self::Check(_, r)
            | Self::Ready(_, _, _, r) => r.is_closed(),
            Self::Read(_, r) => r.is_closed(),
        }
    }
    fn refuse(self) {
        let error = || io::Error::other("会話記憶の実行は停止しています");
        match self {
            Self::Snapshot(r) => {
                let _ = r.send(Err(error()));
            }
            Self::Begin(_, _, r) => {
                let _ = r.send(Err(error()));
            }
            Self::Save(_, _, r)
            | Self::Publish(_, _, r)
            | Self::Check(_, r)
            | Self::Ready(_, _, _, r) => {
                let _ = r.send(Err(error()));
            }
            Self::Read(_, r) => {
                let _ = r.send(Err(error()));
            }
        }
    }
}

pub struct DesktopMemory {
    history: Arc<StrictHistory>,
    database: PathBuf,
    requests: mpsc::Sender<Request>,
}
pub struct DesktopMemoryOwner {
    target: RunTarget,
    database: PathBuf,
    resources: MemoryResources,
    requests: mpsc::Receiver<Request>,
}
impl DesktopMemory {
    /// Inputs are preverified owner resources, never wire/model filesystem paths.
    pub fn pair(
        target: RunTarget,
        history: Arc<StrictHistory>,
        database: PathBuf,
        resources: MemoryResources,
    ) -> io::Result<(Arc<Self>, DesktopMemoryOwner)> {
        resources.validate().map_err(io::Error::other)?;
        let metadata = history.embedding_metadata();
        if metadata.model != resources.embedding_model
            || metadata.revision != resources.embedding_revision
            || metadata.dimension != i64::from(resources.embedding_dimension)
        {
            return Err(io::Error::other("記憶資源と埋め込みbackendが一致しません"));
        }
        let (tx, rx) = mpsc::channel(1);
        Ok((
            Arc::new(Self {
                history,
                database: database.clone(),
                requests: tx,
            }),
            DesktopMemoryOwner {
                target,
                database,
                resources,
                requests: rx,
            },
        ))
    }
    async fn send<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> io::Result<T> {
        let (tx, rx) = oneshot::channel();
        self.requests
            .send(make(tx))
            .await
            .map_err(|_| io::Error::other("記憶の保存所有者が終了しました"))?;
        rx.await
            .map_err(|_| io::Error::other("記憶の保存応答を確認できません"))?
    }

    pub async fn prepare(&self) -> io::Result<(Vec<Message>, Option<Message>)> {
        let snapshot = loop {
            let snapshot = self.send(Request::Snapshot).await?;
            let Some(turn) = snapshot.expired_turn().map_err(io::Error::other)? else {
                break snapshot;
            };
            let work = self
                .send(|r| Request::Begin(Box::new(snapshot), turn, r))
                .await?;
            let summary = match &work.summary {
                Some(summary) => summary.clone(),
                None if work.newly_started => {
                    let summary = self.history.summarize_turn(&work.source, turn).await?;
                    self.send(|r| Request::Save(Box::new(work.clone()), summary.clone(), r))
                        .await?;
                    summary
                }
                None => {
                    return Err(io::Error::other(
                        "以前の要約結果が不明です。自動再送はしていません",
                    ));
                }
            };
            let mut scope = work.expected.view().map_err(io::Error::other)?.scope;
            scope.generation = scope
                .generation
                .checked_add(1)
                .ok_or_else(|| io::Error::other("記憶世代の上限"))?;
            let id = format!("strict10-{turn}-{}", &work.source.raw_hash[..16]);
            let pending = MemoryStore::open(&self.database)
                .map_err(io::Error::other)?
                .pending_summary(&scope, &id)
                .map_err(io::Error::other)?;
            let pending = match pending {
                Some(pending) => pending,
                None => {
                    self.history
                        .embed_summary(&work.source, turn, summary)
                        .await?
                }
            };
            self.send(|r| Request::Publish(Box::new(work), Box::new(pending), r))
                .await?;
        };
        let query = snapshot
            .conversation
            .events
            .iter()
            .rev()
            .find(|e| e.epoch == snapshot.conversation.state.epoch && e.starts_turn)
            .ok_or_else(|| io::Error::other("現在のユーザー入力がありません"))?
            .message
            .content
            .clone();
        let view = snapshot.view().map_err(io::Error::other)?;
        let hits = self.history.retrieve(&view, &self.database, &query).await?;
        self.send(|r| Request::Check(Box::new(snapshot.clone()), r))
            .await?;
        let evidence = hits
            .iter()
            .map(|h| h.render())
            .collect::<Vec<_>>()
            .join("\n");
        if crate::budget::count_tokens(&evidence) > 768 {
            return Err(io::Error::other("会話記憶が768tokensを超えました"));
        }
        let messages =
            crate::session::desktop_request_history(&snapshot.conversation.recent_messages())?;
        let sources = hits
            .iter()
            .map(|h| {
                format!(
                    "conversation://{}?start={}&end={}",
                    h.source.id, h.source.start_turn, h.source.end_turn
                )
            })
            .collect();
        self.send(|r| {
            Request::Ready(
                Box::new(snapshot),
                sources,
                crate::budget::count_tokens(&evidence) as u64,
                r,
            )
        })
        .await?;
        Ok((
            messages,
            (!evidence.is_empty()).then(|| Message::user(evidence)),
        ))
    }
    pub async fn read(&self, uri: &str) -> io::Result<String> {
        if uri.len() > 1024 {
            return Err(io::Error::other("会話原文の参照が長すぎます"));
        }
        self.send(|r| Request::Read(uri.into(), r)).await
    }
}
impl DesktopMemoryOwner {
    /// Called on the same serialized owner as run.cancel, before admitting work.
    pub fn poll(&mut self, mut store: Option<&mut Writer>, cancelled: bool) {
        for _ in 0..4 {
            let Ok(request) = self.requests.try_recv() else {
                break;
            };
            if request.closed() {
                continue;
            }
            let Some(store) = store.as_deref_mut().filter(|_| !cancelled) else {
                request.refuse();
                continue;
            };
            match request {
                Request::Snapshot(reply) => {
                    let result = store
                        .bind_memory(&self.target, self.resources.clone())
                        .and_then(|()| store.memory_snapshot(&self.target))
                        .map_err(io::Error::other);
                    let _ = reply.send(result);
                }
                Request::Begin(expected, turn, reply) => {
                    let _ = reply.send(
                        store
                            .begin_summary(&self.target, &expected, turn)
                            .map_err(io::Error::other),
                    );
                }
                Request::Save(work, summary, reply) => {
                    let _ = reply.send(
                        store
                            .save_summary(&self.target, &work, &summary)
                            .map_err(io::Error::other),
                    );
                }
                Request::Publish(work, pending, reply) => {
                    let _ = reply.send(
                        store
                            .publish_memory(&self.target, &work, &pending, &self.database)
                            .map_err(io::Error::other),
                    );
                }
                Request::Check(expected, reply) => {
                    let result = store
                        .check_memory_snapshot(&self.target, &expected)
                        .map_err(io::Error::other)
                        .and_then(|()| {
                            let view = expected.view().map_err(io::Error::other)?;
                            MemoryStore::open(&self.database)
                                .map_err(io::Error::other)?
                                .with_conversation_marker(&view, || Ok(()))
                                .map_err(io::Error::other)
                        });
                    let _ = reply.send(result);
                }
                Request::Read(uri, reply) => {
                    let _ = reply.send(
                        store
                            .read_memory_source(&self.target, &self.database, &uri)
                            .map_err(io::Error::other),
                    );
                }
                Request::Ready(expected, sources, tokens, reply) => {
                    let result = store
                        .check_memory_snapshot(&self.target, &expected)
                        .and_then(|()| {
                            let mut status = store
                                .snapshot()?
                                .state
                                .runs
                                .iter()
                                .find(|r| r.run.run_id == self.target.run_id)
                                .and_then(|r| r.memory.clone())
                                .ok_or(crate::desktop_store::StoreError::NotFound)?;
                            status.phase = polaris_desktop_protocol::snapshot::MemoryPhase::Ready;
                            status.detail = "会話記憶を要求へ接続しました".into();
                            status.recent_raw_turns =
                                polaris_desktop_protocol::ids::DecimalU64::new(
                                    expected.conversation.state.recent_turn_ids.len() as u64,
                                );
                            status.retrieval_sources = sources;
                            status.reference_tokens =
                                polaris_desktop_protocol::ids::DecimalU64::new(tokens);
                            store.record_memory_status(&self.target, status)
                        })
                        .map_err(io::Error::other);
                    let _ = reply.send(result);
                }
            }
        }
    }
}
