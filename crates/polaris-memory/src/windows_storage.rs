//! Dedicated SQLite VFS. Filesystem callbacks never forward to a pathname VFS.

use polaris_auth::{
    AuthError,
    protection::{
        WindowsFileIdentity, WindowsPrivateRoot, WindowsSqliteFile, WindowsSqliteMapping,
        WindowsSqliteRangeLock,
    },
};
use rusqlite::ffi as sql;
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr, c_char, c_int, c_void},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

const PENDING: u64 = 0x4000_0000;
const RESERVED: u64 = PENDING + 1;
const SHARED: u64 = PENDING + 2;
const SHARED_BYTES: u64 = 510;
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct Context {
    root: WindowsPrivateRoot,
    parent: PathBuf,
    database: String,
    primary: WindowsSqliteFile,
    identities: Mutex<BTreeMap<String, (WindowsFileIdentity, usize)>>,
    closed: Mutex<BTreeMap<String, WindowsFileIdentity>>,
    poisoned: AtomicBool,
    auxiliary: *mut sql::sqlite3_vfs,
}

pub(crate) struct Storage {
    vfs: Box<sql::sqlite3_vfs>,
    name: CString,
    _context: Box<Arc<Context>>,
}

fn protection(error: AuthError) -> crate::Error {
    match error {
        AuthError::Io(error) => error.into(),
        _ => crate::Error::InvalidInput("protected SQLite storage is unavailable"),
    }
}
fn io_code(error: AuthError, code: c_int) -> c_int {
    if matches!(error, AuthError::Busy) {
        sql::SQLITE_BUSY
    } else {
        code
    }
}
fn guarded(f: impl FnOnce() -> std::result::Result<(), c_int>) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => sql::SQLITE_OK,
        Ok(Err(code)) => code,
        Err(_) => sql::SQLITE_IOERR,
    }
}

