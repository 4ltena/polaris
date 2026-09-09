//! Saved source-apply facts only. Neither a live stage nor an execution capability.
//! The service checks session ownership, exact revision/hash, and limit <= 32.
use crate::{ids::*, request::PageLimit, snapshot::ApprovalDecision};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const MAX_SOURCE_APPLY_ITEMS: usize = 32;
pub const MAX_SOURCE_APPLY_RESULT_BYTES: usize = 256 * 1024;

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplySummary {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub approval_id: ApprovalId,
    pub operation_id: OperationId,
    pub policy_revision: DecimalU64,
    pub expires_at_unix_ms: DecimalU64,
    pub payload_hash: String,
    pub source_path: String,
    pub recovery_parent_path: String,
    pub entry_count: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub decision: Option<ApprovalDecision>,
    pub invalidated: bool,
    pub intent_committed: bool,
    /// A persisted result is not a claim that applying succeeded.
    pub result_saved: bool,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyVersion {
    pub hash: String,
    pub mode: u32,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyEntry {
    pub relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub before: Option<SourceApplyVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub after: Option<SourceApplyVersion>,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceApplyPageError {
    Limit,
    Offset,
    TooLarge,
}

/// Runtime validation without changing the shared history PageLimit contract.
pub fn validate_source_apply_limit(limit: PageLimit) -> Result<usize, SourceApplyPageError> {
    let count = usize::from(limit.get());
    if count > MAX_SOURCE_APPLY_ITEMS {
        Err(SourceApplyPageError::Limit)
    } else {
        Ok(count)
    }
}

// Counting sink stops at the serialized byte budget, including JSON escaping.
// Never allocates a serialization-sized temporary buffer.
fn check_bytes(value: &impl Serialize) -> Result<(), SourceApplyPageError> {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("source apply result exceeds byte budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_SOURCE_APPLY_RESULT_BYTES), value)
        .map_err(|_| SourceApplyPageError::TooLarge)
}
// Include the tagged SuccessResult wrapper in the budget as well as its payload.
fn check_result_bytes(kind: &str, payload: &impl Serialize) -> Result<(), SourceApplyPageError> {
    #[derive(Serialize)]
    struct Tagged<'a, T> {
        #[serde(rename = "type")]
        kind: &'a str,
        payload: &'a T,
    }
    check_bytes(&Tagged { kind, payload })
}
fn start(offset: DecimalU64, len: usize) -> Result<usize, SourceApplyPageError> {
    usize::try_from(offset.get())
        .ok()
        .filter(|n| *n <= len)
        .ok_or(SourceApplyPageError::Offset)
}
fn next(end: usize, len: usize) -> Option<DecimalU64> {
    (end < len).then(|| DecimalU64::new(end as u64))
}

// Private wire structs provide the existing object-only, no-null grammar.
crate::object_wire! {
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListWire {
    session_revision: DecimalU64,
    items: Vec<SourceApplySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    next_offset: Option<DecimalU64>,
}
}
crate::object_wire! {
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageWire {
    approval_id: ApprovalId,
    payload_hash: String,
    session_revision: DecimalU64,
    entries: Vec<SourceApplyEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    next_offset: Option<DecimalU64>,
}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceApplyListResult {
    pub session_revision: DecimalU64,
    pub items: Vec<SourceApplySummary>,
    pub next_offset: Option<DecimalU64>,
}
#[derive(Serialize)]
struct ListRef<'a> {
    session_revision: DecimalU64,
    items: &'a [SourceApplySummary],
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<DecimalU64>,
}
impl SourceApplyListResult {
    fn wire(&self) -> ListRef<'_> {
        ListRef {
            session_revision: self.session_revision,
            items: &self.items,
            next_offset: self.next_offset,
        }
    }
    pub fn validate(&self) -> Result<(), SourceApplyPageError> {
        if self.items.len() > MAX_SOURCE_APPLY_ITEMS {
            return Err(SourceApplyPageError::Limit);
        }
        check_result_bytes("source_apply.list", &self.wire())
    }
    /// Caller supplies the complete, revision-pinned list in stable ledger order.
    /// The continuation counts emitted items, not the requested page limit.
    pub fn from_slice(
        session_revision: DecimalU64,
        all: &[SourceApplySummary],
        offset: DecimalU64,
        limit: PageLimit,
    ) -> Result<Self, SourceApplyPageError> {
        let count = validate_source_apply_limit(limit)?;
        let begin = start(offset, all.len())?;
        let mut result = Self {
            session_revision,
            items: Vec::new(),
            next_offset: next(begin, all.len()),
        };
        for item in all.iter().skip(begin).take(count) {
            if check_bytes(item).is_err() {
                if result.items.is_empty() {
                    return Err(SourceApplyPageError::TooLarge);
                }
                break;
            }
            result.items.push(item.clone());
            result.next_offset = next(begin + result.items.len(), all.len());
            if result.validate().is_err() {
                result.items.pop();
                result.next_offset = next(begin + result.items.len(), all.len());
                if result.items.is_empty() {
                    return Err(SourceApplyPageError::TooLarge);
                }
                break;
            }
        }
        result.validate()?;
        Ok(result)
    }
}
impl Serialize for SourceApplyListResult {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.validate()
            .map_err(|e| serde::ser::Error::custom(format!("source apply list: {e:?}")))?;
        self.wire().serialize(s)
    }
}
impl<'de> Deserialize<'de> for SourceApplyListResult {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = ListWire::deserialize(d)?;
        let result = Self {
            session_revision: w.session_revision,
            items: w.items,
            next_offset: w.next_offset,
        };
        result
            .validate()
            .map_err(|e| de::Error::custom(format!("source apply list: {e:?}")))?;
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceApplyPageResult {
    pub approval_id: ApprovalId,
    pub payload_hash: String,
    pub session_revision: DecimalU64,
    pub entries: Vec<SourceApplyEntry>,
    pub next_offset: Option<DecimalU64>,
}
#[derive(Serialize)]
struct PageRef<'a> {
    approval_id: &'a ApprovalId,
    payload_hash: &'a str,
    session_revision: DecimalU64,
    entries: &'a [SourceApplyEntry],
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<DecimalU64>,
}
impl SourceApplyPageResult {
    fn wire(&self) -> PageRef<'_> {
        PageRef {
            approval_id: &self.approval_id,
            payload_hash: &self.payload_hash,
            session_revision: self.session_revision,
            entries: &self.entries,
            next_offset: self.next_offset,
        }
    }
    pub fn validate(&self) -> Result<(), SourceApplyPageError> {
        if self.entries.len() > MAX_SOURCE_APPLY_ITEMS {
            return Err(SourceApplyPageError::Limit);
        }
        check_result_bytes("source_apply.page", &self.wire())
    }
    /// Caller checks ownership, exact revision and payload hash before pagination.
    pub fn from_slice(
        approval_id: ApprovalId,
        payload_hash: String,
        session_revision: DecimalU64,
        all: &[SourceApplyEntry],
        offset: DecimalU64,
        limit: PageLimit,
    ) -> Result<Self, SourceApplyPageError> {
        let count = validate_source_apply_limit(limit)?;
        let begin = start(offset, all.len())?;
        let mut result = Self {
            approval_id,
            payload_hash,
            session_revision,
            entries: Vec::new(),
            next_offset: next(begin, all.len()),
        };
        for item in all.iter().skip(begin).take(count) {
            if check_bytes(item).is_err() {
                if result.entries.is_empty() {
                    return Err(SourceApplyPageError::TooLarge);
                }
                break;
            }
            result.entries.push(item.clone());
            result.next_offset = next(begin + result.entries.len(), all.len());
            if result.validate().is_err() {
                result.entries.pop();
                result.next_offset = next(begin + result.entries.len(), all.len());
                if result.entries.is_empty() {
                    return Err(SourceApplyPageError::TooLarge);
                }
                break;
            }
        }
        result.validate()?;
        Ok(result)
    }
}
impl Serialize for SourceApplyPageResult {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.validate()
            .map_err(|e| serde::ser::Error::custom(format!("source apply page: {e:?}")))?;
        self.wire().serialize(s)
    }
}
impl<'de> Deserialize<'de> for SourceApplyPageResult {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = PageWire::deserialize(d)?;
        let result = Self {
            approval_id: w.approval_id,
            payload_hash: w.payload_hash,
            session_revision: w.session_revision,
            entries: w.entries,
            next_offset: w.next_offset,
        };
        result
            .validate()
            .map_err(|e| de::Error::custom(format!("source apply page: {e:?}")))?;
        Ok(result)
    }
}
