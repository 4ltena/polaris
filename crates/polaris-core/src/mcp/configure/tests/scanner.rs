//! Unity-only hardlink exception and ordinary tree-policy rejection controls.

use super::super::source::{Protection, scan};
use super::*;
use polaris_tools::path_policy::tree_admission::{self, RegisteredDirectory};
use std::os::unix::fs::symlink;

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
