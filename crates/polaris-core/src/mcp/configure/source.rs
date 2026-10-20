//! Descriptor-bound Unity source manifests, executable pins and the exact esbuild pair exception.

use polaris_mcp::registration::{Destination, ProtocolProfile, Server};
use polaris_tools::path_policy::{self, tree_admission};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, OsStr, OsString},
    fs::{File, Metadata, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

const ENTRY: &str = "build/index.js";
const PACKAGE: &str = "package.json";
const LOCKS: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];
const FIXED_BINS: &[&str] = &[
    "node_modules/.bin",
    "node_modules/eslint-plugin-import/node_modules/.bin",
    "node_modules/babel-plugin-istanbul/node_modules/.bin",
    "node_modules/ts-jest/node_modules/.bin",
    "node_modules/@babel/core/node_modules/.bin",
    "node_modules/@babel/helper-compilation-targets/node_modules/.bin",
    "node_modules/@istanbuljs/load-nyc-config/node_modules/.bin",
];
const ESBUILD_BYTES: u64 = 9_817_266;
const PAIR: [&str; 2] = [
    "node_modules/esbuild/bin/esbuild",
    "node_modules/@esbuild/darwin-arm64/bin/esbuild",
];
const MAX_ENTRIES: usize = 100_000;
const MAX_FILES: usize = 50_000;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Stamp {
    pub(super) dev: u64,
    pub(super) ino: u64,
    pub(super) uid: u32,
    pub(super) mode: u32,
    pub(super) links: u64,
    pub(super) bytes: u64,
    pub(super) mtime: (i64, i64),
    pub(super) ctime: (i64, i64),
}

