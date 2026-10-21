//! Adapter contracts, not evidence of a running model or current calibration.
use serde::Serialize;

/// The adapter's immediate transport, not a network confinement guarantee.
/// A local helper or loopback server can itself contact another service;
/// local-only admission also needs the runtime's enforced boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionLocation {
    #[default]
    Unknown,
    LocalProcess,
    LoopbackHttp,
    Remote,
}

impl DecisionLocation {
    pub fn is_local(self) -> bool {
        matches!(self, Self::LocalProcess | Self::LoopbackHttp)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BandCalibration {
    #[default]
    Unmeasured,
    /// Retains the existing Jev band policy and its 2026-09-17 evidence.
    /// This is not a fresh service check or calibration of arbitrary wording.
    LegacyJev,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionCancellation {
    #[default]
    Unknown,
    /// Dropping the caller does not prove that external work stopped.
    CallerFuture,
    /// An independent owner requests stop and observes/reaps the process.
    /// This does not prove that a particular GPU instruction was interrupted.
    OwnedProcess,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionUsageReporting {
    #[default]
    Unknown,
    Optional,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DecisionShapes {
    pub noul: bool,
    pub choice: bool,
    pub score: bool,
}

impl DecisionShapes {
    pub const ALL: Self = Self {
        noul: true,
        choice: true,
        score: true,
    };
}

/// Unknown limits stay unknown. A protocol's theoretical capacity must not be
/// borrowed by an unrelated model, quantization or helper implementation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DecisionCapabilities {
    pub location: DecisionLocation,
    pub protocol: Option<&'static str>,
    pub shapes: Option<DecisionShapes>,
    pub max_state_tokens: Option<usize>,
    pub max_context_tokens: Option<usize>,
    pub max_questions: Option<usize>,
    pub max_options: Option<usize>,
    pub max_frame_bytes: Option<usize>,
    pub cancellation: DecisionCancellation,
    pub usage: DecisionUsageReporting,
    pub band_calibration: BandCalibration,
}
