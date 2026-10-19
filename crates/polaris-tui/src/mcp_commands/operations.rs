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

pub(super) fn confirm_card<B: Backend, R: KeyReader>(
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
                    "y: 実行 / Esc: 取消 / ↑↓: 詳細"
                } else {
                    "↑↓: 全詳細を確認 / Esc: 取消"
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
        .map_err(|_| "MCP処理画面を表示できません".into())
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
                return if display_error { Err("MCP処理中に端末表示を失いました。結果の照合が必要です".into()) } else { Ok(output) };
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
        Error::RevisionConflict => "MCP設定が同時に変更されました。再表示してください",
        Error::Auth => "MCP認証情報が見つからないか、参照されています",
        Error::Protection => "MCP保存先・実行物の保護を確認できません",
        Error::Transport | Error::Timeout | Error::Protocol => "MCPサーバーに接続できません",
        Error::StopUnconfirmed => "MCPサーバーの停止を確認できません",
        _ => "MCP設定を確認できません",
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
        return Err("外部MCPは起動時の上限で拒否されています".into());
    }
    if approval_policy == ApprovalPolicy::Never {
        return Err("現在の承認設定ではMCPカタログを変更できません".into());
    }
    let store = host.store().ok_or("MCP serverが登録されていません")?;
    let registry = store
        .load_registry()
        .map_err(|error| message(error).to_owned())?
        .ok_or("MCP serverが登録されていません")?;
    let server = registry
        .servers
        .iter()
        .find(|server| server.server_id == server_id)
        .ok_or("そのMCP serverは登録されていません")?;
    if !server.enabled_projects.contains(host.project()) {
        return Err("そのMCP serverはこのプロジェクトで無効です".into());
    }
    host.validate(server)
        .map_err(|error| message(error).to_owned())?;
    let target = match &server.transport {
        Destination::StreamableHttp { url, .. } => format!("接続先: {url}"),
        Destination::Stdio {
            command, args, cwd, ..
        } => format!(
            "実行: {} {args:?}\n作業場所: {}",
            command.display(),
            cwd.display()
        ),
    };
    let detail = format!(
        "MCPカタログを取得して索引を保存します\nserver: {server_id}\n{target}\nregistry revision {}\n接続timeout {}秒、カタログtimeout {}秒\nツールは実行しません。取得先へ通信し、stdioは登録済み実行物を起動します。\n索引の変更は巻き戻しでは取り消せません。",
        registry.revision, server.timeouts.connect_seconds, server.timeouts.catalog_seconds
    );
    let (mut runtime, quarantined) = host
        .runtime("tui-mcp-check", "check", ceiling)
        .map_err(|error| message(error).to_owned())?
        .ok_or("このプロジェクトで有効なMCP serverがありません")?;
    if let Some(reason) = quarantined.get(server_id) {
        return Err(message(*reason).into());
    }
    let prepared = journal.prepare("C26", registry.revision, detail.as_bytes())?;
    if !confirm_card(terminal, reader, "MCPカタログの確認", &detail) {
        journal.deny(&prepared)?;
        return Ok(vec![Line::from("MCPカタログの確認を取り消しました")]);
    }
    if !store
        .load_registry()
        .is_ok_and(|current| current.as_ref() == Some(&registry))
    {
        journal.deny(&prepared)?;
        return Err("確認中にMCP registryが変更されました".into());
    }
    let started = journal.start(&prepared)?;
    let result = wait(
        terminal,
        "MCPカタログを取得しています。接続と索引の保存を確認するまでお待ちください。",
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
            return Err("MCPカタログを取得できませんでした。索引は更新していません".into());
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
        "{}: ツール{tools}件{}。索引を保存しました。",
        render::sanitize(server_id),
        if partial {
            "（上限のため一部）"
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
                "検出したMCPサーバーを登録",
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
        "launcher_not_supported" => "先に /mcp materialize <名前> で取得してください",
        "already_registered" => "同じ内容で登録済みです",
        "credential_in_arguments" | "unsupported_auth" | "environment_reference" => {
            "認証情報を安全に取り込めません"
        }
        "unsupported_transport" => "未対応の接続方式です",
        "command_not_found" | "command_not_absolute" | "command_missing" => {
            "実行物の場所を確認できません"
        }
        _ => "設定が登録条件を満たしていません",
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
                let mut lines = vec![Line::from("↑↓: 選択 / Enter: 登録内容を確認 / Esc: 取消")];
                lines.extend(candidates.iter().enumerate().skip(first).take(visible).map(
                    |(index, candidate)| {
                        Line::from(format!(
                            "{} {} — {}",
                            if index == selected { "›" } else { " " },
                            render::sanitize(candidate["name"].as_str().unwrap_or("-")),
                            candidate["reason"]
                                .as_str()
                                .map(reason)
                                .unwrap_or("登録内容を確認できます")
                        ))
                    },
                ));
                frame.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("検出したMCPサーバー"),
                    ),
                    frame.area(),
                );
            })
            .map_err(|_| "MCP検出結果を表示できません")?;
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
        .map_err(|_| "MCP設定を検出できません")?;
    let detected: Value =
        serde_json::from_str(&detected).map_err(|_| "MCP検出結果を確認できません")?;
    let candidates = detected["candidates"]
        .as_array()
        .ok_or("MCP検出結果を確認できません")?;
    if candidates.is_empty() {
        return Ok(vec![Line::from(
            "CodexとClaudeの設定にMCPサーバーは見つかりませんでした",
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
                        "その名前のMCP検出候補がありません。/mcp setup で選択してください".into(),
                    );
                }
                _ => {
                    return Err(
                        "同じ名前の候補が複数あります。/mcp setup で選択してください".into(),
                    );
                }
            }
        }
        None => match select(terminal, approver.reader, candidates)? {
            Some(selected) => selected,
            None => return Ok(vec![Line::from("MCP登録を取り消しました")]),
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
        .map_err(|_| "MCP registryを確認できません")?
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
                "{} を登録しました。次の実行から反映します。",
                render::sanitize(selected["name"].as_str().unwrap_or("MCP server"))
            ))])
        }
        (Err(body), Some(started)) => Err(format!(
            "{}。{}",
            registration_failure(&body),
            journal.unknown(&started)
        )),
        (Err(_), None) if approver.cancelled => Ok(vec![Line::from("MCP登録を取り消しました")]),
        (Err(_), None) => Err(
            "MCP登録を許可されませんでした。起動時の外部MCP上限と承認設定を確認してください".into(),
        ),
        (Ok(_), None) => Err("MCP登録の管理記録を確認できません".into()),
    }
}

