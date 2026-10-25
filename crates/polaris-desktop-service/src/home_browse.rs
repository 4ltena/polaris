//! `--browse-home` helper: one-level listings and selected text files under
//! the OS home. It opens no store, provider, credential body or execution.
use crate::{
    transport::{self, Exit, ServiceError, WorkerExit},
    workspace_view::{denied, open_at},
};
use polaris_auth::protection::ProtectedPathsSnapshot;
use polaris_desktop_protocol::{
    browse::*,
    ids::{DecimalU64, EngineEpoch},
    request::{Request, RequestBody},
    response::*,
};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CString, OsStr, OsString},
    fs::File,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite};

const MAX_ENTRIES: usize = 65_536;
const MAX_LISTING_BYTES: usize = 8 * 1024 * 1024;
const MAX_HELD_BYTES: usize = 16 * 1024 * 1024;
const MAX_LISTINGS: usize = 4;
const MAX_PAGE_BYTES: usize = 512 * 1024;
const LISTING_TTL: Duration = Duration::from_secs(30);

/// Identity and change stamps compared before and after each read.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl From<&std::fs::Metadata> for Stamp {
    fn from(m: &std::fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

struct Listing {
    id: String,
    directory: String,
    stamp: Stamp,
    entries: Vec<BrowseEntry>,
    omitted: u64,
    capacity: bool,
    bytes: usize,
    created: Instant,
    cursors: HashMap<String, usize>,
}

pub(crate) struct HomeBrowser {
    home: PathBuf,
    root: File,
    identity: (u64, u64),
    epoch: String,
    listings: VecDeque<Listing>,
    /// Sticky once home is replaced; only a new helper binds a new root.
    replaced: bool,
    ttl: Duration,
    entry_limit: usize,
    #[cfg(test)]
    pub(crate) before_final_check: Option<Box<dyn Fn() + Send>>,
}

fn token() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: the buffer is valid for its full length.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn failure(code: ErrorCode, message: &str) -> ProtocolError {
    ProtocolError {
        code,
        message: message.into(),
    }
}

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut directory = File::open("/")?;
    for part in path.components().skip(1) {
        directory = open_at(
            &directory,
            part.as_os_str(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?;
    }
    Ok(directory)
}

/// No-follow status of one name without opening it, so FIFOs never block.
fn status_at(parent: &File, name: &OsStr) -> std::io::Result<libc::stat> {
    let name = CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("name"))?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid directory fd, NUL-terminated name and output buffer.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded and initialized the buffer.
    Ok(unsafe { status.assume_init() })
}

fn names(directory: &File, limit: usize) -> Result<(Vec<OsString>, bool), ()> {
    // SAFETY: duplicates an owned fd; fdopendir takes ownership of the copy.
    let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(());
    }
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        unsafe { libc::close(fd) };
        return Err(());
    }
    let mut names = Vec::new();
    let mut truncated = false;
    loop {
        // SAFETY: dir stays open until closedir below.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() == limit {
            truncated = true;
            break;
        }
        names.push(OsStr::from_bytes(name).to_owned());
    }
    unsafe { libc::closedir(dir) };
    names.sort();
    Ok((names, truncated))
}

impl HomeBrowser {
    pub(crate) fn open(home: PathBuf) -> std::io::Result<Self> {
        let root = open_no_follow(&home)?;
        let metadata = root.metadata()?;
        Ok(Self {
            home,
            identity: (metadata.dev(), metadata.ino()),
            root,
            epoch: format!("browse-{}", token()),
            listings: VecDeque::new(),
            replaced: false,
            ttl: LISTING_TTL,
            entry_limit: MAX_ENTRIES,
            #[cfg(test)]
            before_final_check: None,
        })
    }

    fn check_root(&mut self) -> Result<(), ProtocolError> {
        let same = open_no_follow(&self.home)
            .and_then(|file| file.metadata())
            .is_ok_and(|m| (m.dev(), m.ino()) == self.identity);
        self.replaced |= !same;
        if self.replaced {
            self.listings.clear();
            return Err(failure(
                ErrorCode::RevisionConflict,
                "The home folder changed. Reconnect.",
            ));
        }
        Ok(())
    }

