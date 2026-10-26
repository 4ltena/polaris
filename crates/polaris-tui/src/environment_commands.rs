//! Direct local management commands. Work stays on a retained host worker;
//! the terminal keeps rendering and can deliver cancellation without a model.
use crate::{approver::KeyReader, render};
use polaris_core::{
    approval::ApprovalPolicy,
    environment::{
        native,
        owner::{EffectContext, Lease, Prepared},
    },
    management_effect::ManagementEffectStoreV1,
    rewind_workspace::WorkspaceOwner,
};
use polaris_envkit::request::{self, DirectCommand};
use ratatui::{
    Terminal,
    backend::Backend,
    crossterm::event::{self, Event, KeyCode, KeyModifiers},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle<B: Backend, R: KeyReader>(
    text: &str,
    cwd: &std::path::Path,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    policy: ApprovalPolicy,
    pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let command = request::direct(text).map_err(|_| "Usage: /env set|plan <profile> [--version <constraint>] [--path <directory>], or /env status")?;
    if command == DirectCommand::Status {
        return polaris_envkit::catalog::ALL
            .into_iter()
            .map(|id| {
                let definition =
                    polaris_envkit::bundled::load(id).map_err(|error| error.to_string())?;
                Ok(Line::from(format!(
                    "{}: available; verification is operation-specific; versions {}",
                    id.entry().name,
                    definition
                        .candidates
                        .iter()
                        .map(|tuple| tuple.version.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            })
            .collect();
    }
    if let DirectCommand::InspectOperation(root) | DirectCommand::Recover(root) = &command {
        let root = cwd.join(root);
        let recovering = matches!(command, DirectCommand::Recover(_));
        let lease = if recovering {
            if pending || policy == ApprovalPolicy::Never {
                return Err(
                    "Environment recovery requires an idle session and approval permission".into(),
                );
            }
            let lease = Arc::new(
                workspace
                    .try_mutation()
                    .map_err(|error| error.to_string())?,
            );
            workspace
                .require_quiescent_with_lease(&lease)
                .map_err(|error| error.to_string())?;
            Some(lease)
        } else {
            None
        };
        let project = workspace.project_root().to_owned();
        let state = effects.state_dir().to_owned();
        let effects = effects.clone();
        let cancel = AtomicBool::new(false);
        let worker = tokio::task::spawn_blocking(move || {
            if let Some(lease) = lease {
                polaris_core::environment::recovery::reconcile(
                    &project, &state, &root, &effects, &lease,
                )
            } else {
                polaris_core::environment::recovery::inspect(&project, &state, &root)
            }
        });
        let report = wait(
            terminal,
            "Checking retained environment evidence. No steps are repeated.",
            worker,
            &cancel,
        )
        .await?;
        return Ok(serde_json::to_string_pretty(&report)
            .map_err(|error| error.to_string())?
            .lines()
            .map(|line| Line::from(render::sanitize(line)))
            .collect());
    }
    let (mut request, plan_only) = match command {
        DirectCommand::Plan(request) => (request, true),
        DirectCommand::Set(request) => (request, false),
        DirectCommand::Status | DirectCommand::InspectOperation(_) | DirectCommand::Recover(_) => {
            unreachable!()
        }
    };
    if pending || (!plan_only && policy == ApprovalPolicy::Never) {
        return Err(
            "Environment setup is unavailable while inputs are queued or approval is disabled"
                .into(),
        );
    }
    let target = request
        .target
        .as_ref()
        .map_or_else(|| cwd.to_owned(), |path| cwd.join(path))
        .canonicalize()
        .map_err(|error| error.to_string())?;
    request.target = Some(
        target
            .to_str()
            .ok_or("Invalid environment target path")?
            .into(),
    );
    let lease = Arc::new(
        workspace
            .try_mutation()
            .map_err(|error| error.to_string())?,
    );
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|error| error.to_string())?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let root = workspace.project_root().to_owned();
    let state = effects.state_dir().to_owned();
    let profile = request.profile;
    let held = Arc::clone(&lease);
    let cancel = Arc::new(AtomicBool::new(false));
    let preparing_cancel = Arc::clone(&cancel);
    let worker = tokio::task::spawn_blocking(move || {
        let _held = held;
        Prepared::prepare(
            &root,
            &state,
            &request,
            native::capture_with_cancel(profile, || preparing_cancel.load(Ordering::Acquire))?,
        )
    });
    let mut prepared = wait(
        terminal,
        "Inspecting the fixed environment plan. No download has started.",
        worker,
        &cancel,
    )
    .await?;
    let description = prepared.description()?;
    if plan_only {
        return Ok(description
            .lines()
            .map(|line| Line::from(render::sanitize(line)))
            .collect());
    }
    if cancel.load(Ordering::Acquire)
        || !crate::mcp_commands::confirm_environment_card(
            terminal,
            reader,
            "Environment setup",
            &description,
        )
    {
        return Ok(vec![Line::from("Environment setup cancelled")]);
    }
    let digest = prepared.digest()?;
    let effects = effects.clone();
    let worker_cancel = Arc::clone(&cancel);
    let runtime = tokio::runtime::Handle::current();
    let worker = tokio::task::spawn_blocking(move || {
        let prerequisites = prepared.plan.snapshot.host_prerequisites.clone();
        let context = EffectContext {
            store: &effects,
            lease: Lease::Direct(&lease),
            session: None,
        };
        let result = runtime.block_on(prepared.execute(
            &context,
            &digest,
            || native::refresh(&prerequisites, || worker_cancel.load(Ordering::Acquire)),
            || worker_cancel.load(Ordering::Acquire),
        ));
        while prepared.cleanup_pending() {
            let _ = prepared.poll_cleanup();
            std::thread::sleep(Duration::from_millis(25));
        }
        result.map(|receipt| {
            vec![
                format!("Environment verified: {}", receipt.plan_sha256),
                format!(
                    "Receipt: {}",
                    prepared.retained_root().join("receipt.json").display()
                ),
            ]
        })
    });
    wait(terminal, "Setting up the environment offline after pinned downloads.\nEsc or Ctrl-C: request cancellation and wait for child cleanup.", worker, &cancel).await
        .map(|lines| lines.into_iter().map(|line| Line::from(render::sanitize(&line))).collect())
}

async fn wait<B: Backend, T>(
    terminal: &RefCell<Terminal<B>>,
    message: &str,
    mut worker: tokio::task::JoinHandle<Result<T, String>>,
    cancelled: &AtomicBool,
) -> Result<T, String> {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut lost = false;
    loop {
        tokio::select! {
            result = &mut worker => {
                let result = result.map_err(|_| "Environment worker stopped; inspect retained state")?;
                return if lost { Err("Terminal display was lost. Inspect the retained environment receipt".into()) } else { result };
            }
            _ = tick.tick() => {
                lost |= terminal.borrow_mut().draw(|frame| frame.render_widget(Paragraph::new(message).block(Block::default().borders(Borders::ALL).title("Environment")), frame.area())).is_err();
                if lost { cancelled.store(true, Ordering::Release); }
                if event::poll(Duration::ZERO).unwrap_or(false)
                    && let Ok(Event::Key(key)) = crate::terminal::read_event()
                    && (key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)))
                { cancelled.store(true, Ordering::Release); }
            }
        }
    }
}

#[cfg(test)]
mod screen_copy_tests {
    use super::*;
    #[tokio::test]
    async fn environment_wait_text_uses_the_shared_visible_copy_backend() {
        let terminal = RefCell::new(crate::terminal::test_terminal(100, 15));
        let cancelled = AtomicBool::new(false);
        let worker = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok::<_, String>(())
        });
        wait(
            &terminal,
            "synthetic environment progress 日本語",
            worker,
            &cancelled,
        )
        .await
        .unwrap();
        assert!(
            crate::terminal::test_copy_all(&mut terminal.borrow_mut())
                .contains("synthetic environment progress 日本語")
        );
    }
}
