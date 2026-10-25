//! Account commands never put OAuth codes or token values into TUI history.
use crate::{approver::KeyReader, render};
use polaris_core::{
    management_effect::{
        ManagementEffectStoreV1,
        account_change::{AccountChangeOwnerV1, AccountChangeV1},
    },
    rewind_workspace::WorkspaceOwner,
};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{cell::RefCell, sync::Arc, time::Duration};

/// Supplied by the CLI's existing token owner; every hosted provider shares it.
pub trait BindingControl: Send + Sync {
    fn current(&self) -> Option<(String, u64)>;
    fn bind(&self, registration: &str) -> Result<String, String>;
}

pub(crate) fn confirm<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    operation: &str,
    target: &str,
    detail: &str,
) -> bool {
    loop {
        let mut ready = false;
        if terminal
            .borrow_mut()
            .draw(|frame| {
                ready = render::render_management_confirmation(frame, operation, target, detail);
            })
            .is_err()
        {
            return false;
        }
        match reader.read_key() {
            Ok(ratatui::crossterm::event::KeyCode::Esc) | Err(_) => return false,
            Ok(ratatui::crossterm::event::KeyCode::Char('y')) => {
                return ready;
            }
            _ => {}
        }
    }
}

pub(crate) async fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    binding: Option<&Arc<dyn BindingControl>>,
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let request = request.trim();
    if request.is_empty() || request == "list" {
        let mut lines = crate::account_list_lines()?;
        lines.push(Line::from(match binding.and_then(|b| b.current()) {
            Some((id, generation)) => format!("使用登録: {id}（世代 {generation}）"),
            None => "使用登録: 登録への明示的な束縛なし".into(),
        }));
        return Ok(lines);
    }
    if has_pending {
        return Err("送信待ちの入力があるため認証操作を開始できません".into());
    }
    let lease = workspace
        .try_mutation()
        .map_err(|_| "実行中の操作があるため認証状態を変更できません")?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let (command, extra) = request
        .split_once(char::is_whitespace)
        .unwrap_or((request, ""));
    if command == "bind" {
        let binding = binding.ok_or("このproviderは登録アカウント切替に対応していません")?;
        if extra.is_empty() || extra.split_whitespace().count() != 1 {
            return Err("/account bind <登録ID>".into());
        }
        if !confirm(
            terminal,
            reader,
            "使用アカウントの切替",
            extra,
            "次の送信から使用します。登録自体は変更しません。",
        ) {
            return Ok(vec![Line::from("アカウント切替を取り消しました")]);
        }
        let id = binding.bind(extra)?;
        return Ok(vec![Line::from(format!(
            "使用アカウントを切り替えました: {id}"
        ))]);
    }
    let home = std::env::var_os("HOME").ok_or("HOMEを特定できません")?;
    polaris_auth::protection::use_trusted_home(std::path::Path::new(&home))
        .map_err(|_| "認証保存先を確認できません")?;
    let auth = polaris_auth::accounts::owner::AccountOwner::open(polaris_auth::ISSUER)
        .map_err(|_| "アカウントを開けません")?;
    let owner = AccountChangeOwnerV1 {
        auth,
        effects,
        lease: &lease,
    };
    let change = match command {
        "login" => AccountChangeV1::Login {
            label: (!extra.is_empty()).then(|| extra.to_owned()),
        },
        "reauth" | "unlink" => {
            if extra.split_whitespace().count() != 1 {
                return Err(format!("/account {command} <登録ID>"));
            }
            let target = owner.target(extra)?;
            if command == "reauth" {
                AccountChangeV1::Reauth {
                    registration: target.registration_id,
                    generation: target.generation,
                }
            } else {
                AccountChangeV1::Unlink {
                    registration: target.registration_id,
                    generation: target.generation,
                }
            }
        }
        "label" => {
            let (id, label) = extra
                .split_once(char::is_whitespace)
                .ok_or("/account label <登録ID> <表示名>")?;
            let target = owner.target(id)?;
            AccountChangeV1::Label {
                registration: target.registration_id,
                generation: target.generation,
                label: label.to_owned(),
            }
        }
        "logout" => {
            if let Some((id, generation)) = binding.and_then(|b| b.current()) {
                AccountChangeV1::Unlink {
                    registration: id,
                    generation,
                }
            } else {
                let path = polaris_auth::store::default_path()
                    .map_err(|_| "既存の認証保存先を確認できません")?;
                let Some(identity) = polaris_auth::store::credential_identity(&path)
                    .map_err(|_| "既存認証を確認できません")?
                else {
                    return Ok(vec![Line::from("解除する認証はありません")]);
                };
                AccountChangeV1::LegacyLogout { identity }
            }
        }
        _ => return Err("/account list|login|reauth|unlink|label|bind".into()),
    };
    execute_change(
        &owner,
        change,
        terminal,
        reader,
        browser_callback,
        browser_cancel(),
    )
    .await
}

