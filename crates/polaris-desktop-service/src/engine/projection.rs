//! 子・workflow・確認状態を、上限付きの表示用summaryへ投影する。
//!
//! 実行の所有権、受理、routing、取消、永続化、通信dispatchは親に残す。

use sha2::{Digest, Sha256};

use super::session_admission;
use polaris_core::desktop_store::Published;
use polaris_desktop_protocol::orchestration::{
    AcceptanceSummaryStateV2, AcceptanceSummaryV2, ArtifactRefV2, ClarificationFreeTextV2,
    ClarificationQuestionV2, ClarificationStateV2, ClarificationSummaryV2, CommonRunnerStateV2,
    CommonRunnerSummaryV2, ConvergenceStateV2, ConvergenceSummaryV2, EvidenceSummaryV2,
    McpLeaseStateV2, McpLeaseSummaryV2, OrchestrationSummaryV2, SessionManagerSummaryV2,
    SidechatStateV2, SubagentAttemptSummaryV2, WorkflowStepV2, WorkflowSummaryV2,
};
use polaris_desktop_protocol::{
    ids::{AttemptId, DecimalU64, RunId},
    snapshot::WorkflowStatus,
};

pub(super) fn child_control_state(
    state: &polaris_core::subagent_control::ChildStateV1,
) -> polaris_desktop_protocol::orchestration::ChildControlStateV2 {
    use polaris_core::subagent_control::ChildStateV1 as State;
    use polaris_desktop_protocol::orchestration::ChildControlStateV2 as Wire;
    match state {
        State::Queued => Wire::Queued,
        State::Running => Wire::Running,
        State::Waiting { .. } => Wire::Waiting,
        State::Cancelling => Wire::Cancelling,
        State::Succeeded => Wire::Succeeded,
        State::Failed { .. } => Wire::Failed,
        State::Cancelled => Wire::Cancelled,
        State::OutcomeUnknown => Wire::OutcomeUnknown,
    }
}

pub(super) fn child_summary(
    root_run_id: &str,
    root_attempt_id: &str,
    control_revision: u64,
    child: &polaris_core::session::ChildControlProjectionV1,
) -> Option<SubagentAttemptSummaryV2> {
    Some(SubagentAttemptSummaryV2 {
        root_run_id: RunId::new(root_run_id).ok()?,
        root_attempt_id: AttemptId::new(root_attempt_id).ok()?,
        control_revision: DecimalU64::new(control_revision),
        logical_child_id: child.logical_child_id.0.clone(),
        attempt_id: child.attempt_id.0.clone(),
        display_name: child.display_name.clone(),
        role: child.role.clone(),
        state: child_control_state(&child.state),
        activity: child
            .activity
            .as_ref()
            .map(|activity| activity.summary.clone()),
    })
}

pub(super) fn workflow_status(
    saved: &polaris_core::desktop_store::SavedWorkflow,
) -> WorkflowStatus {
    use polaris_desktop_protocol::request::{WorkflowMode as WireMode, WorkflowPhase as WirePhase};
    let mode = match saved.config.mode {
        polaris_core::workflow::WorkflowMode::Manual => WireMode::Manual,
        polaris_core::workflow::WorkflowMode::Adaptive => WireMode::Adaptive,
    };
    let phase = match saved.state.phase {
        polaris_core::workflow::Phase::General => WirePhase::General,
        polaris_core::workflow::Phase::Brainstorm => WirePhase::Brainstorm,
        polaris_core::workflow::Phase::Specify => WirePhase::Specify,
        polaris_core::workflow::Phase::Implement => WirePhase::Implement,
        polaris_core::workflow::Phase::Review => WirePhase::Review,
        polaris_core::workflow::Phase::Verify => WirePhase::Verify,
        polaris_core::workflow::Phase::Deliver => WirePhase::Deliver,
    };
    WorkflowStatus {
        enabled: saved.config.enabled,
        mode,
        phase,
        revision: DecimalU64::new(saved.state.revision),
    }
}

