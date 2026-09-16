//! Synthetic responses from a loopback server; no real account is contacted.
use super::*;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn profile(verified: bool) -> UsageProfile {
    UsageProfile {
        id: "test-profile",
        endpoint: UsageEndpoint::ChatGpt,
        models: &["test-model"],
        bucket_key: "chatgpt",
        window_slot_keys: &["primary", "secondary"],
        verified,
    }
}

fn raw(percent: f64, minutes: i128, resets_in: i128) -> RawUsageWindow {
    RawUsageWindow {
        usage_percent: Some(percent),
        window_minutes: Some(minutes),
        resets_in_seconds: Some(resets_in),
        resets_at_unix_seconds: None,
    }
}

fn complete() -> UsageResponse {
    UsageResponse {
        allowed: Some(true),
        limit_reached: Some(false),
        primary: Some(raw(1.0, 300, 600)),
        secondary: Some(raw(2.0, 10_080, 86_400)),
    }
}

async fn server(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("authorization", "Bearer synthetic-access"))
        .and(header("chatgpt-account-id", "acct-a"))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn get(server: &MockServer) -> Result<UsageResponse, AuthError> {
    fetch(
        &server.uri(),
        UsageEndpoint::ChatGpt,
        "synthetic-access",
        "acct-a",
    )
    .await
}

#[tokio::test]
async fn a_response_keeps_every_field_the_upstream_normalizer_drops() {
    let body = serde_json::json!({"rate_limit_status_details": {
        "allowed": true,
        "limit_reached": false,
        "primary": {"used_percent": 12.5, "window_minutes": 300, "resets_in_seconds": 600, "resets_at": 1_800_000_000i64},
        "secondary": {"used_percent": 0.0, "window_minutes": 10_080, "resets_in_seconds": 86_400},
    }});
    let server = server(ResponseTemplate::new(200).set_body_json(body)).await;
    let observed = get(&server).await.unwrap();
    assert_eq!(observed.allowed, Some(true));
    assert_eq!(observed.limit_reached, Some(false));
    let primary = observed.primary.unwrap();
    assert_eq!(primary.usage_percent, Some(12.5));
    assert_eq!(primary.window_minutes, Some(300));
    assert_eq!(primary.resets_in_seconds, Some(600));
    assert_eq!(primary.resets_at_unix_seconds, Some(1_800_000_000));
    assert_eq!(observed.secondary.unwrap().resets_in_seconds, Some(86_400));
    server.verify().await;
}

#[tokio::test]
async fn a_bounded_failure_carries_a_category_without_the_response_body() {
    for (response, expected) in [
        (
            ResponseTemplate::new(401).set_body_string("synthetic-access"),
            "status 401",
        ),
        (
            ResponseTemplate::new(302)
                .insert_header("location", "http://127.0.0.1:1/backend-api/wham/usage"),
            "status 302",
        ),
    ] {
        let server = server(response).await;
        let error = get(&server).await.unwrap_err();
        assert!(
            matches!(&error, AuthError::Http(kind) if kind == expected),
            "{error:?}"
        );
        assert!(!format!("{error}").contains("synthetic"));
        server.verify().await;
    }
    let malformed = server(ResponseTemplate::new(200).set_body_string("{not json")).await;
    assert!(
        matches!(get(&malformed).await, Err(AuthError::Decode(kind)) if kind == "invalid_json")
    );
    malformed.verify().await;
    // Valid JSON, so only the cap can stop it before it is decoded.
    let body = format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_RESPONSE_BYTES));
    let oversize = server(ResponseTemplate::new(200).set_body_string(body)).await;
    assert!(
        matches!(get(&oversize).await, Err(AuthError::Decode(kind)) if kind == "response_too_large")
    );
    oversize.verify().await;
    let slow = server(
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({}))
            .set_delay(TIMEOUT * 2),
    )
    .await;
    assert!(matches!(get(&slow).await, Err(AuthError::Http(kind)) if kind == "timeout"));
    slow.verify().await;
}

#[test]
fn an_unverified_profile_can_only_produce_an_unknown_mapping() {
    assert_eq!(
        observe(&profile(false), &complete(), 1_000).unwrap(),
        ObservationState::MappingUnknown
    );
    assert!(PROFILES.iter().all(|profile| !profile.verified));
    assert_eq!(
        profile_for("gpt-6-astra").map(|profile| profile.id),
        Some("chatgpt-primary-secondary")
    );
    assert!(profile_for("unknown-model").is_none());
}

#[test]
fn a_verified_profile_keeps_availability_and_derives_absolute_resets() {
    let response = UsageResponse {
        allowed: Some(false),
        limit_reached: Some(true),
        primary: Some(raw(99.9, 300, 600)),
        secondary: Some(RawUsageWindow {
            resets_at_unix_seconds: Some(1_800_000_000),
            ..raw(0.0, 10_080, 86_400)
        }),
    };
    let ObservationState::Known(known) = observe(&profile(true), &response, 1_000).unwrap() else {
        panic!("a verified profile with a complete response is known");
    };
    assert_eq!(known.windows[0].window_millis, 18_000_000);
    assert_eq!(known.windows[0].reset_unix_seconds, 1_600);
    // The absolute value the response carried wins over the remaining seconds.
    assert_eq!(known.windows[1].reset_unix_seconds, 1_800_000_000);
    assert_eq!(
        known.bucket_states,
        vec![BucketState {
            bucket_key: "chatgpt".into(),
            availability: ServerAvailability::Denied,
            reached: ReachedState::Reached,
        }]
    );
}

#[test]
fn a_missing_or_unconvertible_field_is_never_filled_in() {
    for response in [
        UsageResponse {
            secondary: None,
            ..complete()
        },
        UsageResponse {
            primary: Some(RawUsageWindow {
                usage_percent: None,
                ..raw(1.0, 300, 600)
            }),
            ..complete()
        },
        UsageResponse {
            primary: Some(RawUsageWindow {
                usage_percent: Some(-1.0),
                ..raw(1.0, 300, 600)
            }),
            ..complete()
        },
        UsageResponse {
            primary: Some(RawUsageWindow {
                window_minutes: Some(i128::MAX),
                ..raw(1.0, 300, 600)
            }),
            ..complete()
        },
        UsageResponse {
            primary: Some(RawUsageWindow {
                resets_in_seconds: None,
                resets_at_unix_seconds: None,
                ..raw(1.0, 300, 600)
            }),
            ..complete()
        },
    ] {
        assert_eq!(
            observe(&profile(true), &response, 1_000),
            Err(ExclusionReason::InvalidObservation)
        );
    }
}
