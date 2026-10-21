//! Memory recommendations and conservative admission for a trusted model profile.
//! File sizes, CPU RSS and GPU budgets are not interchangeable measurements.
use serde::Serialize;
use std::time::{Duration, Instant};

pub const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityTier {
    Small,
    Medium,
    Large,
}

impl CapacityTier {
    /// Recommendation only: an 8 GiB hint never proves that a model fits.
    /// Smaller and intermediate devices remain selectable.
    pub fn from_bytes(bytes: Option<u64>) -> Option<Self> {
        match bytes {
            None | Some(0) => None,
            Some(bytes) if bytes < 16 * GIB => Some(Self::Small),
            Some(bytes) if bytes < 24 * GIB => Some(Self::Medium),
            Some(_) => Some(Self::Large),
        }
    }

    pub fn local_candidates(self) -> &'static [&'static str] {
        match self {
            Self::Small => &["SemIf", "System One compatible", "Clef-flash", "Clef"],
            Self::Medium => &["Clef-flash", "SemIf", "System One compatible", "Clef"],
            Self::Large => &["Clef", "Clef-flash", "SemIf", "System One compatible"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    SystemRam,
    UnifiedMemory,
    DedicatedVram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputeKind {
    Cpu,
    Gpu,
}

/// One device only. The owner must not sum devices that the runtime cannot
/// jointly use, or reuse a CPU observation as a GPU measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryTarget {
    pub device_id: String,
    pub memory: MemoryKind,
    pub compute: ComputeKind,
}

#[derive(Debug, Clone)]
pub struct CapacityObservation {
    pub target: MemoryTarget,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    /// GPU working-set limit, separate from current availability. Required
    /// for unified-memory GPU admission; physical RAM alone is insufficient.
    pub working_set_limit_bytes: Option<u64>,
    pub captured_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeakEvidence {
    Measured,
    ValidatedUpperBound,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryPeak {
    pub bytes: u64,
    pub evidence: PeakEvidence,
}

/// Supplied by the trusted activation owner for the exact model, runtime,
/// quantization and device profile it has validated. This type does not load
/// a project-authored acceptance flag or promote a smoke test to acceptance.
#[derive(Debug, Clone)]
pub struct ProfileMemory {
    pub accepted: bool,
    pub target: MemoryTarget,
    pub max_context_tokens: u32,
    pub max_parallel: u32,
    pub peak: Option<MemoryPeak>,
}

#[derive(Debug, Clone, Copy)]
pub struct RunFootprint {
    /// The runtime's allocated context capacity, not just this prompt's length.
    pub context_tokens: u32,
    pub parallel: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CapacityPolicy {
    /// Held for the OS and other work; never inferred from model file size.
    pub reserve_bytes: u64,
    /// Other allocations planned after the observation. Already-resident
    /// allocations are reflected in available_bytes and need not be counted twice.
    pub colocated_bytes: u64,
    pub budget_limit_bytes: Option<u64>,
    pub max_age: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnverifiedCapacity {
    Profile,
    Target,
    RequestShape,
    Peak,
    Observation,
    StaleObservation,
    AvailableMemory,
    WorkingSet,
    Arithmetic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapacityAssessment {
    Fits {
        budget_bytes: u64,
        required_bytes: u64,
        peak_evidence: PeakEvidence,
    },
    Insufficient {
        budget_bytes: u64,
        required_bytes: u64,
    },
    Unverified {
        reason: UnverifiedCapacity,
    },
}

/// A pure preflight, not a memory reservation or permission to send data.
/// The execution owner must recheck near dispatch and retain its own admission
/// and cancellation boundary. A user-selected backend is never replaced here.
pub fn assess(
    profile: &ProfileMemory,
    observation: &CapacityObservation,
    request: RunFootprint,
    policy: CapacityPolicy,
    now: Instant,
) -> CapacityAssessment {
    use CapacityAssessment::{Fits, Insufficient, Unverified};
    use UnverifiedCapacity as Why;
    let unknown = |reason| Unverified { reason };
    if !profile.accepted {
        return unknown(Why::Profile);
    }
    if profile.target != observation.target
        || observation.target.device_id.is_empty()
        || matches!(
            (observation.target.memory, observation.target.compute),
            (MemoryKind::DedicatedVram, ComputeKind::Cpu)
                | (MemoryKind::SystemRam, ComputeKind::Gpu)
        )
    {
        return unknown(Why::Target);
    }
    if request.context_tokens == 0
        || request.parallel == 0
        || request.context_tokens > profile.max_context_tokens
        || request.parallel > profile.max_parallel
    {
        return unknown(Why::RequestShape);
    }
    let Some(peak) = profile.peak.filter(|peak| peak.bytes > 0) else {
        return unknown(Why::Peak);
    };
    let Some(age) = now.checked_duration_since(observation.captured_at) else {
        return unknown(Why::Observation);
    };
    if age > policy.max_age {
        return unknown(Why::StaleObservation);
    }
    let Some(total) = observation.total_bytes.filter(|bytes| *bytes > 0) else {
        return unknown(Why::Observation);
    };
    let Some(available) = observation.available_bytes else {
        return unknown(Why::AvailableMemory);
    };
    if available > total {
        return unknown(Why::Observation);
    }
    let mut budget = available;
    if observation.target.memory == MemoryKind::UnifiedMemory
        && observation.target.compute == ComputeKind::Gpu
        && observation.working_set_limit_bytes.is_none()
    {
        return unknown(Why::WorkingSet);
    }
    if let Some(working_set) = observation.working_set_limit_bytes {
        budget = budget.min(working_set);
    }
    if let Some(limit) = policy.budget_limit_bytes {
        budget = budget.min(limit);
    }
    let Some(required) = peak
        .bytes
        .checked_add(policy.reserve_bytes)
        .and_then(|bytes| bytes.checked_add(policy.colocated_bytes))
    else {
        return unknown(Why::Arithmetic);
    };
    if required > budget {
        Insufficient {
            budget_bytes: budget,
            required_bytes: required,
        }
    } else {
        Fits {
            budget_bytes: budget,
            required_bytes: required,
            peak_evidence: peak.evidence,
        }
    }
}

pub mod observation;

#[cfg(test)]
mod tests;