fn registration_failure(body: &str) -> &'static str {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned));
    match code.as_deref() {
        Some("credential_changed") => "検出後に取得元の認証情報が変更されました",
        Some("registry_changed") => "確認中にMCP registryが変更されました",
        Some("unsafe_location") => "実行物か保存先の保護を確認できません",
        Some("invalid") => "MCP設定が登録条件を満たしていません",
        _ => "MCP登録を保存できませんでした",
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
        _ => return Err("/mcp materialize <検出名> [--runtime <絶対パス>]".into()),
    };
    if !permission.0 || permission.1 == ApprovalPolicy::Never {
        return Err("MCPの取得には起動時の外部MCP許可と承認可能な設定が必要です".into());
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
        "取得するMCPパッケージとinterpreterを確認しています。まだ取得は開始していません。",
        preparation,
    )
    .await?
    .map_err(|_| "MCP取得内容の確認を完了できません")?
    .map_err(materialize_refusal)?;
    let detail = format!("server: {name}\n{}", plan.description());
    let prepared = journal.prepare("C28", 0, detail.as_bytes())?;
    if !confirm_card(terminal, reader, "MCPパッケージを取得して固定", &detail) {
        journal.deny(&prepared)?;
        return Ok(vec![Line::from("MCPパッケージの取得を取り消しました")]);
    }
    let started = journal.start(&prepared)?;
    let held_lease = Arc::clone(lease);
    let worker = tokio::task::spawn_blocking(move || {
        // Retain the same project lock if the TUI future is dropped before
        // this existing synchronous package owner finishes its operation.
        let _lease = held_lease;
        detector.materialise_prepared_quiet(plan)
    });
    let pinned = wait(terminal, "MCPパッケージを取得しています。\n取得が終了するまで待っています。Escは送信取消にはなりません。\n登録は取得後に /mcp setup で別途確認します。", worker)
        .await.map_err(|_| journal.unknown(&started))?
        .map_err(|_| journal.unknown(&started))?
        .map_err(|reason| format!("{}。{}", materialize_refusal(reason), journal.unknown(&started)))?;
    let receipt = serde_json::to_vec(&pinned).map_err(|_| journal.unknown(&started))?;
    journal.complete(&started, &receipt)?;
    Ok(vec![Line::from(format!(
        "{} {} を固定しました。/mcp setup で登録内容を確認してください。",
        render::sanitize(&pinned.package),
        render::sanitize(&pinned.version)
    ))])
}

fn materialize_refusal(reason: polaris_core::mcp::setup::launcher::Refused) -> String {
    use polaris_core::mcp::setup::launcher::Refused;
    match reason {
        Refused::NotALauncher => "その検出名の安全な取得設定が見つかりません",
        Refused::ManagerNotSupported => "そのパッケージ取得方法には対応していません",
        Refused::FetchFailed => "MCPパッケージを取得できません",
        Refused::NoEntryPoint => "取得したMCPパッケージの実行入口を確認できません",
        Refused::RuntimeNotPinned => "実行に使うinterpreterの絶対パスを確認できません",
        Refused::NotSaved => "MCPパッケージを安全な保存先へ固定できません",
    }
    .into()
}