    /// Opens a home-relative directory one no-follow component at a time,
    /// refusing protected directories on the way.
    fn directory(
        &self,
        relative: &str,
        registry: &ProtectedPathsSnapshot,
    ) -> std::io::Result<File> {
        let mut directory = self.root.try_clone()?;
        let mut path = self.home.clone();
        for part in relative.split('/').filter(|part| !part.is_empty()) {
            path.push(part);
            directory = open_at(
                &directory,
                OsStr::new(part),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )?;
            if denied(&path, &directory.metadata()?, registry) {
                return Err(std::io::Error::from_raw_os_error(libc::EACCES));
            }
        }
        Ok(directory)
    }

    pub(crate) fn list(&mut self, request: &BrowseList) -> Result<BrowseListPage, ProtocolError> {
        self.check_root()?;
        let now = Instant::now();
        let ttl = self.ttl;
        self.listings
            .retain(|l| now.duration_since(l.created) < ttl);
        polaris_auth::protection::with_protected_paths(|registry| {
            let offset = match &request.cursor {
                Some(cursor) => {
                    let conflict = || {
                        failure(
                            ErrorCode::RevisionConflict,
                            "The listing continuation expired. Reload.",
                        )
                    };
                    let index = self
                        .listings
                        .iter()
                        .position(|l| l.directory == request.path && l.cursors.contains_key(cursor))
                        .ok_or_else(conflict)?;
                    let current = self
                        .directory(&request.path, registry)
                        .and_then(|d| d.metadata())
                        .map(|m| Stamp::from(&m));
                    if current.ok() != Some(self.listings[index].stamp) {
                        self.listings.remove(index);
                        return Err(conflict());
                    }
                    let listing = self.listings.remove(index).unwrap();
                    let offset = listing.cursors[cursor];
                    self.listings.push_back(listing);
                    offset
                }
                None => {
                    let listing = self.snapshot(&request.path, registry)?;
                    self.listings.push_back(listing);
                    while self.listings.len() > MAX_LISTINGS
                        || self.listings.iter().map(|l| l.bytes).sum::<usize>() > MAX_HELD_BYTES
                    {
                        self.listings.pop_front();
                    }
                    0
                }
            };
            let epoch = self.epoch.clone();
            let listing = self.listings.back_mut().unwrap();
            let mut end = offset;
            let mut bytes = 0;
            while end < listing.entries.len() && end - offset < MAX_PAGE_ENTRIES {
                let size =
                    serde_json::to_vec(&listing.entries[end]).map_or(usize::MAX, |b| b.len() + 1);
                if bytes + size > MAX_PAGE_BYTES - 16 * 1024 {
                    break;
                }
                bytes += size;
                end += 1;
            }
            let next_cursor = (end < listing.entries.len()).then(|| {
                let cursor = token();
                listing.cursors.insert(cursor.clone(), end);
                cursor
            });
            Ok(BrowseListPage {
                browse_epoch: epoch,
                snapshot_id: listing.id.clone(),
                directory: listing.directory.clone(),
                entries: listing.entries[offset..end].to_vec(),
                omitted: DecimalU64::new(listing.omitted),
                capacity: listing.capacity,
                next_cursor,
            })
        })
        .unwrap_or_else(|_| {
            Err(failure(
                ErrorCode::PermissionDenied,
                "Could not verify authentication protection.",
            ))
        })
    }