impl Storage {
    pub(crate) fn new(path: &Path, create: bool) -> crate::Result<Self> {
        let text = path
            .as_os_str()
            .to_str()
            .ok_or(crate::Error::InvalidInput("SQLite path encoding"))?;
        let name = text
            .rsplit('\\')
            .next()
            .ok_or(crate::Error::InvalidInput("SQLite component"))?;
        if path.file_name() != Some(OsStr::new(name)) {
            return Err(crate::Error::InvalidInput("SQLite component"));
        }
        let parent = path
            .parent()
            .ok_or(crate::Error::InvalidInput("SQLite parent"))?;
        let root = WindowsPrivateRoot::open_existing(parent).map_err(protection)?;
        // Existing foreign sidecars must fail before database creation/schema writes.
        for suffix in ["-journal", "-wal", "-shm"] {
            match root.open_sqlite_file(OsStr::new(&format!("{name}{suffix}")), false, false, false)
            {
                Ok(file) => {
                    file.revalidate().map_err(protection)?;
                }
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(protection(error)),
            }
        }
        let primary = root
            .open_sqlite_file(OsStr::new(name), true, create, false)
            .map_err(protection)?;
        let auxiliary = unsafe { sql::sqlite3_vfs_find(c"win32".as_ptr()) };
        if auxiliary.is_null() {
            return Err(crate::Error::InvalidInput("SQLite Windows VFS missing"));
        }
        let context = Arc::new(Context {
            root,
            parent: parent.to_owned(),
            database: name.to_owned(),
            primary,
            identities: Mutex::new(BTreeMap::new()),
            closed: Mutex::new(BTreeMap::new()),
            poisoned: AtomicBool::new(false),
            auxiliary,
        });
        context
            .verify()
            .map_err(|_| crate::Error::InvalidInput("SQLite protection changed"))?;
        let mut boxed = Box::new(context);
        let name = CString::new(format!(
            "polaris-protected-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(|_| crate::Error::InvalidInput("VFS name"))?;
        let mut vfs: Box<sql::sqlite3_vfs> = Box::new(unsafe { std::mem::zeroed() });
        vfs.iVersion = 2;
        vfs.szOsFile = std::mem::size_of::<NativeFile>() as c_int;
        vfs.mxPathname = 32_767;
        vfs.zName = name.as_ptr();
        vfs.pAppData = (&mut *boxed as *mut Arc<Context>).cast();
        vfs.xOpen = Some(open);
        vfs.xDelete = Some(delete);
        vfs.xAccess = Some(access);
        vfs.xFullPathname = Some(full_path);
        vfs.xRandomness = Some(randomness);
        vfs.xSleep = Some(sleep);
        vfs.xCurrentTime = Some(time);
        vfs.xCurrentTimeInt64 = Some(time64);
        let code = unsafe { sql::sqlite3_vfs_register(&mut *vfs, 0) };
        if code != sql::SQLITE_OK {
            return Err(rusqlite::Error::SqliteFailure(sql::Error::new(code), None).into());
        }
        Ok(Self {
            vfs,
            name,
            _context: boxed,
        })
    }
    pub(crate) fn name(&self) -> &str {
        self.name.to_str().expect("generated ASCII VFS name")
    }
    pub(crate) fn attach_authorizer(&self, connection: &rusqlite::Connection) -> crate::Result<()> {
        let code = unsafe {
            sql::sqlite3_set_authorizer(connection.handle(), Some(authorize), self.vfs.pAppData)
        };
        if code != sql::SQLITE_OK {
            return Err(rusqlite::Error::SqliteFailure(sql::Error::new(code), None).into());
        }
        Ok(())
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        unsafe { sql::sqlite3_vfs_unregister(&mut *self.vfs) };
    }
}
// Connection owns all use and closes before this registration. SQLite serializes
// each connection; mapped pointers are carried by its private C file state.
unsafe impl Send for Storage {}

impl Context {
    fn verify(&self) -> std::result::Result<(), c_int> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(sql::SQLITE_IOERR);
        }
        self.root.revalidate().map_err(|_| sql::SQLITE_IOERR)?;
        self.primary.revalidate().map_err(|_| sql::SQLITE_IOERR)
    }
    fn verify_family(&self) -> std::result::Result<(), c_int> {
        self.verify()?;
        for suffix in ["-journal", "-wal", "-shm"] {
            let name = format!("{}{suffix}", self.database);
            match self
                .root
                .open_sqlite_file(OsStr::new(&name), false, false, false)
            {
                Ok(file) => {
                    let identities = self.identities.lock().map_err(|_| sql::SQLITE_IOERR)?;
                    if identities
                        .get(&name)
                        .is_some_and(|(expected, _)| *expected != file.identity())
                    {
                        return Err(sql::SQLITE_IOERR);
                    }
                }
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    if self
                        .identities
                        .lock()
                        .map_err(|_| sql::SQLITE_IOERR)?
                        .contains_key(&name)
                    {
                        return Err(sql::SQLITE_IOERR);
                    }
                }
                Err(_) => return Err(sql::SQLITE_IOERR),
            }
        }
        Ok(())
    }
    fn component(&self, name: *const c_char) -> std::result::Result<String, c_int> {
        self.verify()?;
        if name.is_null() {
            return Err(sql::SQLITE_CANTOPEN);
        }
        let value = unsafe { CStr::from_ptr(name) }
            .to_str()
            .map_err(|_| sql::SQLITE_CANTOPEN)?;
        let path = Path::new(value);
        if path.parent() != Some(self.parent.as_path()) {
            return Err(sql::SQLITE_CANTOPEN);
        }
        let leaf = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or(sql::SQLITE_CANTOPEN)?;
        let suffix = leaf
            .strip_prefix(&self.database)
            .ok_or(sql::SQLITE_CANTOPEN)?;
        if !suffix.is_empty()
            && !(suffix.starts_with('-')
                && suffix.len() <= 12
                && suffix[1..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        {
            return Err(sql::SQLITE_CANTOPEN);
        }
        Ok(leaf.to_owned())
    }
    fn remove(&self, name: &str, expected: WindowsFileIdentity) -> std::result::Result<(), c_int> {
        self.verify()?;
        self.root
            .delete_sqlite_sidecar(OsStr::new(name), expected)
            .map_err(|_| sql::SQLITE_IOERR_DELETE)?;
        self.closed
            .lock()
            .map_err(|_| sql::SQLITE_IOERR)?
            .remove(name);
        Ok(())
    }
}

unsafe fn context<'a>(vfs: *mut sql::sqlite3_vfs) -> &'a Arc<Context> {
    unsafe { &*((*vfs).pAppData.cast::<Arc<Context>>()) }
}
unsafe extern "C" fn authorize(
    data: *mut c_void,
    _: c_int,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> c_int {
    let context = unsafe { &*data.cast::<Arc<Context>>() };
    if guarded(|| context.verify_family()) == sql::SQLITE_OK {
        sql::SQLITE_OK
    } else {
        sql::SQLITE_DENY
    }
}

#[repr(C)]
struct NativeFile {
    base: sql::sqlite3_file,
    state: *mut FileState,
}
struct FileState {
    file: WindowsSqliteFile,
    context: Arc<Context>,
    name: String,
    delete_on_close: bool,
    level: c_int,
    shared: Option<WindowsSqliteRangeLock>,
    reserved: Option<WindowsSqliteRangeLock>,
    pending: Option<WindowsSqliteRangeLock>,
    exclusive: Option<WindowsSqliteRangeLock>,
    shm: Option<Shm>,
}
struct Shm {
    file: WindowsSqliteFile,
    deadman: Option<WindowsSqliteRangeLock>,
    mappings: BTreeMap<c_int, WindowsSqliteMapping>,
    locks: BTreeMap<c_int, (bool, WindowsSqliteRangeLock)>,
    region_size: c_int,
}
impl FileState {
    fn verify(&self) -> std::result::Result<(), c_int> {
        self.context.verify()?;
        self.file.revalidate().map_err(|_| sql::SQLITE_IOERR)?;
        if let Some(shm) = &self.shm {
            shm.file.revalidate().map_err(|_| sql::SQLITE_IOERR)?;
        }
        Ok(())
    }
}
unsafe fn state<'a>(file: *mut sql::sqlite3_file) -> &'a mut FileState {
    unsafe {
        &mut *((file.cast::<NativeFile>())
            .as_ref()
            .expect("SQLite file")
            .state)
    }
}

unsafe extern "C" fn open(
    vfs: *mut sql::sqlite3_vfs,
    name: *const c_char,
    file: *mut sql::sqlite3_file,
    flags: c_int,
    output: *mut c_int,
) -> c_int {
    unsafe {
        (*file).pMethods = null();
        (*file.cast::<NativeFile>()).state = null_mut();
    }
    guarded(|| {
        let context = unsafe { context(vfs) }.clone();
        let leaf = if name.is_null() {
            if flags & sql::SQLITE_OPEN_DELETEONCLOSE == 0 {
                return Err(sql::SQLITE_CANTOPEN);
            }
            format!(
                "{}-t{}",
                context.database,
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )
        } else {
            context.component(name)?
        };
        let writable = flags & sql::SQLITE_OPEN_READWRITE != 0;
        let creating = flags & sql::SQLITE_OPEN_CREATE != 0;
        let exclusive = name.is_null() || flags & sql::SQLITE_OPEN_EXCLUSIVE != 0;
        let observed = match context
            .root
            .open_sqlite_file(OsStr::new(&leaf), false, false, false)
        {
            Ok(file) => {
                let identity = file.identity();
                if context
                    .identities
                    .lock()
                    .map_err(|_| sql::SQLITE_IOERR)?
                    .get(&leaf)
                    .is_some_and(|(expected, _)| *expected != identity)
                {
                    return Err(sql::SQLITE_CANTOPEN);
                }
                Some(identity)
            }
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if context
                    .identities
                    .lock()
                    .map_err(|_| sql::SQLITE_IOERR)?
                    .contains_key(&leaf)
                {
                    return Err(sql::SQLITE_CANTOPEN);
                }
                None
            }
            Err(_) => return Err(sql::SQLITE_CANTOPEN),
        };
        let native = context
            .root
            .open_sqlite_file(OsStr::new(&leaf), writable, creating, exclusive)
            .map_err(|error| io_code(error, sql::SQLITE_CANTOPEN))?;
        if observed.is_some_and(|identity| identity != native.identity()) {
            return Err(sql::SQLITE_CANTOPEN);
        }
        if leaf == context.database && native.identity() != context.primary.identity() {
            return Err(sql::SQLITE_CANTOPEN);
        }
        {
            let mut live = context.identities.lock().map_err(|_| sql::SQLITE_IOERR)?;
            let entry = live.entry(leaf.clone()).or_insert((native.identity(), 0));
            if entry.0 != native.identity() {
                return Err(sql::SQLITE_CANTOPEN);
            }
            entry.1 = entry.1.checked_add(1).ok_or(sql::SQLITE_IOERR)?;
        }
        context
            .closed
            .lock()
            .map_err(|_| sql::SQLITE_IOERR)?
            .remove(&leaf);
        let value = Box::new(FileState {
            file: native,
            context,
            name: leaf,
            delete_on_close: flags & sql::SQLITE_OPEN_DELETEONCLOSE != 0,
            level: sql::SQLITE_LOCK_NONE,
            shared: None,
            reserved: None,
            pending: None,
            exclusive: None,
            shm: None,
        });
        unsafe {
            (*file.cast::<NativeFile>()).state = Box::into_raw(value);
            (*file).pMethods = &METHODS;
            if !output.is_null() {
                *output = flags;
            }
        }
        Ok(())
    })
}
unsafe extern "C" fn close(file: *mut sql::sqlite3_file) -> c_int {
    guarded(|| {
        let pointer = unsafe { (*file.cast::<NativeFile>()).state };
        if pointer.is_null() {
            return Ok(());
        }
        let mut value = unsafe { Box::from_raw(pointer) };
        unsafe {
            (*file.cast::<NativeFile>()).state = null_mut();
            (*file).pMethods = null();
        }
        let cleanup = clear_shm(&mut value, false);
        let delete = value.delete_on_close;
        let identity = value.file.identity();
        let context = value.context.clone();
        let name = value.name.clone();
        drop(value); // Close the actual file before requesting DELETE access.
        {
            let mut live = context.identities.lock().map_err(|_| sql::SQLITE_IOERR)?;
            let entry = live.get_mut(&name).ok_or(sql::SQLITE_IOERR)?;
            if entry.0 != identity || entry.1 == 0 {
                return Err(sql::SQLITE_IOERR);
            }
            entry.1 -= 1;
            if entry.1 == 0 {
                live.remove(&name);
                context
                    .closed
                    .lock()
                    .map_err(|_| sql::SQLITE_IOERR)?
                    .insert(name.clone(), identity);
            }
        }
        cleanup?;
        if delete {
            context.remove(&name, identity)?;
        }
        Ok(())
    })
}
unsafe extern "C" fn read(
    file: *mut sql::sqlite3_file,
    output: *mut c_void,
    length: c_int,
    offset: i64,
) -> c_int {
    guarded(|| {
        if length < 0 || offset < 0 || output.is_null() {
            return Err(sql::SQLITE_IOERR_READ);
        }
        let bytes = unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), length as usize) };
        let value = unsafe { state(file) };
        value.verify()?;
        let n = value
            .file
            .read_at(offset as u64, bytes)
            .map_err(|_| sql::SQLITE_IOERR_READ)?;
        if n != bytes.len() {
            bytes[n..].fill(0);
            return Err(sql::SQLITE_IOERR_SHORT_READ);
        }
        Ok(())
    })
}
unsafe extern "C" fn write(
    file: *mut sql::sqlite3_file,
    input: *const c_void,
    length: c_int,
    offset: i64,
) -> c_int {
    guarded(|| {
        if length < 0 || offset < 0 || input.is_null() {
            return Err(sql::SQLITE_IOERR_WRITE);
        }
        let bytes = unsafe { std::slice::from_raw_parts(input.cast::<u8>(), length as usize) };
        let value = unsafe { state(file) };
        value.verify()?;
        value
            .file
            .write_at(offset as u64, bytes)
            .map_err(|_| sql::SQLITE_IOERR_WRITE)
    })
}
unsafe extern "C" fn truncate(file: *mut sql::sqlite3_file, size: i64) -> c_int {
    guarded(|| {
        if size < 0 {
            return Err(sql::SQLITE_IOERR_TRUNCATE);
        }
        let value = unsafe { state(file) };
        value.verify()?;
        value
            .file
            .truncate(size as u64)
            .map_err(|_| sql::SQLITE_IOERR_TRUNCATE)
    })
}
unsafe extern "C" fn sync(file: *mut sql::sqlite3_file, _: c_int) -> c_int {
    guarded(|| {
        let value = unsafe { state(file) };
        value.verify()?;
        value.file.flush().map_err(|_| sql::SQLITE_IOERR_FSYNC)
    })
}
unsafe extern "C" fn size(file: *mut sql::sqlite3_file, output: *mut i64) -> c_int {
    guarded(|| {
        let value = unsafe { state(file) };
        value.verify()?;
        let n = value
            .file
            .file_size()
            .map_err(|_| sql::SQLITE_IOERR_FSTAT)?;
        unsafe { *output = i64::try_from(n).map_err(|_| sql::SQLITE_IOERR_FSTAT)? };
        Ok(())
    })
}
fn release(lock: &mut Option<WindowsSqliteRangeLock>) -> std::result::Result<(), c_int> {
    if let Some(lock) = lock.take() {
        lock.release().map_err(|_| sql::SQLITE_IOERR_UNLOCK)?;
    }
    Ok(())
}
unsafe extern "C" fn lock(file: *mut sql::sqlite3_file, level: c_int) -> c_int {
    guarded(|| {
        let value = unsafe { state(file) };
        value.verify()?;
        if !(sql::SQLITE_LOCK_SHARED..=sql::SQLITE_LOCK_EXCLUSIVE).contains(&level) {
            return Err(sql::SQLITE_IOERR_LOCK);
        }
        if level <= value.level {
            return Ok(());
        }
        if value.level == sql::SQLITE_LOCK_NONE {
            let gate = value
                .file
                .try_lock_range(PENDING, 1, false)
                .map_err(|e| io_code(e, sql::SQLITE_IOERR_LOCK))?;
            let shared = value
                .file
                .try_lock_range(SHARED, SHARED_BYTES, false)
                .map_err(|e| io_code(e, sql::SQLITE_IOERR_LOCK))?;
            gate.release().map_err(|_| sql::SQLITE_IOERR_UNLOCK)?;
            value.shared = Some(shared);
            value.level = sql::SQLITE_LOCK_SHARED;
        }
        if level == sql::SQLITE_LOCK_RESERVED {
            value.reserved = Some(
                value
                    .file
                    .try_lock_range(RESERVED, 1, true)
                    .map_err(|e| io_code(e, sql::SQLITE_IOERR_LOCK))?,
            );
            value.level = level;
            return Ok(());
        }
        if level >= sql::SQLITE_LOCK_PENDING && value.pending.is_none() {
            value.pending = Some(
                value
                    .file
                    .try_lock_range(PENDING, 1, true)
                    .map_err(|e| io_code(e, sql::SQLITE_IOERR_LOCK))?,
            );
            value.level = sql::SQLITE_LOCK_PENDING;
        }
        if level == sql::SQLITE_LOCK_EXCLUSIVE {
            release(&mut value.shared)?;
            match value.file.try_lock_range(SHARED, SHARED_BYTES, true) {
                Ok(lock) => {
                    value.exclusive = Some(lock);
                    value.level = level;
                }
                Err(error) => {
                    value.shared = Some(
                        value
                            .file
                            .try_lock_range(SHARED, SHARED_BYTES, false)
                            .map_err(|e| io_code(e, sql::SQLITE_IOERR_LOCK))?,
                    );
                    return Err(io_code(error, sql::SQLITE_IOERR_LOCK));
                }
            }
        }
        Ok(())
    })
}
unsafe extern "C" fn unlock(file: *mut sql::sqlite3_file, level: c_int) -> c_int {
    guarded(|| {
        let value = unsafe { state(file) };
        if ![sql::SQLITE_LOCK_NONE, sql::SQLITE_LOCK_SHARED].contains(&level) {
            return Err(sql::SQLITE_IOERR_UNLOCK);
        }
        release(&mut value.exclusive)?;
        if level == sql::SQLITE_LOCK_SHARED && value.shared.is_none() {
            value.shared = Some(
                value
                    .file
                    .try_lock_range(SHARED, SHARED_BYTES, false)
                    .map_err(|e| io_code(e, sql::SQLITE_IOERR_LOCK))?,
            );
        }
        release(&mut value.reserved)?;
        release(&mut value.pending)?;
        if level == sql::SQLITE_LOCK_NONE {
            release(&mut value.shared)?;
        }
        value.level = level;
        Ok(())
    })
}
unsafe extern "C" fn reserved(file: *mut sql::sqlite3_file, output: *mut c_int) -> c_int {
    guarded(|| {
        let value = unsafe { state(file) };
        value.verify()?;
        let held = if value.level >= sql::SQLITE_LOCK_RESERVED {
            true
        } else {
            match value.file.try_lock_range(RESERVED, 1, false) {
                Ok(lock) => {
                    lock.release().map_err(|_| sql::SQLITE_IOERR_UNLOCK)?;
                    false
                }
                Err(AuthError::Busy) => true,
                Err(_) => return Err(sql::SQLITE_IOERR_CHECKRESERVEDLOCK),
            }
        };
        unsafe { *output = i32::from(held) };
        Ok(())
    })
}
unsafe extern "C" fn control(file: *mut sql::sqlite3_file, op: c_int, value: *mut c_void) -> c_int {
    if op == sql::SQLITE_FCNTL_LOCKSTATE {
        unsafe { *value.cast::<c_int>() = state(file).level };
        sql::SQLITE_OK
    } else {
        sql::SQLITE_NOTFOUND
    }
}
unsafe extern "C" fn sector(file: *mut sql::sqlite3_file) -> c_int {
    let value = unsafe { state(file) };
    match value.file.sector_size() {
        Ok(bytes) => bytes as c_int,
        Err(_) => {
            value.context.poisoned.store(true, Ordering::Release);
            512
        }
    }
}
unsafe extern "C" fn characteristics(_: *mut sql::sqlite3_file) -> c_int {
    0
}

