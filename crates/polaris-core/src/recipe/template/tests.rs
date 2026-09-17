//! Writes into a temporary project. Nothing is installed and no command is
//! run.
use super::*;

fn project() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary project")
}

#[test]
fn a_fresh_project_gets_every_template() {
    let dir = project();
    let (write, skip) = plan(dir.path(), NEXT_JS);
    assert_eq!(write.len(), NEXT_JS.len());
    assert!(skip.is_empty());

    let written = write_all(dir.path(), &write);
    assert_eq!(written.len(), NEXT_JS.len());
    for template in NEXT_JS {
        assert_eq!(
            std::fs::read_to_string(dir.path().join(template.path)).expect(template.path),
            template.body
        );
    }
}

fn write_all(root: &Path, templates: &[Template]) -> Vec<&'static str> {
    write(root, templates).expect("the templates are written")
}

#[test]
fn a_project_that_already_has_the_file_keeps_its_own() {
    let dir = project();
    let mine = "// 自分で書いた設定\nexport default { reactStrictMode: false };\n";
    std::fs::write(dir.path().join("next.config.mjs"), mine).expect("my file");

    let (write, skip) = plan(dir.path(), NEXT_JS);
    assert!(!write.iter().any(|held| held.path == "next.config.mjs"));
    assert_eq!(
        skip,
        vec![("next.config.mjs", Skipped::AlreadyPresent)],
        "the contents are never compared; the file is the project's"
    );
    write_all(dir.path(), &write);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("next.config.mjs")).expect("my file"),
        mine
    );
}

#[test]
fn a_name_taken_by_something_that_is_not_a_plain_file_is_left_alone() {
    let dir = project();
    std::fs::create_dir(dir.path().join("next.config.mjs")).expect("a directory");
    let elsewhere = project();
    let target = elsewhere.path().join("elsewhere.tsx");
    std::fs::write(&target, "// 別の場所").expect("a file");
    std::fs::create_dir(dir.path().join("app")).expect("the app directory");
    std::os::unix::fs::symlink(&target, dir.path().join("app/page.tsx")).expect("a symlink");

    let (write, skip) = plan(dir.path(), NEXT_JS);
    assert!(!write.iter().any(|held| held.path == "next.config.mjs"));
    assert!(!write.iter().any(|held| held.path == "app/page.tsx"));
    assert!(
        skip.contains(&("next.config.mjs", Skipped::NotAPlainFile)),
        "{skip:?}"
    );
    assert!(
        skip.contains(&("app/page.tsx", Skipped::NotAPlainFile)),
        "{skip:?}"
    );
    write_all(dir.path(), &write);
    // Writing through the link would have landed outside the project.
    assert_eq!(
        std::fs::read_to_string(&target).expect("the other file"),
        "// 別の場所"
    );
}

#[test]
fn a_file_that_appeared_after_the_plan_is_not_overwritten() {
    let dir = project();
    let (planned, _) = plan(dir.path(), NEXT_JS);
    // Someone writes it between the plan and the write.
    let mine = "// 先に置いた\n";
    std::fs::write(dir.path().join("next.config.mjs"), mine).expect("my file");

    let written = write_all(dir.path(), &planned);
    assert!(!written.contains(&"next.config.mjs"), "{written:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("next.config.mjs")).expect("my file"),
        mine
    );
    // The rest still went in.
    assert!(dir.path().join("app/page.tsx").is_file());
}

#[test]
fn a_path_that_cannot_be_read_is_not_treated_as_absent() {
    use std::os::unix::fs::PermissionsExt;
    let dir = project();
    let app = dir.path().join("app");
    std::fs::create_dir(&app).expect("the app directory");
    // Without execute permission the entries inside cannot be looked at, so
    // "is there a layout.tsx?" has no answer. Writing one anyway would be
    // acting on an assumption.
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o000)).expect("locked");
    let readable = std::fs::symlink_metadata(app.join("layout.tsx"))
        .err()
        .map(|error| error.kind() == std::io::ErrorKind::NotFound)
        .unwrap_or(true);
    if !readable {
        let (write, skip) = plan(dir.path(), NEXT_JS);
        assert!(
            !write.iter().any(|held| held.path == "app/layout.tsx"),
            "an unreadable path was planned as a write"
        );
        assert!(
            skip.contains(&("app/layout.tsx", Skipped::NotAPlainFile)),
            "{skip:?}"
        );
    }
    // Restore so the directory can be removed.
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o700)).expect("unlocked");
}

#[test]
fn the_directories_a_template_needs_are_made() {
    let dir = project();
    assert!(!dir.path().join("app").exists());
    let (planned, _) = plan(dir.path(), NEXT_JS);
    write_all(dir.path(), &planned);
    assert!(dir.path().join("app").is_dir());
    assert!(dir.path().join("app/layout.tsx").is_file());
}

#[test]
fn writing_twice_changes_nothing_the_second_time() {
    let dir = project();
    let (planned, _) = plan(dir.path(), NEXT_JS);
    write_all(dir.path(), &planned);
    let (again, skip) = plan(dir.path(), NEXT_JS);
    assert!(again.is_empty());
    assert_eq!(skip.len(), NEXT_JS.len());
    assert!(
        skip.iter().all(|(_, why)| *why == Skipped::AlreadyPresent),
        "{skip:?}"
    );
}
