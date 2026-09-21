//! A loopback issuer, a loopback usage host and temporary homes only.
use super::*;
use crate::Credentials;
use crate::accounts::store::AccountStore;
use crate::quota::usage::VERIFIED_FOR_TESTS;
use crate::quota::{Exclusion, ExclusionReason, UnavailableReason};
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "test-verified-model";

fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = home.path().canonicalize().unwrap();
    (home, path)
}

/// A saved registration whose access token is still valid.
fn registered(home: &std::path::Path, account: &str) -> (String, u64) {
    let store = AccountStore::open_at(home).unwrap();
    let reservation = store.begin_registration(None).unwrap();
    store.mark_exchange_pending(&reservation).unwrap();
    let (generation, _) = store
        .complete_registration(
            &reservation,
            Credentials {
                access_token: format!("synthetic-access-{account}"),
                refresh_token: format!("synthetic-refresh-{account}"),
                account_id: account.into(),
                expires_at: Some(crate::accounts::owner::now_secs() + 3600),
            },
        )
        .unwrap();
    (reservation.registration_id, generation)
}

fn owner(home: &std::path::Path, issuer: &str, usage: &str) -> Arc<AccountOwner> {
    AccountOwner::with_usage_base(AccountStore::open_at(home).unwrap(), issuer, usage).unwrap()
}

fn usage_body() -> serde_json::Value {
    serde_json::json!({"rate_limit_status_details": {
        "allowed": true,
        "limit_reached": false,
        "primary": {"used_percent": 10.0, "window_minutes": 300, "resets_in_seconds": 600},
        "secondary": {"used_percent": 20.0, "window_minutes": 10_080, "resets_in_seconds": 86_400},
    }})
}

async fn usage_mock(server: &MockServer, bearer: &str, response: ResponseTemplate, times: u64) {
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("authorization", format!("Bearer {bearer}")))
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

fn state(owner: &AccountOwner, id: &str) -> ObservationState {
    owner.observation(id, MODEL).unwrap().state
}

#[tokio::test]
async fn an_unverified_profile_is_recorded_without_reaching_the_host() {
    let (_temp, home) = home();
    let (id, generation) = registered(&home, "acct-a");
    let usage = MockServer::start().await;
    let owner = owner(&home, "https://unused.invalid", &usage.uri());

    assert!(owner.observe(&id, generation, "gpt-6-astra").await);
    assert!(owner.observe(&id, generation, "no-such-model").await);
    assert_eq!(
        owner.observation(&id, "gpt-6-astra").unwrap().state,
        ObservationState::MappingUnknown
    );
    assert_eq!(
        owner.observation(&id, "no-such-model").unwrap().state,
        ObservationState::Unsupported
    );
    assert_eq!(
        owner.select(&["gpt-6-astra"]).unwrap(),
        SelectionOutcome::Unavailable {
            reason: UnavailableReason::NoEligibleAccounts,
            exclusions: vec![Exclusion {
                registration_id: id,
                reason: ExclusionReason::MappingUnknown,
            }],
        }
    );
    assert!(usage.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_process_that_has_not_observed_yet_selects_nothing() {
    let (_temp, home) = home();
    let (id, _) = registered(&home, "acct-a");
    let owner = owner(&home, "https://unused.invalid", "https://unused.invalid");

    let snapshot = owner.selection_snapshot(&[MODEL]).unwrap();
    assert_eq!(snapshot.accounts.len(), 1);
    assert_eq!(
        snapshot.accounts[0].observation.state,
        ObservationState::Missing
    );
    assert!(matches!(
        owner.select(&[MODEL]).unwrap(),
        SelectionOutcome::Unavailable {
            reason: UnavailableReason::NoEligibleAccounts,
            ..
        }
    ));
    assert!(owner.observation(&id, MODEL).is_none());
}

#[tokio::test]
async fn a_second_request_inside_the_interval_never_reaches_the_host() {
    let (_temp, home) = home();
    let (id, generation) = registered(&home, "acct-a");
    let usage = MockServer::start().await;
    usage_mock(
        &usage,
        "synthetic-access-acct-a",
        ResponseTemplate::new(200).set_body_json(usage_body()),
        1,
    )
    .await;
    let owner = owner(&home, "https://unused.invalid", &usage.uri());

    assert!(
        owner
            .observe_with(Some(&VERIFIED_FOR_TESTS), &id, generation, MODEL)
            .await
    );
    assert!(
        !owner
            .observe_with(Some(&VERIFIED_FOR_TESTS), &id, generation, MODEL)
            .await
    );
    assert!(matches!(state(&owner, &id), ObservationState::Known(_)));
    let SelectionOutcome::Selected { selection, .. } = owner.select(&[MODEL]).unwrap() else {
        panic!("a known observation is selectable");
    };
    assert_eq!(selection.registration_id, id);
    assert_eq!(selection.profile_id, "test-verified");
    usage.verify().await;
}

#[tokio::test]
async fn one_unauthorized_retry_uses_the_refreshed_token() {
    let (_temp, home) = home();
    let (id, generation) = registered(&home, "acct-a");
    let issuer = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-a",
            "refresh_token": "synthetic-rotated-a",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&issuer)
        .await;
    let usage = MockServer::start().await;
    usage_mock(
        &usage,
        "synthetic-access-acct-a",
        ResponseTemplate::new(401),
        1,
    )
    .await;
    usage_mock(
        &usage,
        "fresh-a",
        ResponseTemplate::new(200).set_body_json(usage_body()),
        1,
    )
    .await;
    let owner = owner(&home, &issuer.uri(), &usage.uri());

    assert!(
        owner
            .observe_with(Some(&VERIFIED_FOR_TESTS), &id, generation, MODEL)
            .await
    );
    assert!(matches!(state(&owner, &id), ObservationState::Known(_)));
    usage.verify().await;
    issuer.verify().await;
}

#[tokio::test]
async fn a_second_unauthorized_fails_the_fetch_and_keeps_the_credentials() {
    let (_temp, home) = home();
    let (id, generation) = registered(&home, "acct-a");
    let issuer = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-a",
            "refresh_token": "synthetic-rotated-a",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&issuer)
        .await;
    let usage = MockServer::start().await;
    usage_mock(
        &usage,
        "synthetic-access-acct-a",
        ResponseTemplate::new(401),
        1,
    )
    .await;
    usage_mock(&usage, "fresh-a", ResponseTemplate::new(401), 1).await;
    let owner = owner(&home, &issuer.uri(), &usage.uri());

    assert!(
        owner
            .observe_with(Some(&VERIFIED_FOR_TESTS), &id, generation, MODEL)
            .await
    );
    assert_eq!(state(&owner, &id), ObservationState::FetchFailed);
    // The refreshed credential survives a failed observation.
    assert_eq!(
        owner.token(&id, generation).await.unwrap().secret(),
        "fresh-a"
    );
    usage.verify().await;
}

