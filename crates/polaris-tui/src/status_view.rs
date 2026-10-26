//! Status projection over the existing session and management owners.
use super::*;

pub(super) struct StatusSnapshot<'a> {
    pub(super) session: &'a polaris_core::session::Session,
    pub(super) session_path: Option<&'a Path>,
    pub(super) accounts: Result<Vec<ratatui::text::Line<'static>>, String>,
    #[cfg(unix)]
    pub(super) binding: Option<&'a dyn account::BindingControl>,
    pub(super) provider_name: &'a str,
    pub(super) model_name: &'a str,
    pub(super) usage_report: polaris_provider::UsageReport,
    pub(super) sandbox: &'a polaris_sandbox::SandboxPolicy,
    pub(super) approval: polaris_core::approval::ApprovalPolicy,
    pub(super) startup_approval: polaris_core::approval::ApprovalPolicy,
    pub(super) mcp_external: bool,
    pub(super) audit_path: Option<&'a Path>,
    pub(super) turn_authority_present: bool,
    pub(super) fixed_effort: Option<&'a str>,
    pub(super) answer_finalizer:
        Option<&'a polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner>,
    #[cfg(unix)]
    pub(super) project_root: &'a Path,
    #[cfg(unix)]
    pub(super) management_effects: &'a polaris_core::management_effect::ManagementEffectStoreV1,
    #[cfg(unix)]
    pub(super) recovery_issue: Option<&'a str>,
}

fn status_section(lines: &mut Vec<ratatui::text::Line<'static>>, title: &str) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};

    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(
        title.to_owned(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
}

pub(super) fn status_row(label: &str, value: &str) -> ratatui::text::Line<'static> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use unicode_width::UnicodeWidthStr;

    const LABEL_WIDTH: usize = 21;
    let prefix = if label.is_empty() {
        " ".repeat(LABEL_WIDTH + 4)
    } else {
        format!(
            "  {label}:{} ",
            " ".repeat(LABEL_WIDTH.saturating_sub(label.width()))
        )
    };
    Line::from(vec![
        Span::styled(prefix, Style::default().add_modifier(Modifier::DIM)),
        Span::raw(render::sanitize(value).escape_debug().to_string()),
    ])
}

pub(super) fn status_lines(snapshot: StatusSnapshot<'_>) -> Vec<ratatui::text::Line<'static>> {
    let StatusSnapshot {
        session,
        session_path,
        accounts,
        #[cfg(unix)]
        binding,
        provider_name,
        model_name,
        usage_report,
        sandbox,
        approval,
        startup_approval,
        mcp_external,
        audit_path,
        turn_authority_present,
        fixed_effort,
        answer_finalizer,
        #[cfg(unix)]
        project_root,
        #[cfg(unix)]
        management_effects,
        #[cfg(unix)]
        recovery_issue,
    } = snapshot;
    let usage = usage_report.usage;
    let mut lines = Vec::new();
    status_section(&mut lines, "Polaris status (read-only)");
    lines.push(status_row("Model", model_name));
    lines.push(status_row("provider", provider_name));
    lines.push(status_row(
        "effort",
        &format!(
            "{} (changing fixed overrides is unsupported in TUI)",
            fixed_effort.unwrap_or("automatic")
        ),
    ));
    lines.push(status_row(
        "Conversation",
        &format!("{} messages", session.messages.len()),
    ));

    let conversation_id = match &session.persistence {
        Some(saved) => Some(
            saved
                .snapshot()
                .map(|snapshot| snapshot.state.session_id)
                .unwrap_or_else(|_| "unreadable".into()),
        ),
        None => session_path
            .and_then(|path| path.file_stem())
            .and_then(|name| name.to_str())
            .map(str::to_owned),
    };
    lines.push(status_row(
        "Session ID",
        conversation_id.as_deref().unwrap_or("unsaved"),
    ));
    status_section(&mut lines, "Account");
    #[cfg(unix)]
    lines.push(status_row(
        "Active registration",
        &binding
            .and_then(|binding| binding.current())
            .map(|(id, generation)| format!("{id} (generation {generation})"))
            .unwrap_or_else(|| {
                "no fixed registration (legacy authentication or external provider)".into()
            }),
    ));
    match accounts {
        Ok(accounts) => lines.extend(accounts),
        Err(error) => lines.push(status_row(
            "Registration state",
            &format!("unverified: {error}"),
        )),
    }

    status_section(&mut lines, "Conditions");
    lines.push(status_row("Approval (current)", &format!("{approval:?}")));
    lines.push(status_row(
        "Approval (startup)",
        &format!("{startup_approval:?}"),
    ));
    lines.push(status_row("sandbox", sandbox.mode().as_str()));
    for (index, root) in sandbox.writable_roots().iter().enumerate() {
        lines.push(status_row(
            if index == 0 { "Writable roots" } else { "" },
            &root.display().to_string(),
        ));
    }
    lines.push(status_row(
        "External MCP",
        &format!("{mcp_external} (inspect individual servers with /mcp)"),
    ));

    status_section(&mut lines, "Observations and workflow");
    lines.push(status_row(
        "tokens",
        &format!(
            "in {} / out {} / cache {} / total {}",
            usage.input_tokens, usage.output_tokens, usage.cached_tokens, usage.total_tokens,
        ),
    ));
    lines.push(status_row(
        "Missing / failed",
        &format!(
            "{} / {}",
            usage_report.missing_responses, usage_report.failed_requests,
        ),
    ));
    if let Some(workflow) = &session.workflow {
        let state = workflow.state();
        lines.push(status_row("phase", &state.phase.to_string()));
        if state.skills.is_empty() {
            lines.push(status_row("skills", "none"));
        } else {
            for (index, skill) in state.skills.iter().enumerate() {
                lines.push(status_row(
                    if index == 0 { "skills" } else { "" },
                    &skill.id,
                ));
            }
        }
        lines.push(status_row("manifest", &state.skill_manifest_hash));
    }

    status_section(&mut lines, "Availability");
    lines.push(status_row(
        "Live connections",
        "unverified (authentication, model, MCP; no connection checks performed)",
    ));
    lines.push(status_row(
        "web-search",
        "disabled (cached/live unavailable; endpoint capability contract unverified)",
    ));
    lines.push(status_row(
        "capacity",
        "observation disconnected (resource capacity unverified)",
    ));

    status_section(&mut lines, "Management and diagnostics");
    lines.push(status_row(
        "audit",
        &format!(
            "{} (restart with --audit to change)",
            audit_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unspecified".into())
        ),
    ));
    lines.push(status_row(
        "Host authority",
        &format!("routine/review configured={turn_authority_present} (content and approval tokens hidden)"),
    ));
    lines.extend(cat_diagnostic_lines(answer_finalizer));
    #[cfg(unix)]
    lines.extend(management_effect_diagnostic(
        project_root,
        management_effects,
        recovery_issue,
    ));
    lines
}
