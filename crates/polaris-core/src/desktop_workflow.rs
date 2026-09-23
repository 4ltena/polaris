//! Durable workflow checkpoint port. The single desktop owner writes state;
//! workers only await the matching acknowledgement before another request.

use crate::desktop_store::{Delta, HubView, SavedWorkflow, Writer, hub};
use polaris_desktop_protocol::request::RunTarget;
use polaris_provider::Message;
use std::{io, sync::Arc};
use tokio::sync::{mpsc, oneshot};

type Reply<T> = oneshot::Sender<io::Result<T>>;

enum Request {
    Confirm {
        full_turn_suffix: Vec<Message>,
        candidate: SavedWorkflow,
        reply: Reply<()>,
    },
    Checkpoint {
        full_turn_suffix: Vec<Message>,
        previous: Box<SavedWorkflow>,
        candidate: SavedWorkflow,
        tool_call_id: Option<String>,
        gate_basis_hash: Option<String>,
        reply: Reply<()>,
    },
    /// M6-1: read the state window. Read-only, so a cancelled run may still
    /// ask, and nothing is written.
    Window {
        since: Option<u64>,
        reply: Reply<Delta>,
    },
    /// M6-3c: take the instructions this run has not applied yet. Taking and
    /// marking are one commit, so the same instruction cannot be applied
    /// twice by the same attempt.
    TakeInstructions { reply: Reply<Vec<String>> },
    PublishRoutineContract {
        expected_workflow_revision: u64,
        contract: crate::workflow::ChangeContractV1,
        reply: Reply<crate::workflow::SessionWorkflow>,
    },
    PublishConvergence {
        expected_workflow_revision: u64,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
        reply: Reply<crate::workflow::SessionWorkflow>,
    },
    ReadArtifact {
        kind: crate::workflow::WorkflowArtifactKindV1,
        reference: crate::workflow::ArtifactRefV1,
        reply: Reply<serde_json::Value>,
    },
}

impl Request {
    fn closed(&self) -> bool {
        match self {
            Self::Checkpoint { reply, .. } | Self::Confirm { reply, .. } => reply.is_closed(),
            Self::Window { reply, .. } => reply.is_closed(),
            Self::TakeInstructions { reply } => reply.is_closed(),
            Self::PublishRoutineContract { reply, .. } => reply.is_closed(),
            Self::PublishConvergence { reply, .. } => reply.is_closed(),
            Self::ReadArtifact { reply, .. } => reply.is_closed(),
        }
    }

    fn refuse(self) {
        let unavailable = || io::Error::other("workflow checkpointの所有者は停止しています");
        match self {
            Self::Checkpoint { reply, .. } | Self::Confirm { reply, .. } => {
                let _ = reply.send(Err(unavailable()));
            }
            Self::Window { reply, .. } => {
                let _ = reply.send(Err(unavailable()));
            }
            Self::TakeInstructions { reply } => {
                let _ = reply.send(Err(unavailable()));
            }
            Self::PublishRoutineContract { reply, .. } => {
                let _ = reply.send(Err(unavailable()));
            }
            Self::PublishConvergence { reply, .. } => {
                let _ = reply.send(Err(unavailable()));
            }
            Self::ReadArtifact { reply, .. } => {
                let _ = reply.send(Err(unavailable()));
            }
        }
    }
}

/// Worker-side port. It never opens or writes a desktop store directly.
pub struct DesktopWorkflow {
    requests: mpsc::Sender<Request>,
}

/// Writer-side half, retained by the desktop service's serialized run owner.
pub struct DesktopWorkflowOwner {
    target: RunTarget,
    requests: mpsc::Receiver<Request>,
    /// The window this owner last returned, so the next read can answer with
    /// the difference. Memory only: a restart resynchronizes.
    window: Option<HubView>,
    #[cfg(test)]
    discard_ack: bool,
}

impl DesktopWorkflow {
    pub fn pair(target: RunTarget) -> (Arc<Self>, DesktopWorkflowOwner) {
        let (requests, receiver) = mpsc::channel(1);
        (
            Arc::new(Self { requests }),
            DesktopWorkflowOwner {
                target,
                requests: receiver,
                window: None,
                #[cfg(test)]
                discard_ack: false,
            },
        )
    }