impl Stamp {
    pub(super) fn of(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            uid: metadata.uid(),
            mode: metadata.mode() & 0o7777,
            links: metadata.nlink(),
            bytes: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    pub(super) fn matches(&self, metadata: &Metadata) -> bool {
        *self == Self::of(metadata)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ManifestEntry {
    pub(super) relative: Vec<u8>,
    pub(super) directory: bool,
    pub(super) stamp: Stamp,
    pub(super) sha256: Option<String>,
}

struct BoundFile {
    path: PathBuf,
    file: File,
    stamp: Stamp,
    sha256: String,
}

impl BoundFile {
    fn verify(&self) -> Result<(), String> {
        let metadata = self
            .file
            .metadata()
            .map_err(|_| "保持したsource descriptorを確認できません")?;
        let named = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&self.path)
            .map_err(|_| "保持したsource pathを再確認できません")?;
        if Stamp::of(&metadata) != self.stamp
            || Stamp::of(
                &named
                    .metadata()
                    .map_err(|_| "source pathを確認できません")?,
            ) != self.stamp
            || hash_file(&self.file, metadata.len())? != self.sha256
        {
            return Err("確認中にUnity sourceが変更されました".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SourceManifest {
    root: Stamp,
    entries: Vec<ManifestEntry>,
    lockfiles: Vec<Vec<u8>>,
    fixed_exclusions: Vec<Vec<u8>>,
}

pub(super) struct Scan {
    manifest: SourceManifest,
    file_count: usize,
    total_bytes: u64,
}

pub(super) struct UnitySourceBinding {
    source: PathBuf,
    root: File,
    ancestors: Vec<(PathBuf, tree_admission::Identity)>,
    manifest: SourceManifest,
    manifest_sha256: String,
    held: Vec<BoundFile>,
    node: BoundFile,
    file_count: usize,
    total_bytes: u64,
}

impl UnitySourceBinding {
    pub(super) fn inspect(source: &Path, server: &Server) -> Result<Self, String> {
        if !source.is_absolute()
            || source
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
            || source
                .canonicalize()
                .map_err(|_| "Unity sourceを確認できません")?
                != source
        {
            return Err("Unity sourceはcanonicalな絶対directoryで指定してください".into());
        }
        let (root, ancestors) = tree_admission::open_root_with_ancestors(source)
            .map_err(|_| "Unity sourceをdescriptorで開けません")?;
        let root_metadata = root
            .metadata()
            .map_err(|_| "Unity sourceを確認できません")?;
        require_owned(&root_metadata, true)?;
        let protection = Protection::capture(source)?;
        protection.check(source, &root_metadata)?;
        let scan = scan(source, &root, &protection)?;
        let manifest_sha256 = manifest_hash(&scan.manifest)?;

        let entry_path = source.join(ENTRY);
        let (command, args, executable_sha256, script, script_sha256) = match &server.transport {
            Destination::Stdio {
                command,
                args,
                executable_sha256,
                script,
                script_sha256,
                ..
            } => (command, args, executable_sha256, script, script_sha256),
            _ => return Err("Unity presetは既存stdio登録だけを更新できます".into()),
        };
        if args.len() != 1 || Path::new(&args[0]) != entry_path {
            return Err("既存Unity登録のsource/entry bindingが一致しません".into());
        }
        if script.as_deref().is_some_and(|script| script != entry_path)
            || script.is_some() != script_sha256.is_some()
        {
            return Err("既存Unity登録のentry pinが一致しません".into());
        }
        let node_expected = executable_sha256
            .as_deref()
            .ok_or("既存Unity登録にnode digestがありません")?;
        let node = inspect_external(command, true, &protection)?;
        if node.sha256 != node_expected {
            return Err("既存Unity登録のnode digestが一致しません".into());
        }
        let entry_digest = entry_digest(&scan.manifest)?;
        if script_sha256
            .as_deref()
            .is_some_and(|entry_expected| entry_digest != entry_expected)
        {
            return Err("既存Unity登録のentry digestが一致しません".into());
        }

        let mut retained = vec![PathBuf::from(PACKAGE), PathBuf::from(ENTRY)];
        retained.extend(
            scan.manifest
                .lockfiles
                .iter()
                .map(|path| PathBuf::from(OsStr::from_bytes(path))),
        );
        retained.extend(PAIR.map(PathBuf::from));
        let held = retained
            .into_iter()
            .map(|relative| bind_relative(&root, source, &relative, &scan.manifest))
            .collect::<Result<Vec<_>, _>>()?;
        let value = Self {
            source: source.to_owned(),
            root,
            ancestors,
            manifest: scan.manifest,
            manifest_sha256,
            held,
            node,
            file_count: scan.file_count,
            total_bytes: scan.total_bytes,
        };
        value.revalidate()?;
        Ok(value)
    }

    pub(super) fn source(&self) -> &Path {
        &self.source
    }

    pub(super) fn node_sha256(&self) -> &str {
        &self.node.sha256
    }

    pub(super) fn entry_sha256(&self) -> &str {
        entry_digest(&self.manifest).expect("validated entry")
    }

    pub(super) fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    pub(super) fn file_count(&self) -> usize {
        self.file_count
    }

    pub(super) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub(super) fn fixed_exclusion_paths(&self) -> Vec<PathBuf> {
        self.manifest
            .fixed_exclusions
            .iter()
            .map(|value| PathBuf::from(OsStr::from_bytes(value)))
            .collect()
    }

    pub(super) fn hardlink_pair(&self) -> [PathBuf; 2] {
        PAIR.map(PathBuf::from)
    }

    pub(super) fn hardlink_pair_sha256(&self) -> &str {
        hardlink_pair_digest(&self.manifest).expect("validated esbuild pair")
    }

    pub(super) fn rewrite_server(
        &self,
        server: &mut Server,
        destination: &Path,
    ) -> Result<(), String> {
        let Destination::Stdio {
            command,
            args,
            cwd,
            sandbox_profile,
            readable_roots,
            executable_sha256,
            script,
            script_sha256,
            ..
        } = &mut server.transport
        else {
            return Err("Unity presetは既存stdio登録だけを更新できます".into());
        };
        self.node.verify()?;
        let new_entry = destination.join(ENTRY);
        let new_entry_argument = new_entry
            .to_str()
            .ok_or("Unity runtime entry pathを文字列として表現できません")?
            .to_owned();
        *args = vec![new_entry_argument];
        *cwd = destination.to_owned();
        *sandbox_profile = "loopback".into();
        readable_roots.retain(|root| command.starts_with(root));
        *executable_sha256 = Some(self.node.sha256.clone());
        *script = Some(new_entry);
        *script_sha256 = Some(self.entry_sha256().to_owned());
        server.protocol_profile = ProtocolProfile::Auto2024Compat;
        server
            .validate()
            .map_err(|_| "Unity MCPの更新内容が無効です".into())
    }

    pub(super) fn revalidate(&self) -> Result<(), String> {
        let protection = Protection::capture(&self.source)?;
        self.node.verify()?;
        protection.check(
            &self.node.path,
            &self
                .node
                .file
                .metadata()
                .map_err(|_| "nodeを確認できません")?,
        )?;
        for file in &self.held {
            file.verify()?;
        }
        let (_, current) = tree_admission::open_root_with_ancestors(&self.source)
            .map_err(|_| "Unity sourceのnamespaceを再確認できません")?;
        if current != self.ancestors {
            return Err("確認中にUnity sourceのnamespaceが変更されました".into());
        }
        let current = scan(&self.source, &self.root, &protection)?;
        if current.manifest != self.manifest
            || manifest_hash(&current.manifest)? != self.manifest_sha256
        {
            return Err("確認中にUnity sourceが変更されました".into());
        }
        Ok(())
    }

    pub(super) fn entries(&self) -> &[ManifestEntry] {
        &self.manifest.entries
    }

    pub(super) fn open_copy_source(&self, relative: &Path) -> Result<File, String> {
        let mut current = self
            .root
            .try_clone()
            .map_err(|_| "source descriptorを複製できません")?;
        let components: Vec<_> = relative.components().collect();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                return Err("source relative pathが無効です".into());
            };
            current = tree_admission::open_at(
                &current,
                name,
                index + 1 != components.len(),
                &self.source.join(relative),
            )
            .map_err(|_| "source fileを再度開けません")?;
        }
        Ok(current)
    }
}

#[derive(Clone)]
pub(super) struct Protection {
    paths: BTreeSet<PathBuf>,
    identities: BTreeSet<polaris_auth::protection::ProtectedIdentity>,
}

impl Protection {
    pub(super) fn capture(root: &Path) -> Result<Self, String> {
        let (paths, identities) = polaris_auth::protection::with_protected_paths(|snapshot| {
            (snapshot.paths().clone(), snapshot.identities().clone())
        })
        .map_err(|_| "保護対象を確認できません")?;
        let value = Self { paths, identities };
        if value
            .paths
            .iter()
            .any(|path| path.starts_with(root) || root.starts_with(path))
        {
            return Err("Unity sourceが保護対象と重なっています".into());
        }
        Ok(value)
    }

    fn check(&self, path: &Path, metadata: &Metadata) -> Result<(), String> {
        if path_policy::is_denied(path)
            || self
                .paths
                .iter()
                .any(|protected| path.starts_with(protected) || protected.starts_with(path))
            || self
                .identities
                .contains(&polaris_auth::protection::ProtectedIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                })
        {
            return Err("Unity sourceに保護対象が含まれています".into());
        }
        Ok(())
    }
}

pub(super) fn scan(root_path: &Path, root: &File, protection: &Protection) -> Result<Scan, String> {
    let root_metadata = root
        .metadata()
        .map_err(|_| "Unity sourceを確認できません")?;
    require_owned(&root_metadata, true)?;
    protection.check(root_path, &root_metadata)?;
    let names = directory_names(root, MAX_ENTRIES)?;
    let lock_names: BTreeSet<_> = names
        .iter()
        .filter(|name| LOCKS.contains(&name.as_os_str().to_str().unwrap_or("")))
        .cloned()
        .collect();
    for required in [
        OsStr::new(PACKAGE),
        OsStr::new("build"),
        OsStr::new("node_modules"),
    ] {
        if !names.iter().any(|name| name == required) {
            return Err("Unity sourceに必要なpackage/build/node_modulesがありません".into());
        }
    }
    let mut state = WalkState {
        root_path,
        protection,
        entries: Vec::new(),
        count: 0,
        files: 0,
        bytes: 0,
        identities: BTreeMap::new(),
        fixed_exclusions: BTreeSet::new(),
    };
    for name in names {
        let selected = name == OsStr::new(PACKAGE)
            || name == OsStr::new("build")
            || name == OsStr::new("node_modules")
            || lock_names.contains(&name);
        if selected {
            state.walk_named(root, Path::new(""), &name, 0)?;
        }
    }
    state.entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    let pair = PAIR.map(|path| path.as_bytes().to_vec());
    let pair_entries = pair.map(|path| {
        state
            .entries
            .iter()
            .find(|entry| entry.relative == path)
            .ok_or_else(|| "Unity sourceのesbuild pairが不足しています".to_owned())
    });
    let [left, right] = pair_entries;
    let left = left?;
    let right = right?;
    if left.directory
        || right.directory
        || left.stamp.dev != right.stamp.dev
        || left.stamp.ino != right.stamp.ino
        || left.stamp.links != 2
        || right.stamp.links != 2
        || left.stamp.bytes != ESBUILD_BYTES
        || right.stamp.bytes != ESBUILD_BYTES
        || left.sha256 != right.sha256
    {
        return Err("Unity sourceのesbuild hardlink pairが承認条件と一致しません".into());
    }
    if Stamp::of(
        &root
            .metadata()
            .map_err(|_| "Unity sourceを確認できません")?,
    ) != Stamp::of(&root_metadata)
    {
        return Err("scan中にUnity source rootが変更されました".into());
    }
    let manifest = SourceManifest {
        root: Stamp::of(&root_metadata),
        entries: state.entries,
        lockfiles: lock_names
            .into_iter()
            .map(|name| name.as_bytes().to_vec())
            .collect(),
        fixed_exclusions: state.fixed_exclusions.into_iter().collect(),
    };
    if entry_digest(&manifest).is_err() {
        return Err("Unity sourceにbuild/index.jsがありません".into());
    }
    Ok(Scan {
        manifest,
        file_count: state.files,
        total_bytes: state.bytes,
    })
}

struct WalkState<'a> {
    root_path: &'a Path,
    protection: &'a Protection,
    entries: Vec<ManifestEntry>,
    count: usize,
    files: usize,
    bytes: u64,
    identities: BTreeMap<(u64, u64), Vec<u8>>,
    fixed_exclusions: BTreeSet<Vec<u8>>,
}

impl WalkState<'_> {
    fn walk_named(
        &mut self,
        parent: &File,
        relative_parent: &Path,
        name: &OsStr,
        depth: usize,
    ) -> Result<(), String> {
        if self.count >= MAX_ENTRIES || depth > MAX_DEPTH {
            return Err("Unity sourceがscan上限を超えています".into());
        }
        self.count += 1;
        let relative = relative_parent.join(name);
        let relative_bytes = relative.as_os_str().as_bytes().to_vec();
        if FIXED_BINS
            .iter()
            .any(|excluded| relative == Path::new(excluded))
        {
            let metadata = lstat_at(parent, name)?;
            if metadata.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32 {
                return Err("node_modules/.binがdirectoryではありません".into());
            }
            self.fixed_exclusions.insert(relative_bytes);
            return Ok(());
        }
        let path = self.root_path.join(&relative);
        if path_policy::is_denied(&path) || tree_admission::is_repository_metadata(&path) {
            return Err("Unity sourceに秘密名またはrepository metadataがあります".into());
        }
        let classified = lstat_at(parent, name)?;
        let kind = classified.mode & libc::S_IFMT as u32;
        if kind != libc::S_IFREG as u32 && kind != libc::S_IFDIR as u32 {
            return Err("Unity sourceにlinkまたは特殊fileがあります".into());
        }
        let directory = kind == libc::S_IFDIR as u32;
        let mut file = tree_admission::open_at(parent, name, directory, &path)
            .map_err(|_| "Unity sourceをdescriptorで開けません")?;
        let metadata = file
            .metadata()
            .map_err(|_| "Unity sourceを確認できません")?;
        if !classified.matches(&metadata) {
            return Err("scan中にUnity sourceが変更されました".into());
        }
        require_owned(&metadata, directory)?;
        self.protection.check(&path, &metadata)?;
        let exact_pair = PAIR.iter().any(|pair| relative == Path::new(pair));
        if !directory && metadata.nlink() != if exact_pair { 2 } else { 1 } {
            return Err("Unity sourceに許可されていないhardlinkがあります".into());
        }
        if let Some(first) = self
            .identities
            .insert((metadata.dev(), metadata.ino()), relative_bytes.clone())
        {
            let valid_pair = PAIR.iter().any(|pair| first == pair.as_bytes())
                && PAIR.iter().any(|pair| relative_bytes == pair.as_bytes());
            if !valid_pair {
                return Err("Unity sourceに重複inodeがあります".into());
            }
        }
        if directory {
            let names = directory_names(&file, MAX_ENTRIES.saturating_sub(self.count))?;
            for child in names {
                self.walk_named(&file, &relative, &child, depth + 1)?;
            }
            if Stamp::of(
                &file
                    .metadata()
                    .map_err(|_| "Unity sourceを確認できません")?,
            ) != Stamp::of(&metadata)
            {
                return Err("scan中にUnity sourceが変更されました".into());
            }
            self.entries.push(ManifestEntry {
                relative: relative_bytes,
                directory: true,
                stamp: Stamp::of(&metadata),
                sha256: None,
            });
        } else {
            if self.files >= MAX_FILES
                || metadata.len() > MAX_FILE_BYTES
                || metadata.len() > MAX_TOTAL_BYTES.saturating_sub(self.bytes)
            {
                return Err("Unity sourceがcopy上限を超えています".into());
            }
            let sha256 = hash_reader(&mut file, metadata.len())?;
            if Stamp::of(
                &file
                    .metadata()
                    .map_err(|_| "Unity sourceを確認できません")?,
            ) != Stamp::of(&metadata)
            {
                return Err("scan中にUnity sourceが変更されました".into());
            }
            self.files += 1;
            self.bytes += metadata.len();
            self.entries.push(ManifestEntry {
                relative: relative_bytes,
                directory: false,
                stamp: Stamp::of(&metadata),
                sha256: Some(sha256),
            });
        }
        Ok(())
    }
}

fn inspect_external(
    path: &Path,
    executable: bool,
    protection: &Protection,
) -> Result<BoundFile, String> {
    if !path.is_absolute() || path.canonicalize().map_err(|_| "nodeを確認できません")? != path
    {
        return Err("nodeはcanonicalな絶対fileである必要があります".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "nodeをdescriptorで開けません")?;
    let metadata = file.metadata().map_err(|_| "nodeを確認できません")?;
    protection.check(path, &metadata)?;
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.nlink() != 1
        || ![0, owner].contains(&metadata.uid())
        || metadata.mode() & 0o7022 != 0
        || (executable && metadata.mode() & 0o111 == 0)
        || metadata.len() == 0
        || metadata.len() > 256 * 1024 * 1024
    {
        return Err("nodeの所有・permission・種別が無効です".into());
    }
    let sha256 = hash_file(&file, metadata.len())?;
    Ok(BoundFile {
        path: path.to_owned(),
        file,
        stamp: Stamp::of(&metadata),
        sha256,
    })
}

fn bind_relative(
    root: &File,
    root_path: &Path,
    relative: &Path,
    manifest: &SourceManifest,
) -> Result<BoundFile, String> {
    let mut current = root
        .try_clone()
        .map_err(|_| "source descriptorを複製できません")?;
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("source relative pathが無効です".into());
        };
        current = tree_admission::open_at(
            &current,
            name,
            index + 1 != components.len(),
            &root_path.join(relative),
        )
        .map_err(|_| "source descriptorを保持できません")?;
    }
    let relative_bytes = relative.as_os_str().as_bytes();
    let evidence = manifest
        .entries
        .iter()
        .find(|entry| entry.relative == relative_bytes && !entry.directory)
        .ok_or("source manifestに必要fileがありません")?;
    let metadata = current
        .metadata()
        .map_err(|_| "source fileを確認できません")?;
    let sha256 = hash_file(&current, metadata.len())?;
    if Stamp::of(&metadata) != evidence.stamp || evidence.sha256.as_deref() != Some(&sha256) {
        return Err("source fileがscan結果と一致しません".into());
    }
    Ok(BoundFile {
        path: root_path.join(relative),
        file: current,
        stamp: evidence.stamp.clone(),
        sha256,
    })
}

