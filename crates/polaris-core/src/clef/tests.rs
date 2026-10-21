//! Protocol and lifecycle fixtures, with no real weights or remote calls.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

const FIXTURE: &str = r#"#!/bin/sh
set -eu
IFS= read -r init
owner=${init#*\"owner\":\"}; owner=${owner%%\"*}
generation=${init#*\"generation\":\"}; generation=${generation%%\"*}
digest=${init#*\"model_sha256\":\"}; digest=${digest%%\"*}
model=${init#*\"model\":\"}; model=${model%%\"*}
test "${TMPDIR%/tmp}" = "${PWD%/runtime}" || exit 40
test "${HOME%/home}" = "${PWD%/runtime}" || exit 40
exec 3<"$model"
value=''; IFS= read -r value <&3 || test "$value" = dummy
test "$value" = dummy
if printf forbidden >>"$model" 2>/dev/null; then exit 41; fi
if { IFS= read -r value <"${model%/*}/fixture"; } 2>/dev/null; then exit 42; fi
printf '{"protocol":1,"owner":"%s","generation":"%s","status":"ready","runtime_revision":"620d2225cc436ab0b001e3200e590e3b286ef9ab+polaris-fileptr-v1","model_sha256":"%s","device":"cpu","context_tokens":1024}\n' "$owner" "$generation" "$digest"
while IFS= read -r request; do
  id=${request#*\"request_id\":}; id=${id%%,*}; id=${id%%\}*}
  case "$request" in
    *'"op":"shutdown"'*)
      printf '{"protocol":1,"owner":"%s","generation":"%s","request_id":%s,"status":"closed"}\n' "$owner" "$generation" "$id"
      exit 0;;
    *'hang'*|*'timeout'*) printf started >"$TMPDIR/inflight"; while :; do :; done;;
    *'crash'*) exit 23;;
    *'bad-generation'*) generation=wrong;;
  esac
  printf '{"protocol":1,"owner":"%s","generation":"%s","request_id":%s,"status":"ok","decision":{"model":"/dev/fd/3","answers":{"yes":{"type":"noul","noul":0.9}},"usage":{"input_tokens":7,"output_tokens":0}}}\n' "$owner" "$generation" "$id"
done
"#;

fn config() -> (tempfile::TempDir, ClefConfig) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let worker = root.join("fixture");
    let model = root.join("model.gguf");
    fs::write(&worker, FIXTURE).unwrap();
    fs::write(&model, b"dummy").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let config = ClefConfig {
        worker,
        worker_sha256: format!("{:x}", Sha256::digest(FIXTURE)),
        model,
        model_bytes: 5,
        model_sha256: format!("{:x}", Sha256::digest(b"dummy")),
        generation: "fixture-1".into(),
        device: ClefDevice::Cpu,
        context_tokens: 1024,
        request_timeout_ms: 1000,
        load_timeout_ms: 1000,
        private_root: root,
        admission: None,
    };
    (dir, config)
}

fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        "yes".into(),
        Question::Noul {
            instructions: "fits?".into(),
        },
    )])
}

fn response() -> serde_json::Value {
    serde_json::json!({"protocol":1,"owner":"owner","generation":"gen","request_id":1,"status":"ok",
        "decision":{"model":"/dev/fd/3","answers":{"yes":{"type":"noul","noul":0.9}},"usage":{"input_tokens":7,"output_tokens":0}}})
}

#[test]
fn construction_is_lazy_and_refuses_unpinned_or_aliased_resources() {
    let (_dir, c) = config();
    let decider = ClefDecider::new(c.clone()).unwrap();
    let held = decider.capabilities();
    assert_eq!(
        held.location,
        polaris_provider::decision::DecisionLocation::LocalProcess
    );
    assert_eq!(
        held.band_calibration,
        polaris_provider::decision::BandCalibration::Unmeasured
    );
    assert_eq!(held.max_context_tokens, Some(1024));
    assert_eq!(held.max_state_tokens, None);
    assert_eq!(held.max_questions, Some(64));
    assert_eq!(held.max_options, Some(255));
    assert_eq!(held.max_frame_bytes, Some(wire::MAX_FRAME));
    assert_eq!(
        held.usage,
        polaris_provider::decision::DecisionUsageReporting::Required
    );
    assert_eq!(
        held.cancellation,
        polaris_provider::decision::DecisionCancellation::OwnedProcess
    );
    assert_eq!(fs::read_dir(&c.private_root).unwrap().count(), 2);
    for bad in [
        ClefConfig {
            generation: "bad owner".into(),
            ..c.clone()
        },
        ClefConfig {
            worker_sha256: "unverified".into(),
            ..c.clone()
        },
        ClefConfig {
            model_bytes: 6,
            ..c.clone()
        },
        ClefConfig {
            context_tokens: 1,
            ..c.clone()
        },
        ClefConfig {
            request_timeout_ms: 0,
            ..c.clone()
        },
    ] {
        assert!(ClefDecider::new(bad).is_err());
    }
    let alias = c.private_root.join("alias");
    std::os::unix::fs::symlink(&c.model, &alias).unwrap();
    assert!(
        ClefDecider::new(ClefConfig {
            model: alias,
            ..c.clone()
        })
        .is_err()
    );
    let target = c.private_root.join("snapshot");
    assert_eq!(
        snapshot_worker(
            &ClefConfig {
                worker_sha256: "0".repeat(64),
                ..c
            },
            &target
        ),
        Err(DecisionError::Malformed)
    );
}

