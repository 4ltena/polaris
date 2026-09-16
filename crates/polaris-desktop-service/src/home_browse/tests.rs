//! Synthetic home only; never the real account home.
use super::*;
use polaris_desktop_protocol::{
    ProtocolVersion, codec,
    ids::{ClientId, RequestId},
    request::ShutdownRequest,
};
use std::{fs, os::unix::fs::PermissionsExt};
use tokio::{io::AsyncWriteExt, net::unix::pipe};

fn home() -> (tempfile::TempDir, PathBuf, HomeBrowser) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let browser = HomeBrowser::open(root.clone()).unwrap();
    (temp, root, browser)
}

fn list(
    browser: &mut HomeBrowser,
    path: &str,
    cursor: Option<String>,
) -> Result<BrowseListPage, ProtocolError> {
    browser.list(&BrowseList {
        path: path.into(),
        cursor,
    })
}

fn identity(root: &Path, path: &str) -> BrowseRead {
    let m = fs::symlink_metadata(root.join(path)).unwrap();
    BrowseRead {
        path: path.into(),
        device: DecimalU64::new(m.dev()),
        inode: DecimalU64::new(m.ino()),
    }
}

fn read(browser: &mut HomeBrowser, request: &BrowseRead) -> BrowseContent {
    browser.read(request).unwrap().content
}

fn unavailable(reason: BrowseUnavailable) -> BrowseContent {
    BrowseContent::Unavailable { reason }
}

#[test]
fn lists_one_level_directories_first_and_reads_text() {
    let (_temp, root, mut browser) = home();
    fs::create_dir(root.join("empty")).unwrap();
    fs::create_dir_all(root.join("b/deep")).unwrap();
    fs::write(root.join("b/deep/never.txt"), "not listed").unwrap();
    fs::write(root.join("a.txt"), "こんにちは").unwrap();
    fs::write(root.join(".hidden"), "shown").unwrap();
    let page = list(&mut browser, "", None).unwrap();
    let names: Vec<_> = page
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("b", BrowseEntryKind::Directory),
            ("empty", BrowseEntryKind::Directory),
            (".hidden", BrowseEntryKind::File),
            ("a.txt", BrowseEntryKind::File),
        ]
    );
    assert!(!page.capacity && page.next_cursor.is_none());
    assert!(
        list(&mut browser, "empty", None)
            .unwrap()
            .entries
            .is_empty()
    );
    let deep = list(&mut browser, "b", None).unwrap();
    assert_eq!(deep.entries[0].path, "b/deep");
    assert_eq!(
        read(&mut browser, &identity(&root, "a.txt")),
        BrowseContent::Ready {
            text: "こんにちは".into()
        }
    );
}

#[test]
fn pages_follow_only_their_own_live_snapshot() {
    let (_temp, root, mut browser) = home();
    fs::create_dir(root.join("many")).unwrap();
    fs::create_dir(root.join("other")).unwrap();
    for i in 0..300 {
        fs::write(root.join(format!("many/{i:03}")), "").unwrap();
    }
    let first = list(&mut browser, "many", None).unwrap();
    assert_eq!(first.entries.len(), MAX_PAGE_ENTRIES);
    let cursor = first.next_cursor.clone().unwrap();
    let wrong = list(&mut browser, "other", Some(cursor.clone())).unwrap_err();
    assert_eq!(wrong.code, ErrorCode::RevisionConflict);
    let second = list(&mut browser, "many", Some(cursor.clone())).unwrap();
    assert_eq!(second.snapshot_id, first.snapshot_id);
    assert_eq!(second.entries.len(), 44);
    assert_eq!(second.entries[0].name, "256");
    assert!(second.next_cursor.is_none());

    let again = list(&mut browser, "many", None).unwrap();
    fs::write(root.join("many/new"), "").unwrap();
    let changed = list(&mut browser, "many", again.next_cursor).unwrap_err();
    assert_eq!(changed.code, ErrorCode::RevisionConflict);

    let fresh = list(&mut browser, "many", None).unwrap();
    browser.ttl = Duration::ZERO;
    let expired = list(&mut browser, "many", fresh.next_cursor).unwrap_err();
    assert_eq!(expired.code, ErrorCode::RevisionConflict);
}

#[test]
fn a_listing_cut_at_the_entry_limit_is_never_complete() {
    let (_temp, root, mut browser) = home();
    for name in ["a", "b", "c", "d"] {
        fs::write(root.join(name), "").unwrap();
    }
    browser.entry_limit = 3;
    let page = list(&mut browser, "", None).unwrap();
    assert!(page.capacity);
    assert_eq!(page.entries.len(), 3);
}

