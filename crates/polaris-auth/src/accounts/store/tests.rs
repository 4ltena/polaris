//! Synthetic credentials and temporary homes only.
use super::*;
use crate::accounts::directory::REGISTRY_FAULT;
use std::os::unix::fs::PermissionsExt;

fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = home.path().canonicalize().unwrap();
    (home, path)
}

fn credentials(account: &str) -> Credentials {
    Credentials {
        access_token: format!("synthetic-access-{account}"),
        refresh_token: format!("synthetic-refresh-{account}"),
        account_id: account.into(),
        expires_at: Some(1),
    }
}

fn register(store: &AccountStore, label: &str, account: &str) -> Reservation {
    let reservation = store.begin_registration(Some(label.into())).unwrap();
    store.mark_exchange_pending(&reservation).unwrap();
    store
        .complete_registration(&reservation, credentials(account))
        .unwrap();
    reservation
}

fn with_fault<T>(point: &'static str, run: impl FnOnce() -> T) -> T {
    REGISTRY_FAULT.with(|f| f.set(Some(point)));
    let value = run();
    REGISTRY_FAULT.with(|f| f.set(None));
    value
}

fn account(snapshot: &AccountSnapshot, id: &str) -> AccountSummary {
    snapshot
        .accounts
        .iter()
        .find(|a| a.registration_id == id)
        .unwrap()
        .clone()
}

#[test]
fn labels_logout_and_slot_reuse_never_move_a_credential_identity() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    let a = register(&store, "A", "acct-a");
    let b = register(&store, "B", "acct-b");
    let before = store.snapshot().unwrap();
    assert_eq!(
        account(&before, &a.registration_id).state,
        AccountState::Ready
    );
    store
        .set_label(&a.registration_id, a.generation + 1, Some("B".into()))
        .unwrap_err();
    // A completed new registration keeps generation 1; the label swap binds to it.
    store
        .set_label(&a.registration_id, 1, Some("B".into()))
        .unwrap();
    store
        .set_label(&b.registration_id, 1, Some("A".into()))
        .unwrap();
    let swapped = store.snapshot().unwrap();
    assert_eq!(
        account(&swapped, &a.registration_id).label.as_deref(),
        Some("B")
    );
    assert_eq!(account(&swapped, &b.registration_id).generation, 1);

    store.logout(&a.registration_id, 1).unwrap();
    let after = store.snapshot().unwrap();
    let a_after = account(&after, &a.registration_id);
    assert_eq!(
        (a_after.state, a_after.generation),
        (AccountState::Disconnected, 2)
    );
    assert!(matches!(
        store.logout(&a.registration_id, 1),
        Err(AuthError::NotLoggedIn)
    ));

    let reused = store.begin_registration(None).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.accounts.len(), 2);
    assert!(
        snapshot
            .accounts
            .iter()
            .all(|s| s.registration_id != a.registration_id)
    );
    assert_eq!(account(&snapshot, &reused.registration_id).generation, 3);
    assert_eq!(
        account(&snapshot, &b.registration_id).state,
        AccountState::Ready
    );
}

#[test]
fn a_cancelled_or_logged_out_attempt_cannot_save_a_late_result() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    let reservation = store.begin_registration(None).unwrap();
    store
        .logout(&reservation.registration_id, reservation.generation)
        .unwrap();
    assert!(store.mark_exchange_pending(&reservation).is_err());
    assert!(
        store
            .complete_registration(&reservation, credentials("late"))
            .is_err()
    );

    let second = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&second).unwrap();
    store.cancel_registration(&second, true).unwrap();
    assert!(
        store
            .complete_registration(&second, credentials("late"))
            .is_err()
    );
    assert_eq!(
        account(&store.snapshot().unwrap(), &second.registration_id).state,
        AccountState::Disconnected
    );
}

#[test]
fn full_registry_duplicate_or_empty_accounts_change_nothing() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    for n in 0..MAX_SLOTS {
        register(&store, &format!("{n}"), &format!("acct-{n}"));
    }
    let full = store.snapshot().unwrap();
    assert!(matches!(
        store.begin_registration(None),
        Err(AuthError::AccountLimit)
    ));
    assert_eq!(store.snapshot().unwrap(), full);

    store.logout(&full.accounts[0].registration_id, 1).unwrap();
    let reservation = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&reservation).unwrap();
    let before = store.snapshot().unwrap();
    assert!(matches!(
        store.complete_registration(&reservation, credentials("acct-1")),
        Err(AuthError::AlreadyRegistered)
    ));
    assert!(matches!(
        store.complete_registration(&reservation, credentials("")),
        Err(AuthError::Denied(_))
    ));
    assert_eq!(store.snapshot().unwrap(), before);
}