#[test]
fn native_reply_binds_identity_shape_and_usage() {
    let good = response();
    let parse = |r: &serde_json::Value| {
        wire::decision(r.to_string().as_bytes(), "owner", "gen", 1, &questions())
    };
    let decision = parse(&good).unwrap();
    assert_eq!(decision.input_tokens, Some(7));
    assert_eq!(decision.answers["yes"].noul, Some(0.9));
    for (key, value) in [
        ("protocol", serde_json::json!(2)),
        ("owner", serde_json::json!("other")),
        ("generation", serde_json::json!("other")),
        ("request_id", serde_json::json!(2)),
        ("unexpected", serde_json::json!(true)),
    ] {
        let mut bad = good.clone();
        bad[key] = value;
        assert_eq!(parse(&bad), Err(DecisionError::InvalidAnswer));
    }
    for (pointer, value) in [
        ("/decision/answers/yes/noul", serde_json::json!(1.1)),
        ("/decision/answers/yes/type", serde_json::json!("choice")),
        ("/decision/usage/output_tokens", serde_json::json!(1)),
    ] {
        let mut bad = good.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert_eq!(parse(&bad), Err(DecisionError::InvalidAnswer));
    }
    let duplicate = good
        .to_string()
        .replace("\"noul\":0.9", "\"noul\":0.9,\"noul\":0.8");
    assert_eq!(
        wire::decision(duplicate.as_bytes(), "owner", "gen", 1, &questions()),
        Err(DecisionError::InvalidAnswer)
    );
    let duplicate = good.to_string().replace(
        "\"yes\":{",
        "\"yes\":{\"type\":\"noul\",\"noul\":0.1},\"yes\":{",
    );
    assert_eq!(
        wire::decision(duplicate.as_bytes(), "owner", "gen", 1, &questions()),
        Err(DecisionError::InvalidAnswer)
    );
}

#[test]
fn native_choice_and_score_preserve_options_distribution_and_scale() {
    let asked = BTreeMap::from([
        (
            "route/担当".into(),
            Question::Choice {
                instructions: "team?".into(),
                criteria: BTreeMap::from([
                    ("billing/請求".into(), "payments".into()),
                    ("shipping/配送".into(), "delivery".into()),
                ]),
            },
        ),
        (
            "urgency".into(),
            Question::Score {
                instructions: "urgency?".into(),
                criteria: vec!["later".into(), "today".into()],
            },
        ),
    ]);
    let good = serde_json::json!({"protocol":1,"owner":"owner","generation":"gen","request_id":1,"status":"ok",
        "decision":{"model":"/dev/fd/3","answers":{
            "route/担当":{"type":"choice","choice":"billing/請求","probabilities":{"billing/請求":0.75,"shipping/配送":0.25},"confidence":0.5},
            "urgency":{"type":"score","score":0.75,"probabilities":{"0":0.25,"1":0.75},"legend":{"0":"later","1":"today"},"confidence":0.5}
        },"usage":{"input_tokens":42,"output_tokens":0}}});
    let parse = |value: &serde_json::Value| {
        wire::decision(value.to_string().as_bytes(), "owner", "gen", 1, &asked)
    };
    let result = parse(&good).unwrap();
    assert_eq!(
        result.answers["route/担当"].choice.as_deref(),
        Some("billing/請求")
    );
    assert_eq!(result.answers["urgency"].score, Some(0.75));
    assert_eq!(result.answers["urgency"].legend["1"], "today");
    assert_eq!(result.input_tokens, Some(42));
    for (pointer, value) in [
        ("/decision/answers/urgency/score", serde_json::json!(0.25)),
        (
            "/decision/answers/urgency/legend/1",
            serde_json::json!("different scale"),
        ),
        (
            "/decision/answers/urgency/probabilities/1",
            serde_json::json!(0.25),
        ),
        (
            "/decision/answers/route~1担当/choice",
            serde_json::json!("unknown"),
        ),
        (
            "/decision/answers/route~1担当/probabilities/billing~1請求",
            serde_json::json!(-0.25),
        ),
    ] {
        let mut bad = good.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            parse(&bad),
            Err(DecisionError::InvalidAnswer),
            "accepted {pointer}"
        );
    }
    let duplicate = good.to_string().replace(
        "\"billing/請求\":0.75",
        "\"billing/請求\":0.75,\"billing/請求\":0.5",
    );
    assert_eq!(
        wire::decision(duplicate.as_bytes(), "owner", "gen", 1, &asked),
        Err(DecisionError::InvalidAnswer)
    );
}

