//! Physical provider attempts, durable reservations and conservative settlement.
//!
//! The ledger deliberately stores only scheduling and accounting metadata.
//! Request bodies, response bodies, and credentials never enter these records.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CompletionResponse, Usage};

tokio::task_local! { static CALL_CONTEXT: AttemptContext; }

/// Bind an independent child/summary scope without changing request bytes or
/// copying a provider's credentials. Concurrent futures retain their own scope.
pub async fn in_scope<T>(
    context: AttemptContext,
    future: impl std::future::Future<Output = T>,
) -> T {
    CALL_CONTEXT.scope(context, future).await
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptKind {
    Completion,
    Summary,
    Embedding,
    WebSearch,
    Child,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptContext {
    pub parent_id: Option<String>,
    pub kind: AttemptKind,
}

impl Default for AttemptContext {
    fn default() -> Self {
        Self {
            parent_id: None,
            kind: AttemptKind::Completion,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum UsageObservation {
    Known {
        input_tokens: u32,
        output_tokens: u32,
        total_tokens: u32,
        cached_tokens: u32,
    },
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptRecord {
    pub logical_id: String,
    pub attempt_id: String,
    pub parent_id: Option<String>,
    pub kind: AttemptKind,
    pub model: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: AttemptStatus,
    pub usage: UsageObservation,
    pub retry_reason: Option<String>,
    #[serde(default)]
    pub reservation: BudgetReservation,
}

#[derive(Debug, Default)]
struct State {
    next_logical_id: u64,
    next_attempt_id: u64,
    records: Vec<AttemptRecord>,
    reserved: BudgetTotals,
    settled: BudgetTotals,
    halted: bool,
    durable: Option<DurableLedger>,
}

#[derive(Debug)]
struct DurableLedger {
    path: PathBuf,
    _lock: std::fs::File,
}

/// Hard cap for physical sends. A 401 refresh consumes two attempts; a
/// logical request never bypasses this cap by retrying.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptBudget {
    pub max_physical_attempts: u64,
}

/// Configured aggregate ceilings for an opt-in benchmark run. Values are
/// supplied by the driver; Polaris never treats an absent or zero usage report
/// as free consumption.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetLimits {
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_hosted_actions: u64,
    pub max_cost_microusd: u64,
}

/// Opt-in endpoint contract. The caller must have verified hidden-input and
/// output/tool ceilings against this exact endpoint before enabling it.
#[derive(Debug, Clone, Copy)]
pub struct RequestCaps {
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub max_hosted_actions: u32,
    pub endpoint_contract_verified: bool,
}

impl RequestCaps {
    pub fn validate(self) -> Result<(), crate::ProviderError> {
        if !self.endpoint_contract_verified {
            return Err(crate::ProviderError::Unsupported(
                "bounded endpoint contract is not verified".into(),
            ));
        }
        if self.max_input_tokens == 0
            || self.max_input_tokens > 32_000
            || self.max_output_tokens == 0
            || self.max_output_tokens > 4096
            || self.max_hosted_actions > 2
        {
            return Err(crate::ProviderError::Budget(
                "invalid request ceilings".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn apply(
        self,
        body: &mut serde_json::Value,
    ) -> Result<BudgetReservation, crate::ProviderError> {
        self.validate()?;
        // UTF-8 byte count is a conservative bound for visible byte-level input.
        // Hidden provider input is covered only by the verified endpoint contract.
        let bytes = serde_json::to_vec(body)
            .map_err(|e| crate::ProviderError::Budget(e.to_string()))?
            .len();
        if bytes > self.max_input_tokens as usize {
            return Err(crate::ProviderError::Budget(
                "visible request exceeds conservative input bound".into(),
            ));
        }
        body["max_output_tokens"] = self.max_output_tokens.into();
        if self.max_hosted_actions > 0 {
            body["max_tool_calls"] = self.max_hosted_actions.into();
        }
        body["truncation"] = "disabled".into();
        Ok(BudgetReservation::new(
            u64::from(self.max_input_tokens),
            u64::from(self.max_output_tokens),
            u64::from(self.max_hosted_actions),
            (u64::from(self.max_input_tokens) * 25).div_ceil(2)
                + u64::from(self.max_output_tokens) * 50
                + u64::from(self.max_hosted_actions) * 10_000,
        ))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetReservation {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub hosted_actions: u64,
    pub cost_microusd: u64,
}

impl BudgetReservation {
    pub fn new(
        input_tokens: u64,
        output_tokens: u64,
        hosted_actions: u64,
        cost_microusd: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            hosted_actions,
            cost_microusd,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub hosted_actions: u64,
    pub cost_microusd: u64,
}

impl BudgetTotals {
    fn add(&mut self, value: BudgetReservation) {
        self.input_tokens = self.input_tokens.saturating_add(value.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(value.output_tokens);
        self.hosted_actions = self.hosted_actions.saturating_add(value.hosted_actions);
        self.cost_microusd = self.cost_microusd.saturating_add(value.cost_microusd);
    }
    fn exceeds(self, limits: BudgetLimits) -> bool {
        self.input_tokens > limits.max_input_tokens
            || self.output_tokens > limits.max_output_tokens
            || self.hosted_actions > limits.max_hosted_actions
            || self.cost_microusd > limits.max_cost_microusd
    }
}

impl AttemptBudget {
    pub fn new(max_physical_attempts: u64) -> Result<Self, AttemptBudgetError> {
        if max_physical_attempts == 0 {
            return Err(AttemptBudgetError::InvalidLimit);
        }
        Ok(Self {
            max_physical_attempts,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AttemptBudgetError {
    #[error("max_physical_attempts must be greater than zero")]
    InvalidLimit,
    #[error("physical attempt budget exhausted ({limit})")]
    Exhausted { limit: u64 },
    #[error("configured budget is halted after unknown usage")]
    UsageUnknown,
    #[error("configured request reservation exceeds a budget limit")]
    ReservationExceeded,
    #[error("attempt ledger persistence failed: {0}")]
    Persistence(String),
}

/// Serializable provider-owned ledger state. Session storage chooses the
/// file and atomic-write protocol; this value contains no request body,
/// response body, credential, or token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedAttemptLedger {
    pub next_logical_id: u64,
    pub next_attempt_id: u64,
    pub records: Vec<AttemptRecord>,
    pub budget: Option<AttemptBudget>,
    #[serde(default)]
    pub limits: Option<BudgetLimits>,
    #[serde(default)]
    pub reserved: BudgetTotals,
    #[serde(default)]
    pub settled: BudgetTotals,
    #[serde(default)]
    pub halted: bool,
}

#[derive(Clone, Default)]
pub struct AttemptLedger {
    state: Arc<Mutex<State>>,
    context: AttemptContext,
    budget: Option<AttemptBudget>,
    limits: Option<BudgetLimits>,
}

impl std::fmt::Debug for AttemptLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptLedger")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

/// Cancellation keeps an embedding attempt in the shared ledger.
pub struct EmbeddingAttempt(AttemptGuard);
impl EmbeddingAttempt {
    pub fn finish(&mut self, tokens: Option<u32>, succeeded: bool) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let record = &mut state.records[self.0.index];
        record.finished_at_ms = Some(now_ms());
        record.status = if succeeded {
            AttemptStatus::Succeeded
        } else {
            AttemptStatus::Failed
        };
        record.usage = usage_observation(tokens.map(|n| Usage {
            input_tokens: n,
            output_tokens: 0,
            total_tokens: n,
            cached_tokens: 0,
        }));
        if !succeeded || tokens.is_none() {
            state.halted = self.0.ledger.limits.is_some();
        }
        if self.0.ledger.persist_locked(&state).is_err() {
            state.halted = true;
        }
        self.0.finished = true;
    }
}

impl AttemptLedger {
    pub fn begin_embedding(&self, model: &str) -> Result<EmbeddingAttempt, AttemptBudgetError> {
        let ledger = self.scoped(AttemptContext {
            parent_id: self.context.parent_id.clone(),
            kind: AttemptKind::Embedding,
        });
        let logical = ledger.begin_logical();
        ledger
            .begin_attempt(&logical, model, None, BudgetReservation::default())
            .map(EmbeddingAttempt)
    }

    /// Open or resume a private ledger while holding an exclusive writer lock.
    /// A crash with a running reservation halts bounded execution on resume.
    pub fn open(
        path: &Path,
        context: AttemptContext,
        budget: AttemptBudget,
        limits: BudgetLimits,
    ) -> Result<Self, AttemptBudgetError> {
        Self::open_durable(path, context, budget, Some(limits), false)
    }

    /// Open or resume a durable observational ledger while holding an
    /// exclusive writer lock. Observational runs reserve only their physical
    /// sends: they have no token, hosted-action, or cost ceilings. A record
    /// left running by a prior process halts the run before another send.
    pub fn open_observed(
        path: &Path,
        context: AttemptContext,
        budget: AttemptBudget,
    ) -> Result<Self, AttemptBudgetError> {
        Self::open_durable(path, context, budget, None, true)
    }

    fn open_durable(
        path: &Path,
        context: AttemptContext,
        budget: AttemptBudget,
        limits: Option<BudgetLimits>,
        halt_unfinished_on_resume: bool,
    ) -> Result<Self, AttemptBudgetError> {
        let fail = |e: std::io::Error| AttemptBudgetError::Persistence(e.to_string());
        let parent = path.parent().ok_or_else(|| {
            AttemptBudgetError::Persistence("ledger needs a parent directory".into())
        })?;
        std::fs::create_dir_all(parent).map_err(fail)?;
        for target in [path.to_path_buf(), path.with_extension("lock")] {
            if std::fs::symlink_metadata(&target).is_ok_and(|m| !m.is_file()) {
                return Err(AttemptBudgetError::Persistence(
                    "ledger must be a regular file".into(),
                ));
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(path.with_extension("lock")).map_err(fail)?;
        lock.try_lock()
            .map_err(|e| AttemptBudgetError::Persistence(e.to_string()))?;
        let ledger = if path.exists() {
            let saved: PersistedAttemptLedger =
                serde_json::from_slice(&std::fs::read(path).map_err(fail)?)
                    .map_err(|e| AttemptBudgetError::Persistence(e.to_string()))?;
            if saved.budget != Some(budget) || saved.limits != limits {
                return Err(AttemptBudgetError::Persistence(
                    "saved budget cannot be changed on resume".into(),
                ));
            }
            Self::from_persisted(saved, context)?
        } else {
            let ledger = Self::with_context(context).with_budget(budget);
            if let Some(limits) = limits {
                ledger.with_budget_limits(limits)
            } else {
                ledger
            }
        };
        {
            let mut state = ledger.state.lock().expect("attempt ledger poisoned");
            if halt_unfinished_on_resume
                && state
                    .records
                    .iter()
                    .any(|record| record.status == AttemptStatus::Running)
            {
                state.halted = true;
            }
            state.durable = Some(DurableLedger {
                path: path.to_path_buf(),
                _lock: lock,
            });
            ledger.persist_locked(&state)?;
        }
        Ok(ledger)
    }

    fn persist_locked(&self, state: &State) -> Result<(), AttemptBudgetError> {
        let Some(durable) = &state.durable else {
            return Ok(());
        };
        let saved = PersistedAttemptLedger {
            next_logical_id: state.next_logical_id,
            next_attempt_id: state.next_attempt_id,
            records: state.records.clone(),
            budget: self.budget,
            limits: self.limits,
            reserved: state.reserved,
            settled: state.settled,
            halted: state.halted,
        };
        let write = || -> Result<(), Box<dyn std::error::Error>> {
            let parent = durable.path.parent().expect("validated parent");
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(&mut temp, &saved)?;
            temp.flush()?;
            temp.as_file().sync_all()?;
            temp.persist(&durable.path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        };
        write().map_err(|e| AttemptBudgetError::Persistence(e.to_string()))
    }

    pub fn with_context(context: AttemptContext) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            context,
            budget: None,
            limits: None,
        }
    }

    pub fn with_budget(mut self, budget: AttemptBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn with_budget_limits(mut self, limits: BudgetLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    pub fn budget_state(&self) -> (BudgetTotals, BudgetTotals, bool) {
        let state = self.state.lock().expect("attempt ledger poisoned");
        (state.reserved, state.settled, state.halted)
    }

    pub fn persisted(&self) -> PersistedAttemptLedger {
        let state = self.state.lock().expect("attempt ledger poisoned");
        PersistedAttemptLedger {
            next_logical_id: state.next_logical_id,
            next_attempt_id: state.next_attempt_id,
            records: state.records.clone(),
            budget: self.budget,
            limits: self.limits,
            reserved: state.reserved,
            settled: state.settled,
            halted: state.halted,
        }
    }

    pub fn from_persisted(
        persisted: PersistedAttemptLedger,
        context: AttemptContext,
    ) -> Result<Self, AttemptBudgetError> {
        if let Some(budget) = persisted.budget
            && persisted
                .records
                .iter()
                .filter(|r| r.kind != AttemptKind::Embedding)
                .count() as u64
                > budget.max_physical_attempts
        {
            return Err(AttemptBudgetError::Exhausted {
                limit: budget.max_physical_attempts,
            });
        }
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                next_logical_id: persisted.next_logical_id,
                next_attempt_id: persisted.next_attempt_id,
                records: persisted.records.clone(),
                reserved: persisted.reserved,
                settled: persisted.settled,
                halted: persisted.halted
                    || (persisted.limits.is_some()
                        && persisted
                            .records
                            .iter()
                            .any(|r| r.status == AttemptStatus::Running)),
                durable: None,
            })),
            context,
            budget: persisted.budget,
            limits: persisted.limits,
        })
    }

    /// Creates a child view with different metadata while retaining this
    /// ledger's logical and physical ID namespace.
    pub fn scoped(&self, context: AttemptContext) -> Self {
        Self {
            state: Arc::clone(&self.state),
            context,
            budget: self.budget,
            limits: self.limits,
        }
    }

    /// Alias for a scoped ledger used by child work.
    pub fn child(&self, context: AttemptContext) -> Self {
        self.scoped(context)
    }

    pub fn snapshot(&self) -> Vec<AttemptRecord> {
        self.state
            .lock()
            .expect("attempt ledger poisoned")
            .records
            .clone()
    }

    pub(crate) fn begin_logical(&self) -> LogicalRequest {
        let mut state = self.state.lock().expect("attempt ledger poisoned");
        state.next_logical_id = state.next_logical_id.saturating_add(1);
        LogicalRequest {
            id: format!("logical-{}", state.next_logical_id),
        }
    }

    pub(crate) fn begin_attempt(
        &self,
        logical: &LogicalRequest,
        model: &str,
        retry_reason: Option<&str>,
        reservation: BudgetReservation,
    ) -> Result<AttemptGuard, AttemptBudgetError> {
        let mut state = self.state.lock().expect("attempt ledger poisoned");
        if state.halted {
            return Err(AttemptBudgetError::UsageUnknown);
        }
        let context = if self.context.kind == AttemptKind::Embedding {
            self.context.clone()
        } else {
            CALL_CONTEXT
                .try_with(Clone::clone)
                .unwrap_or_else(|_| self.context.clone())
        };
        let embeddings = state
            .records
            .iter()
            .filter(|r| r.kind == AttemptKind::Embedding)
            .count() as u64;
        if context.kind == AttemptKind::Embedding && embeddings >= 1600 {
            return Err(AttemptBudgetError::Exhausted { limit: 1600 });
        }
        if let Some(budget) = self.budget
            && context.kind != AttemptKind::Embedding
            && state.next_attempt_id.saturating_sub(embeddings) >= budget.max_physical_attempts
        {
            return Err(AttemptBudgetError::Exhausted {
                limit: budget.max_physical_attempts,
            });
        }
        if let Some(limits) = self
            .limits
            .filter(|_| context.kind != AttemptKind::Embedding)
        {
            if reservation.input_tokens == 0
                || reservation.output_tokens == 0
                || reservation.cost_microusd == 0
            {
                return Err(AttemptBudgetError::ReservationExceeded);
            }
            let mut next = state.reserved;
            next.add(BudgetReservation::new(
                state.settled.input_tokens,
                state.settled.output_tokens,
                state.settled.hosted_actions,
                state.settled.cost_microusd,
            ));
            next.add(reservation);
            if next.exceeds(limits) {
                return Err(AttemptBudgetError::ReservationExceeded);
            }
            state.reserved.add(reservation);
        }
        state.next_attempt_id = state.next_attempt_id.saturating_add(1);
        let attempt_id = state.next_attempt_id;
        let index = state.records.len();
        let now = now_ms();
        state.records.push(AttemptRecord {
            logical_id: logical.id.clone(),
            attempt_id: format!("attempt-{attempt_id}"),
            parent_id: context.parent_id,
            kind: context.kind,
            model: model.to_string(),
            started_at_ms: now,
            finished_at_ms: None,
            status: AttemptStatus::Running,
            usage: UsageObservation::Missing,
            retry_reason: retry_reason.map(str::to_string),
            reservation,
        });
        if let Err(error) = self.persist_locked(&state) {
            state.halted = true;
            return Err(error);
        }
        Ok(AttemptGuard {
            state: Arc::clone(&self.state),
            index,
            ledger: self.clone(),
            reservation,
            finished: false,
        })
    }
}

#[derive(Debug)]
pub(crate) struct LogicalRequest {
    id: String,
}

pub(crate) struct AttemptGuard {
    state: Arc<Mutex<State>>,
    index: usize,
    finished: bool,
    reservation: BudgetReservation,
    ledger: AttemptLedger,
}

impl std::fmt::Debug for AttemptGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptGuard")
            .field("index", &self.index)
            .finish()
    }
}

impl AttemptGuard {
    pub(crate) fn finish(&mut self, result: &Result<CompletionResponse, crate::ProviderError>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let record = &mut state.records[self.index];
        record.finished_at_ms = Some(now_ms());
        record.status = if result.is_ok() {
            AttemptStatus::Succeeded
        } else {
            AttemptStatus::Failed
        };
        if let Ok(response) = result {
            record.usage = usage_observation(response.usage);
            if let Some(usage) = response.usage {
                // Charge all input at the cache-write worst case. This intentionally
                // does not infer an unreported cache-write count from cache reads.
                let actual = BudgetReservation::new(
                    usage.input_tokens.into(),
                    usage.output_tokens.into(),
                    response.hosted_web_search.len() as u64,
                    (u64::from(usage.input_tokens) * 25).div_ceil(2)
                        + u64::from(usage.output_tokens) * 50
                        + response.hosted_web_search.len() as u64 * 10_000,
                );
                state.settled.add(actual);
                if self.ledger.limits.is_some() {
                    state.reserved.input_tokens = state
                        .reserved
                        .input_tokens
                        .saturating_sub(self.reservation.input_tokens);
                    state.reserved.output_tokens = state
                        .reserved
                        .output_tokens
                        .saturating_sub(self.reservation.output_tokens);
                    state.reserved.hosted_actions = state
                        .reserved
                        .hosted_actions
                        .saturating_sub(self.reservation.hosted_actions);
                    state.reserved.cost_microusd = state
                        .reserved
                        .cost_microusd
                        .saturating_sub(self.reservation.cost_microusd);
                    if actual.input_tokens > self.reservation.input_tokens
                        || actual.output_tokens > self.reservation.output_tokens
                        || actual.hosted_actions > self.reservation.hosted_actions
                        || actual.cost_microusd > self.reservation.cost_microusd
                    {
                        state.halted = true;
                    }
                }
            } else if self.ledger.limits.is_some() {
                state.halted = true;
            }
        } else if self.ledger.limits.is_some() {
            state.halted = true;
        }
        if self.ledger.persist_locked(&state).is_err() {
            state.halted = true;
        }
        self.finished = true;
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let record = &mut state.records[self.index];
        record.finished_at_ms = Some(now_ms());
        record.status = AttemptStatus::Cancelled;
        record.usage = UsageObservation::Missing;
        if self.ledger.limits.is_some() {
            state.halted = true;
        }
        if self.ledger.persist_locked(&state).is_err() {
            state.halted = true;
        }
    }
}

fn usage_observation(usage: Option<Usage>) -> UsageObservation {
    match usage {
        Some(usage) => UsageObservation::Known {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_tokens: usage.cached_tokens,
        },
        None => UsageObservation::Missing,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_limits() -> BudgetLimits {
        BudgetLimits {
            max_input_tokens: 64_000,
            max_output_tokens: 8192,
            max_hosted_actions: 4,
            max_cost_microusd: 1_300_000,
        }
    }
    #[test]
    fn durable_reservation_settles_and_unknown_usage_survives_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger.json");
        let budget = AttemptBudget::new(4).unwrap();
        let ledger =
            AttemptLedger::open(&path, AttemptContext::default(), budget, test_limits()).unwrap();
        assert!(
            AttemptLedger::open(&path, AttemptContext::default(), budget, test_limits()).is_err()
        );
        let logical = ledger.begin_logical();
        let reservation = BudgetReservation::new(32_000, 4096, 2, 624_800);
        let mut first = ledger
            .begin_attempt(&logical, "gpt-6-astra", None, reservation)
            .unwrap();
        let on_disk: PersistedAttemptLedger =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk.reserved.cost_microusd, 624_800);
        assert_eq!(on_disk.records[0].status, AttemptStatus::Running);
        first.finish(&Ok(CompletionResponse {
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
                cached_tokens: 50,
            }),
            ..Default::default()
        }));
        drop(first);
        assert_eq!(ledger.budget_state().0, BudgetTotals::default());
        assert_eq!(ledger.budget_state().1.cost_microusd, 1750);
        let unknown = ledger
            .begin_attempt(&logical, "gpt-6-astra", None, reservation)
            .unwrap();
        drop(unknown);
        drop(ledger);
        let restored =
            AttemptLedger::open(&path, AttemptContext::default(), budget, test_limits()).unwrap();
        assert_eq!(restored.budget_state().0.cost_microusd, 624_800);
        assert_eq!(
            restored
                .begin_attempt(&restored.begin_logical(), "gpt-6-astra", None, reservation)
                .unwrap_err(),
            AttemptBudgetError::UsageUnknown
        );
    }

    #[test]
    fn observed_ledger_persists_before_send_and_halts_an_unfinished_resume() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("observed.json");
        let budget = AttemptBudget::new(1).unwrap();
        let ledger =
            AttemptLedger::open_observed(&path, AttemptContext::default(), budget).unwrap();
        assert!(AttemptLedger::open_observed(&path, AttemptContext::default(), budget).is_err());

        let logical = ledger.begin_logical();
        let guard = ledger
            .begin_attempt(&logical, "gpt-6-astra", None, BudgetReservation::default())
            .unwrap();
        let running: PersistedAttemptLedger =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(running.budget, Some(budget));
        assert_eq!(running.limits, None);
        assert_eq!(running.records[0].status, AttemptStatus::Running);

        // Simulate a process dying after the durable pre-send record but before
        // its guard can mark the attempt cancelled.
        let saved = ledger.persisted();
        drop(guard);
        drop(ledger);
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();

        assert!(
            AttemptLedger::open_observed(
                &path,
                AttemptContext::default(),
                AttemptBudget::new(2).unwrap(),
            )
            .is_err()
        );
        let resumed =
            AttemptLedger::open_observed(&path, AttemptContext::default(), budget).unwrap();
        assert!(
            resumed.budget_state().2,
            "unfinished send must halt the run"
        );
        assert_eq!(
            resumed
                .begin_attempt(
                    &resumed.begin_logical(),
                    "gpt-6-astra",
                    None,
                    BudgetReservation::default(),
                )
                .unwrap_err(),
            AttemptBudgetError::UsageUnknown
        );
    }

    #[test]
    fn bounded_ledger_rejects_zero_reservations_and_cumulative_excess() {
        let ledger = AttemptLedger::default().with_budget_limits(test_limits());
        let logical = ledger.begin_logical();
        assert_eq!(
            ledger
                .begin_attempt(&logical, "m", None, BudgetReservation::default())
                .unwrap_err(),
            AttemptBudgetError::ReservationExceeded
        );
        let mut first = ledger
            .begin_attempt(
                &logical,
                "m",
                None,
                BudgetReservation::new(32_000, 4096, 0, 604_800),
            )
            .unwrap();
        first.finish(&Ok(CompletionResponse {
            usage: Some(Usage {
                input_tokens: 32_000,
                output_tokens: 4096,
                total_tokens: 36096,
                cached_tokens: 0,
            }),
            ..Default::default()
        }));
        let second = ledger
            .begin_attempt(
                &logical,
                "m",
                None,
                BudgetReservation::new(32_000, 4096, 0, 604_800),
            )
            .unwrap();
        assert_eq!(
            ledger
                .begin_attempt(&logical, "m", None, BudgetReservation::new(1, 1, 0, 63))
                .unwrap_err(),
            AttemptBudgetError::ReservationExceeded
        );
        drop(second);
    }

    #[test]
    fn request_caps_fail_closed_and_reserve_cache_write_price() {
        let mut caps = RequestCaps {
            max_input_tokens: 32_000,
            max_output_tokens: 4096,
            max_hosted_actions: 0,
            endpoint_contract_verified: false,
        };
        assert!(caps.apply(&mut serde_json::json!({})).is_err());
        caps.endpoint_contract_verified = true;
        let mut body = serde_json::json!({"input": []});
        assert_eq!(caps.apply(&mut body).unwrap().cost_microusd, 604_800);
        assert_eq!(body["max_output_tokens"], 4096);
        assert_eq!(body["truncation"], "disabled");
        assert!(
            caps.apply(&mut serde_json::json!({"input": "x".repeat(32_001)}))
                .is_err()
        );
    }

    #[test]
    fn records_only_physical_attempts_and_serializes_without_payloads() {
        let ledger = AttemptLedger::with_context(AttemptContext {
            parent_id: Some("parent-7".into()),
            kind: AttemptKind::Summary,
        });
        let logical = ledger.begin_logical();
        assert!(ledger.snapshot().is_empty());

        let mut attempt = ledger
            .begin_attempt(
                &logical,
                "gpt-6-astra",
                Some("401"),
                BudgetReservation::default(),
            )
            .expect("unlimited ledger");
        attempt.finish(&Ok(CompletionResponse {
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 5,
                total_tokens: 8,
                cached_tokens: 2,
            }),
            ..CompletionResponse::default()
        }));

        let records = ledger.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].parent_id.as_deref(), Some("parent-7"));
        assert_eq!(records[0].kind, AttemptKind::Summary);
        assert_eq!(records[0].status, AttemptStatus::Succeeded);
        assert!(records[0].finished_at_ms.is_some());
        assert!(matches!(
            records[0].usage,
            UsageObservation::Known {
                total_tokens: 8,
                ..
            }
        ));
        let serialized = serde_json::to_string(&records).expect("ledger serializes");
        assert!(!serialized.contains("authorization"));
        assert!(!serialized.contains("content"));
    }

    #[test]
    fn dropped_attempt_is_cancelled_with_missing_usage() {
        let ledger = AttemptLedger::default();
        let logical = ledger.begin_logical();
        let attempt = ledger
            .begin_attempt(&logical, "m", None, BudgetReservation::default())
            .expect("unlimited ledger");
        drop(attempt);

        let record = ledger.snapshot().pop().expect("one attempt");
        assert_eq!(record.status, AttemptStatus::Cancelled);
        assert!(record.finished_at_ms.is_some());
        assert_eq!(record.usage, UsageObservation::Missing);
    }

    #[test]
    fn scoped_ledgers_share_ids_and_distinguish_running_from_completed() {
        let root = AttemptLedger::default();
        let child = root.child(AttemptContext {
            parent_id: Some("parent-1".into()),
            kind: AttemptKind::Child,
        });
        let root_logical = root.begin_logical();
        let child_logical = child.begin_logical();
        let mut completed = root
            .begin_attempt(&root_logical, "main", None, BudgetReservation::default())
            .expect("unlimited ledger");
        let running = child
            .begin_attempt(&child_logical, "worker", None, BudgetReservation::default())
            .expect("unlimited ledger");

        let records = root.snapshot();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].logical_id, "logical-1");
        assert_eq!(records[1].logical_id, "logical-2");
        assert_eq!(records[0].attempt_id, "attempt-1");
        assert_eq!(records[1].attempt_id, "attempt-2");
        assert_eq!(records[1].status, AttemptStatus::Running);
        assert_eq!(records[1].finished_at_ms, None);

        completed.finish(&Ok(CompletionResponse::default()));
        let records = child.snapshot();
        assert_eq!(records[0].status, AttemptStatus::Succeeded);
        assert!(records[0].finished_at_ms.is_some());
        assert_eq!(records[1].status, AttemptStatus::Running);
        drop(running);
    }

    #[test]
    fn persisted_ledger_keeps_ids_and_enforces_a_hard_cap_after_restore() {
        let budget = AttemptBudget::new(2).expect("positive cap");
        let ledger = AttemptLedger::default().with_budget(budget);
        let logical = ledger.begin_logical();
        let first = ledger
            .begin_attempt(&logical, "m", None, BudgetReservation::default())
            .expect("first attempt");
        drop(first);
        let persisted = ledger.persisted();
        let encoded = serde_json::to_string(&persisted).expect("serializes");
        assert!(!encoded.contains("content"));

        let restored = AttemptLedger::from_persisted(
            serde_json::from_str(&encoded).expect("deserializes"),
            AttemptContext::default(),
        )
        .expect("within budget");
        let logical = restored.begin_logical();
        let second = restored
            .begin_attempt(&logical, "m", None, BudgetReservation::default())
            .expect("second attempt");
        drop(second);
        let error = restored
            .begin_attempt(&logical, "m", Some("retry"), BudgetReservation::default())
            .expect_err("third physical send must be rejected");
        assert_eq!(error, AttemptBudgetError::Exhausted { limit: 2 });
        let records = restored.snapshot();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].attempt_id, "attempt-1");
        assert_eq!(records[1].attempt_id, "attempt-2");
    }
}
