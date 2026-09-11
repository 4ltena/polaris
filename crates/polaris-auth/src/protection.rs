//! Process-wide metadata-only protection for authentication stores.
//!
//! This coordinates cooperating loaders and workspace snapshots, not arbitrary
//! external filesystem writers. Entries are never evicted during the process.

use std::collections::BTreeSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::AuthError;

const MAX_PATHS: usize = 4096;
const MAX_IDENTITIES: usize = 65_536;
const MAX_PATH_BYTES: usize = 16_384;
const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Read at most one byte beyond the bound, including files that grow after stat.
pub(crate) fn read_body(file: File) -> Result<Vec<u8>, AuthError> {
    let metadata = file.metadata()?;
    require_regular(&metadata)?;
    if metadata.len() > MAX_BODY_BYTES {
        return Err(AuthError::Protection);
    }
    let mut body = Vec::new();
    file.take(MAX_BODY_BYTES + 1).read_to_end(&mut body)?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(AuthError::Protection);
    }
    Ok(body)
}

fn require_regular(metadata: &Metadata) -> Result<(), AuthError> {
    if !metadata.is_file() {
        return Err(AuthError::Protection);
    }
    Ok(())
}

/// Check before open, and again on the descriptor: a replaced FIFO must neither
/// block open nor be used as a body source/sink. Never truncate before fstat.
pub(crate) fn open_regular(path: &Path, write: bool) -> Result<File, AuthError> {
    match std::fs::metadata(path) {
        Ok(metadata) => require_regular(&metadata)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AuthError::Io(e)),
    }
    open_regular_checked(path, write)
}

fn open_regular_checked(path: &Path, write: bool) -> Result<File, AuthError> {
    let file = OpenOptions::new()
        .read(!write)
        .write(write)
        .create(write)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    require_regular(&file.metadata()?)?;
    Ok(file)
}

/// Resolve only existing ancestors; unresolved `..` is rejected, never folded
/// lexically across a missing directory or symlink.
fn resolve_future_path(path: &Path) -> Result<PathBuf, AuthError> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for name in suffix.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(Component::Normal(name)) = ancestor.components().next_back() else {
                    return Err(AuthError::Protection);
                };
                suffix.push(name);
                ancestor = ancestor.parent().ok_or(AuthError::Protection)?;
            }
            Err(_) => return Err(AuthError::Protection),
        }
    }
}

