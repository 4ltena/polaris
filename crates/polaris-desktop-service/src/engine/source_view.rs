//! Bounded saved source-apply pages; never a live job state or dispatch authority.
use super::*;
use polaris_desktop_protocol::{request, source_apply};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn page_error(error: source_apply::SourceApplyPageError) -> ProtocolError {
    match error {
        source_apply::SourceApplyPageError::TooLarge => {
            super::error(ErrorCode::CapabilityUnavailable)
        }
        _ => super::error(ErrorCode::InvalidRequest),
    }
}

fn failure_kind(value: &str) -> Option<source_apply::SourceApplyFailureKind> {
    use source_apply::SourceApplyFailureKind::*;
    Some(match value {
        "ProtectionUnavailable" => ProtectionUnavailable,
        "InvalidChangeSet" => InvalidChangeSet,
        "UnsupportedEntry" => UnsupportedEntry,
        "Conflict" => Conflict,
        "Secret" => Secret,
        "AncestorChanged" => AncestorChanged,
        "CrossDevice" => CrossDevice,
        "Io" => Io,
        "RestoreConflict" => RestoreConflict,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
struct ReportEntry<'a> {
    installed: bool,
    deleted: bool,
    restored: bool,
    old_retained: bool,
    phase: &'a str,
}

fn optional_string(object: &serde_json::Map<String, Value>, name: &str) -> bool {
    matches!(object.get(name), Some(Value::Null | Value::String(_)))
}

fn identity(value: &Value) -> bool {
    matches!(value.as_array(), Some(parts) if parts.len() == 2 && parts.iter().all(Value::is_u64))
}

fn optional_identity(object: &serde_json::Map<String, Value>, name: &str) -> bool {
    match object.get(name) {
        Some(Value::Null) => true,
        Some(value) => identity(value),
        None => false,
    }
}

fn report_entry(value: &Value) -> Option<(&str, ReportEntry<'_>)> {
    let object = value.as_object()?;
    let parent_chain = object.get("source_parent_chain")?.as_array()?;
    if !parent_chain.iter().all(|item| {
        matches!(item.as_array(), Some(parts) if parts.len() == 2
            && parts[0].is_string() && identity(&parts[1]))
    }) || !object.get("old_name")?.is_string()
        || !optional_string(object, "candidate_name")
        || !optional_string(object, "install_name")
        || !optional_identity(object, "candidate_identity")
        || !optional_identity(object, "install_identity")
    {
        return None;
    }
    Some((
        object.get("relative_path")?.as_str()?,
        ReportEntry {
            installed: object.get("installed")?.as_bool()?,
            deleted: object.get("deleted")?.as_bool()?,
            restored: object.get("restored")?.as_bool()?,
            old_retained: object.get("old_retained")?.as_bool()?,
            phase: object.get("phase")?.as_str()?,
        },
    ))
}

fn projected_result(
    saved: Option<&polaris_core::desktop_store::SourceApplyResult>,
    candidates: &[polaris_core::desktop_store::SourceApplyEntry],
) -> Option<source_apply::SourceApplyResultSummary> {
    let saved = saved?;
    let unknown = || source_apply::SourceApplyResultSummary {
        result_id: saved.result_id.clone(),
        status: source_apply::SourceApplyResultStatus::Unknown,
        failure_kind: None,
        installed_count: DecimalU64::new(0),
        deleted_count: DecimalU64::new(0),
        restored_count: DecimalU64::new(0),
    };
    // Empty reports never establish that an apply completed, even when failure
    // is explicitly null. A candidate is similarly required for a projection.
    if candidates.is_empty() {
        return Some(unknown());
    }
    let object = match saved.report.as_object() {
        Some(object) => object,
        None => return Some(unknown()),
    };
    if !object.get("source_path").is_some_and(Value::is_string)
        || !optional_string(object, "recovery_path")
        || !optional_identity(object, "recovery_identity")
    {
        return Some(unknown());
    }
    let failure = match object.get("failure") {
        Some(Value::Null) => None,
        Some(Value::Object(failure)) => {
            let Some(kind) = failure
                .get("kind")
                .and_then(Value::as_str)
                .and_then(failure_kind)
            else {
                return Some(unknown());
            };
            let optional_u64 = |name: &str| match failure.get(name) {
                Some(Value::Null) => true,
                Some(value) => value.is_u64(),
                None => false,
            };
            let optional_i64 = |name: &str| match failure.get(name) {
                Some(Value::Null) => true,
                Some(value) => value.is_i64(),
                None => false,
            };
            if !optional_u64("entry") || !optional_i64("os_error") {
                return Some(unknown());
            }
            Some(kind)
        }
        _ => return Some(unknown()),
    };
    let entries = match object.get("entries").and_then(Value::as_array) {
        Some(entries)
            if (failure.is_none() && !entries.is_empty() && entries.len() == candidates.len())
                || (failure.is_some() && entries.len() <= candidates.len()) =>
        {
            entries
        }
        _ => return Some(unknown()),
    };
    let mut candidate_paths = BTreeSet::new();
    for candidate in candidates {
        if !candidate_paths.insert(candidate.relative_path.as_str()) {
            return Some(unknown());
        }
    }
    let mut by_path = BTreeMap::new();
    let mut installed = 0_u64;
    let mut deleted = 0_u64;
    let mut restored = 0_u64;
    let mut retained = false;
    for entry in entries {
        let Some((path, report)) = report_entry(entry) else {
            return Some(unknown());
        };
        if by_path.insert(path, report).is_some() {
            return Some(unknown());
        }
        if !candidate_paths.contains(path) {
            return Some(unknown());
        }
        installed += u64::from(report.installed);
        deleted += u64::from(report.deleted);
        restored += u64::from(report.restored);
        retained |= report.old_retained;
    }
    for candidate in candidates {
        // A failed report may legitimately stop before an intended install or
        // delete. Only an explicit failure:null can certify every candidate's
        // completed operation.
        if failure.is_none() {
            let Some(report) = by_path.get(candidate.relative_path.as_str()) else {
                return Some(unknown());
            };
            let expected = match (&candidate.before, &candidate.after) {
                (None, Some(_)) | (Some(_), Some(_)) => (true, false, "Installed"),
                (Some(_), None) => (false, true, "Deleted"),
                (None, None) => return Some(unknown()),
            };
            if report.installed != expected.0
                || report.deleted != expected.1
                || report.restored
                || report.phase != expected.2
            {
                return Some(unknown());
            }
        }
    }
    let counts = (
        DecimalU64::new(installed),
        DecimalU64::new(deleted),
        DecimalU64::new(restored),
    );
    let status = match failure {
        None => source_apply::SourceApplyResultStatus::Applied,
        Some(_) if installed != 0 || deleted != 0 || restored != 0 || retained => {
            source_apply::SourceApplyResultStatus::Partial
        }
        Some(_) => source_apply::SourceApplyResultStatus::Failed,
    };
    Some(source_apply::SourceApplyResultSummary {
        result_id: saved.result_id.clone(),
        status,
        failure_kind: failure,
        installed_count: counts.0,
        deleted_count: counts.1,
        restored_count: counts.2,
    })
}

impl Engine {
    pub(super) fn source_list(
        &self,
        params: &request::SourceApplyList,
    ) -> Result<SuccessResult, ProtocolError> {
        let saved = self.store.snapshot().map_err(store_error)?;
        if saved.marker.session_revision != params.expected_session_revision {
            return Err(error(ErrorCode::RevisionConflict));
        }
        let items = saved
            .state
            .source_applies
            .iter()
            .map(|r| {
                let c = &r.candidate;
                source_apply::SourceApplySummary {
                    run_id: c.run_id.clone(),
                    attempt_id: c.attempt_id.clone(),
                    approval_id: c.approval_id.clone(),
                    operation_id: c.operation_id.clone(),
                    policy_revision: c.policy_revision,
                    expires_at_unix_ms: c.expires_at_unix_ms,
                    payload_hash: c.payload_hash.clone(),
                    source_path: c.payload.source_path.clone(),
                    recovery_parent_path: c.payload.recovery_parent_path.clone(),
                    entry_count: DecimalU64::new(c.payload.entries.len() as u64),
                    decision: r.decision,
                    invalidated: r.invalidated,
                    intent_committed: r.intent_revision.is_some(),
                    result_saved: r.result.is_some(),
                    result: projected_result(r.result.as_ref(), &c.payload.entries),
                }
            })
            .collect::<Vec<_>>();
        source_apply::SourceApplyListResult::from_slice(
            saved.marker.session_revision,
            &items,
            params.offset,
            params.limit,
        )
        .map(SuccessResult::SourceApplyList)
        .map_err(page_error)
    }
    pub(super) fn source_page(
        &self,
        params: &request::SourceApplyPage,
    ) -> Result<SuccessResult, ProtocolError> {
        let saved = self.store.snapshot().map_err(store_error)?;
        if saved.marker.session_revision != params.expected_session_revision {
            return Err(error(ErrorCode::RevisionConflict));
        }
        let record = saved
            .state
            .source_applies
            .iter()
            .find(|r| r.candidate.approval_id == params.approval_id)
            .ok_or_else(|| error(ErrorCode::NotFound))?;
        let c = &record.candidate;
        if c.payload_hash != params.payload_hash {
            return Err(error(ErrorCode::RevisionConflict));
        }
        let version = |v: &polaris_core::desktop_store::SourceApplyVersion| {
            source_apply::SourceApplyVersion {
                hash: v.hash.clone(),
                mode: v.mode,
            }
        };
        let entries = c
            .payload
            .entries
            .iter()
            .map(|e| source_apply::SourceApplyEntry {
                relative_path: e.relative_path.clone(),
                before: e.before.as_ref().map(version),
                after: e.after.as_ref().map(version),
            })
            .collect::<Vec<_>>();
        source_apply::SourceApplyPageResult::from_slice(
            c.approval_id.clone(),
            c.payload_hash.clone(),
            saved.marker.session_revision,
            &entries,
            params.offset,
            params.limit,
        )
        .map(SuccessResult::SourceApplyPage)
        .map_err(page_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::desktop_store::{
        SourceApplyCandidate, SourceApplyEntry, SourceApplyGuard, SourceApplyIdentity,
        SourceApplyIdentityProof, SourceApplyPayload, SourceApplyVersion,
    };
    use polaris_desktop_protocol::snapshot::ApprovalDecision;
    fn req(id: &str, body: RequestBody) -> Request {
        Request {
            protocol_version: ProtocolVersion,
            client_id: ClientId::new("viewer").unwrap(),
            request_id: RequestId::new(id).unwrap(),
            body,
        }
    }
    fn fixture() -> (PrototypeRoot, Engine, SourceApplyCandidate) {
        let root = PrototypeRoot::new().unwrap();
        let mut e = Engine::create(&root, Options::default()).unwrap();
        e.handle(&req("hello", RequestBody::Hello), &mut vec![])
            .unwrap();
        let t = RunTarget {
            run_id: RunId::new("r").unwrap(),
            attempt_id: AttemptId::new("a").unwrap(),
        };
        let p = e.store.snapshot().unwrap();
        e.store
            .apply(
                &req(
                    "start",
                    RequestBody::RunStart(
                        e.session.clone(),
                        request::RunStart {
                            expected_draft_revision: p.state.draft.draft_revision,
                            expected_configuration_revision: p
                                .state
                                .configuration
                                .configuration_revision,
                            expected_policy_revision: p.state.policy_revision,
                        },
                    ),
                ),
                Some(t.clone()),
            )
            .unwrap();
        let op = OperationId::new("op").unwrap();
        let result = ResultId::new("result").unwrap();
        e.store.record_intent(&t, op.clone()).unwrap();
        e.store
            .record_operation_result(&t, &op, result.clone())
            .unwrap();
        e.store.finish(&t, Observation::Succeeded, result).unwrap();
        let source = SourceApplyIdentity {
            device: DecimalU64::new(1),
            inode: DecimalU64::new(2),
        };
        let recovery = SourceApplyIdentity {
            device: DecimalU64::new(1),
            inode: DecimalU64::new(3),
        };
        let payload = SourceApplyPayload {
            source_path: "/synthetic-source".into(),
            source_identity: source,
            recovery_parent_path: "/synthetic-recovery".into(),
            recovery_parent_identity: recovery,
            entries: vec![SourceApplyEntry {
                relative_path: "file.txt".into(),
                before: None,
                after: Some(SourceApplyVersion {
                    hash: "a".repeat(64),
                    mode: 0o600,
                }),
            }],
        };
        let p = e.store.snapshot().unwrap();
        let candidate = SourceApplyCandidate {
            run_id: t.run_id.clone(),
            attempt_id: t.attempt_id.clone(),
            approval_id: ApprovalId::new("source-approval").unwrap(),
            operation_id: OperationId::new("source-op").unwrap(),
            policy_revision: p.state.policy_revision,
            expires_at_unix_ms: DecimalU64::new(100),
            payload_hash: payload.payload_hash().unwrap(),
            payload,
        };
        e.store
            .publish_source_apply(
                &t,
                candidate.clone(),
                SourceApplyGuard {
                    expected_session_revision: p.marker.session_revision,
                    expected_policy_revision: p.state.policy_revision,
                    now_ms: 1,
                },
                SourceApplyIdentityProof {
                    source,
                    recovery_parent: recovery,
                },
            )
            .unwrap();
        (root, e, candidate)
    }

    fn candidate_entry(before: bool, after: bool) -> SourceApplyEntry {
        let version = || SourceApplyVersion {
            hash: "a".repeat(64),
            mode: 0o600,
        };
        SourceApplyEntry {
            relative_path: "file.txt".into(),
            before: before.then(version),
            after: after.then(version),
        }
    }

    fn saved_report(report: serde_json::Value) -> polaris_core::desktop_store::SourceApplyResult {
        polaris_core::desktop_store::SourceApplyResult {
            result_id: ResultId::new("saved-result").unwrap(),
            report,
        }
    }

    fn entry_report(
        installed: bool,
        deleted: bool,
        restored: bool,
        old_retained: bool,
        phase: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "relative_path": "file.txt",
            "source_parent_chain": [],
            "installed": installed,
            "deleted": deleted,
            "restored": restored,
            "old_retained": old_retained,
            "old_name": "old-file.txt",
            "candidate_name": "candidate-file.txt",
            "install_name": "install-file.txt",
            "candidate_identity": [1, 2],
            "install_identity": [1, 3],
            "phase": phase,
        })
    }

    fn report(entries: serde_json::Value, failure: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "source_path": "/source",
            "recovery_path": "/recovery",
            "recovery_identity": [1, 4],
            "entries": entries,
            "failure": failure,
        })
    }

    #[test]
    fn source_apply_projection_reports_actual_success_only_after_complete_report_match() {
        let saved = saved_report(report(
            serde_json::json!([entry_report(true, false, false, true, "Installed")]),
            serde_json::Value::Null,
        ));
        let result = projected_result(Some(&saved), &[candidate_entry(false, true)]).unwrap();
        assert_eq!(
            result.status,
            source_apply::SourceApplyResultStatus::Applied
        );
        assert_eq!(result.installed_count, DecimalU64::new(1));
        assert_eq!(result.deleted_count, DecimalU64::new(0));
        assert_eq!(result.restored_count, DecimalU64::new(0));
    }

    #[test]
    fn source_apply_projection_failure_and_partial_preserve_are_not_run_success() {
        let failed = saved_report(report(
            serde_json::json!([entry_report(false, false, false, false, "Prepared")]),
            serde_json::json!({"kind": "Conflict", "entry": null, "os_error": null}),
        ));
        let result = projected_result(Some(&failed), &[candidate_entry(false, true)]).unwrap();
        assert_eq!(result.status, source_apply::SourceApplyResultStatus::Failed);
        assert_eq!(
            result.failure_kind,
            Some(source_apply::SourceApplyFailureKind::Conflict)
        );

        let partial = saved_report(report(
            serde_json::json!([entry_report(false, false, true, true, "Restored")]),
            serde_json::json!({"kind": "Conflict", "entry": null, "os_error": null}),
        ));
        let result = projected_result(Some(&partial), &[candidate_entry(false, true)]).unwrap();
        assert_eq!(
            result.status,
            source_apply::SourceApplyResultStatus::Partial
        );
        assert_eq!(result.restored_count, DecimalU64::new(1));

        let preflight_conflict = saved_report(report(
            serde_json::json!([]),
            serde_json::json!({"kind": "Conflict", "entry": null, "os_error": null}),
        ));
        let result =
            projected_result(Some(&preflight_conflict), &[candidate_entry(false, true)]).unwrap();
        assert_eq!(result.status, source_apply::SourceApplyResultStatus::Failed);
    }

    #[test]
    fn source_apply_projection_malformed_empty_and_incomplete_reports_are_unknown() {
        for report in [
            report(serde_json::json!([]), serde_json::Value::Null),
            serde_json::json!({"entries": [entry_report(true, false, false, false, "Installed")]}),
            serde_json::json!({
                "entries": [{"relative_path":"file.txt", "installed":true}],
                "failure": null,
            }),
            report(
                serde_json::json!([entry_report(true, false, false, false, "Installed")]),
                serde_json::Value::Null,
            ),
        ] {
            let saved = saved_report(report);
            let candidates = match saved.report["entries"].as_array().map(Vec::len) {
                Some(0) => vec![candidate_entry(false, true)],
                _ => vec![candidate_entry(true, false)],
            };
            assert_eq!(
                projected_result(Some(&saved), &candidates).unwrap().status,
                source_apply::SourceApplyResultStatus::Unknown
            );
        }
    }
    #[test]
    fn saved_source_pages_require_scope_revision_hash_and_do_not_authorize() {
        let (_root, mut e, c) = fixture();
        let before = e.store.snapshot().unwrap();
        let params = request::SourceApplyList {
            expected_session_revision: before.marker.session_revision,
            offset: DecimalU64::new(0),
            limit: request::PageLimit::new(32).unwrap(),
        };
        let result = e
            .handle(
                &req(
                    "list",
                    RequestBody::SourceApplyList(e.session.clone(), params.clone()),
                ),
                &mut vec![],
            )
            .unwrap();
        let SuccessResult::SourceApplyList(list) = result else {
            panic!("wrong result")
        };
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].payload_hash, c.payload_hash);
        assert!(!list.items[0].intent_committed);
        assert!(!list.items[0].result_saved);
        assert_eq!(
            e.handle(
                &req(
                    "foreign",
                    RequestBody::SourceApplyList(SessionId::new("other").unwrap(), params)
                ),
                &mut vec![]
            )
            .unwrap_err()
            .code,
            ErrorCode::PermissionDenied
        );
        let mut page = request::SourceApplyPage {
            approval_id: c.approval_id.clone(),
            payload_hash: c.payload_hash.clone(),
            expected_session_revision: before.marker.session_revision,
            offset: DecimalU64::new(0),
            limit: request::PageLimit::new(1).unwrap(),
        };
        let result = e.source_page(&page).unwrap();
        let SuccessResult::SourceApplyPage(result) = result else {
            panic!("wrong result")
        };
        assert_eq!(result.entries[0].relative_path, "file.txt");
        assert!(result.next_offset.is_none());
        page.payload_hash = "f".repeat(64);
        assert_eq!(
            e.source_page(&page).unwrap_err().code,
            ErrorCode::RevisionConflict
        );
        page.payload_hash = c.payload_hash.clone();
        page.expected_session_revision = DecimalU64::new(0);
        assert_eq!(
            e.source_page(&page).unwrap_err().code,
            ErrorCode::RevisionConflict
        );
        let answer = request::SourceApplyResolve {
            run_id: c.run_id,
            attempt_id: c.attempt_id,
            approval_id: c.approval_id,
            payload_hash: c.payload_hash,
            expected_session_revision: before.marker.session_revision,
            expected_policy_revision: c.policy_revision,
            decision: ApprovalDecision::Allow,
        };
        assert_eq!(
            e.handle(
                &req(
                    "answer",
                    RequestBody::SourceApplyResolve(e.session.clone(), answer)
                ),
                &mut vec![]
            )
            .unwrap_err()
            .code,
            ErrorCode::CapabilityUnavailable
        );
        let after = e.store.snapshot().unwrap();
        assert_eq!(after.marker, before.marker);
        assert_eq!(after.state, before.state);
    }
}
