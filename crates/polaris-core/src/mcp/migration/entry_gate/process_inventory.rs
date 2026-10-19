//! Native inventory only. Unknown live identities are blockers, never zero.

use std::collections::BTreeSet;
use std::path::Path;

use super::{
    EntryGateErrorV2, ProductionBinaryIdentityV2, ProductionEntryManifestV2, validate_runtime_copy,
};

const MAX_PROCESSES: usize = 16384;

pub(super) fn reject_entry_path(
    manifest: &ProductionEntryManifestV2,
    path: &Path,
) -> Result<(), EntryGateErrorV2> {
    if manifest
        .entries
        .iter()
        .any(|entry| entry.binary.path == path)
    {
        // A path whose running vnode was replaced may no longer have the
        // packaged digest. It is still active/unresolved, never excluded.
        return Err(EntryGateErrorV2::ProcessActive);
    }
    if path.file_name().and_then(|part| part.to_str()) == Some("polaris")
        && path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|part| part.to_str())
            == Some("runtime")
    {
        let binary = ProductionBinaryIdentityV2::inspect(path)
            .map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        validate_runtime_copy(&binary).map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        if !manifest.runtime_copy_source_hashes.contains(&binary.sha256) {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        return Err(EntryGateErrorV2::ProcessActive);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn verify_stop_zero(
    manifest: &ProductionEntryManifestV2,
) -> Result<usize, EntryGateErrorV2> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    manifest.validate()?;
    let pids = own_pids()?;
    let own = unsafe { libc::getpid() };
    for pid in pids.iter().copied().filter(|pid| *pid != own) {
        let Some(before) = process_info(pid)? else {
            continue;
        };
        if before.pbi_status == libc::SZOMB {
            continue;
        }
        let mut buffer = [0u8; 4096];
        let length =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if length <= 0 {
            match process_info(pid)? {
                None => continue,
                Some(after) if after.pbi_status == libc::SZOMB => continue,
                _ => {
                    #[cfg(test)]
                    eprintln!("inventory unresolved: pidpath pid={pid}");
                    return Err(EntryGateErrorV2::ProcessUnresolved);
                }
            }
        }
        let path =
            CStr::from_bytes_until_nul(&buffer).map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        if path.to_bytes().is_empty() {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        let Some(after) = process_info(pid)? else {
            continue;
        };
        if after.pbi_status == libc::SZOMB {
            continue;
        }
        if (
            before.pbi_pid,
            before.pbi_uid,
            before.pbi_start_tvsec,
            before.pbi_start_tvusec,
        ) != (
            after.pbi_pid,
            after.pbi_uid,
            after.pbi_start_tvsec,
            after.pbi_start_tvusec,
        ) {
            #[cfg(test)]
            eprintln!("inventory unresolved: process identity changed pid={pid}");
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        reject_entry_path(
            manifest,
            Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())),
        )?;
    }
    // No silent loss due to a full buffer, PID churn or a raced new entry.
    let after_pids = own_pids()?;
    if after_pids != pids {
        #[cfg(test)]
        eprintln!(
            "inventory unresolved: pid set changed before={} after={} added={:?} removed={:?}",
            pids.len(),
            after_pids.len(),
            after_pids.difference(&pids).collect::<Vec<_>>(),
            pids.difference(&after_pids).collect::<Vec<_>>()
        );
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    Ok(pids.len())
}

#[cfg(target_os = "macos")]
fn own_pids() -> Result<BTreeSet<i32>, EntryGateErrorV2> {
    // macOS SDK sys/proc_info.h: effective UID filter is 4; 5 is real UID.
    const PROC_UID_ONLY: u32 = 4;
    let mut buffer = vec![0i32; MAX_PROCESSES];
    let filled = unsafe {
        libc::proc_listpids(
            PROC_UID_ONLY,
            libc::geteuid(),
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 4) as i32,
        )
    };
    if filled <= 0 || filled as usize >= buffer.len() * 4 || filled % 4 != 0 {
        #[cfg(test)]
        eprintln!(
            "inventory unresolved: list filled={filled} errno={:?}",
            std::io::Error::last_os_error().raw_os_error()
        );
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    buffer.truncate(filled as usize / 4);
    let nonzero: Vec<_> = buffer.into_iter().filter(|pid| *pid > 0).collect();
    let pids: BTreeSet<_> = nonzero.iter().copied().collect();
    if pids.is_empty() || pids.len() != nonzero.len() {
        #[cfg(test)]
        eprintln!(
            "inventory unresolved: list empty or duplicate count={}",
            nonzero.len()
        );
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    Ok(pids)
}

#[cfg(target_os = "macos")]
fn process_info(pid: i32) -> Result<Option<libc::proc_bsdinfo>, EntryGateErrorV2> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        if read <= 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        #[cfg(test)]
        eprintln!(
            "inventory unresolved: bsdinfo pid={pid} read={read} size={size} errno={:?}",
            std::io::Error::last_os_error().raw_os_error()
        );
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid as u32 || info.pbi_uid != unsafe { libc::geteuid() } {
        #[cfg(test)]
        eprintln!(
            "inventory unresolved: bsdinfo mismatch pid={pid} actual_pid={} uid={}",
            info.pbi_pid, info.pbi_uid
        );
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    Ok(Some(info))
}

#[cfg(target_os = "linux")]
pub(super) fn verify_stop_zero(
    manifest: &ProductionEntryManifestV2,
) -> Result<usize, EntryGateErrorV2> {
    use std::os::unix::fs::MetadataExt;
    manifest.validate()?;
    let pids = own_pids()?;
    let own = std::process::id();
    for pid in pids.iter().copied().filter(|pid| *pid != own) {
        let root = std::path::PathBuf::from(format!("/proc/{pid}"));
        let stat = std::fs::read_to_string(root.join("stat"))
            .map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        let end = stat.rfind(')').ok_or(EntryGateErrorV2::ProcessUnresolved)?;
        let fields: Vec<_> = stat[end + 1..].split_whitespace().collect();
        if fields.len() < 20 {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        if fields[0] == "Z" {
            continue;
        }
        let before = fields[19];
        let path = std::fs::read_link(root.join("exe"))
            .map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        if path.as_os_str().to_string_lossy().ends_with(" (deleted)") {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        let after = std::fs::read_to_string(root.join("stat"))
            .map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        let end = after
            .rfind(')')
            .ok_or(EntryGateErrorV2::ProcessUnresolved)?;
        let fields: Vec<_> = after[end + 1..].split_whitespace().collect();
        if fields.len() < 20
            || fields[19] != before
            || std::fs::metadata(&root)
                .map_err(|_| EntryGateErrorV2::ProcessUnresolved)?
                .uid()
                != unsafe { libc::geteuid() }
        {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        if fields[0] != "Z" {
            reject_entry_path(manifest, &path)?;
        }
    }
    if own_pids()? != pids {
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    Ok(pids.len())
}
#[cfg(target_os = "linux")]
fn own_pids() -> Result<BTreeSet<u32>, EntryGateErrorV2> {
    use std::os::unix::fs::MetadataExt;
    let mut pids = BTreeSet::new();
    for entry in std::fs::read_dir("/proc").map_err(|_| EntryGateErrorV2::ProcessUnresolved)? {
        let entry = entry.map_err(|_| EntryGateErrorV2::ProcessUnresolved)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let meta = match entry.metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(EntryGateErrorV2::ProcessUnresolved);
            }
            result => result.map_err(|_| EntryGateErrorV2::ProcessUnresolved)?,
        };
        if meta.uid() == unsafe { libc::geteuid() } {
            pids.insert(pid);
        }
        if pids.len() > MAX_PROCESSES {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
    }
    if pids.is_empty() {
        return Err(EntryGateErrorV2::ProcessUnresolved);
    }
    Ok(pids)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn verify_stop_zero(_: &ProductionEntryManifestV2) -> Result<usize, EntryGateErrorV2> {
    Err(EntryGateErrorV2::ProcessUnresolved)
}
