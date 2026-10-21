//! Read-only host memory hints. Missing GPU/availability readings stay unknown.
use super::{CapacityObservation, CapacityTier, ComputeKind, MemoryKind, MemoryTarget};
use std::time::Instant;

/// No helper, model, network or credential access. On Apple Silicon the
/// physical RAM reading is unified memory, never dedicated VRAM or GPU budget.
pub fn host_memory() -> CapacityObservation {
    CapacityObservation {
        target: MemoryTarget {
            device_id: "host-cpu".into(),
            memory: if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                MemoryKind::UnifiedMemory
            } else {
                MemoryKind::SystemRam
            },
            compute: ComputeKind::Cpu,
        },
        total_bytes: physical_memory(),
        available_bytes: None,
        working_set_limit_bytes: None,
        captured_at: Instant::now(),
    }
}

#[cfg(target_os = "macos")]
fn physical_memory() -> Option<u64> {
    let mut bytes = 0u64;
    let mut size = std::mem::size_of_val(&bytes);
    // SAFETY: the sysctl has a fixed name, a correctly sized output buffer
    // and no input buffer. A denied/short/zero reading remains unknown.
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && size == std::mem::size_of_val(&bytes) && bytes > 0).then_some(bytes)
}

#[cfg(not(target_os = "macos"))]
fn physical_memory() -> Option<u64> {
    None
}

pub fn diagnostic_lines(observation: &CapacityObservation) -> Vec<String> {
    let total = observation.total_bytes.map_or_else(
        || "unknown".to_owned(),
        |bytes| format!("{:.1} GiB", bytes as f64 / super::GIB as f64),
    );
    let memory = match observation.target.memory {
        MemoryKind::SystemRam => "system RAM (not GPU memory)",
        MemoryKind::UnifiedMemory => "unified memory (not dedicated VRAM)",
        MemoryKind::DedicatedVram => "dedicated VRAM",
    };
    let mut lines = vec![format!("Memory: {memory}; total={total}")];
    // System RAM does not identify a discrete GPU's capacity tier.
    if observation.target.memory != MemoryKind::SystemRam
        && let Some(tier) = CapacityTier::from_bytes(observation.total_bytes)
    {
        let label = match tier {
            CapacityTier::Small => "below 16 GiB (8 GiB class)",
            CapacityTier::Medium => "16 to below 24 GiB",
            CapacityTier::Large => "24 GiB or more",
        };
        lines.push(format!(
            "Recommendation hint: {label}; {}",
            tier.local_candidates().join(" > ")
        ));
    } else {
        lines.push("Local families: Clef / Clef-flash / SemIf / System One compatible; capacity order unknown".into());
    }
    lines.push("Hosted Jev/remote System One: independent of local memory; explicit configuration required".into());
    lines.push("Execution fit: not verified here; requires fresh budget and accepted model/runtime profile".into());
    lines
}
