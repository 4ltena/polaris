//! TUI MCP catalog and registration routes use the admitted domain owners.
use super::{journal::Journal, *};
use polaris_core::{
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    management_effect::StartedManagementEffectV1,
};
use polaris_mcp::registration::Destination;
use ratatui::{
    crossterm::event::KeyCode,
    widgets::{Block, Borders, Paragraph},
};
use serde_json::Value;
use std::time::Duration;

pub(crate) fn confirm_card<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    title: &str,
    detail: &str,
) -> bool {
    let input: Vec<_> = detail
        .lines()
        .map(|line| render::HistoryLine {
            line: Line::from(render::sanitize(line)),
            shaded: false,
        })
        .collect();
    let mut offset = 0usize;
    loop {
        let mut ready = false;
        let mut maximum = 0;
        if terminal
            .borrow_mut()
            .draw(|frame| {
                let area = frame.area();
                let width = area.width.saturating_sub(2);
                let visible = area.height.saturating_sub(3) as usize;
                let wrapped = render::wrap_history_lines(&input, width);
                maximum = wrapped.len().saturating_sub(visible);
                offset = offset.min(maximum);
                ready = width > 0 && visible > 0 && offset + visible >= wrapped.len();
                let mut lines: Vec<_> = wrapped
                    .into_iter()
                    .skip(offset)
                    .take(visible)
                    .map(|row| row.line)
                    .collect();
                lines.push(Line::from(if ready {
                    "y: execute / Esc: cancel / ↑↓: details"
                } else {
                    "↑↓: review all details / Esc: cancel"
                }));
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title(title)),
                    area,
                );
            })
            .is_err()
        {
            return false;
        }
        match reader.read_key() {
            Ok(KeyCode::Char('y')) if ready => return true,
            Ok(KeyCode::Down) => offset = offset.saturating_add(1).min(maximum),
            Ok(KeyCode::Up) => offset = offset.saturating_sub(1),
            Ok(KeyCode::PageDown | KeyCode::End) => offset = maximum,
            Ok(KeyCode::PageUp | KeyCode::Home) => offset = 0,
            Ok(KeyCode::Esc) | Err(_) => return false,
            _ => {}
        }
    }
}

pub(super) fn progress<B: Backend>(
    terminal: &RefCell<Terminal<B>>,
    text: &str,
) -> Result<(), String> {
    terminal
        .borrow_mut()
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("MCP")),
                frame.area(),
            );
        })
        .map(|_| ())
        .map_err(|_| "Cannot display the MCP processing screen".into())
}

pub(super) async fn wait<B: Backend, F: std::future::Future>(
    terminal: &RefCell<Terminal<B>>,
    text: &str,
    future: F,
) -> Result<F::Output, String> {
    tokio::pin!(future);
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut display_error = false;
    loop {
        tokio::select! {
            output = &mut future => {
                return if display_error { Err("Terminal display was lost during MCP processing. Outcome reconciliation is required".into()) } else { Ok(output) };
            }
            _ = timer.tick() => {
                // Do not drop an owner while it still owns a connection or download.
                display_error |= progress(terminal, text).is_err();
            }
        }
    }
}

fn message(error: polaris_mcp::Error) -> &'static str {
    use polaris_mcp::Error;
    match error {
        Error::RevisionConflict => "MCP configuration changed concurrently. Redisplay it",
        Error::Auth => "MCP credentials are missing or still referenced",
        Error::Protection => "Cannot verify MCP storage or executable protection",
        Error::Transport | Error::Timeout | Error::Protocol => "Cannot connect to the MCP server",
        Error::StopUnconfirmed => "Cannot verify that the MCP server stopped",
        _ => "Cannot verify MCP configuration",
    }
}

