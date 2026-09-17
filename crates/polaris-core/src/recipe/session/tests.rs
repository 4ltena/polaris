//! Opens a real store under a temporary `POLARIS_DATA_DIR`. Nothing is
//! installed and no command is run.
use super::*;

/// `POLARIS_DATA_DIR` is process-wide, so these run one at a time and never
/// touch the real one under `HOME`.
use crate::project::DATA_DIR_SERIAL as SERIAL;

fn with_data_dir<T>(body: impl FnOnce(&Path) -> T) -> T {
    let held = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let data = tempfile::tempdir().expect("a temporary data directory");
    let previous = std::env::var_os("POLARIS_DATA_DIR");
    // SAFETY: the lock above keeps this the only thread changing it.
    unsafe { std::env::set_var("POLARIS_DATA_DIR", data.path()) };
    let out = body(data.path());
    unsafe {
        match previous {
            Some(value) => std::env::set_var("POLARIS_DATA_DIR", value),
            None => std::env::remove_var("POLARIS_DATA_DIR"),
        }
    }
    drop(held);
    out
}

fn project_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary project");
    std::fs::create_dir(dir.path().join(".git")).expect("a repository marker");
    dir
}

#[test]
fn the_same_project_reopens_the_same_session_from_any_subdirectory() {
    with_data_dir(|_| {
        let project = project_dir();
        let nested = project.path().join("packages/app");
        std::fs::create_dir_all(&nested).expect("the subdirectory");

        let (writer, id, session) = open(project.path()).expect("the session opens");
        drop(writer);
        let (writer, nested_id, nested_session) = open(&nested).expect("the session opens");
        drop(writer);
        assert_eq!(
            id, nested_id,
            "the launch directory must not split a project"
        );
        assert_eq!(session, nested_session);
        assert!(session.as_str().starts_with("setting-"));
    });
}

#[test]
fn a_second_writer_on_the_same_project_is_refused_rather_than_opened() {
    with_data_dir(|_| {
        let project = project_dir();
        let (held, ..) = open(project.path()).expect("the session opens");
        // A second ledger for one project is how two runs would each think
        // they finished a step the other did not.
        let second = open(project.path());
        assert!(matches!(second, Err(StoreError::Busy)), "{second:?}");
        assert_eq!(
            refusal(&StoreError::Busy),
            "この project の設定は別の場所で実行中です。終わってから実行してください。"
        );
        drop(held);
        // Once the first one lets go, the project opens again.
        open(project.path()).expect("the session reopens");
    });
}

#[test]
fn two_projects_do_not_share_a_ledger() {
    with_data_dir(|_| {
        let one = project_dir();
        let two = project_dir();
        let (first, first_id, _) = open(one.path()).expect("the first opens");
        let (_second, second_id, _) = open(two.path()).expect("the second opens too");
        assert_ne!(first_id, second_id);
        // Both are open at once, so one project never blocks another.
        drop(first);
    });
}

#[test]
fn the_store_directory_is_created_for_this_user_alone() {
    with_data_dir(|data| {
        let project = project_dir();
        let (writer, ..) = open(project.path()).expect("the session opens");
        drop(writer);
        let store = data.join("desktop");
        assert!(store.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store)
                .expect("the store")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        }
    });
}
