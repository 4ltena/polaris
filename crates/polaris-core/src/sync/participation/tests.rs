//! Temporary state directories only.
use super::*;

fn link(share: &str) -> ShareLink {
    ShareLink {
        remote: "ssh://git@example.invalid/sync.git".into(),
        share_id: share.into(),
        invitation_id: "invite-0001".into(),
        expires_unix_seconds: 2_000,
    }
}

#[test]
fn joining_records_the_share_without_a_credential_and_refuses_a_spent_invitation() {
    let state = tempfile::tempdir().unwrap();
    assert!(read(state.path()).is_none());

    let participation = join(
        state.path(),
        link("share-0001"),
        ShareManifest::default(),
        1_000,
    )
    .unwrap();
    assert_eq!(read(state.path()), Some(participation.clone()));
    assert_eq!(participation.agreed_commit, None);
    let body = fs::read_to_string(path(state.path())).unwrap();
    assert!(!body.contains("password") && !body.contains("token"));

    // An invitation that has run out cannot be used later.
    assert!(
        join(
            state.path(),
            link("share-0001"),
            ShareManifest::default(),
            2_000
        )
        .is_err()
    );
    // One project belongs to one share.
    assert!(
        join(
            state.path(),
            link("share-0002"),
            ShareManifest::default(),
            1_000
        )
        .is_err()
    );
    assert_eq!(read(state.path()).unwrap().share.share_id, "share-0001");

    assert!(leave(state.path()).unwrap());
    assert!(!leave(state.path()).unwrap());
    assert!(read(state.path()).is_none());
}
