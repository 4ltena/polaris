//! v3記憶処理のworker/owner境界。Writerはserviceの所有者だけが操作する。
use crate::conversation_memory::related::{EmbeddedRelatedQuery, SEARCH_URI, is_search_path};
use crate::conversation_memory::structured::packing::StructuredContext;
use crate::{
    conversation_memory::{StrictHistory, structured::ValidatedStructuredResponse},
    desktop_store::{
        MemoryResources, MemorySnapshot, OwnerRequestHistory, SavedStructuredResponse, SummaryWork,
        Writer,
    },
};
use polaris_desktop_protocol::{
    request::RunTarget,
    snapshot::{MemoryRelatedOmission, MemoryRetrievalStatus},
};
use polaris_memory::{
    MemoryStore,
    conversation::{PendingSummary, RelatedSummaryBudget},
};
use polaris_provider::Message;
use std::{io, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

type Reply<T> = oneshot::Sender<io::Result<T>>;
enum Request {
    Snapshot(Reply<MemorySnapshot>),
    SearchSnapshot(Reply<MemorySnapshot>),
    Search(Box<MemorySnapshot>, EmbeddedRelatedQuery, Reply<String>),
    Begin(Box<MemorySnapshot>, u64, Reply<SummaryWork>),
    Save(Box<SummaryWork>, String, Reply<()>),
    SaveStructured(
        Box<SummaryWork>,
        Box<ValidatedStructuredResponse>,
        Reply<()>,
    ),
    Publish(Box<SummaryWork>, Box<PendingSummary>, Reply<()>),
    PublishStructured(
        Box<SummaryWork>,
        Box<PendingSummary>,
        Box<SavedStructuredResponse>,
        Reply<()>,
    ),
    Check(Box<MemorySnapshot>, Reply<()>),
    Ready(
        Box<MemorySnapshot>,
        Vec<String>,
        u64,
        Option<MemoryRetrievalStatus>,
        Option<RelatedSummaryBudget>,
        Reply<OwnerRequestHistory>,
    ),
    Structured(Box<MemorySnapshot>, Reply<StructuredContext>),
    Read(String, Reply<String>),
}
impl Request {
    fn closed(&self) -> bool {
        match self {
            Self::Snapshot(r) | Self::SearchSnapshot(r) => r.is_closed(),
            Self::Search(_, _, r) => r.is_closed(),
            Self::Structured(_, r) => r.is_closed(),
            Self::Begin(_, _, r) => r.is_closed(),
            Self::Save(_, _, r)
            | Self::SaveStructured(_, _, r)
            | Self::Publish(_, _, r)
            | Self::PublishStructured(_, _, _, r)
            | Self::Check(_, r) => r.is_closed(),
            Self::Ready(_, _, _, _, _, r) => r.is_closed(),
            Self::Read(_, r) => r.is_closed(),
        }
    }
    fn refuse(self) {
        let error = || io::Error::other("会話記憶の実行は停止しています");
        match self {
            Self::Snapshot(r) | Self::SearchSnapshot(r) => {
                let _ = r.send(Err(error()));
            }
            Self::Structured(_, r) => {
                let _ = r.send(Err(error()));
            }
            Self::Begin(_, _, r) => {
                let _ = r.send(Err(error()));
            }
            Self::Save(_, _, r)
            | Self::SaveStructured(_, _, r)
            | Self::Publish(_, _, r)
            | Self::PublishStructured(_, _, _, r)
            | Self::Check(_, r) => {
                let _ = r.send(Err(error()));
            }
            Self::Ready(_, _, _, _, _, r) => {
                let _ = r.send(Err(error()));
            }
            Self::Read(_, r) | Self::Search(_, _, r) => {
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

/// Readyの保存を確認した要求履歴と、その取得時だけの観測値。
#[derive(Debug)]
pub struct PreparedDesktopHistory {
    pub messages: Vec<Message>,
    pub(crate) input_source: Option<crate::request_input::BoundInput>,
    pub evidence: Option<Message>,
    pub observation: crate::conversation_memory::observation::MemoryObservation,
}

fn wire_omission(budget: RelatedSummaryBudget) -> MemoryRelatedOmission {
    match budget {
        RelatedSummaryBudget::QueryTerms => MemoryRelatedOmission::QueryTerms,
        RelatedSummaryBudget::Grants => MemoryRelatedOmission::Grants,
        RelatedSummaryBudget::SerializedRows => MemoryRelatedOmission::SerializedRows,
        RelatedSummaryBudget::MatchingWork => MemoryRelatedOmission::MatchingWork,
        RelatedSummaryBudget::VectorWork => MemoryRelatedOmission::VectorWork,
        RelatedSummaryBudget::RawProofWork => MemoryRelatedOmission::RawProofWork,
    }
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

    pub async fn prepare(&self) -> io::Result<PreparedDesktopHistory> {
        let snapshot = loop {
            let snapshot = self.send(Request::Snapshot).await?;
            let Some(turn) = snapshot.expired_turn().map_err(io::Error::other)? else {
                break snapshot;
            };
            let work = self
                .send(|r| Request::Begin(Box::new(snapshot), turn, r))
                .await?;
            let structured = if let Some(intent) = &work.structured_intent {
                match &work.structured_response {
                    Some(response) => Some(response.clone()),
                    None if work.newly_started => {
                        let view = work.expected.structured_view().map_err(io::Error::other)?;
                        let response = self
                            .history
                            .summarize_structured_turn(
                                &work.source,
                                turn,
                                intent,
                                &self.database,
                                &view,
                            )
                            .await?;
                        self.send(|r| {
                            Request::SaveStructured(
                                Box::new(work.clone()),
                                Box::new(response.clone()),
                                r,
                            )
                        })
                        .await?;
                        Some(SavedStructuredResponse {
                            summary: response.summary,
                            delta: response.delta,
                            coverage: response.coverage,
                        })
                    }
                    None => {
                        return Err(io::Error::other(
                            "以前の構造化抽出結果が不明です。自動再送はしていません",
                        ));
                    }
                }
            } else {
                None
            };
            let summary = match (&structured, &work.summary) {
                (Some(response), _) => response.summary.clone(),
                (None, Some(summary)) => summary.clone(),
                (None, None) if work.newly_started => {
                    let summary = self.history.summarize_turn(&work.source, turn).await?;
                    self.send(|r| Request::Save(Box::new(work.clone()), summary.clone(), r))
                        .await?;
                    summary
                }
                (None, None) => {
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
            if let Some(response) = structured {
                self.send(|r| {
                    Request::PublishStructured(
                        Box::new(work),
                        Box::new(pending),
                        Box::new(response),
                        r,
                    )
                })
                .await?;
            } else {
                self.send(|r| Request::Publish(Box::new(work), Box::new(pending), r))
                    .await?;
            }
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
        if snapshot.conversation.state.history_mode
            == polaris_desktop_protocol::snapshot::HistoryMode::Structured10
        {
            let context = self
                .send(|r| Request::Structured(Box::new(snapshot.clone()), r))
                .await?;
            let request = crate::conversation_memory::observation::prepare_structured(&context)?;
            self.send(|r| Request::Check(Box::new(snapshot.clone()), r))
                .await?;
            let history = self
                .send(|r| {
                    Request::Ready(
                        Box::new(snapshot.clone()),
                        request.observation.source_uris.clone(),
                        request.observation.reference_tokens,
                        request.observation.retrieval_status,
                        request.observation.related_omission,
                        r,
                    )
                })
                .await?;
            // Only an acknowledged Ready may expose a successful or blocked observation.
            let request = request.require_dispatch()?;
            return Ok(PreparedDesktopHistory {
                input_source: history.input_source(),
                messages: history.into_messages(),
                evidence: request.text.map(Message::user),
                observation: request.observation,
            });
        }

        let view = snapshot.view().map_err(io::Error::other)?;
        let hits = self.history.retrieve(&view, &self.database, &query).await?;
        self.send(|r| Request::Check(Box::new(snapshot.clone()), r))
            .await?;
        let request = crate::conversation_memory::observation::prepare_strict(&hits)?;
        let history = self
            .send(|r| {
                Request::Ready(
                    Box::new(snapshot),
                    request.observation.source_uris.clone(),
                    request.observation.reference_tokens,
                    None,
                    None,
                    r,
                )
            })
            .await?;
        Ok(PreparedDesktopHistory {
            input_source: history.input_source(),
            messages: history.into_messages(),
            evidence: request.text.map(Message::user),
            observation: request.observation,
        })
    }

    pub async fn read(&self, uri: &str) -> io::Result<String> {
        if is_search_path(uri) {
            if uri != SEARCH_URI {
                return Err(io::Error::other("関連検索のURIに追加指定は使えません"));
            }
            // This snapshot must not bind resources, write status, or summarize.
            let snapshot = self.send(Request::SearchSnapshot).await?;
            let query = snapshot.related_query().map_err(io::Error::other)?;
            let embedded = self.history.embed_related_query(query).await?;
            return self
                .send(|r| Request::Search(Box::new(snapshot), embedded, r))
                .await;
        }
        if uri.starts_with("conversation://structured/") {
            crate::conversation_memory::structured::sources::parse_source_uri(uri)?;
        } else if uri.len() > 1024 {
            return Err(io::Error::other("会話原文の参照が長すぎます"));
        }
        self.send(|r| Request::Read(uri.into(), r)).await
    }
}
impl DesktopMemoryOwner {
    #[cfg(test)]
    pub(crate) fn pending_requests(&self) -> usize {
        self.requests.len()
    }

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
                Request::SearchSnapshot(reply) => {
                    let result = store
                        .memory_snapshot(&self.target)
                        .and_then(|snapshot| {
                            store.preflight_related_memory(
                                &self.target,
                                &snapshot,
                                &self.database,
                            )?;
                            Ok(snapshot)
                        })
                        .map_err(io::Error::other);
                    let _ = reply.send(result);
                }
                Request::Search(expected, embedded, reply) => {
                    let result = store
                        .search_related_memory(&self.target, &expected, &self.database, &embedded)
                        .map_err(io::Error::other);
                    let _ = reply.send(result);
                }
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
                Request::SaveStructured(work, response, reply) => {
                    let _ = reply.send(
                        store
                            .save_structured_response(&self.target, &work, &response)
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
                Request::PublishStructured(work, pending, response, reply) => {
                    let _ = reply.send(
                        store
                            .publish_structured_memory(
                                &self.target,
                                &work,
                                &pending,
                                &response,
                                &self.database,
                            )
                            .map_err(io::Error::other),
                    );
                }
                Request::Check(expected, reply) => {
                    let result = store
                        .check_memory_snapshot(&self.target, &expected)
                        .map_err(io::Error::other)
                        .and_then(|()| {
                            let view = expected.view().map_err(io::Error::other)?;
                            let mut memory =
                                MemoryStore::open(&self.database).map_err(io::Error::other)?;
                            memory
                                .with_conversation_marker(&view, || Ok(()))
                                .map_err(io::Error::other)?;
                            if expected.conversation.state.history_mode
                                == polaris_desktop_protocol::snapshot::HistoryMode::Structured10
                            {
                                let scope = polaris_memory::structured::PublicationScope {
                                    project_id: view.scope.project_id.clone(),
                                    session_id: view.scope.session_id.clone(),
                                    epoch: view.scope.epoch,
                                    generation: view.scope.generation,
                                };
                                for id in &expected
                                    .conversation
                                    .state
                                    .visible_structured_publication_ids
                                {
                                    if !memory
                                        .structured_publication_visible(&scope, id)
                                        .map_err(io::Error::other)?
                                    {
                                        return Err(io::Error::other("index_incomplete"));
                                    }
                                }
                            }
                            Ok(())
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
                Request::Structured(expected, reply) => {
                    let _ = reply.send(
                        store
                            .prepare_structured_memory_with_related(
                                &self.target,
                                &expected,
                                &self.database,
                            )
                            .map_err(io::Error::other),
                    );
                }
                Request::Ready(expected, sources, tokens, retrieval, omission, reply) => {
                    let result = store
                        .check_memory_snapshot(&self.target, &expected)
                        .and_then(|()| {
                            let history = store.project_request_history(&self.target)?;
                            let mut status = store
                                .snapshot()?
                                .state
                                .runs
                                .iter()
                                .find(|r| r.run.run_id == self.target.run_id)
                                .and_then(|r| r.memory.clone())
                                .ok_or(crate::desktop_store::StoreError::NotFound)?;
                            status.phase = polaris_desktop_protocol::snapshot::MemoryPhase::Ready;
                            status.retrieval_status = retrieval;
                            status.related_omission = omission.map(wire_omission);
                            status.detail = match retrieval {
                                None => "会話記憶を要求へ接続しました",
                                Some(MemoryRetrievalStatus::Found) => {
                                    "必要な記憶と出典を要求へ接続しました"
                                }
                                Some(MemoryRetrievalStatus::NotFound) => "該当する記憶はありません",
                                Some(MemoryRetrievalStatus::AmbiguousEntity) => {
                                    "同名の候補を特定できません"
                                }
                                Some(MemoryRetrievalStatus::PendingReconfirmation) => {
                                    "撤回・未定・要再確認の状態があります"
                                }
                                Some(MemoryRetrievalStatus::ExtractionIncomplete) => {
                                    "最新の入力まで記憶の抽出が完了していません"
                                }
                                Some(MemoryRetrievalStatus::EvidenceBudgetExceeded) => {
                                    "必要な記憶が参照枠を超えたため実行を停止しました"
                                }
                            }
                            .into();
                            if matches!(
                                retrieval,
                                Some(
                                    MemoryRetrievalStatus::ExtractionIncomplete
                                        | MemoryRetrievalStatus::EvidenceBudgetExceeded
                                )
                            ) {
                                status.phase =
                                    polaris_desktop_protocol::snapshot::MemoryPhase::Failed;
                            }
                            status.recent_raw_turns =
                                polaris_desktop_protocol::ids::DecimalU64::new(
                                    expected.conversation.state.recent_turn_ids.len() as u64,
                                );
                            status.retrieval_sources = sources;
                            status.reference_tokens =
                                polaris_desktop_protocol::ids::DecimalU64::new(tokens);
                            store.record_memory_status(&self.target, status)?;
                            Ok(history)
                        })
                        .map_err(io::Error::other);
                    let _ = reply.send(result);
                }
            }
        }
    }
}