async fn execute_change<B: Backend, R: KeyReader, C, F>(
    owner: &AccountChangeOwnerV1<'_>,
    change: AccountChangeV1,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    callback: C,
    cancel: impl std::future::Future<Output = ()> + Send,
) -> Result<Vec<Line<'static>>, String>
where
    C: FnOnce(String, String) -> F,
    F: std::future::Future<Output = Result<String, polaris_auth::AuthError>>,
{
    let prepared = owner.prepare(change)?;
    let operation = match prepared.change() {
        AccountChangeV1::Login { .. } => "アカウントを登録",
        AccountChangeV1::Reauth { .. } => "同じアカウントを再認証",
        AccountChangeV1::Unlink { .. } | AccountChangeV1::LegacyLogout { .. } => "認証を解除",
        AccountChangeV1::Label { .. } => "アカウント表示名を変更",
    };
    let accepted = confirm(
        terminal,
        reader,
        operation,
        prepared.change().target(),
        &match prepared.change() {
            AccountChangeV1::Label { label, .. } => {
                format!("新しい表示名: {label}。会話の巻き戻しで取り消せません。")
            }
            _ => "会話の巻き戻しで取り消せません。登録だけでは使用アカウントを切り替えません。"
                .into(),
        },
    );
    let result = if matches!(
        prepared.change(),
        AccountChangeV1::Login { .. } | AccountChangeV1::Reauth { .. }
    ) {
        if accepted {
            terminal
                .borrow_mut()
                .draw(|f| {
                    render::render_management_confirmation(
                        f,
                        "ブラウザで認可してください",
                        prepared.change().target(),
                        "Escで中止。認可URL・code・tokenは会話へ保存しません。",
                    );
                })
                .map_err(|_| "端末へ表示できません")?;
        }
        owner
            .apply_browser(&prepared, accepted, callback, cancel)
            .await?
    } else {
        owner.apply_local(&prepared, accepted)?
    };
    Ok(vec![Line::from(match result {
        Some(id) => format!("{operation}を完了しました: {id}"),
        None => "認証操作を取り消しました".into(),
    })])
}

pub(crate) async fn browser_callback(
    url: String,
    state: String,
) -> Result<String, polaris_auth::AuthError> {
    let listener = polaris_auth::login::bind_callback().await?;
    let command = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "/usr/bin/xdg-open"
    };
    let mut child = tokio::process::Command::new(command)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(polaris_auth::AuthError::Io)?;
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        _ => {
            return Err(polaris_auth::AuthError::Denied(
                "browser_launcher_failed".into(),
            ));
        }
    }
    polaris_auth::login::wait_for_callback(listener, &state, Duration::from_secs(300)).await
}

fn cancel_key(key: ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    key.kind != KeyEventKind::Release
        && (key.code == KeyCode::Esc
            || (matches!(key.code, KeyCode::Char('c' | 'C'))
                && key.modifiers.contains(KeyModifiers::CONTROL)))
}

async fn browser_cancel_with(
    mut read: impl FnMut() -> std::io::Result<Option<ratatui::crossterm::event::KeyEvent>>,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        #[cfg(target_os = "linux")]
        if crate::signal::pending_linux_process_shutdown().is_some() {
            return;
        }
        match read() {
            Ok(Some(key)) if cancel_key(key) => return,
            Err(_) => return,
            _ => {}
        }
    }
}

