//! Read-only workspace projections under the authentication protection lock.
//! UI-only source and Git projection; never used as a model-context reader.
//! Bound directories and protected metadata are checked before reading bodies.

use polaris_auth::protection::ProtectedPathsSnapshot;
use polaris_desktop_protocol::workspace_view::{
    AttachmentState, AttachmentView, FileView, GitState, GitView, ViewState, WorkspaceView,
};
use std::{
    ffi::{CString, OsStr},
    fs::{self, File},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

const MAX_FILES: usize = 256;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceSourceIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug)]
pub struct WorkspaceViewReader {
    source: PathBuf,
    expected: WorkspaceSourceIdentity,
}

impl WorkspaceViewReader {
    /// The caller supplies the owner-confirmed source identity once at startup.
    pub fn new(
        source: PathBuf,
        expected: WorkspaceSourceIdentity,
    ) -> Result<Self, WorkspaceViewError> {
        if !absolute_components(&source) {
            return Err(WorkspaceViewError::InvalidSource);
        }
        let metadata =
            fs::symlink_metadata(&source).map_err(|_| WorkspaceViewError::InvalidSource)?;
        if !metadata.is_dir()
            || metadata.dev() != expected.device
            || metadata.ino() != expected.inode
        {
            return Err(WorkspaceViewError::SourceChanged);
        }
        Ok(Self { source, expected })
    }

    pub fn read(&self, selected_path: Option<&str>) -> WorkspaceView {
        polaris_auth::protection::with_protected_paths(|registry| {
            let Ok(root) = open_absolute_regular(&self.source) else {
                return unavailable("フォルダを開けませんでした。");
            };
            let Ok(metadata) = root.metadata() else {
                return unavailable("フォルダを確認できませんでした。");
            };
            if !metadata.is_dir()
                || metadata.dev() != self.expected.device
                || metadata.ino() != self.expected.inode
            {
                return unavailable("選択したフォルダが変更されました。");
            }
            let mut files = Vec::new();
            let mut count = 0;
            walk(
                &root,
                &self.source,
                Path::new(""),
                registry,
                selected_path,
                &mut files,
                &mut count,
                0,
            );
            let mut notices = Vec::new();
            if count >= 4096 || files.len() >= MAX_FILES {
                notices.push("一覧の上限に達したため一部のファイルを省略しています。".into());
            }
            let git = git_view(&root, &self.source, registry, selected_path, &mut files);
            if fs::symlink_metadata(&self.source).map_or(true, |m| {
                m.dev() != metadata.dev() || m.ino() != metadata.ino()
            }) {
                return unavailable("読み取り中にフォルダが変更されました。");
            }
            let view = WorkspaceView {
                state: ViewState::Ready,
                files,
                git,
                notices,
            };
            if serde_json::to_vec(&view).map_or(true, |body| body.len() > 512 * 1024) {
                return unavailable("表示情報が上限を超えています。");
            }
            view
        })
        .unwrap_or_else(|_| unavailable("認証保護を確認できませんでした。"))
    }