#[test]
fn reauthentication_restores_before_post_and_invalidates_after_it() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    let a = register(&store, "A", "acct-a");
    let first = store.begin_reauthentication(&a.registration_id, 1).unwrap();
    store.cancel_registration(&first, false).unwrap();
    let restored = account(&store.snapshot().unwrap(), &a.registration_id);
    assert_eq!(
        (restored.state, restored.generation),
        (AccountState::Ready, 1)
    );

    let other = store.begin_reauthentication(&a.registration_id, 1).unwrap();
    store.mark_exchange_pending(&other).unwrap();
    assert!(matches!(
        store.complete_registration(&other, credentials("acct-z")),
        Err(AuthError::Denied(_))
    ));
    store
        .complete_registration(&other, credentials("acct-a"))
        .unwrap();
    assert_eq!(
        account(&store.snapshot().unwrap(), &a.registration_id).generation,
        2
    );

    let last = store.begin_reauthentication(&a.registration_id, 2).unwrap();
    store.mark_exchange_pending(&last).unwrap();
    store.cancel_registration(&last, true).unwrap();
    let gone = account(&store.snapshot().unwrap(), &a.registration_id);
    assert_eq!(
        (gone.state, gone.generation),
        (AccountState::ReauthRequired, 3)
    );

    // Without credentials the registration still belongs to acct-a.
    let taken = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&taken).unwrap();
    assert!(matches!(
        store.complete_registration(&taken, credentials("acct-a")),
        Err(AuthError::AlreadyRegistered)
    ));
    let again = store.begin_reauthentication(&a.registration_id, 3).unwrap();
    store.mark_exchange_pending(&again).unwrap();
    assert!(matches!(
        store.complete_registration(&again, credentials("acct-other")),
        Err(AuthError::Denied(_))
    ));
    store
        .complete_registration(&again, credentials("acct-a"))
        .unwrap();
    assert!(matches!(
        store.begin_registration(Some("\u{7}".into())),
        Err(AuthError::Denied(_))
    ));
}

#[test]
fn a_complete_tmp_is_published_and_any_other_tmp_waits_for_recovery() {
    let (_temp, home) = home();
    // Genesis interrupted after the initial tmp sync.
    let store = AccountStore::open_at(&home).unwrap();
    assert!(with_fault("registry_tmp_synced", || store.snapshot()).is_err());
    let registry = home.join(".polaris/accounts/registry.json");
    assert!(!registry.exists());
    drop(store);
    let store = AccountStore::open_at(&home).unwrap();
    assert_eq!(store.snapshot().unwrap().registry_revision, 1);

    // A synced registration tmp completes without repeating the operation.
    assert!(with_fault("registry_tmp_synced", || store.begin_registration(None)).is_err());
    let recovered = store.snapshot().unwrap();
    assert_eq!(
        (recovered.registry_revision, recovered.accounts.len()),
        (2, 1)
    );

    // A partial tmp is kept, and nothing else is written until it is resolved.
    let canonical = std::fs::read(&registry).unwrap();
    assert!(with_fault("registry_tmp_partial", || store.begin_registration(None)).is_err());
    let tmp = home.join(".polaris/accounts/registry.json.tmp");
    let partial = std::fs::read(&tmp).unwrap();
    assert!(matches!(store.snapshot(), Err(AuthError::RecoveryRequired)));
    assert!(matches!(
        store.begin_registration(None),
        Err(AuthError::RecoveryRequired)
    ));
    assert_eq!(std::fs::read(&tmp).unwrap(), partial);
    assert_eq!(std::fs::read(&registry).unwrap(), canonical);
}