    /// Publish an acknowledged transition with the complete, unmodified turn
    /// suffix. The owner persists only the previously uncommitted tail.
    pub async fn checkpoint(
        &self,
        full_turn_suffix: Vec<Message>,
        previous: SavedWorkflow,
        candidate: SavedWorkflow,
        tool_call_id: Option<String>,
        gate_basis_hash: Option<String>,
    ) -> io::Result<()> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Checkpoint {
                full_turn_suffix: full_turn_suffix.clone(),
                previous: Box::new(previous),
                candidate: candidate.clone(),
                tool_call_id,
                gate_basis_hash,
                reply,
            })
            .await
            .map_err(|_| io::Error::other("workflow checkpointの所有者が終了しました"))?;
        if matches!(response.await, Ok(Ok(()))) {
            return Ok(());
        }
        // One read-only reconciliation after an ambiguous ACK. Never repeat
        // the checkpoint write or any provider/tool invocation.
        let (reply, confirmation) = oneshot::channel();
        self.requests
            .send(Request::Confirm {
                full_turn_suffix,
                candidate,
                reply,
            })
            .await
            .map_err(|_| io::Error::other("workflow checkpointの保存応答を確認できません"))?;
        confirmation
            .await
            .map_err(|_| io::Error::other("workflow checkpointの保存状態が不明です"))?
    }
}

impl DesktopWorkflow {
    /// The instructions that arrived for this run and have not been applied
    /// by it yet, in arrival order. Empty when there is nothing to take.
    pub async fn take_instructions(&self) -> io::Result<Vec<String>> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::TakeInstructions { reply })
            .await
            .map_err(|_| io::Error::other("追加指示の所有者へ要求を渡せません"))?;
        response
            .await
            .map_err(|_| io::Error::other("追加指示の応答を確認できません"))?
    }

    /// The state window: what changed since the revision the caller holds.
    pub async fn window(&self, since: Option<u64>) -> io::Result<Delta> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Window { since, reply })
            .await
            .map_err(|_| io::Error::other("状態窓口の所有者へ要求を渡せません"))?;
        response
            .await
            .map_err(|_| io::Error::other("状態窓口の応答を確認できません"))?
    }

    pub async fn publish_and_approve_routine_contract(
        &self,
        expected_workflow_revision: u64,
        contract: crate::workflow::ChangeContractV1,
    ) -> io::Result<crate::workflow::SessionWorkflow> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::PublishRoutineContract {
                expected_workflow_revision,
                contract,
                reply,
            })
            .await
            .map_err(|_| io::Error::other("workflow artifactの所有者が終了しました"))?;
        response
            .await
            .map_err(|_| io::Error::other("workflow artifactの保存状態が不明です"))?
    }

    pub async fn publish_convergence_trusted(
        &self,
        expected_workflow_revision: u64,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> io::Result<crate::workflow::SessionWorkflow> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::PublishConvergence {
                expected_workflow_revision,
                bundle,
                reply,
            })
            .await
            .map_err(|_| io::Error::other("convergenceの保存所有者が終了しました"))?;
        response
            .await
            .map_err(|_| io::Error::other("convergenceの保存状態が不明です"))?
    }

    pub async fn read_workflow_artifact(
        &self,
        kind: crate::workflow::WorkflowArtifactKindV1,
        reference: crate::workflow::ArtifactRefV1,
    ) -> io::Result<serde_json::Value> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::ReadArtifact {
                kind,
                reference,
                reply,
            })
            .await
            .map_err(|_| io::Error::other("workflow artifactの所有者が終了しました"))?;
        response
            .await
            .map_err(|_| io::Error::other("workflow artifactの読取り結果を確認できません"))?
    }
}