pub(super) fn orchestration_summary(
    saved: &Published,
    admission: Option<&session_admission::AdmissionOwner>,
    delegation: Option<&polaris_core::session::RootDelegationProjectionV1>,
) -> OrchestrationSummaryV2 {
    let workflow = saved.state.workflow.as_ref();
    let phase = workflow.map(workflow_status).map_or(
        polaris_desktop_protocol::request::WorkflowPhase::General,
        |value| value.phase,
    );
    let step = workflow
        .and_then(|saved| saved.state.step)
        .map_or(WorkflowStepV2::Idle, |step| match step {
            polaris_core::workflow::WorkflowStepV1::Intake
            | polaris_core::workflow::WorkflowStepV1::Research => WorkflowStepV2::Context,
            polaris_core::workflow::WorkflowStepV1::Define
            | polaris_core::workflow::WorkflowStepV1::Shape
            | polaris_core::workflow::WorkflowStepV1::Decide
            | polaris_core::workflow::WorkflowStepV1::Specify
            | polaris_core::workflow::WorkflowStepV1::Clarify
            | polaris_core::workflow::WorkflowStepV1::Plan
            | polaris_core::workflow::WorkflowStepV1::Decompose => WorkflowStepV2::Planning,
            polaris_core::workflow::WorkflowStepV1::Analyze
            | polaris_core::workflow::WorkflowStepV1::Implement => WorkflowStepV2::Executing,
            polaris_core::workflow::WorkflowStepV1::Review => WorkflowStepV2::Reviewing,
            polaris_core::workflow::WorkflowStepV1::Verify
            | polaris_core::workflow::WorkflowStepV1::Converge => WorkflowStepV2::Converging,
            polaris_core::workflow::WorkflowStepV1::Deliver => WorkflowStepV2::Complete,
        });
    let active_feature = workflow
        .and_then(|saved| saved.state.active_feature.as_ref())
        .map(|reference| ArtifactRefV2 {
            artifact_id: reference.id.clone(),
            content_hash: reference.content_hash.clone(),
        });
    let active_task = workflow
        .and_then(|saved| saved.state.active_task.as_ref())
        .map(|reference| ArtifactRefV2 {
            artifact_id: reference.task_id.clone(),
            content_hash: reference.definition_hash.clone(),
        });
    let active_plan = workflow
        .and_then(|saved| saved.state.active_task.as_ref())
        .and_then(|active| {
            saved
                .state
                .workflow_artifacts
                .artifacts
                .iter()
                .find(|metadata| metadata.reference() == active.task_graph)
        })
        .and_then(|task_graph| {
            task_graph.header.created_from.iter().find(|reference| {
                saved
                    .state
                    .workflow_artifacts
                    .artifacts
                    .iter()
                    .any(|metadata| {
                        metadata.reference() == **reference
                            && metadata.kind
                                == polaris_core::workflow::WorkflowArtifactKindV1::ImplementationPlan
                    })
            })
        })
        .map(|reference| ArtifactRefV2 {
            artifact_id: reference.id.clone(),
            content_hash: reference.content_hash.clone(),
        });
    let convergence = workflow.and_then(|saved| saved.gates.convergence.as_ref());
    let convergence_report = convergence.map(polaris_core::workflow::evaluate_convergence);
    let acceptance = convergence.map_or_else(Vec::new, |input| {
        input
            .required_criterion_ids
            .iter()
            .map(|criterion_id| {
                let records = input
                    .evidence
                    .iter()
                    .filter(|record| {
                        &record.criterion_id == criterion_id && record.binding == input.binding
                    })
                    .collect::<Vec<_>>();
                let state =
                    if records.iter().any(|record| {
                        record.result == polaris_core::workflow::EvidenceResultV1::Failed
                    }) {
                        AcceptanceSummaryStateV2::Failed
                    } else if records.iter().any(|record| {
                        record.result == polaris_core::workflow::EvidenceResultV1::Stale
                    }) {
                        AcceptanceSummaryStateV2::Stale
                    } else if records.iter().any(|record| {
                        record.result == polaris_core::workflow::EvidenceResultV1::Unavailable
                    }) {
                        AcceptanceSummaryStateV2::Unavailable
                    } else if records.iter().any(|record| {
                        record.result == polaris_core::workflow::EvidenceResultV1::Passed
                    }) {
                        AcceptanceSummaryStateV2::Passed
                    } else {
                        AcceptanceSummaryStateV2::Pending
                    };
                AcceptanceSummaryV2 {
                    criterion_id: criterion_id.clone(),
                    state,
                    evidence_count: DecimalU64::new(records.len() as u64),
                }
            })
            .collect()
    });
    let evidence_summary = convergence.map_or(
        EvidenceSummaryV2 {
            passed: DecimalU64::new(0),
            failed: DecimalU64::new(0),
            stale: DecimalU64::new(0),
            unavailable: DecimalU64::new(0),
        },
        |input| {
            let count = |result| {
                DecimalU64::new(
                    input
                        .evidence
                        .iter()
                        .filter(|record| record.result == result)
                        .count() as u64,
                )
            };
            EvidenceSummaryV2 {
                passed: count(polaris_core::workflow::EvidenceResultV1::Passed),
                failed: count(polaris_core::workflow::EvidenceResultV1::Failed),
                stale: count(polaris_core::workflow::EvidenceResultV1::Stale),
                unavailable: count(polaris_core::workflow::EvidenceResultV1::Unavailable),
            }
        },
    );
    let convergence_summary = match (convergence, convergence_report.as_ref()) {
        (None, _) => ConvergenceSummaryV2 {
            state: ConvergenceStateV2::Unavailable,
            required_criteria: DecimalU64::new(0),
            unresolved_gaps: DecimalU64::new(0),
        },
        (Some(input), Some(report)) => ConvergenceSummaryV2 {
            state: if report.converged() {
                ConvergenceStateV2::Allowed
            } else if input.evidence.is_empty()
                && input.reviews.is_empty()
                && input.implementation_receipts.is_empty()
            {
                ConvergenceStateV2::Pending
            } else {
                ConvergenceStateV2::Blocked
            },
            required_criteria: DecimalU64::new(input.required_criterion_ids.len() as u64),
            unresolved_gaps: DecimalU64::new(report.gaps.len() as u64),
        },
        _ => unreachable!("a report exists for every convergence input"),
    };
    let subagent_total = delegation.map_or(0, |projection| projection.children.len());
    let subagents = delegation.map_or_else(Vec::new, |projection| {
        projection
            .children
            .iter()
            .take(32)
            .filter_map(|child| {
                child_summary(
                    &projection.root_run_id.0,
                    &projection.root_attempt_id.0,
                    projection.revision,
                    child,
                )
            })
            .collect::<Vec<_>>()
    });
    let mcp_lease_total = delegation.map_or(0, |projection| projection.mcp_leases.len());
    let mcp_leases = delegation
        .into_iter()
        .flat_map(|projection| projection.mcp_leases.iter())
        .take(32)
        .filter_map(|lease| {
            Some(McpLeaseSummaryV2 {
                lease_id: lease.lease_id.clone(),
                server_id: lease.server_id.clone(),
                holder_run_id: RunId::new(lease.root_run_id.0.clone()).ok()?,
                holder_attempt_id: AttemptId::new(lease.child_attempt_id.0.clone()).ok()?,
                state: match lease.state {
                    polaris_core::mcp::lease::McpLeaseProjectionStateV1::Active => {
                        McpLeaseStateV2::Active
                    }
                },
                expires_at_unix_ms: DecimalU64::new(lease.expires_at_ms),
            })
        })
        .collect::<Vec<_>>();
    let common_evidence = convergence.map_or_else(Vec::new, |input| {
        input
            .evidence
            .iter()
            .filter(|record| {
                record.binding == input.binding
                    && record.provenance.source_kind
                        == polaris_core::workflow::EvidenceSourceKindV1::TestExecution
            })
            .collect::<Vec<_>>()
    });
    let common_runner = if common_evidence.is_empty() {
        CommonRunnerSummaryV2 {
            state: CommonRunnerStateV2::Unavailable,
            case_id: None,
            completed_cases: DecimalU64::new(0),
            failed_cases: DecimalU64::new(0),
            cleanup_pending: false,
        }
    } else {
        let completed = common_evidence
            .iter()
            .filter(|record| record.result == polaris_core::workflow::EvidenceResultV1::Passed)
            .count();
        let failed = common_evidence
            .iter()
            .filter(|record| record.result == polaris_core::workflow::EvidenceResultV1::Failed)
            .count();
        let unknown = common_evidence.iter().any(|record| {
            matches!(
                record.result,
                polaris_core::workflow::EvidenceResultV1::Stale
                    | polaris_core::workflow::EvidenceResultV1::Unavailable
            )
        });
        let mut sources = common_evidence
            .iter()
            .map(|record| record.provenance.source_ref.as_str());
        let first = sources.next();
        let case_id = first
            .filter(|first| {
                !first.is_empty() && first.len() <= 128 && sources.all(|source| source == *first)
            })
            .map(str::to_owned);
        CommonRunnerSummaryV2 {
            state: if failed != 0 {
                CommonRunnerStateV2::Failed
            } else if unknown {
                CommonRunnerStateV2::OutcomeUnknown
            } else {
                CommonRunnerStateV2::Completed
            },
            case_id,
            completed_cases: DecimalU64::new(completed.min(32) as u64),
            failed_cases: DecimalU64::new(failed.min(32) as u64),
            cleanup_pending: false,
        }
    };
    let raw_messages = saved
        .raw
        .iter()
        .map(|event| event.message.clone())
        .collect::<Vec<_>>();
    let wire_questions = |asked: Vec<polaris_core::turn_intent::PendingClarification>| {
        asked
            .into_iter()
            .take(3)
            .enumerate()
            .map(|(index, question)| {
                let mut hash = Sha256::new();
                hash.update(b"polaris-clarification-question-v1\0");
                hash.update(saved.marker.session_id.as_str().as_bytes());
                hash.update(question.turn.to_be_bytes());
                hash.update((index as u64).to_be_bytes());
                hash.update(question.question.as_bytes());
                ClarificationQuestionV2 {
                    question_id: format!("question-{:x}", hash.finalize()),
                    text: question.question,
                    // The current core clarify contract accepts only strings.
                    // An empty choice list plus required free text is its exact
                    // answer shape, rather than a fabricated option set.
                    options: Vec::new(),
                    free_text: ClarificationFreeTextV2 {
                        allowed: true,
                        required: true,
                        max_bytes: DecimalU64::new(32 * 1024),
                    },
                    answered: question.answer_hash.is_some(),
                }
            })
            .collect::<Vec<_>>()
    };
    let clarification =
        match polaris_core::turn_intent::ClarificationState::from_history(&raw_messages) {
            polaris_core::turn_intent::ClarificationState::None => ClarificationSummaryV2 {
                state: ClarificationStateV2::None,
                questions: Vec::new(),
                unanswered_count: DecimalU64::new(0),
            },
            polaris_core::turn_intent::ClarificationState::Pending { asked } => {
                let questions = wire_questions(asked);
                ClarificationSummaryV2 {
                    state: ClarificationStateV2::Pending,
                    unanswered_count: DecimalU64::new(
                        questions
                            .iter()
                            .filter(|question| !question.answered)
                            .count() as u64,
                    ),
                    questions,
                }
            }
            polaris_core::turn_intent::ClarificationState::Resolved { asked } => {
                ClarificationSummaryV2 {
                    state: ClarificationStateV2::Resolved,
                    questions: wire_questions(asked),
                    unanswered_count: DecimalU64::new(0),
                }
            }
        };
    OrchestrationSummaryV2 {
        workflow: WorkflowSummaryV2 {
            phase,
            step,
            active_feature,
            active_plan,
            active_task,
            acceptance,
            evidence: evidence_summary,
            convergence: convergence_summary,
        },
        session_manager: admission.map_or(
            SessionManagerSummaryV2 {
                manager_revision: DecimalU64::new(0),
                session_count: DecimalU64::new(0),
                session_total: DecimalU64::new(0),
                session_omitted: false,
                sessions: Vec::new(),
                sidechat: SidechatStateV2::None,
                queued_admissions: DecimalU64::new(0),
                active_runs: DecimalU64::new(0),
            },
            session_admission::AdmissionOwner::summary,
        ),
        subagent_total: DecimalU64::new(subagent_total as u64),
        subagent_omitted: subagents.len() != subagent_total,
        subagents,
        mcp_lease_total: DecimalU64::new(mcp_lease_total as u64),
        mcp_lease_omitted: mcp_leases.len() != mcp_lease_total,
        mcp_leases,
        common_runner,
        clarification,
    }
}