    fn snapshot(
        &self,
        relative: &str,
        registry: &ProtectedPathsSnapshot,
    ) -> Result<Listing, ProtocolError> {
        let unavailable = |e: std::io::Error| match e.raw_os_error() {
            Some(libc::ENOENT) => failure(ErrorCode::NotFound, "Folder not found."),
            _ => failure(ErrorCode::PermissionDenied, "Cannot open the folder."),
        };
        let directory = self.directory(relative, registry).map_err(unavailable)?;
        let before = Stamp::from(&directory.metadata().map_err(unavailable)?);
        let (names, truncated) = names(&directory, self.entry_limit)
            .map_err(|()| failure(ErrorCode::PermissionDenied, "Cannot read the folder."))?;
        let mut directories = Vec::new();
        let mut others = Vec::new();
        let mut omitted = 0;
        let mut bytes = 0;
        let mut capacity = truncated;
        for name in names {
            let Ok(status) = status_at(&directory, &name) else {
                continue;
            };
            let Some(text) = name.to_str().filter(|t| !t.chars().any(char::is_control)) else {
                omitted += 1;
                continue;
            };
            let path = if relative.is_empty() {
                text.to_owned()
            } else {
                format!("{relative}/{text}")
            };
            let absolute = self.home.join(&path);
            let format = status.st_mode & libc::S_IFMT;
            if polaris_tools::path_policy::is_denied(&absolute)
                || registry.paths().iter().any(|p| absolute.starts_with(p))
                || registry
                    .identities()
                    .iter()
                    .any(|id| id.dev == status.st_dev as u64 && id.ino == status.st_ino)
                || (format == libc::S_IFREG && status.st_nlink != 1)
            {
                continue;
            }
            bytes += text.len() + path.len();
            if bytes > MAX_LISTING_BYTES {
                capacity = true;
                break;
            }
            let entry = BrowseEntry {
                name: text.to_owned(),
                path,
                kind: match format {
                    libc::S_IFDIR => BrowseEntryKind::Directory,
                    libc::S_IFREG => BrowseEntryKind::File,
                    libc::S_IFLNK => BrowseEntryKind::Symlink,
                    _ => BrowseEntryKind::Unsupported,
                },
                device: DecimalU64::new(status.st_dev as u64),
                inode: DecimalU64::new(status.st_ino),
            };
            if format == libc::S_IFDIR {
                directories.push(entry);
            } else {
                others.push(entry);
            }
        }
        let after = directory
            .metadata()
            .map(|m| Stamp::from(&m))
            .map_err(unavailable)?;
        if before != after {
            return Err(failure(
                ErrorCode::RevisionConflict,
                "The folder changed while listing. Reload.",
            ));
        }
        directories.extend(others);
        Ok(Listing {
            id: token(),
            directory: relative.to_owned(),
            stamp: before,
            entries: directories,
            omitted,
            capacity,
            bytes,
            created: Instant::now(),
            cursors: HashMap::new(),
        })
    }

    pub(crate) fn read(&mut self, request: &BrowseRead) -> Result<BrowseReadResult, ProtocolError> {
        self.check_root()?;
        let content = polaris_auth::protection::with_protected_paths(|registry| {
            self.read_locked(request, registry)
        })
        .map_err(|_| {
            failure(
                ErrorCode::PermissionDenied,
                "Could not verify authentication protection.",
            )
        })?;
        Ok(BrowseReadResult {
            browse_epoch: self.epoch.clone(),
            path: request.path.clone(),
            device: request.device,
            inode: request.inode,
            content: match content {
                Ok(text) => BrowseContent::Ready { text },
                Err(reason) => BrowseContent::Unavailable { reason },
            },
        })
    }

    fn read_locked(
        &self,
        request: &BrowseRead,
        registry: &ProtectedPathsSnapshot,
    ) -> Result<String, BrowseUnavailable> {
        use BrowseUnavailable::*;
        let reason = |e: std::io::Error| match e.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => Deleted,
            Some(libc::EACCES | libc::EPERM) => PermissionDenied,
            Some(libc::ELOOP) => NotRegularFile,
            _ => PermissionDenied,
        };
        let (parent, name) = request.path.rsplit_once('/').unwrap_or(("", &request.path));
        let absolute = self.home.join(&request.path);
        if polaris_tools::path_policy::is_denied(&absolute)
            || registry.paths().iter().any(|p| absolute.starts_with(p))
        {
            return Err(Protected);
        }
        let directory = self.directory(parent, registry).map_err(reason)?;
        let identity = |status: &libc::stat| {
            (status.st_dev as u64, status.st_ino) == (request.device.get(), request.inode.get())
        };
        let status = status_at(&directory, OsStr::new(name)).map_err(reason)?;
        if !identity(&status) {
            return Err(Replaced);
        }
        if status.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(NotRegularFile);
        }
        let file = open_at(
            &directory,
            OsStr::new(name),
            libc::O_RDONLY | libc::O_NONBLOCK,
        )
        .map_err(reason)?;
        let metadata = file.metadata().map_err(reason)?;
        let before = Stamp::from(&metadata);
        if (before.device, before.inode) != (request.device.get(), request.inode.get()) {
            return Err(Replaced);
        }
        if !metadata.is_file() {
            return Err(NotRegularFile);
        }
        if denied(&absolute, &metadata, registry) {
            return Err(Protected);
        }
        if before.size > MAX_BODY_BYTES as u64 {
            return Err(TooLarge);
        }
        let mut bytes = Vec::new();
        (&file)
            .take(MAX_BODY_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(reason)?;
        #[cfg(test)]
        if let Some(hook) = &self.before_final_check {
            hook();
        }
        let after = file.metadata().map(|m| Stamp::from(&m)).map_err(reason)?;
        let still_named = status_at(&directory, OsStr::new(name)).is_ok_and(|s| identity(&s));
        if before != after || !still_named || bytes.len() as u64 != before.size {
            return Err(RevisionConflict);
        }
        if bytes.len() > MAX_BODY_BYTES {
            return Err(TooLarge);
        }
        String::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0'))
            .ok_or(Binary)
    }

    fn handle(&mut self, request: &Request) -> Result<SuccessResult, ProtocolError> {
        match &request.body {
            RequestBody::BrowseList(params) => self.list(params).map(SuccessResult::BrowseList),
            RequestBody::BrowseRead(params) => self.read(params).map(SuccessResult::BrowseRead),
            _ => Err(failure(
                ErrorCode::CapabilityUnavailable,
                "Unavailable in a browsing-only helper.",
            )),
        }
    }
}