fn valid_registered_path(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= MAX_PATH_BYTES
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

struct PreparedPath {
    operation: PathBuf,
    names: Vec<PathBuf>,
    metadata: Option<Metadata>,
}

impl PreparedPath {
    fn new(path: &Path) -> Result<Self, AuthError> {
        let absolute = std::path::absolute(path).map_err(|_| AuthError::Protection)?;
        if absolute.as_os_str().len() > MAX_PATH_BYTES
            || absolute.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(AuthError::Protection);
        }
        let Some(Component::Normal(name)) = absolute.components().next_back() else {
            return Err(AuthError::Protection);
        };
        // Preserve rename/unlink semantics for the final symlink itself.
        let operation =
            resolve_future_path(absolute.parent().ok_or(AuthError::Protection)?)?.join(name);
        let mut names = vec![operation.clone(), resolve_future_path(&operation)?];
        if valid_registered_path(&absolute) {
            names.push(absolute);
        }
        if !names.iter().all(|path| valid_registered_path(path)) {
            return Err(AuthError::Protection);
        }
        let metadata = match std::fs::metadata(&operation) {
            Ok(metadata) => {
                require_regular(&metadata)?;
                Some(metadata)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(AuthError::Protection),
        };
        Ok(Self {
            operation,
            names,
            metadata,
        })
    }
}

/// Filesystem identity, including identities of replaced or deleted stores.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProtectedIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Immutable metadata supplied while the process-wide auth lock is held.
#[derive(Clone, Default)]
pub struct ProtectedPathsSnapshot {
    paths: BTreeSet<PathBuf>,
    identities: BTreeSet<ProtectedIdentity>,
}

impl ProtectedPathsSnapshot {
    pub fn paths(&self) -> &BTreeSet<PathBuf> {
        &self.paths
    }

    pub fn identities(&self) -> &BTreeSet<ProtectedIdentity> {
        &self.identities
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    snapshot: ProtectedPathsSnapshot,
    failed: bool,
    trusted_defaults: Option<[PathBuf; 2]>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

pub(crate) fn lock() -> Result<MutexGuard<'static, Registry>, AuthError> {
    let guard = REGISTRY
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_| AuthError::Protection)?;
    if guard.failed {
        return Err(AuthError::Protection);
    }
    Ok(guard)
}

/// Install the desktop owner's OS-resolved home before any protected reads.
/// This is process-lifetime authority, never a model-selected path. Rebinding
/// is refused; existing protected names and inode identities are retained.
pub fn use_trusted_home(home: &Path) -> Result<(), AuthError> {
    if !home.is_absolute() || !valid_registered_path(home) {
        return Err(AuthError::Protection);
    }
    let defaults = [
        home.join(".polaris/auth.json"),
        home.join(".polaris/api_key.json"),
    ];
    let mut registry = lock()?;
    if let Some(existing) = &registry.trusted_defaults {
        return if existing == &defaults {
            Ok(())
        } else {
            Err(AuthError::Protection)
        };
    }
    for path in &defaults {
        registry.register_store(path)?;
    }
    registry.trusted_defaults = Some(defaults);
    Ok(())
}

/// Runs a synchronous callback under the same lock as save/load/delete.
/// Default stores and their temporary paths are registered from `default_path`
/// on every call, unless the native owner installed its trusted home. Only
/// filesystem metadata is inspected; no body is read.
///
/// The callback must not reenter this API or any auth storage operation, or
/// wait for work that needs the auth lock. Complete snapshot copying inside
/// the callback: using a cloned value afterwards does not retain the lock.
pub fn with_protected_paths<R>(
    callback: impl FnOnce(&ProtectedPathsSnapshot) -> R,
) -> Result<R, AuthError> {
    let mut registry = lock()?;
    let defaults = match &registry.trusted_defaults {
        Some(paths) => paths.clone(),
        None => [
            crate::store::default_path()?,
            crate::api_key::default_path()?,
        ],
    };
    for path in &defaults {
        registry.register_store(path)?;
    }
    // Refresh registered names as well, retaining old identities after rotation.
    let paths: Vec<_> = registry.snapshot.paths.iter().cloned().collect();
    for path in paths {
        registry.register_path(&path)?;
    }
    Ok(callback(&registry.snapshot))
}

impl Registry {
    pub(crate) fn register_store(&mut self, path: &Path) -> Result<PathBuf, AuthError> {
        // Validate both names before mutating the persistent registry.
        let store = PreparedPath::new(path)?;
        let tmp = PreparedPath::new(&path.with_extension("json.tmp"))?;
        let operation = store.operation.clone();
        self.register_prepared(store)?;
        self.register_prepared(tmp)?;
        Ok(operation)
    }

    fn insert_path(&mut self, path: PathBuf) -> Result<(), AuthError> {
        if self.failed
            || path.as_os_str().len() > MAX_PATH_BYTES
            || (!self.snapshot.paths.contains(&path) && self.snapshot.paths.len() >= MAX_PATHS)
        {
            self.failed = true;
            return Err(AuthError::Protection);
        }
        self.snapshot.paths.insert(path);
        Ok(())
    }

    fn register_path(&mut self, path: &Path) -> Result<(), AuthError> {
        self.register_prepared(PreparedPath::new(path)?)
    }

    fn register_prepared(&mut self, path: PreparedPath) -> Result<(), AuthError> {
        for name in path.names {
            self.insert_path(name)?;
        }
        if let Some(metadata) = path.metadata {
            self.remember(&metadata)?;
        }
        Ok(())
    }

    pub(crate) fn remember(&mut self, metadata: &Metadata) -> Result<(), AuthError> {
        let identity = ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        if self.failed
            || (!self.snapshot.identities.contains(&identity)
                && self.snapshot.identities.len() >= MAX_IDENTITIES)
        {
            self.failed = true;
            return Err(AuthError::Protection);
        }
        self.snapshot.identities.insert(identity);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Credentials, api_key, store};
    use std::os::unix::fs::FileTypeExt;

    fn credentials() -> Credentials {
        Credentials {
            access_token: "dummy-access".into(),
            refresh_token: "dummy-refresh".into(),
            account_id: "dummy-account".into(),
            expires_at: None,
        }
    }

    fn identity(path: &Path) -> ProtectedIdentity {
        let metadata = std::fs::metadata(path).unwrap();
        ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    #[test]
    fn overrides_reserve_missing_store_and_tmp_before_first_save() {
        let dir = tempfile::tempdir().unwrap();
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            if api {
                assert!(api_key::load_from(&path).unwrap().is_none());
            } else {
                assert!(store::load_from(&path).unwrap().is_none());
            }
            with_protected_paths(|snapshot| {
                assert!(snapshot.paths().contains(&path));
                assert!(snapshot.paths().contains(&path.with_extension("json.tmp")));
                assert!(snapshot.paths().iter().all(|p| p.is_absolute()));
            })
            .unwrap();
            assert!(!path.exists());
        }
    }

    #[test]
    fn defaults_are_reserved_without_loading_a_body() {
        // The test process HOME is isolated by the test runner. No environment
        // mutation is needed inside this parallel test suite.
        with_protected_paths(|snapshot| {
            for path in [
                store::default_path().unwrap(),
                api_key::default_path().unwrap(),
            ] {
                assert!(
                    snapshot
                        .paths()
                        .contains(&std::path::absolute(&path).unwrap())
                );
                assert!(
                    snapshot
                        .paths()
                        .contains(&std::path::absolute(path.with_extension("json.tmp")).unwrap())
                );
            }
        })
        .unwrap();
    }

    #[test]
    fn preexisting_old_and_temporary_identities_survive_rotation_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            let tmp = path.with_extension("json.tmp");
            // These inodes existed before any loader registered them.
            std::fs::write(&path, b"dummy-old").unwrap();
            std::fs::hard_link(
                &path,
                dir.path().join(if api { "key-link" } else { "oauth-link" }),
            )
            .unwrap();
            std::fs::write(&tmp, b"dummy-interrupted").unwrap();
            let old = identity(&path);
            let temporary = identity(&tmp);
            if api {
                api_key::save_to(&path, "dummy-key").unwrap();
            } else {
                store::save_to(&path, &credentials()).unwrap();
            }
            let first = identity(&path);
            assert_eq!(first, temporary);
            if api {
                assert_eq!(
                    api_key::load_from(&path).unwrap().as_deref(),
                    Some("dummy-key")
                );
                api_key::save_to(&path, "dummy-key-2").unwrap();
            } else {
                assert_eq!(store::load_from(&path).unwrap(), Some(credentials()));
                store::save_to(&path, &credentials()).unwrap();
            }
            let second = identity(&path);
            assert_ne!(first, second);
            assert!(store::delete_at(&path).unwrap());
            with_protected_paths(|snapshot| {
                for id in [old, temporary, first, second] {
                    assert!(snapshot.identities().contains(&id));
                }
                assert!(snapshot.paths().contains(&path));
            })
            .unwrap();
        }
    }

    #[test]
    fn load_and_delete_register_preexisting_inodes() {
        let dir = tempfile::tempdir().unwrap();
        let oauth = dir.path().join("oauth");
        let key = dir.path().join("key");
        let deleted = dir.path().join("deleted");
        std::fs::write(&oauth, serde_json::to_vec(&credentials()).unwrap()).unwrap();
        std::fs::write(&key, br#"{"key":"dummy-key"}"#).unwrap();
        std::fs::write(&deleted, b"dummy-delete").unwrap();
        let ids = [identity(&oauth), identity(&key), identity(&deleted)];
        assert_eq!(store::load_from(&oauth).unwrap(), Some(credentials()));
        assert_eq!(
            api_key::load_from(&key).unwrap().as_deref(),
            Some("dummy-key")
        );
        assert!(store::delete_at(&deleted).unwrap());
        with_protected_paths(|snapshot| {
            assert!(ids.iter().all(|id| snapshot.identities().contains(id)));
        })
        .unwrap();
    }

    #[test]
    fn reserves_canonical_missing_paths_below_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let path = alias.join("future").join("auth");
        assert!(store::load_from(&path).unwrap().is_none());
        let resolved = real.canonicalize().unwrap().join("future").join("auth");
        with_protected_paths(|snapshot| {
            assert!(snapshot.paths().contains(&path));
            assert!(snapshot.paths().contains(&resolved));
            assert!(
                snapshot
                    .paths()
                    .contains(&resolved.with_extension("json.tmp"))
            );
        })
        .unwrap();
    }

    #[test]
    fn decode_errors_never_retain_input_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid");
        for body in [
            br#"{"key":true,"access_token":"DUMMY_SECRET","refresh_token":"r","account_id":"a","expires_at":"DUMMY_SECRET"}"#.as_slice(),
            br#""DUMMY_SECRET""#.as_slice(),
        ] {
            std::fs::write(&path, body).unwrap();
            for error in [store::load_from(&path).unwrap_err(), api_key::load_from(&path).unwrap_err()] {
                assert!(!format!("{error:?} {error}").contains("DUMMY_SECRET"));
            }
        }
    }

    #[test]
    fn bounded_registry_never_evicts_and_stays_failed_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        for i in 0..MAX_PATHS {
            registry
                .insert_path(dir.path().join(i.to_string()))
                .unwrap();
        }
        let existing = dir.path().join("0");
        registry.insert_path(existing.clone()).unwrap();
        assert!(registry.insert_path(dir.path().join("overflow")).is_err());
        assert!(registry.insert_path(existing).is_err());
        assert_eq!(registry.snapshot.paths.len(), MAX_PATHS);

        let mut registry = Registry::default();
        let file = dir.path().join("metadata");
        std::fs::write(&file, b"dummy").unwrap();
        let metadata = std::fs::metadata(&file).unwrap();
        for ino in 0..MAX_IDENTITIES as u64 {
            registry.snapshot.identities.insert(ProtectedIdentity {
                dev: metadata.dev().wrapping_add(1),
                ino,
            });
        }
        assert!(registry.remember(&metadata).is_err());
        assert!(registry.register_store(&file).is_err());
        assert_eq!(registry.snapshot.identities.len(), MAX_IDENTITIES);
        assert_eq!(std::fs::read(&file).unwrap(), b"dummy");
    }

    #[test]
    fn callback_holds_lock_against_both_loaders_and_delete() {
        use std::sync::mpsc;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth");
        store::save_to(&path, &credentials()).unwrap();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            with_protected_paths(|_| {
                assert!(matches!(
                    REGISTRY.get().unwrap().try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                for operation in 0..5 {
                    let started = started_tx.clone();
                    let done = done_tx.clone();
                    let path = path.clone();
                    let key = dir.path().join("key");
                    scope.spawn(move || {
                        started.send(()).unwrap();
                        match operation {
                            0 => {
                                store::save_to(&path, &credentials()).unwrap();
                            }
                            1 => {
                                store::load_from(&path).unwrap();
                            }
                            2 => {
                                store::delete_at(&path).unwrap();
                            }
                            3 => {
                                api_key::save_to(&key, "dummy-key").unwrap();
                            }
                            _ => {
                                api_key::load_from(&key).unwrap();
                            }
                        }
                        done.send(()).unwrap();
                    });
                }
                for _ in 0..5 {
                    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
            })
            .unwrap();
            for _ in 0..5 {
                done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        });
    }

    #[test]
    fn parent_components_resolve_existing_ancestors_without_poisoning_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        for api in [false, true] {
            let name = if api { "key" } else { "oauth" };
            let path = dir.path().join("config").join("..").join(name);
            let actual = dir.path().join(name);
            if api {
                api_key::save_to(&path, "dummy-key").unwrap();
                assert_eq!(
                    api_key::load_from(&path).unwrap().as_deref(),
                    Some("dummy-key")
                );
                assert_eq!(
                    api_key::load_from(&actual).unwrap().as_deref(),
                    Some("dummy-key")
                );
            } else {
                store::save_to(&path, &credentials()).unwrap();
                assert_eq!(store::load_from(&path).unwrap(), Some(credentials()));
                assert_eq!(store::load_from(&actual).unwrap(), Some(credentials()));
            }
            with_protected_paths(|snapshot| {
                assert!(snapshot.paths().iter().all(|p| valid_registered_path(p)));
                assert!(snapshot.paths().contains(&actual.canonicalize().unwrap()));
            })
            .unwrap();
            assert!(store::delete_at(&path).unwrap());
            assert!(!actual.exists());
        }
        // A subsequent ordinary snapshot must still succeed after deletion.
        with_protected_paths(|snapshot| {
            assert!(snapshot.paths().iter().all(|p| valid_registered_path(p)))
        })
        .unwrap();
    }

    #[test]
    fn parent_after_symlink_keeps_filesystem_meaning_and_final_link_rename() {
        let dir = tempfile::tempdir().unwrap();
        let actual = dir.path().join("actual");
        std::fs::create_dir_all(actual.join("config")).unwrap();
        std::os::unix::fs::symlink(actual.join("config"), dir.path().join("alias")).unwrap();
        let path = dir.path().join("alias/../key");
        api_key::save_to(&path, "dummy-real").unwrap();
        assert!(!dir.path().join("key").exists());
        assert_eq!(
            api_key::load_from(&actual.join("key")).unwrap().as_deref(),
            Some("dummy-real")
        );
        assert_eq!(
            api_key::load_from(&path).unwrap().as_deref(),
            Some("dummy-real")
        );

        let link = dir.path().join("last-link");
        std::os::unix::fs::symlink(actual.join("key"), &link).unwrap();
        api_key::save_to(&link, "dummy-new").unwrap();
        assert!(
            !std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            api_key::load_from(&actual.join("key")).unwrap().as_deref(),
            Some("dummy-real")
        );
        assert_eq!(
            api_key::load_from(&link).unwrap().as_deref(),
            Some("dummy-new")
        );
    }

    #[test]
    fn invalid_override_is_rejected_before_registration_or_io() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let invalid = [
            dir.path().join("missing/../key"),
            dir.path().join(std::ffi::OsStr::from_bytes(b"bad\0key")),
        ];
        let mut registry = Registry::default();
        for path in invalid {
            assert!(registry.register_store(&path).is_err());
            assert!(registry.snapshot.paths().is_empty());
            assert!(registry.snapshot.identities().is_empty());
            assert!(!registry.failed);
            assert!(store::load_from(&path).is_err());
            assert!(api_key::load_from(&path).is_err());
            assert!(store::save_to(&path, &credentials()).is_err());
            assert!(api_key::save_to(&path, "dummy").is_err());
            assert!(store::delete_at(&path).is_err());
        }
        assert!(!dir.path().join("missing").exists());
        assert!(!dir.path().join("key").exists());
        with_protected_paths(|snapshot| {
            assert!(snapshot.paths().iter().all(|p| valid_registered_path(p)))
        })
        .unwrap();
        let good = dir.path().join("good");
        api_key::save_to(&good, "dummy-good").unwrap();
        assert_eq!(
            api_key::load_from(&good).unwrap().as_deref(),
            Some("dummy-good")
        );
    }

    fn fifo(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a live NUL-terminated pathname in the test's TempDir.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn fifo_loads_and_tmp_saves_fail_without_blocking_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let pipe = dir.path().join("pipe");
        fifo(&pipe);
        for error in [
            store::load_from(&pipe).unwrap_err(),
            api_key::load_from(&pipe).unwrap_err(),
        ] {
            assert!(matches!(error, AuthError::Protection));
        }
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            let tmp = path.with_extension("json.tmp");
            fifo(&tmp);
            let error = if api {
                api_key::save_to(&path, "dummy")
            } else {
                store::save_to(&path, &credentials())
            }
            .unwrap_err();
            assert!(matches!(error, AuthError::Protection));
            assert!(!path.exists());
            assert!(std::fs::metadata(&tmp).unwrap().file_type().is_fifo());
        }
        with_protected_paths(|_| ()).unwrap();
        api_key::save_to(&dir.path().join("good"), "dummy").unwrap();
    }

    #[test]
    fn descriptor_check_rejects_a_fifo_substituted_after_precheck() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race");
        std::fs::write(&path, b"dummy").unwrap();
        require_regular(&std::fs::metadata(&path).unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        fifo(&path);
        // Enter the post-precheck stage directly: neither open can wait for a
        // peer, and a successful open must still reject the descriptor type.
        assert!(matches!(
            open_regular_checked(&path, false),
            Err(AuthError::Protection)
        ));
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        assert!(matches!(
            open_regular_checked(&path, true),
            Err(AuthError::Protection)
        ));
    }

    #[test]
    fn body_limit_accepts_boundary_and_rejects_oversize_without_poisoning_lock() {
        let dir = tempfile::tempdir().unwrap();
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            let mut body = if api {
                br#"{"key":"dummy"}"#.to_vec()
            } else {
                serde_json::to_vec(&credentials()).unwrap()
            };
            body.resize(MAX_BODY_BYTES as usize, b' ');
            std::fs::write(&path, &body).unwrap();
            if api {
                assert_eq!(api_key::load_from(&path).unwrap().as_deref(), Some("dummy"));
            } else {
                assert_eq!(store::load_from(&path).unwrap(), Some(credentials()));
            }
            // One byte over the boundary, then a sparse huge input. Stat must
            // reject the latter before allocating its body.
            for length in [MAX_BODY_BYTES + 1, 1 << 32] {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(length)
                    .unwrap();
                let error = if api {
                    api_key::load_from(&path).map(|_| ())
                } else {
                    store::load_from(&path).map(|_| ())
                }
                .unwrap_err();
                assert!(matches!(error, AuthError::Protection));
                assert_eq!(
                    error.to_string(),
                    "authentication path protection is unavailable"
                );
                with_protected_paths(|_| ()).unwrap();
            }
        }
    }
}