pub(crate) async fn browser_cancel() {
    use ratatui::crossterm::event::{self, Event};
    browser_cancel_with(|| {
        if !event::poll(Duration::ZERO)? {
            return Ok(None);
        }
        Ok(match event::read()? {
            Event::Key(key) => Some(key),
            _ => None,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
    use std::collections::VecDeque;

    #[tokio::test]
    async fn browser_wait_cancels_on_escape_and_control_c_key_events() {
        use ratatui::crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut keys = VecDeque::from([
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
                KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release),
                key,
            ]);
            tokio::time::timeout(
                Duration::from_secs(1),
                browser_cancel_with(|| Ok(keys.pop_front())),
            )
            .await
            .expect("the real cancellation future must observe the cancel key");
            assert!(keys.is_empty(), "ordinary and release keys must not cancel");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn browser_cancel_keys_release_registration_during_wait_and_exchange() {
        const CHILD: &str = "POLARIS_CANCEL_KEYS_CHILD";
        const TEST: &str =
            "account::tests::browser_cancel_keys_release_registration_during_wait_and_exchange";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--nocapture"])
                .env(CHILD, "1")
                .env("HOME", home.path().canonicalize().unwrap())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use base64::Engine;
        use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let owner =
                polaris_auth::accounts::owner::AccountOwner::open("http://127.0.0.1:1").unwrap();
            let result = owner
                .register(
                    None,
                    |_, _| std::future::pending::<Result<String, polaris_auth::AuthError>>(),
                    browser_cancel_with(|| Ok(Some(key))),
                )
                .await;
            assert!(
                matches!(result, Err(polaris_auth::AuthError::Denied(reason)) if reason == "cancelled")
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let owner = polaris_auth::accounts::owner::AccountOwner::open(&format!(
                "http://{}",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let posted = Arc::new(AtomicBool::new(false));
            let observed = posted.clone();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    input.extend_from_slice(&buffer[..n]);
                    if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&input[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if input.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                observed.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic"}})).unwrap());
                let body = serde_json::json!({"access_token":"synthetic", "refresh_token":"synthetic", "expires_in":3600,
                    "id_token":format!("header.{payload}.signature")}).to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            });
            let result = owner
                .register(
                    None,
                    |_, _| async { Ok("synthetic-code".into()) },
                    browser_cancel_with(|| Ok(posted.load(Ordering::SeqCst).then_some(key))),
                )
                .await;
            assert!(
                matches!(result, Err(polaris_auth::AuthError::Denied(reason)) if reason == "cancelled")
            );
            server.await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                owner
                    .snapshot()
                    .unwrap()
                    .accounts
                    .iter()
                    .all(|a| a.state == polaris_auth::accounts::store::AccountState::Disconnected)
            );
        }
    }

    #[tokio::test]
    async fn m3b_tui_binding_confirmation_keeps_the_current_account_on_denial() {
        struct HostBinding(std::sync::Mutex<String>);
        impl BindingControl for HostBinding {
            fn current(&self) -> Option<(String, u64)> {
                Some((self.0.lock().unwrap().clone(), 1))
            }
            fn bind(&self, id: &str) -> Result<String, String> {
                if id != "explicit-target" {
                    return Err("unknown registration".into());
                }
                *self.0.lock().unwrap() = id.into();
                Ok(id.into())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let binding: Arc<dyn BindingControl> =
            Arc::new(HostBinding(std::sync::Mutex::new("original".into())));
        let terminal =
            RefCell::new(Terminal::new(ratatui::backend::TestBackend::new(140, 25)).unwrap());
        let mut no = Keys(VecDeque::from([KeyCode::Esc]));
        handle(
            "bind explicit-target",
            &terminal,
            &mut no,
            &workspace,
            &effects,
            Some(&binding),
            false,
        )
        .await
        .unwrap();
        assert_eq!(binding.current().unwrap().0, "original");
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        handle(
            "bind explicit-target",
            &terminal,
            &mut yes,
            &workspace,
            &effects,
            Some(&binding),
            false,
        )
        .await
        .unwrap();
        assert_eq!(binding.current().unwrap().0, "explicit-target");
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        assert!(
            handle(
                "bind missing",
                &terminal,
                &mut yes,
                &workspace,
                &effects,
                Some(&binding),
                false
            )
            .await
            .is_err()
        );
        assert_eq!(binding.current().unwrap().0, "explicit-target");
        assert!(
            matches!(crate::slash::parse("/login label"),Some(crate::slash::Action::Account(request)) if request=="login label")
        );
    }

    struct Keys(VecDeque<KeyCode>);
    impl KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            self.0
                .pop_front()
                .ok_or_else(|| std::io::Error::other("no key"))
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn m3a_tui_label_unlink_and_exact_logout() {
        if std::env::var_os("POLARIS_M3A_TUI_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "account::tests::m3a_tui_label_unlink_and_exact_logout",
                    "--exact",
                    "--nocapture",
                ])
                .env("POLARIS_M3A_TUI_CHILD", "1")
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let state = polaris_core::project::state_dir(&project).unwrap();
        crate::source_apply::prepare_state(&project, &state).unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let store = polaris_auth::accounts::store::AccountStore::open().unwrap();
        let reservation = store.begin_registration(None).unwrap();
        store.mark_exchange_pending(&reservation).unwrap();
        let credentials = polaris_auth::Credentials {
            access_token: "synthetic-access-secret".into(),
            refresh_token: "synthetic-refresh-secret".into(),
            account_id: "fixture-principal".into(),
            expires_at: Some(u64::MAX),
        };
        store
            .complete_registration(&reservation, credentials.clone())
            .unwrap();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(140, 25)).unwrap());
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        let request = format!("label {} named-account", reservation.registration_id);
        let output = handle(
            &request, &terminal, &mut yes, &workspace, &effects, None, false,
        )
        .await
        .unwrap();
        assert!(output.iter().any(|line| line.to_string().contains("完了")));
        assert_eq!(
            store.snapshot().unwrap().accounts[0].label.as_deref(),
            Some("named-account")
        );
        let list = handle(
            "list",
            &terminal,
            &mut Keys(VecDeque::new()),
            &workspace,
            &effects,
            None,
            false,
        )
        .await
        .unwrap();
        let list = list
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(list.contains(&reservation.registration_id));
        assert!(!list.contains("synthetic-access-secret"));
        assert!(!list.contains("synthetic-refresh-secret"));
        let request = format!("unlink {}", reservation.registration_id);
        let mut cancel = Keys(VecDeque::from([KeyCode::Esc]));
        handle(
            &request,
            &terminal,
            &mut cancel,
            &workspace,
            &effects,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            store.snapshot().unwrap().accounts[0].state,
            polaris_auth::accounts::store::AccountState::Ready
        );
        assert!(
            handle(
                &request,
                &terminal,
                &mut Keys(VecDeque::new()),
                &workspace,
                &effects,
                None,
                true
            )
            .await
            .is_err()
        );
        handle(
            &request,
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Char('y')])),
            &workspace,
            &effects,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            store.snapshot().unwrap().accounts[0].state,
            polaris_auth::accounts::store::AccountState::Disconnected
        );
        let path = polaris_auth::store::default_path().unwrap();
        polaris_auth::store::save_to(&path, &credentials).unwrap();
        handle(
            "logout",
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Esc])),
            &workspace,
            &effects,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(path.exists());
        handle(
            "logout",
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Char('y')])),
            &workspace,
            &effects,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(!path.exists());
        assert!(effects.blocker().unwrap().is_none());
    }

    #[test]
    fn m3a_narrow_confirmation_does_not_accept_hidden_target() {
        let terminal = RefCell::new(Terminal::new(TestBackend::new(20, 5)).unwrap());
        assert!(!confirm(
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Char('y')])),
            "解除",
            "exact-target",
            "irreversible"
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn m3a_browser_flow_registers_and_reauthenticates_without_implicit_bind() {
        if std::env::var_os("POLARIS_M3A_BROWSER_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir(home.path().join("tmp")).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["account::tests::m3a_browser_flow_registers_and_reauthenticates_without_implicit_bind", "--exact", "--nocapture"])
                .env("POLARIS_M3A_BROWSER_CHILD", "1").env("HOME", home.path()).env("TMPDIR", home.path().join("tmp"))
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use base64::Engine;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let state = polaris_core::project::state_dir(&project).unwrap();
        crate::source_apply::prepare_state(&project, &state).unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let auth = polaris_auth::accounts::owner::AccountOwner::open(&issuer).unwrap();
        let owner = AccountChangeOwnerV1 {
            auth: auth.clone(),
            effects: &effects,
            lease: &lease,
        };
        let terminal = RefCell::new(Terminal::new(TestBackend::new(140, 25)).unwrap());
        execute_change(
            &owner,
            AccountChangeV1::Login { label: None },
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Esc])),
            |_, _| async { panic!("cancelled confirmation must not open a browser") },
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(auth.snapshot().unwrap().accounts.is_empty());
        let server = tokio::spawn(async move {
            for n in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 2048];
                    let count = socket.read(&mut bytes).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&bytes[..count]);
                    assert!(request.len() <= 8192);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                assert!(String::from_utf8_lossy(&request).contains("code=code-fixture"));
                let claims = serde_json::json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "fixture-principal" } });
                let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&claims).unwrap());
                let body = serde_json::json!({ "id_token": format!("header.{encoded}.signature"), "access_token": format!("synthetic-browser-access-{n}"), "refresh_token": "synthetic-browser-refresh", "expires_in": 3600 }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let output = execute_change(
            &owner,
            AccountChangeV1::Login {
                label: Some("fixture label".into()),
            },
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Char('y')])),
            |_, _| async { Ok("code-fixture".into()) },
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(output.iter().any(|line| line.to_string().contains("完了")));
        assert!(
            !output
                .iter()
                .any(|line| line.to_string().contains("synthetic-browser"))
        );
        let before = auth.snapshot().unwrap().accounts[0].clone();
        execute_change(
            &owner,
            AccountChangeV1::Reauth {
                registration: before.registration_id.clone(),
                generation: before.generation,
            },
            &terminal,
            &mut Keys(VecDeque::from([KeyCode::Char('y')])),
            |_, _| async { Ok("code-fixture".into()) },
            std::future::pending(),
        )
        .await
        .unwrap();
        let after = auth.snapshot().unwrap().accounts[0].clone();
        assert_eq!(before.registration_id, after.registration_id);
        assert!(after.generation > before.generation);
        assert_eq!(auth.snapshot().unwrap().accounts.len(), 1);
        assert!(effects.blocker().unwrap().is_none());
        server.await.unwrap();
    }
}