/// Serves until EOF or an accepted shutdown. The client must say hello first.
pub(crate) async fn serve<R, W>(
    mut browser: HomeBrowser,
    reader: R,
    writer: W,
) -> Result<Exit, ServiceError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut input, mut workers) = transport::workers(reader, writer, Duration::from_secs(5));
    let mut client = None;
    let mut exit = Exit::Eof;
    while let Some(request) = input.recv().await {
        let outcome = match (&request.body, &client) {
            (RequestBody::Hello, None) => {
                client = Some(request.client_id.clone());
                Ok(SuccessResult::Hello(Hello {
                    protocol_version: polaris_desktop_protocol::ProtocolVersion,
                    engine_epoch: EngineEpoch::new(browser.epoch.clone())
                        .map_err(|_| ServiceError::Options)?,
                    capabilities: vec![Capability::HomeBrowse, Capability::Shutdown],
                    limits: Limits::default(),
                }))
            }
            (_, Some(id)) if *id != request.client_id => Err(failure(
                ErrorCode::PermissionDenied,
                "Not the connected client.",
            )),
            (_, None) | (RequestBody::Hello, Some(_)) => Err(failure(
                ErrorCode::InvalidRequest,
                "hello is required first.",
            )),
            (RequestBody::ShutdownRequest(params), Some(_)) => {
                if params.engine_epoch.as_str() == browser.epoch {
                    exit = Exit::Ready;
                    Ok(SuccessResult::ShutdownRequest(ShutdownResult {
                        engine_epoch: params.engine_epoch.clone(),
                        state: ShutdownState::Ready,
                    }))
                } else {
                    Err(failure(
                        ErrorCode::RevisionConflict,
                        "The helper has restarted.",
                    ))
                }
            }
            (_, Some(_)) => {
                let request = request.clone();
                let (returned, outcome) = tokio::task::spawn_blocking(move || {
                    let outcome = browser.handle(&request);
                    (browser, outcome)
                })
                .await
                .map_err(|_| ServiceError::Worker)?;
                browser = returned;
                outcome
            }
        };
        let sent = Response::for_request(&request, outcome)
            .map_err(|_| ServiceError::Worker)
            .and_then(|response| out.send(&response));
        if sent.is_err() {
            workers.abort_all();
            return Ok(Exit::OutputClosed);
        }
        if exit == Exit::Ready {
            break;
        }
    }
    drop(out);
    drop(input);
    if exit != Exit::Ready {
        workers.abort_all();
        return Ok(exit);
    }
    let flushed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(done) = workers.join_next().await {
            if let Ok(WorkerExit::Writer(result)) = done {
                return result;
            }
        }
        Err(ServiceError::Worker)
    })
    .await;
    Ok(if matches!(flushed, Ok(Ok(()))) {
        Exit::Ready
    } else {
        Exit::OutputClosed
    })
}

/// Production entry: the root comes from the OS account record only.
pub async fn serve_home_browser<R, W>(
    reader: R,
    writer: W,
) -> Result<Exit, Box<dyn std::error::Error>>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let home = crate::trusted_user_home()?;
    polaris_auth::protection::use_trusted_home(&home)?;
    let browser = tokio::task::spawn_blocking(move || HomeBrowser::open(home)).await??;
    Ok(serve(browser, reader, writer).await?)
}

#[cfg(test)]
mod tests;