#[tokio::test]
async fn framing_bounds_the_whole_message_and_refuses_partial_eof() {
    let mut input = BufReader::new(&b"{a}\n{b}\n"[..]);
    assert_eq!(wire::frame(&mut input).await.unwrap(), b"{a}");
    assert_eq!(wire::frame(&mut input).await.unwrap(), b"{b}");
    let bytes = vec![b'x'; wire::MAX_FRAME + 1];
    assert_eq!(
        wire::frame(&mut BufReader::new(bytes.as_slice())).await,
        Err(DecisionError::InvalidAnswer)
    );
    assert_eq!(
        wire::frame(&mut BufReader::new(&b"partial"[..])).await,
        Err(DecisionError::InvalidAnswer)
    );
}

#[test]
fn oversized_or_media_requests_do_not_enter_the_actor() {
    assert!(wire::validate_request(&serde_json::Value::Null, &questions()).is_ok());
    let huge = serde_json::json!("x".repeat(wire::MAX_FRAME));
    assert_eq!(
        wire::validate_request(&huge, &questions()),
        Err(DecisionError::TooLarge)
    );
    for kind in ["image_url", "input_audio"] {
        let media = serde_json::json!({"messages":[{"content":[{"type":kind}]}]});
        assert_eq!(
            wire::validate_request(&media, &questions()),
            Err(DecisionError::Malformed)
        );
    }
}

#[tokio::test]
async fn owner_reuses_one_process_then_reaps_on_graceful_shutdown() {
    let (_dir, c) = config();
    let root = c.private_root.clone();
    let decider = ClefDecider::new(c).unwrap();
    for _ in 0..3 {
        assert_eq!(
            decider
                .decide(&serde_json::Value::Null, &questions())
                .await
                .unwrap()
                .answers["yes"]
                .noul,
            Some(0.9)
        );
    }
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        3,
        "one private generation"
    );
    assert_eq!(decider.shutdown().await, Ok(()));
    assert_eq!(
        decider.decide(&serde_json::Value::Null, &questions()).await,
        Err(DecisionError::Unavailable)
    );
    // The actor removes its private generation after the shutdown receipt.
    tokio::task::yield_now().await;
    assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
}

#[tokio::test]
async fn cancelled_or_invalid_work_is_terminal_without_restart() {
    for mode in ["hang", "timeout", "crash", "bad-generation"] {
        let (_dir, c) = config();
        let root = c.private_root.clone();
        let decider = Arc::new(ClefDecider::new(c).unwrap());
        decider
            .decide(&serde_json::Value::Null, &questions())
            .await
            .unwrap();
        let inflight = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .unwrap()
            .path()
            .join("tmp/inflight");
        let held = decider.clone();
        let mode = mode.to_owned();
        let hang = mode == "hang";
        let job =
            tokio::spawn(async move { held.decide(&serde_json::json!(mode), &questions()).await });
        if hang {
            // The fixture acknowledges that it started work before we abort
            // the caller. Admission alone would allow a pre-dispatch cancel.
            for _ in 0..100 {
                if inflight.is_file() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(inflight.is_file());
            assert_eq!(decider.capacity.available_permits(), 0);
            assert_eq!(
                decider.decide(&serde_json::Value::Null, &questions()).await,
                Err(DecisionError::Unavailable),
                "busy has no queued copy"
            );
            job.abort();
            let _ = job.await;
        } else {
            assert!(job.await.unwrap().is_err());
        }
        for _ in 0..300 {
            if fs::read_dir(&root).unwrap().count() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            2,
            "owned process cleanup completed"
        );
        assert_eq!(
            decider.decide(&serde_json::Value::Null, &questions()).await,
            Err(DecisionError::Unavailable),
            "no silent restart"
        );
    }
}

#[tokio::test]
async fn a_refused_managed_capacity_profile_never_starts_the_worker() {
    use crate::decision_capacity::{CapacityPolicy, ProfileMemory};
    let (_root, mut c) = config();
    let directory = c.private_root.clone();
    c.admission = Some(CapacityAdmission {
        profile: ProfileMemory {
            accepted: false,
            target: crate::decision_capacity::observation::host_memory().target,
            max_context_tokens: 1024,
            max_parallel: 1,
            peak: None,
        },
        policy: CapacityPolicy {
            reserve_bytes: 0,
            colocated_bytes: 0,
            budget_limit_bytes: None,
            max_age: Duration::from_secs(15),
        },
    });
    let decider = ClefDecider::new(c).unwrap();
    assert_eq!(
        decider.decide(&serde_json::json!({}), &questions()).await,
        Err(DecisionError::Unavailable)
    );
    assert_eq!(
        fs::read_dir(&directory).unwrap().count(),
        2,
        "a refused capacity profile created runtime scratch"
    );
    assert_eq!(
        decider.decide(&serde_json::json!({}), &questions()).await,
        Err(DecisionError::Unavailable)
    );
}
