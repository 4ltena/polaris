
use super::*;

/// The configured judge is the one that answers, and a judge that
/// cannot start yields none rather than the other one.
///
/// The env var is set inside the test because the Jev branch reads
/// it: without it the second case would pass for the wrong reason,
/// reporting "no key" where the property under test is "no silent
/// substitution". Nothing else in this binary reads that name.
#[test]
fn a_configured_backend_answers_or_nothing_does() {
    use polaris_core::config::{DecisionBackend, DecisionConfig};

    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("semif-helper");
    std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
    // SAFETY: single-threaded test, and no other test in this binary
    // reads or writes this variable.
    unsafe { std::env::set_var("POLARIS_JEV_API_KEY", "test-key") };

    let semif = DecisionConfig {
        enabled: true,
        backend: DecisionBackend::SemIf,
        helper: Some(helper.clone()),
        ..Default::default()
    };
    assert_eq!(
        build_decider(&semif).map(|d| d.name().to_owned()),
        Some("semif".to_owned())
    );

    let missing = DecisionConfig {
        helper: Some(dir.path().join("absent")),
        ..semif.clone()
    };
    assert!(
        build_decider(&missing).is_none(),
        "a SemIf that cannot start was answered by the remote judge"
    );

    let jev = DecisionConfig {
        enabled: true,
        backend: DecisionBackend::Jev,
        ..Default::default()
    };
    assert_eq!(
        build_decider(&jev).map(|d| d.name().to_owned()),
        Some("jev".to_owned())
    );

    let clef = DecisionConfig {
        enabled: true,
        backend: DecisionBackend::Clef,
        model: Some("clef-flash-q4-cpu-v1".into()),
        ..Default::default()
    };
    assert!(
        build_decider(&clef).is_none(),
        "an unverified Clef profile was answered by the remote judge"
    );
    assert!(build_finalizer_decider(&clef).is_none());

    let off = DecisionConfig {
        enabled: false,
        ..semif
    };
    assert!(build_decider(&off).is_none(), "disabled still built one");
    unsafe { std::env::remove_var("POLARIS_JEV_API_KEY") };
}

#[test]
fn tool_memory_defaults_off_and_is_independent_of_remember() {
    let args = Args::try_parse_from(["polaris"]).unwrap();
    assert_eq!(args.tool_memory.retention(), None);
    assert!(args.validate_tool_memory().is_ok());
    for mode in ["history", "retrieval"] {
        let args = Args::try_parse_from(["polaris", "--tool-memory", mode]).unwrap();
        assert!(args.tool_memory.retention().is_some());
        assert!(!args.remember);
        assert!(args.validate_tool_memory().is_ok());
    }
    let args = Args::try_parse_from(["polaris", "--remember"]).unwrap();
    assert_eq!(args.tool_memory.retention(), None);
    assert!(Args::try_parse_from(["polaris", "--tool-memory", "invalid"]).is_err());
}

#[test]
fn tool_memory_embedding_requires_pair_and_enabled_mode() {
    for flag in [
        "--tool-memory-embedding-url",
        "--tool-memory-embedding-model",
    ] {
        assert!(Args::try_parse_from(["polaris", flag, "value"]).is_err());
    }
    for mode in ["off", "history", "retrieval"] {
        let args = Args::try_parse_from([
            "polaris",
            "--tool-memory",
            mode,
            "--tool-memory-embedding-url",
            "http://localhost:8080/v1",
            "--tool-memory-embedding-model",
            "local-model",
        ])
        .unwrap();
        assert_eq!(args.validate_tool_memory().is_ok(), mode != "off");
    }
    let args = Args::try_parse_from([
        "polaris",
        "--tool-memory",
        "retrieval",
        "--tool-memory-embedding-url",
        " ",
        "--tool-memory-embedding-model",
        "model",
    ])
    .unwrap();
    assert!(args.validate_tool_memory().is_err());
}

#[test]
fn tool_memory_help_lists_settings() {
    let help = <Args as clap::CommandFactory>::command()
        .render_long_help()
        .to_string();
    for flag in [
        "--tool-memory",
        "--tool-memory-embedding-url",
        "--tool-memory-embedding-model",
    ] {
        assert!(help.contains(flag));
    }
    assert!(help.contains("off, history, retrieval"));
}

#[tokio::test]
async fn tui_answer_finalizer_obeys_off_shadow_and_enforce_modes() {
    use polaris_core::config::{
        AnswerFinalizerMode, JudgeProfile, PresentationLanguage, TranslationActivation,
        TranslationProfile,
    };

    let root = tempfile::tempdir().unwrap();
    let mut config = polaris_core::config::Config::default();
    assert!(
        build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
            .await
            .unwrap()
            .is_none()
    );

    config.presentation.language = PresentationLanguage::Ja;
    config.answer_finalizer.mode = AnswerFinalizerMode::Shadow;
    config.answer_finalizer.judge_profile = Some(JudgeProfile::FulfillmentV1);
    assert!(
        build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
            .await
            .unwrap()
            .is_some()
    );

    config.answer_finalizer.translation.activation = TranslationActivation::Requested;
    config.answer_finalizer.translation.profile = Some(TranslationProfile::CatTranslate3_3b);
    let owner = build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
        .await
        .unwrap()
        .expect("missing managed model remains an observational shadow failure");
    assert_eq!(
        owner.translation_observation().unwrap(),
        (true, None, false, 0)
    );

    config.answer_finalizer.mode = AnswerFinalizerMode::Enforce;
    assert!(
        build_tui_answer_finalizer(&config, root.path(), Some(root.path()))
            .await
            .is_err()
    );
}