#[test]
fn a_complete_tmp_for_another_revision_or_attempt_is_not_published() {
    let (_temp, home) = home();
    let store = AccountStore::open_at(&home).unwrap();
    let reservation = store.begin_registration(None).unwrap();
    // Capture a valid next-step tmp, then move canonical past it.
    assert!(
        with_fault("registry_tmp_synced", || store
            .mark_exchange_pending(&reservation))
        .is_err()
    );
    let accounts = home.join(".polaris/accounts");
    let stale = std::fs::read(accounts.join("registry.json.tmp")).unwrap();
    store.snapshot().unwrap();
    store
        .logout(&reservation.registration_id, reservation.generation)
        .unwrap();
    let canonical = std::fs::read(accounts.join("registry.json")).unwrap();

    // Journaled by a real prepare, then overwritten with the stale envelope.
    assert!(
        with_fault("registry_tmp_created", || store.set_label(
            &reservation.registration_id,
            2,
            None
        ))
        .is_err()
    );
    std::fs::write(accounts.join("registry.json.tmp"), &stale).unwrap();
    assert!(matches!(store.snapshot(), Err(AuthError::RecoveryRequired)));
    assert_eq!(
        std::fs::read(accounts.join("registry.json")).unwrap(),
        canonical
    );

    let mut foreign: serde_json::Value = serde_json::from_slice(&stale).unwrap();
    foreign["operation"]["attempt_id"] = serde_json::json!("x".repeat(22));
    std::fs::write(
        accounts.join("registry.json.tmp"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();
    assert!(matches!(store.snapshot(), Err(AuthError::RecoveryRequired)));
}

#[test]
fn read_only_snapshots_create_nothing_and_refuse_unresolved_tmp() {
    let (_temp, home) = home();
    assert!(matches!(
        AccountStore::read_only_at(&home),
        Err(AuthError::ReadOnly)
    ));
    assert!(!home.join(".polaris").exists());
    let store = AccountStore::open_at(&home).unwrap();
    let a = register(&store, "A", "acct-a");
    let snapshot = AccountStore::read_only_at(&home).unwrap();
    assert_eq!(
        account(&snapshot, &a.registration_id).state,
        AccountState::Ready
    );
    assert!(
        with_fault("registry_tmp_synced", || store
            .logout(&a.registration_id, 1))
        .is_err()
    );
    assert!(matches!(
        AccountStore::read_only_at(&home),
        Err(AuthError::RecoveryRequired)
    ));
}

#[test]
fn the_single_auth_file_is_never_read_or_changed() {
    let (_temp, home) = home();
    std::fs::create_dir(home.join(".polaris")).unwrap();
    std::fs::set_permissions(
        home.join(".polaris"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(home.join(".polaris/auth.json"), b"synthetic legacy").unwrap();
    let store = AccountStore::open_at(&home).unwrap();
    register(&store, "A", "acct-a");
    assert_eq!(
        std::fs::read(home.join(".polaris/auth.json")).unwrap(),
        b"synthetic legacy"
    );
    let saved = std::fs::read_to_string(home.join(".polaris/accounts/registry.json")).unwrap();
    assert!(!saved.contains("synthetic legacy"));
}

#[test]
fn generation_and_revision_overflow_are_refused() {
    let mut registry = AccountRegistry::initial(1);
    registry.next_registration_order = 1;
    registry.slots.push(Slot {
        registration_id: "a".repeat(22),
        generation: u64::MAX,
        principal_key: "b".repeat(22),
        registration_order: 0,
        token_revision: 1,
        label: None,
        upstream_account_id: Some("acct".into()),
        state: SlotState::ReauthRequired,
    });
    registry.previous_revision = 1;
    registry.revision = 2;
    let logout = Operation::Logout {
        registration_id: "a".repeat(22),
        generation: u64::MAX,
    };
    assert!(matches!(
        registry.apply(&logout, None),
        Err(AuthError::AccountLimit)
    ));
    registry.previous_revision = u64::MAX - 1;
    registry.revision = u64::MAX;
    registry.slots[0].generation = 1;
    let logout = Operation::Logout {
        registration_id: "a".repeat(22),
        generation: 1,
    };
    assert!(matches!(
        registry.apply(&logout, None),
        Err(AuthError::AccountLimit)
    ));
    // Credentials never appear in Debug output.
    let debug = format!("{:?}", StoredCredentials::from(credentials("acct-a")));
    assert!(!debug.contains("synthetic"));
}
