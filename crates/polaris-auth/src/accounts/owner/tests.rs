//! Synthetic tokens, a loopback issuer and temporary homes only.
use super::*;
use crate::token::id_token_with_account;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = home.path().canonicalize().unwrap();
    (home, path)
}

fn credentials(account: &str, expires_at: u64) -> Credentials {
    Credentials {
        access_token: format!("synthetic-access-{account}"),
        refresh_token: format!("synthetic-refresh-{account}"),
        account_id: account.into(),
        expires_at: Some(expires_at),
    }
}

/// A saved registration whose access token has already expired.
fn expired(home: &std::path::Path, account: &str) -> (String, u64) {
    let store = AccountStore::open_at(home).unwrap();
    let reservation = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&reservation).unwrap();
    let (generation, _) = store
        .complete_registration(&reservation, credentials(account, 1))
        .unwrap();
    (reservation.registration_id, generation)
}

fn owner(home: &std::path::Path, server: &MockServer) -> Arc<AccountOwner> {
    AccountOwner::with_store(AccountStore::open_at(home).unwrap(), &server.uri()).unwrap()
}

fn refreshed(account: &str, access: &str) -> serde_json::Value {
    serde_json::json!({
        "access_token": access,
        "refresh_token": format!("synthetic-rotated-{account}"),
        "expires_in": 3600,
    })
}

async fn token_endpoint(server: &MockServer, marker: &str, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(marker))
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

fn state(home: &std::path::Path, id: &str) -> crate::accounts::store::AccountSummary {
    AccountStore::open_at(home)
        .unwrap()
        .snapshot()
        .unwrap()
        .accounts
        .into_iter()
        .find(|a| a.registration_id == id)
        .unwrap()
}

#[tokio::test]
async fn concurrent_waiters_share_one_refresh_and_one_new_revision() {
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200)
            .set_body_json(refreshed("acct-a", "fresh-a"))
            .set_delay(Duration::from_millis(300)),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    let waiters: Vec<_> = (0..5)
        .map(|_| {
            let (owner, id) = (Arc::clone(&owner), id.clone());
            tokio::spawn(async move { owner.token(&id, generation).await })
        })
        .collect();
    for waiter in waiters {
        let token = waiter.await.unwrap().unwrap();
        assert_eq!((token.secret(), token.token_revision), ("fresh-a", 3));
    }
    server.verify().await;
}

