//! Unity-only hardlink exception and ordinary tree-policy rejection controls.

use super::super::source::{Protection, scan};
use super::*;
use polaris_tools::path_policy::tree_admission::{self, RegisteredDirectory};
use std::os::unix::fs::symlink;

const FIXED_LAUNCHER_DIRECTORIES: [&str; 7] = [
    "node_modules/.bin",
    "node_modules/eslint-plugin-import/node_modules/.bin",
    "node_modules/babel-plugin-istanbul/node_modules/.bin",
    "node_modules/ts-jest/node_modules/.bin",
    "node_modules/@babel/core/node_modules/.bin",
    "node_modules/@babel/helper-compilation-targets/node_modules/.bin",
    "node_modules/@istanbuljs/load-nyc-config/node_modules/.bin",
];

fn private_directories(root: &Path, relative: &Path) {
    let mut current = root.to_owned();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            panic!("fixture directories must use normal relative components")
        };
        current.push(name);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("fixture directory: {error}"),
        }
        fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Broken launcher links prove that the selected exclusions are not followed.
/// Ordinary neighboring files must remain part of the bound/copied runtime.
fn fixed_launcher_fixture(fixture: &Fixture) -> Vec<(PathBuf, Vec<u8>)> {
    let mut modules = Vec::new();
    for (index, relative) in FIXED_LAUNCHER_DIRECTORIES.iter().enumerate() {
        let relative = Path::new(relative);
        private_directories(&fixture.source, relative);
        symlink(
            fixture.root.join("missing-launcher-target"),
            fixture.source.join(relative).join("synthetic-launcher"),
        )
        .unwrap();
        if index > 0 {
            let relative = relative.parent().unwrap().join("ordinary-module.js");
            let bytes = format!("module_{index}\n").into_bytes();
            fs::write(fixture.source.join(&relative), &bytes).unwrap();
            modules.push((relative, bytes));
        }
    }
    modules
}