fn require_owned(metadata: &Metadata, directory: bool) -> Result<(), String> {
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err("Unity sourceの所有・permission・種別が無効です".into());
    }
    Ok(())
}

struct Classified {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
    links: u64,
    bytes: i64,
}

impl Classified {
    fn matches(&self, metadata: &Metadata) -> bool {
        self.bytes >= 0
            && self.dev == metadata.dev()
            && self.ino == metadata.ino()
            && self.uid == metadata.uid()
            && self.mode == metadata.mode()
            && self.links == metadata.nlink()
            && self.bytes as u64 == metadata.len()
    }
}

fn lstat_at(parent: &File, name: &OsStr) -> Result<Classified, String> {
    let name = CString::new(name.as_bytes()).map_err(|_| "source pathが無効です")?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err("Unity sourceを確認できません".into());
    }
    let stat = unsafe { stat.assume_init() };
    #[cfg(target_os = "linux")]
    let dev = stat.st_dev;
    #[cfg(not(target_os = "linux"))]
    let dev = stat.st_dev as u64;
    Ok(Classified {
        dev,
        ino: stat.st_ino,
        uid: stat.st_uid,
        mode: stat.st_mode as u32,
        links: stat.st_nlink as u64,
        bytes: stat.st_size,
    })
}

fn directory_names(directory: &File, limit: usize) -> Result<Vec<OsString>, String> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err("Unity source directoryを開けません".into());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err("Unity source directoryを列挙できません".into());
    }
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Dir(stream);
    let mut result = Vec::new();
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::dirent>::uninit();
        let mut found = std::ptr::null_mut();
        if unsafe { libc::readdir_r(stream.0, entry.as_mut_ptr(), &mut found) } != 0 {
            return Err("Unity source directoryを列挙できません".into());
        }
        if found.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*found).d_name.as_ptr()) }.to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        if result.len() >= limit {
            return Err("Unity sourceがscan上限を超えています".into());
        }
        result.push(OsString::from_vec(name.to_vec()));
    }
    result.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(result)
}