fn initialize_shm(value: &mut FileState) -> std::result::Result<(), c_int> {
    if value.shm.is_some() {
        return Ok(());
    }
    let name = format!("{}-shm", value.context.database);
    let file = value
        .context
        .root
        .open_sqlite_file(OsStr::new(&name), true, true, false)
        .map_err(|_| sql::SQLITE_IOERR_SHMOPEN)?;
    let deadman = match file.try_lock_range(128, 1, true) {
        Ok(exclusive) => {
            file.truncate(0).map_err(|_| sql::SQLITE_IOERR_SHMSIZE)?;
            let shared = file
                .try_lock_range(128, 1, false)
                .map_err(|e| io_code(e, sql::SQLITE_IOERR_SHMLOCK))?;
            exclusive.release().map_err(|_| sql::SQLITE_IOERR_SHMLOCK)?;
            shared
        }
        Err(AuthError::Busy) => file
            .try_lock_range(128, 1, false)
            .map_err(|e| io_code(e, sql::SQLITE_IOERR_SHMLOCK))?,
        Err(_) => return Err(sql::SQLITE_IOERR_SHMLOCK),
    };
    value.shm = Some(Shm {
        file,
        deadman: Some(deadman),
        mappings: BTreeMap::new(),
        locks: BTreeMap::new(),
        region_size: 0,
    });
    Ok(())
}
unsafe extern "C" fn shm_map(
    file: *mut sql::sqlite3_file,
    region: c_int,
    bytes: c_int,
    extend: c_int,
    output: *mut *mut c_void,
) -> c_int {
    unsafe { *output = null_mut() };
    guarded(|| {
        if region < 0 || bytes <= 0 {
            return Err(sql::SQLITE_IOERR_SHMMAP);
        }
        let value = unsafe { state(file) };
        value.context.verify()?;
        initialize_shm(value)?;
        let shm = value.shm.as_mut().ok_or(sql::SQLITE_IOERR_SHMMAP)?;
        if shm.region_size != 0 && shm.region_size != bytes {
            return Err(sql::SQLITE_IOERR_SHMSIZE);
        }
        shm.region_size = bytes;
        let offset = (region as u64)
            .checked_mul(bytes as u64)
            .ok_or(sql::SQLITE_IOERR_SHMSIZE)?;
        let end = offset
            .checked_add(bytes as u64)
            .ok_or(sql::SQLITE_IOERR_SHMSIZE)?;
        if shm
            .file
            .file_size()
            .map_err(|_| sql::SQLITE_IOERR_SHMSIZE)?
            < end
        {
            if extend == 0 {
                return Ok(());
            }
            shm.file
                .ensure_len(end)
                .map_err(|_| sql::SQLITE_IOERR_SHMSIZE)?;
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = shm.mappings.entry(region) {
            entry.insert(
                shm.file
                    .map_shared(offset, bytes as usize)
                    .map_err(|_| sql::SQLITE_IOERR_SHMMAP)?,
            );
        }
        let mapping = shm.mappings.get(&region).ok_or(sql::SQLITE_IOERR_SHMMAP)?;
        mapping.revalidate().map_err(|_| sql::SQLITE_IOERR_SHMMAP)?;
        unsafe { *output = mapping.as_mut_ptr() };
        Ok(())
    })
}
unsafe extern "C" fn shm_lock(
    file: *mut sql::sqlite3_file,
    offset: c_int,
    n: c_int,
    flags: c_int,
) -> c_int {
    guarded(|| {
        if offset < 0 || n <= 0 || offset.checked_add(n).is_none_or(|end| end > 8) {
            return Err(sql::SQLITE_IOERR_SHMLOCK);
        }
        let lock = flags & sql::SQLITE_SHM_LOCK != 0;
        let exclusive = flags & sql::SQLITE_SHM_EXCLUSIVE != 0;
        if ![
            sql::SQLITE_SHM_LOCK | sql::SQLITE_SHM_SHARED,
            sql::SQLITE_SHM_LOCK | sql::SQLITE_SHM_EXCLUSIVE,
            sql::SQLITE_SHM_UNLOCK | sql::SQLITE_SHM_SHARED,
            sql::SQLITE_SHM_UNLOCK | sql::SQLITE_SHM_EXCLUSIVE,
        ]
        .contains(&flags)
        {
            return Err(sql::SQLITE_IOERR_SHMLOCK);
        }
        let value = unsafe { state(file) };
        value.context.verify()?;
        initialize_shm(value)?;
        let shm = value.shm.as_mut().ok_or(sql::SQLITE_IOERR_SHMLOCK)?;
        if lock {
            let mut acquired = Vec::new();
            for at in offset..offset + n {
                if let Some((was_exclusive, _)) = shm.locks.get(&at) {
                    if *was_exclusive != exclusive {
                        return Err(sql::SQLITE_IOERR_SHMLOCK);
                    }
                } else {
                    let held = shm
                        .file
                        .try_lock_range(120 + at as u64, 1, exclusive)
                        .map_err(|e| io_code(e, sql::SQLITE_IOERR_SHMLOCK))?;
                    acquired.push((at, (exclusive, held)));
                }
            }
            // A failed acquisition drops only the new locks; existing grants survive.
            shm.locks.extend(acquired);
        } else {
            for at in offset..offset + n {
                if shm
                    .locks
                    .get(&at)
                    .is_none_or(|(mode, _)| *mode != exclusive)
                {
                    return Err(sql::SQLITE_IOERR_SHMLOCK);
                }
            }
            for at in offset..offset + n {
                let (_, held) = shm.locks.remove(&at).ok_or(sql::SQLITE_IOERR_SHMLOCK)?;
                held.release().map_err(|_| sql::SQLITE_IOERR_SHMLOCK)?;
            }
        }
        Ok(())
    })
}
unsafe extern "C" fn shm_barrier(file: *mut sql::sqlite3_file) {
    std::sync::atomic::fence(Ordering::SeqCst);
    let value = unsafe { state(file) };
    if value.context.verify().is_err()
        || value
            .shm
            .as_ref()
            .is_some_and(|shm| shm.file.revalidate().is_err())
    {
        value.context.poisoned.store(true, Ordering::Release);
    }
}
fn clear_shm(value: &mut FileState, delete: bool) -> std::result::Result<(), c_int> {
    if let Some(mut shm) = value.shm.take() {
        for mapping in shm.mappings.values_mut() {
            mapping.unmap().map_err(|_| sql::SQLITE_IOERR_SHMMAP)?;
        }
        let identity = shm.file.identity();
        let mut last_user = None;
        if delete {
            for (_, (_, held)) in std::mem::take(&mut shm.locks) {
                held.release().map_err(|_| sql::SQLITE_IOERR_SHMLOCK)?;
            }
            release(&mut shm.deadman)?;
            last_user = match shm.file.try_lock_range(128, 1, true) {
                Ok(held) => Some(held),
                Err(AuthError::Busy) => None,
                Err(_) => return Err(sql::SQLITE_IOERR_SHMLOCK),
            };
        }
        let remove = delete && last_user.is_some();
        drop(shm);
        drop(last_user);
        if remove {
            value
                .context
                .remove(&format!("{}-shm", value.context.database), identity)?;
        }
    }
    Ok(())
}
unsafe extern "C" fn shm_unmap(file: *mut sql::sqlite3_file, delete: c_int) -> c_int {
    guarded(|| clear_shm(unsafe { state(file) }, delete != 0))
}

static METHODS: sql::sqlite3_io_methods = sql::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn delete(vfs: *mut sql::sqlite3_vfs, name: *const c_char, _: c_int) -> c_int {
    guarded(|| {
        let context = unsafe { context(vfs) };
        let name = context.component(name)?;
        if name == context.database {
            return Err(sql::SQLITE_IOERR_DELETE);
        }
        let native = match context
            .root
            .open_sqlite_file(OsStr::new(&name), false, false, false)
        {
            Ok(file) => file,
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(_) => return Err(sql::SQLITE_IOERR_DELETE),
        };
        let identity = native.identity();
        if context
            .identities
            .lock()
            .map_err(|_| sql::SQLITE_IOERR)?
            .contains_key(&name)
            || context
                .closed
                .lock()
                .map_err(|_| sql::SQLITE_IOERR)?
                .get(&name)
                .is_some_and(|expected| *expected != identity)
        {
            return Err(sql::SQLITE_IOERR_DELETE);
        }
        drop(native);
        context.remove(&name, identity)
    })
}
unsafe extern "C" fn access(
    vfs: *mut sql::sqlite3_vfs,
    name: *const c_char,
    _: c_int,
    output: *mut c_int,
) -> c_int {
    unsafe { *output = 0 };
    guarded(|| {
        let context = unsafe { context(vfs) };
        let leaf = context.component(name)?;
        match context
            .root
            .open_sqlite_file(OsStr::new(&leaf), false, false, false)
        {
            Ok(file) => {
                file.revalidate().map_err(|_| sql::SQLITE_IOERR_ACCESS)?;
                unsafe { *output = 1 };
            }
            Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(sql::SQLITE_IOERR_ACCESS),
        }
        Ok(())
    })
}
unsafe extern "C" fn full_path(
    vfs: *mut sql::sqlite3_vfs,
    name: *const c_char,
    length: c_int,
    output: *mut c_char,
) -> c_int {
    guarded(|| {
        unsafe { context(vfs) }.component(name)?;
        let name = unsafe { CStr::from_ptr(name) }.to_bytes_with_nul();
        if length < 0 || name.len() > length as usize {
            return Err(sql::SQLITE_CANTOPEN);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(name.as_ptr().cast::<c_char>(), output, name.len())
        };
        Ok(())
    })
}
// These four built-in callbacks do not perform pathname filesystem I/O.
unsafe extern "C" fn randomness(
    vfs: *mut sql::sqlite3_vfs,
    n: c_int,
    output: *mut c_char,
) -> c_int {
    let original = unsafe { context(vfs) }.auxiliary;
    unsafe {
        (*original)
            .xRandomness
            .map_or(0, |f| f(original, n, output))
    }
}
unsafe extern "C" fn sleep(vfs: *mut sql::sqlite3_vfs, n: c_int) -> c_int {
    let original = unsafe { context(vfs) }.auxiliary;
    unsafe { (*original).xSleep.map_or(0, |f| f(original, n)) }
}
unsafe extern "C" fn time(vfs: *mut sql::sqlite3_vfs, output: *mut f64) -> c_int {
    let original = unsafe { context(vfs) }.auxiliary;
    unsafe {
        (*original)
            .xCurrentTime
            .map_or(sql::SQLITE_ERROR, |f| f(original, output))
    }
}
unsafe extern "C" fn time64(vfs: *mut sql::sqlite3_vfs, output: *mut i64) -> c_int {
    let original = unsafe { context(vfs) }.auxiliary;
    unsafe {
        (*original)
            .xCurrentTimeInt64
            .map_or(sql::SQLITE_ERROR, |f| f(original, output))
    }
}

#[cfg(test)]
mod tests;