pub(super) async fn check<B: Backend, R: KeyReader>(
    server_id: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    host: &LocalHost,
    (ceiling, approval_policy): (ExternalCeiling, ApprovalPolicy),
    journal: &Journal<'_>,
) -> Result<Vec<Line<'static>>, String> {
    if !ceiling.permits_external() {
        return Err("External MCP is denied by the startup ceiling".into());
    }
    if approval_policy == ApprovalPolicy::Never {
        return Err("Current approval settings do not permit MCP catalog changes".into());
    }
    let store = host.store().ok_or("MCP server is not registered")?;
    let registry = store
        .load_registry()
        .map_err(|error| message(error).to_owned())?
        .ok_or("MCP server is not registered")?;
    let server = registry
        .servers
        .iter()
        .find(|server| server.server_id == server_id)
        .ok_or("That MCP server is not registered")?;
    if !server.enabled_projects.contains(host.project()) {
        return Err("That MCP server is disabled for this project".into());
    }
    host.validate(server)
        .map_err(|error| message(error).to_owned())?;
    let target = match &server.transport {
        Destination::StreamableHttp { url, .. } => format!("Endpoint: {url}"),
        Destination::Stdio {
            command, args, cwd, ..
        } => format!(
            "Run: {} {args:?}\nWorking directory: {}",
            command.display(),
            cwd.display()
        ),
    };
    let detail = format!(
        "Retrieve the MCP catalog and save its index\nserver: {server_id}\n{target}\nregistry revision {}\nConnection timeout {} s, catalog timeout {} s\nNo tools are called. Contacts the endpoint; stdio launches the registered executable.\nRewind cannot undo index changes.",
        registry.revision, server.timeouts.connect_seconds, server.timeouts.catalog_seconds
    );
    let (mut runtime, quarantined) = host
        .runtime("tui-mcp-check", "check", ceiling)
        .map_err(|error| message(error).to_owned())?
        .ok_or("No MCP servers enabled for this project")?;
    if let Some(reason) = quarantined.get(server_id) {
        return Err(message(*reason).into());
    }
    let prepared = journal.prepare("C26", registry.revision, detail.as_bytes())?;
    if !confirm_card(terminal, reader, "Verify MCP catalog", &detail) {
        journal.deny(&prepared)?;
        return Ok(vec![Line::from("MCP catalog verification cancelled")]);
    }
    if !store
        .load_registry()
        .is_ok_and(|current| current.as_ref() == Some(&registry))
    {
        journal.deny(&prepared)?;
        return Err("MCP registry changed during confirmation".into());
    }
    let started = journal.start(&prepared)?;
    let result = wait(
        terminal,
        "Retrieving the MCP catalog. Waiting for connection and index persistence to be verified.",
        async {
            let entry = runtime.index_entry(server_id, std::future::pending()).await;
            let stopped = runtime.shutdown().await;
            (entry, stopped)
        },
    )
    .await
    .map_err(|_| journal.unknown(&started))?;
    let (entry, stopped) = result;
    if stopped.is_err() {
        return Err(journal.unknown(&started));
    }
    let entry = match entry {
        Ok(entry) => entry,
        Err(_) => {
            journal.complete(&started, b"mcp-catalog:stopped-without-index-update")?;
            return Err("Could not retrieve the MCP catalog. The index is unchanged".into());
        }
    };
    let (tools, partial) = store
        .save_catalog_entry(server_id, entry)
        .map_err(|_| journal.unknown(&started))?;
    journal.complete(
        &started,
        format!("mcp-catalog:{server_id}:{tools}:{partial}").as_bytes(),
    )?;
    Ok(vec![Line::from(format!(
        "{}: {tools} tools{}. Index saved.",
        render::sanitize(server_id),
        if partial {
            " (partial due to limit)"
        } else {
            ""
        }
    ))])
}

struct RegistrationApprover<'a, 'j, B: Backend, R: KeyReader> {
    terminal: &'a RefCell<Terminal<B>>,
    reader: &'a mut R,
    journal: &'a Journal<'j>,
    revision: u64,
    started: Option<StartedManagementEffectV1>,
    error: Option<String>,
    cancelled: bool,
}

impl<B: Backend, R: KeyReader> Approver for RegistrationApprover<'_, '_, B, R> {
    fn ask(&mut self, reason: &str) -> Decision {
        let result = (|| {
            let prepared = self
                .journal
                .prepare("C27", self.revision, reason.as_bytes())?;
            if !confirm_card(
                self.terminal,
                self.reader,
                "Register a detected MCP server",
                reason,
            ) {
                self.journal.deny(&prepared)?;
                self.cancelled = true;
                return Ok(Decision::Deny);
            }
            self.started = Some(self.journal.start(&prepared)?);
            Ok(Decision::Allow)
        })();
        match result {
            Ok(decision) => decision,
            Err(error) => {
                self.error = Some(error);
                Decision::Deny
            }
        }
    }
}

fn reason(code: &str) -> &'static str {
    match code {
        "launcher_not_supported" => "Download first with /mcp materialize <name>",
        "already_registered" => "An identical registration already exists",
        "credential_in_arguments" | "unsupported_auth" | "environment_reference" => {
            "Cannot safely import credentials"
        }
        "unsupported_transport" => "Unsupported transport",
        "command_not_found" | "command_not_absolute" | "command_missing" => {
            "Cannot verify the executable location"
        }
        _ => "Configuration does not meet registration requirements",
    }
}