#[tokio::test]
async fn one_accounts_refresh_never_blocks_or_overwrites_another() {
    let (_temp, home) = home();
    let (a, a_generation) = expired(&home, "acct-a");
    let (b, b_generation) = expired(&home, "acct-b");
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200)
            .set_body_json(refreshed("acct-a", "fresh-a"))
            .set_delay(Duration::from_millis(600)),
        1,
    )
    .await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-b",
        ResponseTemplate::new(200).set_body_json(refreshed("acct-b", "fresh-b")),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    let slow = {
        let (owner, a) = (Arc::clone(&owner), a.clone());
        tokio::spawn(async move { owner.token(&a, a_generation).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    assert_eq!(
        owner.token(&b, b_generation).await.unwrap().token_revision,
        3
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(slow.await.unwrap().unwrap().token_revision, 3);
    let b_again = owner.token(&b, b_generation).await.unwrap();
    assert_eq!((b_again.secret(), b_again.token_revision), ("fresh-b", 3));
    server.verify().await;
}

#[tokio::test]
async fn a_late_unauthorized_uses_the_newer_token_and_a_future_revision_is_refused() {
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200).set_body_json(refreshed("acct-a", "fresh-a")),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    assert_eq!(
        owner.token(&id, generation).await.unwrap().token_revision,
        3
    );
    let late = owner
        .refresh_after_unauthorized(&id, generation, 2)
        .await
        .unwrap();
    assert_eq!((late.secret(), late.token_revision), ("fresh-a", 3));
    assert!(matches!(
        owner.refresh_after_unauthorized(&id, generation, 9).await,
        Err(AuthError::Denied(_))
    ));
    server.verify().await;
}

#[tokio::test]
async fn logout_during_a_refresh_is_immediate_and_the_late_token_is_discarded() {
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200)
            .set_body_json(refreshed("acct-a", "fresh-a"))
            .set_delay(Duration::from_millis(800)),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    let pending = {
        let (owner, id) = (Arc::clone(&owner), id.clone());
        tokio::spawn(async move { owner.token(&id, generation).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    AccountStore::open_at(&home)
        .unwrap()
        .logout(&id, generation)
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(300));
    assert!(matches!(
        pending.await.unwrap(),
        Err(AuthError::NotLoggedIn)
    ));
    assert_eq!(state(&home, &id).state, AccountState::Disconnected);
    server.verify().await;
}

#[tokio::test]
async fn refresh_identity_may_be_omitted_but_never_changed_or_malformed() {
    let (_temp, home) = home();
    let server = MockServer::start().await;
    let (same, same_generation) = expired(&home, "acct-same");
    let mut body = refreshed("acct-same", "fresh-same");
    body["id_token"] = serde_json::json!(id_token_with_account("acct-same"));
    token_endpoint(
        &server,
        "synthetic-refresh-acct-same",
        ResponseTemplate::new(200).set_body_json(body),
        1,
    )
    .await;
    let (other, other_generation) = expired(&home, "acct-other");
    let mut body = refreshed("acct-other", "fresh-other");
    body["id_token"] = serde_json::json!(id_token_with_account("acct-elsewhere"));
    token_endpoint(
        &server,
        "synthetic-refresh-acct-other",
        ResponseTemplate::new(200).set_body_json(body),
        1,
    )
    .await;
    let (bad, bad_generation) = expired(&home, "acct-bad");
    let mut body = refreshed("acct-bad", "fresh-bad");
    body["id_token"] = serde_json::Value::Null;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-bad",
        ResponseTemplate::new(200).set_body_json(body),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    assert_eq!(
        owner
            .token(&same, same_generation)
            .await
            .unwrap()
            .account_id,
        "acct-same"
    );
    assert!(matches!(
        owner.token(&other, other_generation).await,
        Err(AuthError::Denied(_))
    ));
    assert!(matches!(
        owner.token(&bad, bad_generation).await,
        Err(AuthError::Decode(_))
    ));
    for id in [&other, &bad] {
        assert_eq!(state(&home, id).state, AccountState::ReauthRequired);
    }
    server.verify().await;
}

#[tokio::test]
async fn http_failures_are_classified_once_without_retry_or_secret_text() {
    let (_temp, home) = home();
    let server = MockServer::start().await;
    let cases = [
        (
            "acct-500",
            ResponseTemplate::new(500).set_body_string("synthetic-refresh-acct-500"),
        ),
        (
            "acct-redirect",
            ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:1/oauth/token"),
        ),
        (
            "acct-json",
            ResponseTemplate::new(200).set_body_string("{not json"),
        ),
        (
            "acct-large",
            ResponseTemplate::new(200).set_body_string("x".repeat(1024 * 1024 + 1)),
        ),
        (
            "acct-slow",
            ResponseTemplate::new(200)
                .set_body_json(refreshed("acct-slow", "late"))
                .set_delay(Duration::from_secs(2)),
        ),
    ];
    let mut registrations = Vec::new();
    for (account, response) in cases {
        registrations.push((account, expired(&home, account)));
        token_endpoint(
            &server,
            &format!("synthetic-refresh-{account}"),
            response,
            1,
        )
        .await;
    }
    let owner = owner(&home, &server);
    for (account, (id, generation)) in registrations {
        let error = owner.token(&id, generation).await.unwrap_err();
        let text = format!("{error} {error:?}");
        assert!(!text.contains("synthetic"), "{account}: {text}");
        assert_eq!(
            state(&home, &id).state,
            AccountState::ReauthRequired,
            "{account}"
        );
        // A second request neither resends nor revives the old token.
        assert!(owner.token(&id, generation).await.is_err());
    }
    server.verify().await;
}

#[tokio::test]
async fn a_pending_refresh_without_a_live_owner_is_never_resent() {
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    let store = AccountStore::open_at(&home).unwrap();
    // The previous owner saved the pending attempt and then vanished.
    store.begin_refresh(&id, generation, 2).unwrap();
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200),
        0,
    )
    .await;
    let owner = owner(&home, &server);
    assert_eq!(state(&home, &id).state, AccountState::ReauthRequired);
    assert!(matches!(
        owner.token(&id, generation).await,
        Err(AuthError::Denied(_))
    ));
    server.verify().await;
}

#[tokio::test]
async fn a_dropped_waiter_leaves_the_owned_refresh_to_finish_and_save() {
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200)
            .set_body_json(refreshed("acct-a", "fresh-a"))
            .set_delay(Duration::from_millis(500)),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), owner.token(&id, generation))
            .await
            .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while state(&home, &id).state != AccountState::Ready && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let token = owner.token(&id, generation).await.unwrap();
    assert_eq!((token.secret(), token.token_revision), ("fresh-a", 3));
    server.verify().await;
}