#[test]
fn odd_names_links_and_protected_entries_are_never_followed() {
    let (_temp, root, mut browser) = home();
    // APFS refuses non-UTF-8 names, so a control character stands in.
    fs::write(root.join("line\nbreak"), "").unwrap();
    fs::write(root.join("target.txt"), "linked").unwrap();
    std::os::unix::fs::symlink(root.join("target.txt"), root.join("link")).unwrap();
    fs::write(root.join("original"), "two names").unwrap();
    fs::hard_link(root.join("original"), root.join("alias")).unwrap();
    let fifo = CString::new(root.join("pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    fs::write(root.join(".env"), "synthetic secret").unwrap();
    fs::create_dir_all(root.join(".polaris")).unwrap();
    fs::write(root.join(".polaris/auth.json"), "{}").unwrap();
    polaris_auth::protection::protect_path(&root.join(".polaris/auth.json")).unwrap();

    let page = list(&mut browser, "", None).unwrap();
    let names: Vec<_> = page
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert_eq!(page.omitted.get(), 1);
    assert!(names.contains(&("link", BrowseEntryKind::Symlink)));
    assert!(names.contains(&("pipe", BrowseEntryKind::Unsupported)));
    assert!(names.contains(&("target.txt", BrowseEntryKind::File)));
    for hidden in ["original", "alias", ".env"] {
        assert!(!names.iter().any(|(n, _)| *n == hidden), "{hidden}");
    }
    let polaris = list(&mut browser, ".polaris", None).unwrap();
    assert!(polaris.entries.is_empty());

    assert_eq!(
        read(&mut browser, &identity(&root, "link")),
        unavailable(BrowseUnavailable::NotRegularFile)
    );
    assert_eq!(
        read(&mut browser, &identity(&root, "pipe")),
        unavailable(BrowseUnavailable::NotRegularFile)
    );
    assert_eq!(
        read(&mut browser, &identity(&root, "alias")),
        unavailable(BrowseUnavailable::Protected)
    );
    assert_eq!(
        read(&mut browser, &identity(&root, ".env")),
        unavailable(BrowseUnavailable::Protected)
    );
    assert_eq!(
        read(&mut browser, &identity(&root, ".polaris/auth.json")),
        unavailable(BrowseUnavailable::Protected)
    );
}

#[test]
fn unreadable_changed_or_unsuitable_files_are_reported_as_such() {
    let (_temp, root, mut browser) = home();
    fs::write(root.join("binary"), [0u8, 1, 2]).unwrap();
    fs::write(root.join("large"), vec![b'x'; MAX_BODY_BYTES + 1]).unwrap();
    fs::write(root.join("gone"), "x").unwrap();
    fs::write(root.join("swapped"), "old").unwrap();
    fs::write(root.join("locked"), "x").unwrap();
    let gone = identity(&root, "gone");
    let swapped = identity(&root, "swapped");
    fs::remove_file(root.join("gone")).unwrap();
    fs::remove_file(root.join("swapped")).unwrap();
    fs::write(root.join("swapped"), "new").unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

    assert_eq!(
        read(&mut browser, &identity(&root, "binary")),
        unavailable(BrowseUnavailable::Binary)
    );
    assert_eq!(
        read(&mut browser, &identity(&root, "large")),
        unavailable(BrowseUnavailable::TooLarge)
    );
    assert_eq!(
        read(&mut browser, &gone),
        unavailable(BrowseUnavailable::Deleted)
    );
    assert_eq!(
        read(&mut browser, &swapped),
        unavailable(BrowseUnavailable::Replaced)
    );
    assert_eq!(
        read(&mut browser, &identity(&root, "locked")),
        unavailable(BrowseUnavailable::PermissionDenied)
    );
}

#[test]
fn an_overwrite_during_the_read_is_a_conflict_but_one_before_it_is_current() {
    let (_temp, root, mut browser) = home();
    fs::write(root.join("note"), "aaaa").unwrap();
    let request = identity(&root, "note");
    fs::write(root.join("note"), "bbbb").unwrap();
    assert_eq!(
        read(&mut browser, &request),
        BrowseContent::Ready {
            text: "bbbb".into()
        }
    );
    let path = root.join("note");
    browser.before_final_check = Some(Box::new(move || {
        std::thread::sleep(Duration::from_millis(5));
        fs::write(&path, "cccc").unwrap();
    }));
    assert_eq!(
        read(&mut browser, &request),
        unavailable(BrowseUnavailable::RevisionConflict)
    );
}

#[test]
fn a_replaced_home_stops_every_later_read() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().canonicalize().unwrap().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    let mut browser = HomeBrowser::open(root.clone()).unwrap();
    let request = identity(&root, "a.txt");
    fs::rename(&root, root.with_file_name("moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    assert_eq!(
        list(&mut browser, "", None).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    fs::remove_dir_all(&root).unwrap();
    fs::rename(root.with_file_name("moved"), &root).unwrap();
    assert_eq!(
        browser.read(&request).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
}

fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: ProtocolVersion,
        client_id: ClientId::new("client").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}

#[tokio::test]
async fn the_helper_serves_only_browsing_and_shutdown() {
    let (_temp, root, browser) = home();
    fs::write(root.join("a.txt"), "a").unwrap();
    let (mut tx, reader) = pipe::pipe().unwrap();
    let (writer, mut rx) = pipe::pipe().unwrap();
    let task = tokio::spawn(serve(browser, reader, writer));
    let mut call = async |request: Request| {
        tx.write_all(&codec::encode(&request).unwrap())
            .await
            .unwrap();
        let response: Response = crate::transport::read_frame(&mut rx)
            .await
            .unwrap()
            .unwrap();
        response.validate_for(&request).unwrap();
        response.outcome
    };
    let early = call(request(
        "0",
        RequestBody::BrowseList(BrowseList {
            path: String::new(),
            cursor: None,
        }),
    ))
    .await;
    assert_eq!(early.unwrap_err().code, ErrorCode::InvalidRequest);
    let Ok(SuccessResult::Hello(hello)) = call(request("1", RequestBody::Hello)).await else {
        panic!("hello");
    };
    assert_eq!(
        hello.capabilities,
        [Capability::HomeBrowse, Capability::Shutdown]
    );
    let refused = call(request(
        "2",
        RequestBody::SessionSnapshot(polaris_desktop_protocol::ids::SessionId::new("s").unwrap()),
    ))
    .await;
    assert_eq!(refused.unwrap_err().code, ErrorCode::CapabilityUnavailable);
    let Ok(SuccessResult::BrowseList(page)) = call(request(
        "3",
        RequestBody::BrowseList(BrowseList {
            path: String::new(),
            cursor: None,
        }),
    ))
    .await
    else {
        panic!("list");
    };
    assert_eq!(page.entries[0].name, "a.txt");
    let Ok(SuccessResult::ShutdownRequest(done)) = call(request(
        "4",
        RequestBody::ShutdownRequest(ShutdownRequest {
            engine_epoch: hello.engine_epoch.clone(),
        }),
    ))
    .await
    else {
        panic!("shutdown");
    };
    assert_eq!(done.state, ShutdownState::Ready);
    assert_eq!(task.await.unwrap().unwrap(), Exit::Ready);
}

/// M5-2e: with the account home trusted, the helper hides the accounts
/// subtree and refuses a journaled credential inode even when its last link
/// is outside it. Runs in a child: the trusted home is process-wide.
#[test]
fn account_credentials_stay_unreadable_through_the_trusted_home_helper() {
    use std::os::unix::fs::MetadataExt;
    if let Some(home) = std::env::var_os("POLARIS_BROWSE_ACCOUNTS_HOME") {
        let home = PathBuf::from(home);
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let store = polaris_auth::accounts::store::AccountStore::open().unwrap();
        let reservation = store.begin_registration(Some("synthetic".into())).unwrap();
        let escaped = home.join("escaped-registry");
        fs::hard_link(home.join(".polaris/accounts/registry.json"), &escaped).unwrap();
        // The writer refuses a hardlinked registry instead of rotating it.
        assert!(
            store
                .set_label(&reservation.registration_id, reservation.generation, None)
                .is_err()
        );
        // Leave the journaled inode reachable only under the escaped name.
        fs::remove_file(home.join(".polaris/accounts/registry.json")).unwrap();
        assert_eq!(fs::metadata(&escaped).unwrap().nlink(), 1);

        let mut browser = HomeBrowser::open(home.clone()).unwrap();
        let top = list(&mut browser, "", None).unwrap();
        assert!(!top.entries.iter().any(|e| e.name == "escaped-registry"));
        let polaris = list(&mut browser, ".polaris", None).unwrap();
        assert!(!polaris.entries.iter().any(|e| e.name == "accounts"));
        assert!(list(&mut browser, ".polaris/accounts", None).is_err());
        assert_eq!(
            read(&mut browser, &identity(&home, "escaped-registry")),
            unavailable(BrowseUnavailable::Protected)
        );
        println!("accounts-protected");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let home = temp.path().canonicalize().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "home_browse::tests::account_credentials_stay_unreadable_through_the_trusted_home_helper",
            "--nocapture",
        ])
        .env("POLARIS_BROWSE_ACCOUNTS_HOME", &home)
        .env_remove("POLARIS_AUTH_READ_ONLY")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("accounts-protected"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