    pub fn read_attachment(path: &Path) -> AttachmentView {
        let result = polaris_auth::protection::with_protected_paths(|registry| {
            read_attachment_locked(path, registry)
        });
        match result {
            Ok(Ok(view)) => view,
            _ => attachment_unavailable("添付ファイルを安全に読み取れませんでした。"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceViewError {
    #[error("invalid source binding")]
    InvalidSource,
    #[error("source identity changed")]
    SourceChanged,
}

fn unavailable(reason: &str) -> WorkspaceView {
    WorkspaceView {
        state: ViewState::Unavailable,
        files: Vec::new(),
        notices: Vec::new(),
        git: GitView {
            state: GitState::Unavailable,
            reason: Some(reason.into()),
            branch: None,
            head: None,
            change_summary: None,
            revisions: Vec::new(),
        },
    }
}

fn read_attachment_locked(
    path: &Path,
    registry: &polaris_auth::protection::ProtectedPathsSnapshot,
) -> Result<AttachmentView, ()> {
    if !absolute_components(path)
        || polaris_tools::path_policy::is_denied(path)
        || registry
            .paths()
            .iter()
            .any(|protected| path.starts_with(protected))
    {
        return Ok(attachment_unavailable(
            "認証保護対象の添付は読み取りません。",
        ));
    }
    let file = open_absolute_regular(path).map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.len() > MAX_ATTACHMENT_BYTES as u64
        || registry
            .identities()
            .iter()
            .any(|protected| protected.dev == metadata.dev() && protected.ino == metadata.ino())
    {
        return Ok(attachment_unavailable(
            "添付ファイルは表示条件を満たしません。",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize + 1);
    file.take(MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Ok(attachment_unavailable("添付ファイルが大きすぎます。"));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        Ok(_) => {
            return Ok(attachment_unavailable(
                "バイナリ形式の添付は読み取りません。",
            ));
        }
        Err(_) => {
            return Ok(attachment_unavailable(
                "バイナリ形式の添付は読み取りません。",
            ));
        }
    };
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or(())?
        .to_owned();
    Ok(AttachmentView {
        state: AttachmentState::Ready,
        name: Some(name),
        text: Some(text),
        reason: None,
    })
}

fn attachment_unavailable(reason: &str) -> AttachmentView {
    AttachmentView {
        state: AttachmentState::Unavailable,
        name: None,
        text: None,
        reason: Some(reason.into()),
    }
}

fn absolute_components(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

fn valid_relative(value: &str) -> Option<String> {
    (!value.is_empty()
        && !value.starts_with('/')
        && !value.as_bytes().contains(&0)
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."))
    .then(|| value.to_owned())
}

fn open_absolute_regular(path: &Path) -> Result<File, ()> {
    let mut directory = File::open("/").map_err(|_| ())?;
    let components: Vec<_> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let (name, parents) = components.split_last().ok_or(())?;
    for parent in parents {
        directory =
            open_at(&directory, parent, libc::O_RDONLY | libc::O_DIRECTORY).map_err(|_| ())?;
    }
    open_at(&directory, name, libc::O_RDONLY | libc::O_NONBLOCK).map_err(|_| ())
}

fn open_at(parent: &File, name: &OsStr, flags: i32) -> Result<File, ()> {
    let name = CString::new(name.as_bytes()).map_err(|_| ())?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(());
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn denied(path: &Path, metadata: &std::fs::Metadata, registry: &ProtectedPathsSnapshot) -> bool {
    polaris_tools::path_policy::is_denied(path)
        || registry.paths().iter().any(|p| path.starts_with(p))
        || registry
            .identities()
            .iter()
            .any(|id| id.dev == metadata.dev() && id.ino == metadata.ino())
        || (metadata.is_file() && metadata.nlink() != 1)
}

fn names(directory: &File) -> Result<Vec<std::ffi::OsString>, ()> {
    let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(());
    }
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(());
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
        if names.len() > 4096 {
            unsafe {
                libc::closedir(dir);
            }
            return Err(());
        }
    }
    unsafe {
        libc::closedir(dir);
    }
    names.sort();
    Ok(names)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Traversal keeps the source identity, registry and output budget explicit"
)]
fn walk(
    dir: &File,
    source: &Path,
    prefix: &Path,
    registry: &ProtectedPathsSnapshot,
    selected: Option<&str>,
    files: &mut Vec<FileView>,
    count: &mut usize,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    let Ok(entries) = names(dir) else {
        *count = 4096;
        return;
    };
    for name in entries {
        if *count >= 4096 || files.len() >= MAX_FILES {
            return;
        }
        *count += 1;
        if [".git", "target", ".build", "node_modules", ".next"]
            .iter()
            .any(|s| name.eq_ignore_ascii_case(s))
        {
            continue;
        }
        let relative = prefix.join(&name);
        let path = source.join(&relative);
        if polaris_tools::path_policy::is_denied(&path)
            || registry.paths().iter().any(|p| path.starts_with(p))
        {
            continue;
        }
        let Ok(file) = open_at(dir, &name, libc::O_RDONLY | libc::O_NONBLOCK) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if denied(&path, &metadata, registry) {
            continue;
        }
        if metadata.is_dir() {
            walk(
                &file,
                source,
                &relative,
                registry,
                selected,
                files,
                count,
                depth + 1,
            );
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Some(path) = relative
            .to_str()
            .filter(|p| !p.chars().any(char::is_control))
        else {
            continue;
        };
        let mut body = String::new();
        let mut reason = None;
        if selected == Some(path) {
            if metadata.len() > MAX_BODY_BYTES as u64 {
                reason = Some("本文が64KiBを超えています。".into());
            } else {
                let mut bytes = Vec::new();
                if file
                    .take(MAX_BODY_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .is_err()
                    || bytes.len() > MAX_BODY_BYTES
                {
                    reason = Some("本文を安全に読み取れませんでした。".into());
                } else if let Ok(text) = String::from_utf8(bytes) {
                    if text.contains('\0') {
                        reason = Some("バイナリ形式です。".into());
                    } else {
                        body = text;
                    }
                } else {
                    reason = Some("UTF-8テキストではありません。".into());
                }
            }
        }
        files.push(FileView {
            id: path.into(),
            path: path.into(),
            body,
            unavailable_reason: reason,
            unstaged_diff: None,
            staged_diff: None,
            modified: false,
            staged: false,
            unsaved: false,
        });
    }
}

fn git_unavailable(reason: &str) -> GitView {
    GitView {
        state: GitState::Unavailable,
        reason: Some(reason.into()),
        branch: None,
        head: None,
        change_summary: None,
        revisions: Vec::new(),
    }
}

fn git_view(
    root: &File,
    source: &Path,
    registry: &ProtectedPathsSnapshot,
    selected: Option<&str>,
    files: &mut [FileView],
) -> GitView {
    let Ok(git) = open_at(root, OsStr::new(".git"), libc::O_RDONLY | libc::O_DIRECTORY) else {
        return git_unavailable("Git管理外、または外部gitdirです。");
    };
    // Reject metadata aliases before Git can follow them. Git objects are never sent to the model.
    if !safe_git_tree(&git, &source.join(".git"), registry, &mut 0, 0) {
        return git_unavailable("Gitメタデータの安全性を確認できません。");
    }
    if let Ok(config) = open_at(
        &git,
        OsStr::new("config"),
        libc::O_RDONLY | libc::O_NONBLOCK,
    ) {
        let mut text = String::new();
        if config.take(65_537).read_to_string(&mut text).is_err()
            || text.len() > 65_536
            || ["[include", "worktreeconfig", "promisor", "partialclone"]
                .iter()
                .any(|marker| {
                    text.chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>()
                        .to_ascii_lowercase()
                        .contains(marker)
                })
        {
            return git_unavailable("外部設定を含むGit情報は表示できません。");
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let run = |args: &[&str]| git_command(root, args, deadline);
    let Ok(status) = run(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=normal",
        "--ignore-submodules=all",
    ]) else {
        return git_unavailable("Git状態を取得できませんでした。");
    };
    for record in status.split(|b| *b == 0).filter(|b| b.len() > 3) {
        if let Ok(path) = std::str::from_utf8(&record[3..])
            && let Some(file) = files.iter_mut().find(|f| f.path == path)
        {
            file.staged = record[0] != b' ' && record[0] != b'?';
            file.modified = record[1] != b' ';
            if record[0] == b'U'
                || record[1] == b'U'
                || &record[..2] == b"AA"
                || &record[..2] == b"DD"
            {
                file.unavailable_reason = Some("Gitの競合を解消してください。".into());
            }
        }
    }
    let head = run(&["rev-parse", "--verify", "--short=12", "HEAD"])
        .ok()
        .and_then(|s| String::from_utf8(s).ok())
        .unwrap_or_else(|| "コミットなし".into());
    let branch = run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .and_then(|s| String::from_utf8(s).ok())
        .unwrap_or_else(|| "detached HEAD".into());
    let mut revisions = Vec::new();
    if let Some(path) = selected.filter(|p| valid_relative(p).is_some())
        && let Some(file) = files
            .iter_mut()
            .find(|f| f.path == path && f.unavailable_reason.is_none())
    {
        file.unstaged_diff = run(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--unified=3",
            "--",
            path,
        ])
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .or_else(|| Some("差分を取得できませんでした。".into()));
        file.staged_diff = run(&[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--unified=3",
            "--",
            path,
        ])
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .or_else(|| Some("差分を取得できませんでした。".into()));
        if let Ok(log) = run(&[
            "log",
            "--no-decorate",
            "--no-notes",
            "--no-show-signature",
            "--format=%H%x00%P%x00%s",
            "-n",
            "1",
            "--",
            path,
        ]) && let Ok(log) = String::from_utf8(log)
        {
            let parts: Vec<_> = log.trim().splitn(3, '\0').collect();
            if parts.len() == 3
                && parts[0].len() == 40
                && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
            {
                let object = format!("{}:{}", parts[0], path);
                if let Ok(body) = run(&["show", "--no-ext-diff", "--no-textconv", &object])
                    .and_then(|v| String::from_utf8(v).map_err(|_| ()))
                {
                    let diff = run(&[
                        "show",
                        "--format=",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        parts[0],
                        "--",
                        path,
                    ])
                    .ok()
                    .and_then(|v| String::from_utf8(v).ok())
                    .unwrap_or_else(|| "差分を取得できませんでした。".into());
                    revisions.push(polaris_desktop_protocol::workspace_view::RevisionView {
                        id: parts[0].into(),
                        parent: parts[1].split_whitespace().next().unwrap_or("なし").into(),
                        title: parts[2].into(),
                        file_id: path.into(),
                        body,
                        diff,
                    });
                }
            }
        }
    }
    GitView {
        state: GitState::Ready,
        reason: None,
        branch: Some(branch.trim().into()),
        head: Some(head.trim().into()),
        change_summary: Some(format!(
            "変更 {}件 · upstream未取得",
            files.iter().filter(|f| f.modified || f.staged).count()
        )),
        revisions,
    }
}

fn safe_git_tree(
    dir: &File,
    path: &Path,
    registry: &ProtectedPathsSnapshot,
    count: &mut usize,
    depth: usize,
) -> bool {
    if depth > 16 {
        return false;
    }
    let Ok(entries) = names(dir) else {
        return false;
    };
    for name in entries {
        *count += 1;
        if *count > 40_000 {
            return false;
        }
        if name == "alternates"
            || name == "commondir"
            || name == "gitdir"
            || name.as_bytes().ends_with(b".promisor")
        {
            return false;
        }
        let Ok(file) = open_at(dir, &name, libc::O_RDONLY | libc::O_NONBLOCK) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        let path = path.join(name);
        if denied(&path, &metadata, registry) {
            return false;
        }
        if metadata.is_dir() {
            if !safe_git_tree(&file, &path, registry, count, depth + 1) {
                return false;
            }
        } else if !metadata.is_file() {
            return false;
        }
    }
    true
}

fn git_command(root: &File, args: &[&str], deadline: std::time::Instant) -> Result<Vec<u8>, ()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if std::time::Instant::now() >= deadline {
        return Err(());
    }
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "diff.external=",
            "-c",
            "log.showSignature=false",
            "-c",
            "core.worktree=.",
            "-c",
            "core.bare=false",
            "-c",
            "status.renames=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    polaris_sandbox::protect_child_descriptors(&mut command);
    let fd = root.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(fd) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| ())?;
    let mut out = child.stdout.take().ok_or(())?;
    unsafe {
        libc::fcntl(out.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match out.read(&mut chunk) {
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
        }
        if bytes.len() > MAX_BODY_BYTES || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                loop {
                    match out.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                    if bytes.len() > MAX_BODY_BYTES {
                        return Err(());
                    }
                }
                return if status.success() { Ok(bytes) } else { Err(()) };
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn reader(source: &Path) -> WorkspaceViewReader {
        let source = source.canonicalize().unwrap();
        let metadata = fs::metadata(&source).unwrap();
        WorkspaceViewReader::new(
            source.to_owned(),
            WorkspaceSourceIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        )
        .unwrap()
    }

    #[test]
    fn selected_text_is_projected_from_bound_directory() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("visible.txt"), "visible").unwrap();
        let view = reader(root.path()).read(Some("visible.txt"));
        assert_eq!(view.state, ViewState::Ready);
        assert_eq!(view.files[0].body, "visible");
        assert_eq!(view.git.state, GitState::Unavailable);
    }

    #[test]
    fn changed_binding_is_unavailable() {
        let base = tempdir().unwrap();
        let source = base.path().join("source");
        fs::create_dir(&source).unwrap();
        let stale = reader(&source);
        let moved = base.path().join("moved");
        fs::rename(&source, &moved).unwrap();
        fs::create_dir(&source).unwrap();
        let view = stale.read(None);
        assert_eq!(view.state, ViewState::Unavailable);
    }

    #[test]
    fn attachment_rejects_symlink_and_binary() {
        let root = tempdir().unwrap();
        let binary = root.path().join("binary");
        fs::write(&binary, [0xff]).unwrap();
        assert_eq!(
            WorkspaceViewReader::read_attachment(&binary).state,
            AttachmentState::Unavailable
        );
        let target = root.path().join("target");
        fs::write(&target, "x").unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            WorkspaceViewReader::read_attachment(&link).state,
            AttachmentState::Unavailable
        );
    }

    #[test]
    fn selected_read_skips_generated_trees_and_secret_names() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/huge"), vec![0; 2 * 1024 * 1024]).unwrap();
        fs::write(root.join(".env"), "synthetic secret").unwrap();
        fs::write(root.join("file.txt"), "read me").unwrap();
        let view = reader(&root).read(Some("file.txt"));
        assert_eq!(view.state, ViewState::Ready);
        assert_eq!(view.files.len(), 1);
        assert_eq!(view.files[0].body, "read me");
        assert_eq!(
            WorkspaceViewReader::read_attachment(&root.join(".env")).state,
            AttachmentState::Unavailable
        );
        assert_eq!(
            WorkspaceViewReader::read_attachment(&root.join("file.txt"))
                .text
                .as_deref(),
            Some("read me")
        );
    }

    #[test]
    fn git_status_uses_bound_root_and_refuses_external_configuration() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let init = std::process::Command::new("/usr/bin/git")
            .arg("init")
            .arg("--quiet")
            .arg(&root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(root.join("file.txt"), "read me").unwrap();
        let view = reader(&root).read(Some("file.txt"));
        assert_eq!(view.git.state, GitState::Ready);
        assert!(view.files[0].modified);
        fs::write(root.join(".git/config"), "[ include ]\npath = /not-read\n").unwrap();
        let view = reader(&root).read(None);
        assert_eq!(view.git.state, GitState::Unavailable);
    }
}