#[tokio::test]
async fn logins_complete_or_cancel_before_and_after_the_exchange() {
    let (_temp, home) = home();
    let server = MockServer::start().await;
    let mut body = refreshed("acct-new", "first-access");
    body["id_token"] = serde_json::json!(id_token_with_account("acct-new"));
    token_endpoint(
        &server,
        "code=code-ok",
        ResponseTemplate::new(200).set_body_json(body.clone()),
        1,
    )
    .await;
    token_endpoint(
        &server,
        "code=code-late",
        ResponseTemplate::new(200)
            .set_body_json(body)
            .set_delay(Duration::from_millis(600)),
        1,
    )
    .await;
    let owner = owner(&home, &server);

    let token = owner
        .register(
            Some("仕事".into()),
            |url, _state| async move {
                assert!(url.contains("code_challenge="));
                Ok("code-ok".to_owned())
            },
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_eq!(
        (token.account_id.as_str(), token.generation),
        ("acct-new", 1)
    );
    assert_eq!(
        state(&home, &token.registration_id).state,
        AccountState::Ready
    );

    // Cancelled while waiting for the browser: nothing is sent.
    let before = owner.store.snapshot().unwrap();
    let cancelled = owner
        .register(None, |_, _| std::future::pending(), std::future::ready(()))
        .await;
    assert!(matches!(cancelled, Err(AuthError::Denied(_))));
    let after = owner.store.snapshot().unwrap();
    let new = after
        .accounts
        .iter()
        .find(|a| {
            !before
                .accounts
                .iter()
                .any(|b| b.registration_id == a.registration_id)
        })
        .unwrap();
    assert_eq!((new.state, new.generation), (AccountState::Disconnected, 2));

    // Cancelled after the POST started: the late result is refused.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let late = {
        let owner = Arc::clone(&owner);
        tokio::spawn(async move {
            owner
                .register(None, |_, _| async { Ok("code-late".to_owned()) }, async {
                    let _ = rx.await;
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(()).unwrap();
    assert!(matches!(late.await.unwrap(), Err(AuthError::Denied(_))));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        owner
            .store
            .snapshot()
            .unwrap()
            .accounts
            .iter()
            .all(|a| a.state != AccountState::Registering)
    );
    assert_eq!(
        owner
            .store
            .snapshot()
            .unwrap()
            .accounts
            .iter()
            .filter(|a| a.state == AccountState::Ready)
            .count(),
        1
    );
    server.verify().await;
}

#[tokio::test]
async fn an_interrupted_login_is_settled_without_resending_the_code() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    let waiting = store.begin_registration(None).unwrap();
    let exchanging = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&exchanging).unwrap();
    drop(store);
    let server = MockServer::start().await;
    token_endpoint(&server, "authorization_code", ResponseTemplate::new(200), 0).await;
    let _owner = owner(&home, &server);
    for reservation in [&waiting, &exchanging] {
        let settled = state(&home, &reservation.registration_id);
        assert_eq!(
            (settled.state, settled.generation),
            (AccountState::Disconnected, 2)
        );
    }
    server.verify().await;
}

#[test]
fn read_only_tokens_never_refresh_or_hide_unresolved_state() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    let fresh = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&fresh).unwrap();
    store
        .complete_registration(&fresh, credentials("acct-fresh", u64::MAX / 2))
        .unwrap();
    let (stale, stale_generation) = expired(&home, "acct-stale");
    let pending = store.begin_registration(None).unwrap();
    assert_eq!(
        read_only_token_at(&home, &fresh.registration_id, 1)
            .unwrap()
            .secret(),
        "synthetic-access-acct-fresh"
    );
    assert!(matches!(
        read_only_token_at(&home, &stale, stale_generation),
        Err(AuthError::ReadOnly)
    ));
    assert!(matches!(
        read_only_token_at(&home, &pending.registration_id, 1),
        Err(AuthError::RecoveryRequired)
    ));
}