fn hash_file(file: &File, expected: u64) -> Result<String, String> {
    let mut reader = file
        .try_clone()
        .map_err(|_| "file descriptorを複製できません")?;
    hash_reader(&mut reader, expected)
}

fn hash_reader(reader: &mut File, expected: u64) -> Result<String, String> {
    use std::io::{Seek, SeekFrom};
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| "fileを読めません")?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = reader.read(&mut buffer).map_err(|_| "fileを読めません")?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or("file sizeが無効です")?;
        if total > expected {
            return Err("読取り中にfileが変更されました".into());
        }
        hash.update(&buffer[..count]);
    }
    if total != expected {
        return Err("読取り中にfileが変更されました".into());
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn manifest_hash(manifest: &SourceManifest) -> Result<String, String> {
    let bytes = serde_json::to_vec(manifest).map_err(|_| "source manifestを作成できません")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn entry_digest(manifest: &SourceManifest) -> Result<&str, String> {
    manifest
        .entries
        .iter()
        .find(|entry| entry.relative == ENTRY.as_bytes() && !entry.directory)
        .and_then(|entry| entry.sha256.as_deref())
        .ok_or_else(|| "Unity entryを確認できません".into())
}

fn hardlink_pair_digest(manifest: &SourceManifest) -> Result<&str, String> {
    manifest
        .entries
        .iter()
        .find(|entry| entry.relative == PAIR[0].as_bytes() && !entry.directory)
        .and_then(|entry| entry.sha256.as_deref())
        .ok_or_else(|| "Unity sourceにesbuild pairがありません".into())
}