fn select<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    candidates: &[Value],
) -> Result<Option<usize>, String> {
    let mut selected = 0usize;
    loop {
        terminal
            .borrow_mut()
            .draw(|frame| {
                let visible = frame.area().height.saturating_sub(4).max(1) as usize;
                let first = selected.saturating_sub(visible.saturating_sub(1));
                let mut lines = vec![Line::from(
                    "↑↓: select / Enter: review registration / Esc: cancel",
                )];
                lines.extend(candidates.iter().enumerate().skip(first).take(visible).map(
                    |(index, candidate)| {
                        Line::from(format!(
                            "{} {} — {}",
                            if index == selected { "›" } else { " " },
                            render::sanitize(candidate["name"].as_str().unwrap_or("-")),
                            candidate["reason"]
                                .as_str()
                                .map(reason)
                                .unwrap_or("Registration details available for review")
                        ))
                    },
                ));
                frame.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Detected MCP servers"),
                    ),
                    frame.area(),
                );
            })
            .map_err(|_| "Cannot display MCP discovery results")?;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1).min(candidates.len() - 1),
            Ok(KeyCode::Up) => selected = selected.saturating_sub(1),
            Ok(KeyCode::Enter) => return Ok(Some(selected)),
            Ok(KeyCode::Esc) | Err(_) => return Ok(None),
            _ => {}
        }
    }
}

pub(super) async fn setup<B: Backend, R: KeyReader>(
    name: Option<&str>,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    host: &LocalHost,
    permission: (ExternalCeiling, ApprovalPolicy),
    journal: &Journal<'_>,
) -> Result<Vec<Line<'static>>, String> {
    let tool = host.setup_tool(permission.0);
    let mut gate = Gate::new(permission.1);
    let mut approver = RegistrationApprover {
        terminal,
        reader,
        journal,
        revision: 0,
        started: None,
        error: None,
        cancelled: false,
    };
    let detected = tool
        .call(
            &serde_json::json!({"op":"detect"}),
            std::future::pending(),
            &mut gate,
            &mut approver,
        )
        .await
        .map_err(|_| "Cannot detect MCP configuration")?;
    let detected: Value =
        serde_json::from_str(&detected).map_err(|_| "Cannot verify MCP discovery results")?;
    let candidates = detected["candidates"]
        .as_array()
        .ok_or("Cannot verify MCP discovery results")?;
    if candidates.is_empty() {
        return Ok(vec![Line::from(
            "No MCP servers found in Codex or Claude configuration",
        )]);
    }
    let selected = match name {
        Some(name) => {
            let matches: Vec<_> = candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate["name"].as_str() == Some(name))
                .map(|(index, _)| index)
                .collect();
            match matches.as_slice() {
                [selected] => *selected,
                [] => {
                    return Err(
                        "No detected MCP candidate with that name. Select one with /mcp setup"
                            .into(),
                    );
                }
                _ => {
                    return Err(
                        "Multiple candidates share that name. Select one with /mcp setup".into(),
                    );
                }
            }
        }
        None => match select(terminal, approver.reader, candidates)? {
            Some(selected) => selected,
            None => return Ok(vec![Line::from("MCP registration cancelled")]),
        },
    };
    let selected = &candidates[selected];
    if let Some(code) = selected["reason"].as_str() {
        return Err(reason(code).into());
    }
    approver.revision = host
        .store()
        .map(|store| store.load_registry())
        .transpose()
        .map_err(|_| "Cannot verify the MCP registry")?
        .flatten()
        .map_or(0, |registry| registry.revision);
    let result = tool
        .call(
            &serde_json::json!({"op":"register","candidate":selected["candidate"]}),
            std::future::pending(),
            &mut gate,
            &mut approver,
        )
        .await;
    if let Some(error) = approver.error {
        return Err(error);
    }
    match (result, approver.started) {
        (Ok(receipt), Some(started)) => {
            journal.complete(&started, receipt.as_bytes())?;
            Ok(vec![Line::from(format!(
                "Registered {}. Effective from the next run.",
                render::sanitize(selected["name"].as_str().unwrap_or("MCP server"))
            ))])
        }
        (Err(body), Some(started)) => Err(format!(
            "{}。{}",
            registration_failure(&body),
            journal.unknown(&started)
        )),
        (Err(_), None) if approver.cancelled => Ok(vec![Line::from("MCP registration cancelled")]),
        (Err(_), None) => Err(
            "MCP registration denied. Check the startup external MCP ceiling and approval settings"
                .into(),
        ),
        (Ok(_), None) => Err("Cannot verify MCP registration management records".into()),
    }
}

