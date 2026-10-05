//! Explicit argv execution under the startup ceiling and durable C38 receipt.
use crate::{account, approver::KeyReader};
use polaris_core::{
    management_effect::ManagementEffectStoreV1, rewind_workspace::WorkspaceOwner,
    sandbox_command::SandboxCommandOwnerV1, session::Session,
};
use polaris_sandbox::{ControlledEnd, SandboxPolicy};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    session: &Session,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    startup_policy: &SandboxPolicy,
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    if has_pending {
        return Err("送信待ちの入力があるためsandboxを実行できません".into());
    }
    session.check_persistence().map_err(|e| e.to_string())?;
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("保存された会話がありません")?;
    let request = request.trim().trim_start_matches('/');
    let request = request.strip_prefix("sandbox ").unwrap_or(request).trim();
    if request.len() > 8 * 1024 {
        return Err("sandbox commandは8KiB以内で指定してください".into());
    }
    let argv: Vec<String> = serde_json::from_str(request)
        .map_err(|_| "sandboxは絶対pathのprogramと引数をJSON配列で指定してください")?;
    let preview =
        SandboxCommandOwnerV1::preview(persistence, workspace.project_root(), startup_policy, argv)
            .map_err(|e| e.to_string())?;
    let command = serde_json::to_string(preview.argv()).map_err(|e| e.to_string())?;
    let confinement = if let Some(boundary) = preview.policy().isolated_boundary() {
        format!(
            "読み取り: {:?}\nnetwork: {} / 子孫作成禁止",
            boundary.readable_roots,
            if boundary.loopback_network {
                "loopbackのみ"
            } else {
                "禁止"
            }
        )
    } else {
        "既存CLIのLinux Landlock policy。書き込みは表示の範囲のみ。読み取りはfilesystem全体、networkと子孫作成は制限しません。process groupを監視し、未分類の効果を巻き戻しません。".into()
    };
    let detail = format!(
        "{command}\n{}\n{confinement}\n実行上限30秒 / 出力各256KiB\n実行結果が不明な場合、新しい操作を停止します。",
        preview.policy().describe()
    );
    let lease = workspace
        .try_mutation()
        .map_err(|_| "projectで実行中の操作があります")?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|e| e.to_string())?;
    let owner = SandboxCommandOwnerV1 {
        effects,
        lease: &lease,
        project: workspace.project_root(),
    };
    let prepared = owner
        .prepare(persistence, preview)
        .map_err(|e| e.to_string())?;
    if !account::confirm(
        terminal,
        reader,
        "sandbox commandを実行",
        workspace.project_root().to_string_lossy().as_ref(),
        &detail,
    ) {
        owner.deny(&prepared).map_err(|e| e.to_string())?;
        return Ok(vec![Line::from("sandboxの実行を取り消しました")]);
    }
    let started = Instant::now();
    let outcome = owner
        .execute(persistence, &prepared, || {
            #[cfg(target_os = "linux")]
            if crate::signal::pending_linux_process_shutdown().is_some() {
                return true;
            }
            started.elapsed() >= Duration::from_secs(30)
        })
        .map_err(|e| e.to_string())?;
    let unknown = outcome.end == ControlledEnd::StopUnconfirmed
        || outcome.pending.is_some()
        || !outcome.stdout_eof
        || !outcome.stderr_eof
        || outcome.problem.is_some();
    let mut lines = vec![Line::from(format!(
        "C38 {} / {} / exit {}",
        prepared.operation_id().as_hex(),
        if unknown {
            "結果不明・新しい操作を停止"
        } else if outcome.end == ControlledEnd::Exited {
            "終了"
        } else {
            "停止"
        },
        outcome
            .status
            .as_ref()
            .and_then(|status| status.code())
            .map_or_else(|| "未確認".into(), |code| code.to_string())
    ))];
    // Captured streams have independent native bounds; the reader projection is smaller.
    let mut remaining = 64 * 1024;
    for (name, bytes, truncated) in [
        ("stdout", &outcome.stdout, outcome.stdout_truncated),
        ("stderr", &outcome.stderr, outcome.stderr_truncated),
    ] {
        let take = bytes.len().min(remaining);
        remaining -= take;
        if take != 0 {
            lines.push(Line::from(format!("{name}:")));
            lines.extend(
                String::from_utf8_lossy(&bytes.as_bytes()[..take])
                    .lines()
                    .map(|s| Line::from(crate::render::sanitize(s))),
            );
        }
        if truncated || take < bytes.len() {
            lines.push(Line::from(format!("{name}: 表示上限で省略しました")));
        }
    }
    // An unresolved owned child is now durably classified unknown; dropping it
    // transfers cleanup to the existing sandbox registry before releasing EX.
    drop(outcome);
    Ok(lines)
}

#[cfg(test)]
#[path = "sandbox_commands/tests.rs"]
mod tests;
