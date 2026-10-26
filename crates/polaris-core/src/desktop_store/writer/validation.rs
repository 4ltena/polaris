//! In-memory ledger validation; publication and locking stay with Writer.
use super::*;

pub(super) fn validate_ledger(p: &Published) -> StoreResult<()> {
    p.state
        .workflow_artifacts
        .validate()
        .map_err(|_| StoreError::Corrupt("workflow artifact graph"))?;
    if let Some(workflow) = &p.state.workflow {
        workflow.validate_with_artifacts(&p.state.workflow_artifacts)?;
    }
    for run in &p.state.runs {
        if let Some(workflow) = &run.workflow {
            workflow.validate_with_artifacts(&p.state.workflow_artifacts)?;
        }
    }
    if p.state.inherited_turn_count.get() > 10
        || p.raw.iter().filter(|event| event.starts_turn).count() as u64
            != p.state
                .inherited_turn_count
                .get()
                .checked_add(p.state.runs.len() as u64)
                .ok_or(StoreError::Overflow)?
    {
        return Err(StoreError::Corrupt(
            "Original turn and run ledger association",
        ));
    }
    super::super::memory::validate(p)?;
    prompt::validate(p)?;
    mcp::validate(p)?;
    super::super::children::validate(&p.state)?;
    super::super::source_apply::validate(&p.state)?;
    use std::collections::BTreeSet;
    let mut keys = BTreeSet::new();
    let mut runs = BTreeSet::new();
    let mut attempts = BTreeSet::new();
    let mut admissions = BTreeSet::new();
    let mut operations = BTreeSet::new();
    let mut active = 0;
    for record in &p.state.runs {
        if !runs.insert(&record.run.run_id) || !attempts.insert(&record.run.attempt_id) {
            return Err(StoreError::Corrupt("Duplicate run or attempt"));
        }
        if let Some(binding) = &record.admission
            && (binding.admission_id.is_empty()
                || binding.admission_id.len() > 128
                || !binding
                    .admission_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                || !valid_hash(&binding.payload_hash)
                || binding.destination_session_id != p.marker.session_id
                || !admissions.insert(&binding.admission_id))
        {
            return Err(StoreError::Corrupt("admission binding"));
        }
        active +=
            usize::from(!record.run.state.is_terminal() && record.run.state != RunState::Queued);
        for op in &record.operations {
            if !operations.insert(&op.operation_id) {
                return Err(StoreError::Corrupt("Duplicate operation"));
            }
        }
    }
    if active > 1 {
        return Err(StoreError::Corrupt("Multiple nonterminal parent runs"));
    }
    for request in &p.state.requests {
        if let RequestResult::SourceApplyResolved { approval_id } = &request.result
            && !p
                .state
                .source_applies
                .iter()
                .any(|r| &r.candidate.approval_id == approval_id && r.decision.is_some())
        {
            return Err(StoreError::Corrupt("source answer reference"));
        }
        if !keys.insert((&request.client_id, &request.request_id))
            || request.accepted_revision > p.marker.session_revision
            || request.request_hash.len() != 64
            || !request.request_hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(StoreError::Corrupt(
                "Request ledger key, generation, or hash",
            ));
        }
        if let RequestResult::RunAccepted { run_id, attempt_id }
        | RequestResult::CancelRequested { run_id, attempt_id } = &request.result
            && !p
                .state
                .runs
                .iter()
                .any(|r| &r.run.run_id == run_id && &r.run.attempt_id == attempt_id)
        {
            return Err(StoreError::Corrupt("Request ledger run reference"));
        }
    }
    let mut approval_ids = BTreeSet::new();
    let mut approval_operations = BTreeSet::new();
    for r in &p.state.approval_records {
        let a = &r.pending;
        if !approval_ids.insert(&a.approval_id)
            || !approval_operations.insert(&a.operation_id)
            || !valid_hash(&a.payload_hash)
            || a.operation.is_empty()
            || !p
                .state
                .runs
                .iter()
                .any(|run| run.run.run_id == a.run_id && run.run.attempt_id == a.attempt_id)
            || (r.consumed && r.decision != Some(ApprovalDecision::Allow))
            || (r.consumed
                && !p.state.runs.iter().any(|run| {
                    run.run.run_id == a.run_id
                        && run.run.attempt_id == a.attempt_id
                        && run
                            .operations
                            .iter()
                            .any(|op| op.operation_id == a.operation_id)
                }))
        {
            return Err(StoreError::Corrupt(
                "Approval ledger ID, target, hash, or consumption",
            ));
        }
        let pending = p
            .state
            .unresolved_approvals
            .iter()
            .filter(|p| p.approval_id == a.approval_id)
            .collect::<Vec<_>>();
        if !r.invalidated && r.decision.is_none() {
            if pending.len() != 1 || pending[0] != a {
                return Err(StoreError::Corrupt("Pending approval mismatch"));
            }
        } else if !pending.is_empty() {
            return Err(StoreError::Corrupt("Answered approval still pending"));
        }
    }
    for request in &p.state.requests {
        if let RequestResult::ApprovalResolved { approval_id } = &request.result
            && !p
                .state
                .approval_records
                .iter()
                .any(|r| &r.pending.approval_id == approval_id && r.decision.is_some())
        {
            return Err(StoreError::Corrupt("Response approval reference"));
        }
    }
    Ok(())
}
