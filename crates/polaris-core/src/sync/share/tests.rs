//! Paths and rules only; no Git and no network.
use super::*;

fn link() -> ShareLink {
    ShareLink {
        remote: "ssh://git@example.invalid/polaris-projects.git".into(),
        share_id: "share-0001".into(),
        invitation_id: "invite-0001".into(),
        expires_unix_seconds: 2_000,
    }
}

#[test]
fn a_link_carries_no_credential_and_an_invitation_that_ends() {
    assert!(link().accept(1_000).is_ok());
    assert_eq!(link().accept(2_000), Err(LinkRefusal::Expired));
    for remote in [
        "https://user:secret@example.invalid/x.git",
        "ftp://example.invalid/x.git",
        "example.invalid/x.git",
    ] {
        assert_eq!(
            ShareLink {
                remote: remote.into(),
                ..link()
            }
            .accept(1_000),
            Err(LinkRefusal::Remote),
            "{remote}"
        );
    }
    for id in ["short", "has space", &"x".repeat(65)] {
        assert_eq!(
            ShareLink {
                share_id: id.into(),
                ..link()
            }
            .accept(1_000),
            Err(LinkRefusal::Identifier)
        );
    }
}

#[test]
fn the_synchronization_repository_never_sits_inside_the_project() {
    let paths = SyncPaths::under(Path::new("/state/project-a"));
    assert_eq!(paths.git_dir, Path::new("/state/project-a/sync/repository"));
    assert!(paths.work_tree.ends_with("sync/staging"));
    assert!(!paths.overlaps(Path::new("/work/project")));
    // A state directory placed inside the project would let Git reach it.
    assert!(
        SyncPaths::under(Path::new("/work/project/.polaris")).overlaps(Path::new("/work/project"))
    );
}

#[test]
fn the_project_repository_state_and_credentials_are_never_shared() {
    let manifest = ShareManifest {
        version: 1,
        excluded: vec!["private".into()],
    };
    for shared in ["src/main.rs", "package.json", "docs/readme.md"] {
        assert!(manifest.shares(Path::new(shared)), "{shared}");
    }
    for refused in [
        ".git",
        ".git/config",
        "src/.git/config",
        ".polaris/state.json",
        ".env",
        // A key lives in a `.env` variant far more often than in `.env`.
        ".env.local",
        ".env.production.local",
        ".envrc",
        "apps/web/.env.development.local",
        "node_modules/x/index.js",
        "target/debug/app",
        ".next/build-manifest.json",
        "coverage/lcov.info",
        "private/notes.md",
        "../outside",
        "",
    ] {
        assert!(!manifest.shares(Path::new(refused)), "{refused}");
    }
}

#[test]
fn both_sides_compare_one_digest_of_the_rules() {
    let manifest = ShareManifest {
        version: 1,
        excluded: vec!["b".into(), "a".into()],
    };
    let reordered = ShareManifest {
        version: 1,
        excluded: vec!["a".into(), "b".into(), "a".into()],
    };
    assert_eq!(manifest.digest(), reordered.digest());
    assert_ne!(manifest.digest(), ShareManifest::default().digest());
    assert_ne!(
        manifest.digest(),
        ShareManifest {
            version: 2,
            ..manifest.clone()
        }
        .digest()
    );
}

#[test]
fn p2a_production_remote_rejects_legacy_and_noncanonical_locators() {
    let valid = ShareLink {
        remote: "https://example.invalid:443/team/project.git".into(),
        ..link()
    };
    assert!(valid.accept_production(1_000).is_ok());
    for remote in [
        "https://example.invalid/team/project.git",
        "https://EXAMPLE.invalid:443/team/project.git",
        "https://example.invalid:443/team/../project.git",
        "https://example.invalid:443/team//project.git",
        "https://example.invalid:443/team/%70roject.git",
        "https://user@example.invalid:443/project.git",
        "https://user:secret@example.invalid:443/project.git",
        "https://example.invalid:443/project.git?token=secret",
        "https://example.invalid:443/project.git#secret",
        "https://example.invalid:443/project.git\n",
        "file:///tmp/project.git",
        "ssh://git@example.invalid/project.git",
        "git@example.invalid:project.git",
    ] {
        assert_eq!(
            ShareLink {
                remote: remote.into(),
                ..link()
            }
            .accept_production(1_000),
            Err(LinkRefusal::Remote)
        );
    }
}

#[test]
fn p2a_legacy_inspection_does_not_renew_or_rewrite_a_locator() {
    let legacy = link();
    assert!(legacy.validate_legacy().is_ok());
    assert_eq!(legacy.accept(2_000), Err(LinkRefusal::Expired));
    assert_eq!(
        legacy.remote,
        "ssh://git@example.invalid/polaris-projects.git"
    );
    for remote in [
        "https://user@example.invalid/project.git",
        "https://example.invalid/project.git?token=secret",
        "ssh://git:secret@example.invalid/project.git",
        "file:///tmp/project.git#secret",
        "ssh:///project.git",
        "git@example.invalid",
    ] {
        assert_eq!(
            ShareLink {
                remote: remote.into(),
                ..link()
            }
            .validate_legacy(),
            Err(LinkRefusal::Remote)
        );
    }
}

#[test]
fn p2a_debug_and_serialized_display_never_retain_unsafe_locator() {
    let unsafe_link = ShareLink {
        remote: "https://user:SECRET@example.invalid/private.git?token=TOKEN".into(),
        ..link()
    };
    let debug = format!("{unsafe_link:?}");
    let display = unsafe_link.remote_display();
    let encoded = serde_json::to_string(&display).unwrap();
    for text in [debug, display.to_string(), encoded] {
        for omitted in [
            "SECRET",
            "TOKEN",
            "user",
            "example.invalid",
            "private.git",
            &unsafe_link.remote,
        ] {
            assert!(!text.contains(omitted));
        }
    }
}