#[test]
fn exact_nested_launcher_exclusions_are_disclosed_and_only_launchers_are_omitted() {
    if isolated(concat!(
        module_path!(),
        "::exact_nested_launcher_exclusions_are_disclosed_and_only_launchers_are_omitted"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let modules = fixed_launcher_fixture(&fixture);
    let source_before = snapshot(&fixture.source);
    let preparation = fixture.prepare();
    for (relative, _) in &modules {
        let entry = preparation
            .source
            .entries()
            .iter()
            .find(|entry| entry.relative == relative.as_os_str().as_encoded_bytes())
            .expect("normal dependency file must be inventoried beside a fixed launcher directory");
        let expected_hash = digest(&fixture.source.join(relative));
        assert_eq!(entry.sha256.as_deref(), Some(expected_hash.as_str()));
    }
    let preview = preparation.preview().clone();
    let mut expected = FIXED_LAUNCHER_DIRECTORIES.map(PathBuf::from).to_vec();
    expected.sort();
    assert_eq!(preview.fixed_exclusions, expected);
    assert_eq!(preview.selected_files, 5 + modules.len());
    let (lease, effects) = fixture.effects();
    let receipt = preparation
        .apply(&fixture.host, &effects, &lease, &fixture.project)
        .unwrap();
    for relative in FIXED_LAUNCHER_DIRECTORIES {
        assert!(
            fs::symlink_metadata(receipt.cwd.join(relative)).is_err(),
            "excluded launcher directory must not be copied: {relative}"
        );
    }
    for (relative, bytes) in modules {
        let copied = receipt.cwd.join(&relative);
        assert_eq!(fs::read(&copied).unwrap(), bytes);
        assert_eq!(digest(&copied), digest(&fixture.source.join(relative)));
        assert_eq!(fs::metadata(copied).unwrap().nlink(), 1);
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(receipt.cwd.join(".polaris-unity-runtime-v1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["fixed_exclusions"],
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(manifest["source_manifest_sha256"], preview.manifest_sha256);
    assert_eq!(snapshot(&fixture.source), source_before);
}

#[test]
fn ordinary_files_beside_fixed_launchers_remain_hash_bound_to_approval() {
    if isolated(concat!(
        module_path!(),
        "::ordinary_files_beside_fixed_launchers_remain_hash_bound_to_approval"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let modules = fixed_launcher_fixture(&fixture);
    let preparation = fixture.prepare();
    let destination = preparation.preview().new_cwd.clone();
    fs::write(fixture.source.join(&modules[0].0), b"changed\n").unwrap();
    let (lease, effects) = fixture.effects();
    assert!(
        preparation
            .apply(&fixture.host, &effects, &lease, &fixture.project)
            .is_err()
    );
    assert!(!destination.exists());
    assert!(effects.effects().unwrap().is_empty());
    assert_eq!(fixture.registry(), fixture.before);
}

#[test]
fn unlisted_nested_launcher_links_are_rejected_without_a_generic_bin_exclusion() {
    if isolated(concat!(
        module_path!(),
        "::unlisted_nested_launcher_links_are_rejected_without_a_generic_bin_exclusion"
    )) {
        return;
    }
    let mut fixtures = Vec::new();
    for relative in [
        "node_modules/unlisted-package/node_modules/.bin",
        "node_modules/eslint-plugin-import-extra/node_modules/.bin",
        "node_modules/ts-jest/node_modules/deeper/node_modules/.bin",
    ] {
        let fixture = Fixture::new();
        drop(fixture.prepare());
        let relative = Path::new(relative);
        private_directories(&fixture.source, relative);
        symlink(
            fixture.root.join("missing-launcher-target"),
            fixture.source.join(relative).join("synthetic-launcher"),
        )
        .unwrap();
        let before = snapshot(&fixture.root);
        assert!(
            UnityLocalV2Preparation::prepare(&fixture.host, SERVER, &fixture.source).is_err(),
            "unlisted launcher must remain subject to the normal link policy: {}",
            relative.display()
        );
        assert_eq!(snapshot(&fixture.root), before);
        fixtures.push(fixture);
    }
}

#[test]
fn unity_scanner_accepts_exact_pair_but_generic_tree_still_rejects_it() {
    if isolated(concat!(
        module_path!(),
        "::unity_scanner_accepts_exact_pair_but_generic_tree_still_rejects_it"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let (directory, _) = tree_admission::open_root_with_ancestors(&fixture.source).unwrap();
    let protection = Protection::capture(&fixture.source).unwrap();
    assert!(scan(&fixture.source, &directory, &protection).is_ok());
    assert!(RegisteredDirectory::open(&fixture.source, crate::mcp::local::LIMITS).is_err());
    assert_eq!(
        fs::metadata(fixture.source.join(PAIR[0])).unwrap().nlink(),
        2
    );
    // Same names and bytes with independent files pass the ordinary policy;
    // its refusal above must therefore come from the shared inode.
    let right = fixture.source.join(PAIR[1]);
    fs::remove_file(&right).unwrap();
    fs::copy(fixture.source.join(PAIR[0]), &right).unwrap();
    assert!(RegisteredDirectory::open(&fixture.source, crate::mcp::local::LIMITS).is_ok());
}

#[test]
fn unity_scanner_rejects_general_hardlinks_links_special_files_and_pair_variations() {
    if isolated(concat!(
        module_path!(),
        "::unity_scanner_rejects_general_hardlinks_links_special_files_and_pair_variations"
    )) {
        return;
    }
    let mut fixtures = Vec::new();
    for case in [
        "third-link",
        "wrong-path",
        "wrong-size",
        "independent-files",
        "symlink",
        "package-link",
        "secret-name",
        "repository",
        "writable",
        "oversized",
        "special",
    ] {
        let fixture = Fixture::new();
        let left = fixture.source.join(PAIR[0]);
        let right = fixture.source.join(PAIR[1]);
        match case {
            "third-link" => {
                fs::hard_link(&left, fixture.source.join("node_modules/third")).unwrap()
            }
            "wrong-path" => fs::rename(
                &right,
                fixture.source.join("node_modules/esbuild/bin/other"),
            )
            .unwrap(),
            "wrong-size" => fs::OpenOptions::new()
                .write(true)
                .open(&left)
                .unwrap()
                .set_len(9_817_265)
                .unwrap(),
            "independent-files" => {
                fs::remove_file(&right).unwrap();
                fs::copy(&left, &right).unwrap();
            }
            "symlink" => {
                fs::remove_file(&right).unwrap();
                symlink(&left, &right).unwrap();
            }
            "package-link" => fs::hard_link(
                fixture.source.join("package.json"),
                fixture.source.join("build/alias.json"),
            )
            .unwrap(),
            "secret-name" => fs::write(fixture.source.join("build/.env"), b"synthetic").unwrap(),
            "repository" => fs::create_dir(fixture.source.join("build/.git")).unwrap(),
            "writable" => fs::set_permissions(
                fixture.source.join("package.json"),
                fs::Permissions::from_mode(0o666),
            )
            .unwrap(),
            "oversized" => fs::File::create(fixture.source.join("build/too-large"))
                .unwrap()
                .set_len(32 * 1024 * 1024 + 1)
                .unwrap(),
            "special" => {
                use std::ffi::CString;
                use std::os::unix::ffi::OsStrExt;
                let path =
                    CString::new(fixture.source.join("build/pipe").as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let (directory, _) = tree_admission::open_root_with_ancestors(&fixture.source).unwrap();
        let protection = Protection::capture(&fixture.source).unwrap();
        assert!(
            scan(&fixture.source, &directory, &protection).is_err(),
            "case {case}"
        );
        fixtures.push(fixture);
    }
}
