use super::*;

const GIT_FIXTURE: &[u8] = b"retained\0\xffgit-fixture";

fn identity(path: PathBuf) -> ExecutableIdentityV1 {
    // Structural test data only; this is not a native executable identity.
    let identity = ExecutableIdentityV1 {
        path,
        device: 1,
        inode: 2,
        owner_uid: 0,
        mode: 0o755,
        sha256: SyncDigestV1::digest(GIT_FIXTURE).as_hex(),
    };
    identity.validate_structure().unwrap();
    identity
}

fn assert_unavailable(project: &Path, state: &Path, git: &ExecutableIdentityV1) {
    let manifest = ShareManifest::default();
    assert!(matches!(
        C10PayloadSnapshotV1::capture(project, &manifest),
        Err(C10RepositoryErrorV1::Unavailable)
    ));
    assert!(matches!(
        C10LocalRepositoryV1::inspect(state, git),
        Err(C10RepositoryErrorV1::Unavailable)
    ));
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_c10_repository_unavailable_does_not_create_roots() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("absent-project");
    let state = root.path().join("absent-state");
    let git_path = root.path().join("absent-git.exe");
    let before = names(root.path());
    let git = identity(git_path.clone());
    assert_unavailable(&project, &state, &git);
    assert!(!project.exists());
    assert!(!state.exists());
    assert!(!git_path.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_c10_repository_unavailable_preserves_existing_bytes_and_names() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let source = project.join("src");
    let state = root.path().join("state");
    let sync = state.join("sync");
    let repository = sync.join("repository");
    let objects = repository.join("objects");
    let refs = repository.join("refs");
    let heads = refs.join("heads");
    let staging = sync.join("staging");
    let nested = staging.join("nested");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&objects).unwrap();
    fs::create_dir_all(&heads).unwrap();
    fs::create_dir_all(&nested).unwrap();
    let git_path = root.path().join("git-fixture.exe");
    let files = [
        source.join("retained.bin"),
        repository.join("config"),
        repository.join("HEAD"),
        sync.join("index"),
        nested.join("retained.bin"),
        git_path.clone(),
    ];
    let contents: [&[u8]; 6] = [
        b"retained\0\xffpayload",
        b"[core]\nrepositoryformatversion=0\nfilemode=true\nbare=false\nlogallrefupdates=true\n",
        b"ref: refs/heads/polaris\n",
        b"retained\0\xffindex",
        b"retained\0\xffstaging",
        GIT_FIXTURE,
    ];
    for (path, bytes) in files.iter().zip(contents) {
        fs::write(path, bytes).unwrap();
    }
    let directories = [
        root.path(),
        &project,
        &source,
        &state,
        &sync,
        &repository,
        &objects,
        &refs,
        &heads,
        &staging,
        &nested,
    ];
    let before: Vec<_> = directories.iter().map(|path| names(path)).collect();
    let git = identity(git_path);
    assert_unavailable(&project, &state, &git);
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    let after: Vec<_> = directories.iter().map(|path| names(path)).collect();
    assert_eq!(after, before);
}