fn registration_failure(body: &str) -> &'static str {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned));
    match code.as_deref() {
        Some("credential_changed") => "Source credentials changed after discovery",
        Some("registry_changed") => "MCP registry changed during confirmation",
        Some("unsafe_location") => "Cannot verify executable or storage protection",
        Some("invalid") => "MCP configuration does not meet registration requirements",
        _ => "Could not save the MCP registration",
    }
}

pub(super) async fn materialize<B: Backend, R: KeyReader>(
    parts: &[&str],
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    host: &LocalHost,
    permission: (bool, ApprovalPolicy),
    (journal, lease): (
        &Journal<'_>,
        &Arc<polaris_core::project_mutation::ProjectMutationLeaseV1>,
    ),
) -> Result<Vec<Line<'static>>, String> {
    let (name, runtime) = match parts {
        [name] => (*name, None),
        [name, "--runtime", runtime] => (*name, Some(std::path::Path::new(runtime))),
        _ => return Err("/mcp materialize <detected-name> [--runtime <absolute-path>]".into()),
    };
    if !permission.0 || permission.1 == ApprovalPolicy::Never {
        return Err("MCP downloads require startup external MCP permission and settings that permit approval".into());
    }
    let home = host.account_home().to_owned();
    let name_owned = name.to_owned();
    let runtime = runtime.map(Path::to_owned);
    let held_lease = Arc::clone(lease);
    let preparation = tokio::task::spawn_blocking(move || {
        let _lease = held_lease;
        let detector = polaris_core::mcp::setup::Detector::new(
            home,
            polaris_core::mcp::setup::HOST_PATH
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
        );
        detector
            .prepare_materialise(&name_owned, runtime.as_deref())
            .map(|plan| (detector, plan))
    });
    let (detector, plan) = wait(
        terminal,
        "Checking the MCP package and interpreter. Download has not started.",
        preparation,
    )
    .await?
    .map_err(|_| "Cannot complete MCP download review")?
    .map_err(materialize_refusal)?;
    let detail = format!("server: {name}\n{}", plan.description());
    let prepared = journal.prepare("C28", 0, detail.as_bytes())?;
    if !confirm_card(terminal, reader, "Download and pin MCP package", &detail) {
        journal.deny(&prepared)?;
        return Ok(vec![Line::from("MCP package download cancelled")]);
    }
    let started = journal.start(&prepared)?;
    let held_lease = Arc::clone(lease);
    let worker = tokio::task::spawn_blocking(move || {
        // Retain the same project lock if the TUI future is dropped before
        // this existing synchronous package owner finishes its operation.
        let _lease = held_lease;
        detector.materialise_prepared_quiet(plan)
    });
    let pinned = wait(terminal, "Downloading the MCP package.\nWaiting for the download to finish. Esc does not cancel the transfer.\nReview registration separately afterward with /mcp setup.", worker)
        .await.map_err(|_| journal.unknown(&started))?
        .map_err(|_| journal.unknown(&started))?
        .map_err(|reason| format!("{}。{}", materialize_refusal(reason), journal.unknown(&started)))?;
    let receipt = serde_json::to_vec(&pinned).map_err(|_| journal.unknown(&started))?;
    journal.complete(&started, &receipt)?;
    Ok(vec![Line::from(format!(
        "Pinned {} {}. Review registration with /mcp setup.",
        render::sanitize(&pinned.package),
        render::sanitize(&pinned.version)
    ))])
}

fn materialize_refusal(reason: polaris_core::mcp::setup::launcher::Refused) -> String {
    use polaris_core::mcp::setup::launcher::Refused;
    match reason {
        Refused::NotALauncher => "No safe download configuration for that detected name",
        Refused::ManagerNotSupported => "Unsupported package download method",
        Refused::FetchFailed => "Cannot download the MCP package",
        Refused::NoEntryPoint => "Cannot verify the downloaded MCP package entry point",
        Refused::RuntimeNotPinned => "Cannot verify the runtime interpreter's absolute path",
        Refused::NotSaved => "Cannot pin the MCP package in safe storage",
    }
    .into()
}