#[tokio::test]
async fn a_busy_owner_lock_at_save_is_waited_out_without_a_second_post() {
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    let server = MockServer::start().await;
    token_endpoint(
        &server,
        "synthetic-refresh-acct-a",
        ResponseTemplate::new(200)
            .set_body_json(refreshed("acct-a", "fresh-a"))
            .set_delay(Duration::from_millis(300)),
        1,
    )
    .await;
    let owner = owner(&home, &server);
    let pending = {
        let (owner, id) = (Arc::clone(&owner), id.clone());
        tokio::spawn(async move { owner.token(&id, generation).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let directory = crate::accounts::directory::AccountDirectory::open_for_test(&home).unwrap();
    let held = directory.registry_writer().unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!pending.is_finished());
    drop(held);
    let token = pending.await.unwrap().unwrap();
    assert_eq!(token.token_revision, 3);
    server.verify().await;
}

#[test]
fn another_process_waits_for_the_live_refresh_instead_of_posting() {
    if let Some(home) = std::env::var_os("POLARIS_OWNER_CHILD_HOME") {
        let issuer = std::env::var("POLARIS_OWNER_CHILD_ISSUER").unwrap();
        let id = std::env::var("POLARIS_OWNER_CHILD_ID").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = AccountStore::open_at(std::path::Path::new(&home)).unwrap();
        let owner = AccountOwner::with_store(store, &issuer).unwrap();
        let token = runtime.block_on(owner.token(&id, 1)).unwrap();
        assert_eq!((token.secret(), token.token_revision), ("fresh-a", 3));
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (_temp, home) = home();
    let (id, generation) = expired(&home, "acct-a");
    runtime.block_on(async {
        let server = MockServer::start().await;
        token_endpoint(
            &server,
            "synthetic-refresh-acct-a",
            ResponseTemplate::new(200)
                .set_body_json(refreshed("acct-a", "fresh-a"))
                .set_delay(Duration::from_millis(700)),
            1,
        )
        .await;
        let owner = owner(&home, &server);
        let pending = {
            let (owner, id) = (Arc::clone(&owner), id.clone());
            tokio::spawn(async move { owner.token(&id, generation).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "accounts::owner::tests::another_process_waits_for_the_live_refresh_instead_of_posting",
            ])
            .env("POLARIS_OWNER_CHILD_HOME", &home)
            .env("POLARIS_OWNER_CHILD_ISSUER", server.uri())
            .env("POLARIS_OWNER_CHILD_ID", &id)
            .status();
        let (parent, child) = tokio::join!(pending, child);
        assert_eq!(parent.unwrap().unwrap().token_revision, 3);
        assert!(child.unwrap().success());
        server.verify().await;
    });
}

#[tokio::test]
async fn a_cancel_that_loses_the_race_still_takes_back_the_saved_login() {
    let (_temp, home) = home();
    let server = MockServer::start().await;
    let mut body = refreshed("acct-new", "first-access");
    body["id_token"] = serde_json::json!(id_token_with_account("acct-new"));
    token_endpoint(
        &server,
        "code=code-registered",
        ResponseTemplate::new(200).set_body_json(body),
        2,
    )
    .await;
    let owner = owner(&home, &server);
    // Resolves only once the exchange has already saved its result, which the
    // gate below holds inside its task until the cancel has been recorded.
    let after_save = |generation: u64| {
        let home = home.clone();
        async move {
            loop {
                let saved = AccountStore::open_at(&home)
                    .unwrap()
                    .snapshot()
                    .unwrap()
                    .accounts
                    .iter()
                    .any(|a| a.state == AccountState::Ready && a.generation == generation);
                if saved {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let hold = |owner: &Arc<AccountOwner>, gate: &Arc<tokio::sync::Semaphore>| {
        *owner.completion_gate.lock().unwrap() = Some(Arc::clone(gate));
        let gate = Arc::clone(gate);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            gate.add_permits(1);
        })
    };
    let release = hold(&owner, &gate);
    let cancelled = owner
        .register(
            None,
            |_, _| async { Ok("code-registered".to_owned()) },
            after_save(1),
        )
        .await;
    release.await.unwrap();
    *owner.completion_gate.lock().unwrap() = None;
    assert!(matches!(cancelled, Err(AuthError::Denied(_))));
    let taken_back = owner.store.snapshot().unwrap().accounts[0].clone();
    assert_eq!(
        (taken_back.state, taken_back.generation),
        (AccountState::Disconnected, 2)
    );

    // The same race on a re-authentication leaves the account needing a login.
    let token = owner
        .register(
            None,
            |_, _| async { Ok("code-registered".to_owned()) },
            std::future::pending(),
        )
        .await
        .unwrap();
    let mut body = refreshed("acct-new", "second-access");
    body["id_token"] = serde_json::json!(id_token_with_account("acct-new"));
    token_endpoint(
        &server,
        "code=code-reauth",
        ResponseTemplate::new(200).set_body_json(body),
        1,
    )
    .await;
    let reauth_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let release = hold(&owner, &reauth_gate);
    let cancelled = owner
        .reauthenticate(
            &token.registration_id,
            token.generation,
            |_, _| async { Ok("code-reauth".to_owned()) },
            after_save(token.generation + 1),
        )
        .await;
    release.await.unwrap();
    *owner.completion_gate.lock().unwrap() = None;
    assert!(matches!(cancelled, Err(AuthError::Denied(_))));
    let summary = state(&home, &token.registration_id);
    assert_eq!(
        (summary.state, summary.generation),
        (AccountState::ReauthRequired, token.generation + 2)
    );
    server.verify().await;
}
