//! Fixed inputs only; nothing is sent anywhere.
use super::*;

fn implementation() -> Implementation {
    Implementation {
        run_id: "run-1".into(),
        attempt_id: "attempt-1".into(),
        project_root: "/work/app".into(),
        paths: vec!["src/main.rs".into(), "README.md".into()],
        summary: "起動時の設定読み込みを直した。".into(),
        checks: vec!["アプリを起動する".into(), "設定画面を開く".into()],
    }
}

#[test]
fn the_prompt_names_the_generation_and_never_claims_the_check_passed() {
    let text = prompt(&implementation()).unwrap();
    assert!(text.contains("run run-1 / attempt attempt-1"));
    assert!(text.contains("/work/app"));
    assert!(text.contains("- src/main.rs"));
    assert!(text.contains("1. アプリを起動する"));
    assert!(text.contains("実機での合格は含まない"));
    assert!(!text.contains("合格した"));
}

#[test]
fn a_run_with_nothing_to_try_produces_no_prompt() {
    assert_eq!(
        prompt(&Implementation {
            paths: vec![],
            ..implementation()
        }),
        Err(Refusal::NoChanges)
    );
    assert_eq!(
        prompt(&Implementation {
            checks: vec![],
            ..implementation()
        }),
        Err(Refusal::NoChecks)
    );
    for over in [
        Implementation {
            paths: (0..=MAX_PATHS).map(|n| format!("f{n}")).collect(),
            ..implementation()
        },
        Implementation {
            summary: "x".repeat(MAX_SUMMARY_BYTES + 1),
            ..implementation()
        },
        Implementation {
            checks: vec!["x".repeat(MAX_CHECK_BYTES + 1)],
            ..implementation()
        },
        Implementation {
            run_id: String::new(),
            ..implementation()
        },
    ] {
        assert_eq!(prompt(&over), Err(Refusal::Bounds));
    }
}