impl DesktopWorkflowOwner {
    /// Instructions this attempt has not taken. One that an abandoned attempt
    /// took is offered again, so a crash between taking and using it does not
    /// lose the instruction.
    fn take(&mut self, store: &mut Writer) -> io::Result<Vec<String>> {
        let published = store.snapshot().map_err(io::Error::other)?;
        let terminal =
            |run_id: &polaris_desktop_protocol::ids::RunId,
             attempt_id: &polaris_desktop_protocol::ids::AttemptId| {
                published.state.runs.iter().any(|record| {
                    record.run.run_id == *run_id
                        && record.run.attempt_id == *attempt_id
                        && record.run.state.is_terminal()
                })
            };
        let mut ids = Vec::new();
        let mut texts = Vec::new();
        for instruction in &published.state.instructions {
            let take = match &instruction.applied_to {
                None => true,
                Some(applied) => {
                    applied.attempt_id != self.target.attempt_id
                        && !terminal(&applied.run_id, &applied.attempt_id)
                }
            };
            if take {
                ids.push(instruction.instruction_id.clone());
                texts.push(instruction.text.clone());
            }
        }
        if ids.is_empty() {
            return Ok(texts);
        }
        store
            .apply_instructions(&self.target, &ids)
            .map_err(io::Error::other)?;
        Ok(texts)
    }

    #[cfg(test)]
    pub(crate) fn discard_next_ack(&mut self) {
        self.discard_ack = true;
    }

    /// Called on the same serialized owner as cancellation. A cancelled or
    /// disconnected run never acknowledges a new checkpoint. Cancellation
    /// still permits read-only confirmation of an already published marker.
    pub fn poll(
        &mut self,
        mut store: Option<&mut Writer>,
        cancelled: bool,
        activity: Option<crate::activity::Activity>,
    ) {
        for _ in 0..4 {
            let Ok(request) = self.requests.try_recv() else {
                break;
            };
            if request.closed() {
                continue;
            }
            let Some(store) = store.as_deref_mut() else {
                request.refuse();
                continue;
            };
            if cancelled
                && matches!(
                    &request,
                    Request::Checkpoint { .. }
                        | Request::TakeInstructions { .. }
                        | Request::PublishRoutineContract { .. }
                        | Request::PublishConvergence { .. }
                )
            {
                request.refuse();
                continue;
            }
            match request {
                // Read-only: a cancelled run may still see where things stand.
                Request::Window { since, reply } => {
                    let answer = store.snapshot().map_err(io::Error::other).map(|published| {
                        let current = hub::view(
                            &published.state,
                            published.marker.session_revision.get(),
                            activity,
                        );
                        let delta = hub::delta(self.window.as_ref(), &current, since);
                        self.window = Some(current);
                        delta
                    });
                    let _ = reply.send(answer);
                }
                Request::TakeInstructions { reply } => {
                    let _ = reply.send(self.take(store));
                }
                Request::PublishRoutineContract {
                    expected_workflow_revision,
                    contract,
                    reply,
                } => {
                    let _ = reply.send(
                        store
                            .publish_and_approve_routine_contract(
                                &self.target,
                                expected_workflow_revision,
                                contract,
                            )
                            .map_err(io::Error::other),
                    );
                }
                Request::PublishConvergence {
                    expected_workflow_revision,
                    bundle,
                    reply,
                } => {
                    let _ = reply.send(
                        store
                            .publish_convergence_trusted(
                                &self.target,
                                expected_workflow_revision,
                                bundle,
                            )
                            .map_err(io::Error::other),
                    );
                }
                Request::ReadArtifact {
                    kind,
                    reference,
                    reply,
                } => {
                    let _ = reply.send(
                        store
                            .read_workflow_artifact(kind, &reference)
                            .map_err(io::Error::other),
                    );
                }
                Request::Confirm {
                    full_turn_suffix,
                    candidate,
                    reply,
                } => {
                    let _ = reply.send(
                        store
                            .confirm_workflow_checkpoint(
                                &self.target,
                                &full_turn_suffix,
                                &candidate,
                            )
                            .map_err(io::Error::other),
                    );
                }
                Request::Checkpoint {
                    full_turn_suffix,
                    previous,
                    candidate,
                    tool_call_id,
                    gate_basis_hash,
                    reply,
                } => {
                    let outcome = store
                        .checkpoint_workflow(
                            &self.target,
                            full_turn_suffix,
                            *previous,
                            candidate,
                            tool_call_id.as_deref(),
                            gate_basis_hash.as_deref(),
                        )
                        .map_err(io::Error::other);
                    #[cfg(test)]
                    if std::mem::take(&mut self.discard_ack) {
                        drop(reply);
                        continue;
                    }
                    let _ = reply.send(outcome);
                }
            }
        }
    }
}
