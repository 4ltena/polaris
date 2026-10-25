//! Real product entry; all input and state live under disposable fixtures.
#![cfg(windows)]

use std::fs;

#[path = "support/product_binary.rs"]
mod product_binary;

#[path = "support/windows_fixture.rs"]
mod windows_fixture;
use windows_fixture::contents;

#[test]
fn windows_sync_apply_refuses_before_creating_or_changing_project_state() {
    for existing_state in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let data = root.path().join("data");
        let home = root.path().join("home");
        fs::create_dir_all(project.join(".polaris")).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(project.join("file.rs"), b"project before").unwrap();
        if existing_state {
            let state = data.join("state").join(polaris_core::project::project_id(
                &project.canonicalize().unwrap(),
            ));
            let paths = polaris_core::sync::share::SyncPaths::under(&state);
            fs::create_dir_all(&paths.work_tree).unwrap();
            fs::write(paths.work_tree.join("file.rs"), b"staging before").unwrap();
            fs::write(state.join("sync/participation.json"), b"saved fixture").unwrap();
        }
        let before = contents(root.path());
        let output = product_binary::command()
            .args(["sync", "apply"])
            .current_dir(&project)
            .env("POLARIS_DATA_DIR", &data)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .output()
            .expect("launch Windows product");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(
                "Applying shared content is unsupported on Windows. No changes were made."
            )
        );
        assert_eq!(contents(root.path()), before);
    }
}