#[tokio::test]
async fn a_logout_during_the_request_discards_the_result_and_no_lease_is_held() {
    let (_temp, home) = home();
    let (id, generation) = registered(&home, "acct-a");
    let usage = MockServer::start().await;
    usage_mock(
        &usage,
        "synthetic-access-acct-a",
        ResponseTemplate::new(200)
            .set_body_json(usage_body())
            .set_delay(Duration::from_millis(400)),
        1,
    )
    .await;
    let owner = owner(&home, "https://unused.invalid", &usage.uri());

    let job = tokio::spawn({
        let owner = Arc::clone(&owner);
        let id = id.clone();
        async move {
            owner
                .observe_with(Some(&VERIFIED_FOR_TESTS), &id, generation, MODEL)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let store = AccountStore::open_at(&home).unwrap();
    // The fetch holds no slot lease, so another owner could refresh meanwhile.
    assert!(store.try_slot_lease(0).unwrap().is_some());
    store.logout(&id, generation).unwrap();

    assert!(job.await.unwrap());
    assert!(owner.observation(&id, MODEL).is_none());
    usage.verify().await;
}

#[tokio::test]
async fn a_bound_source_stops_at_a_logout_and_never_reaches_another_registration() {
    let (_temp, home) = home();
    let (id, generation) = registered(&home, "acct-a");
    let (other, other_generation) = registered(&home, "acct-b");
    let owner = owner(&home, "https://unused.invalid", "https://unused.invalid");
    let bound = BoundTokenSource::new(Arc::clone(&owner), id.clone(), generation);
    assert_eq!(
        bound.token().await.unwrap().secret(),
        "synthetic-access-acct-a"
    );

    AccountStore::open_at(&home)
        .unwrap()
        .logout(&id, generation)
        .unwrap();
    // A queued run's first send and a running run's next send both stop here.
    assert!(bound.token().await.is_err());
    assert!(bound.refresh_after_unauthorized(1).await.is_err());
    // The other registration stays usable, but never through this source.
    let elsewhere = BoundTokenSource::new(owner, other.clone(), other_generation);
    assert_eq!(elsewhere.token().await.unwrap().registration_id, other);
}

/// D-02: a run sends its summary and light model to the same registration, so
/// an account whose coverage for any of them is unknown is not selected.
#[tokio::test]
async fn an_account_is_excluded_when_another_model_of_the_run_is_unknown() {
    const OTHER: &str = "other-model";
    let (_temp, home) = home();
    let (first, first_generation) = registered(&home, "acct-a");
    let (second, second_generation) = registered(&home, "acct-b");
    let owner = owner(&home, "https://unused.invalid", "https://unused.invalid");
    let known = || {
        crate::quota::usage::observe(
            &VERIFIED_FOR_TESTS,
            &crate::quota::usage::UsageResponse {
                allowed: Some(true),
                limit_reached: Some(false),
                primary: Some(crate::quota::usage::RawUsageWindow {
                    usage_percent: Some(10.0),
                    window_minutes: Some(300),
                    window_seconds: None,
                    resets_in_seconds: Some(600),
                    resets_at_unix_seconds: None,
                }),
                secondary: Some(crate::quota::usage::RawUsageWindow {
                    usage_percent: Some(20.0),
                    window_minutes: Some(10_080),
                    window_seconds: None,
                    resets_in_seconds: Some(86_400),
                    resets_at_unix_seconds: None,
                }),
            },
            crate::accounts::owner::now_secs(),
        )
        .unwrap()
    };
    for (id, generation) in [(&first, first_generation), (&second, second_generation)] {
        owner.record(id, generation, MODEL, known());
    }
    owner.record(&first, first_generation, OTHER, known());
    owner.record(
        &second,
        second_generation,
        OTHER,
        ObservationState::MappingUnknown,
    );

    let SelectionOutcome::Selected {
        selection,
        exclusions,
    } = owner.select(&[MODEL, OTHER]).unwrap()
    else {
        panic!("the covered registration is selectable");
    };
    assert_eq!(selection.registration_id, first);
    assert_eq!(
        exclusions,
        vec![Exclusion {
            registration_id: second,
            reason: ExclusionReason::MappingUnknown,
        }]
    );
}
