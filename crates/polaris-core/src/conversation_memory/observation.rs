//! One accepted history observation shared by desktop, terminal and audit output.
use super::structured::packing::{RetrievalStatus, StructuredContext};
use crate::conversation_state::HistoryMode;
use polaris_memory::conversation::{ConversationHit, RelatedSummaryBudget};
use std::{fmt, io};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryObservation {
    pub mode: HistoryMode,
    pub retrieval_status: Option<RetrievalStatus>,
    pub source_uris: Vec<String>,
    pub reference_tokens: u64,
    pub related_omission: Option<RelatedSummaryBudget>,
}

impl MemoryObservation {
    pub fn dispatch_allowed(&self) -> bool {
        !matches!(
            self.retrieval_status,
            Some(RetrievalStatus::ExtractionIncomplete | RetrievalStatus::EvidenceBudgetExceeded)
        )
    }

    /// Counts and closed enums only: never persist source URIs or user text here.
    pub(crate) fn audit_detail(&self) -> String {
        format!(
            "mode={} status={} source_count={} reference_tokens={} dispatch_allowed={} related_omitted={}",
            self.mode_name(),
            self.status_name(),
            self.source_uris.len(),
            self.reference_tokens,
            self.dispatch_allowed(),
            self.related_omission
                .map_or_else(|| "none".into(), |budget| budget.to_string())
        )
    }

    pub fn display_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "記憶: {} / {} / 参照 {} tokens / 出典 {}件",
            self.mode_name(),
            self.status_label(),
            self.reference_tokens,
            self.source_uris.len()
        )];
        if let Some(budget) = self.related_omission {
            lines.push(format!("補足検索を省略: {budget}"));
        }
        // JSON escaping prevents URI control characters becoming terminal output.
        lines.extend(self.source_uris.iter().map(|uri| {
            format!(
                "出典: {}",
                serde_json::to_string(uri).expect("String serialization is infallible")
            )
        }));
        lines
    }

    fn mode_name(&self) -> &'static str {
        match self.mode {
            HistoryMode::Legacy => "legacy",
            HistoryMode::Strict10 => "strict10",
            HistoryMode::Structured10 => "structured10",
        }
    }
    fn status_name(&self) -> &'static str {
        match self.retrieval_status {
            None => "none",
            Some(RetrievalStatus::Found) => "found",
            Some(RetrievalStatus::NotFound) => "not_found",
            Some(RetrievalStatus::AmbiguousEntity) => "ambiguous_entity",
            Some(RetrievalStatus::PendingReconfirmation) => "pending_reconfirmation",
            Some(RetrievalStatus::ExtractionIncomplete) => "extraction_incomplete",
            Some(RetrievalStatus::EvidenceBudgetExceeded) => "evidence_budget_exceeded",
        }
    }
    fn status_label(&self) -> &'static str {
        match self.retrieval_status {
            None => "準備済み",
            Some(RetrievalStatus::Found) => "取得済み",
            Some(RetrievalStatus::NotFound) => "該当なし",
            Some(RetrievalStatus::AmbiguousEntity) => "対象を特定できません",
            Some(RetrievalStatus::PendingReconfirmation) => "要再確認",
            Some(RetrievalStatus::ExtractionIncomplete) => "抽出不足・送信停止",
            Some(RetrievalStatus::EvidenceBudgetExceeded) => "参照枠超過・送信停止",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryPreparationBlocked {
    pub observation: MemoryObservation,
}
impl fmt::Display for MemoryPreparationBlocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.observation.status_name())
    }
}
impl std::error::Error for MemoryPreparationBlocked {}

pub(crate) struct PreparedMemoryRequest {
    pub observation: MemoryObservation,
    pub text: Option<String>,
}
impl PreparedMemoryRequest {
    pub(crate) fn require_dispatch(self) -> io::Result<Self> {
        if !self.observation.dispatch_allowed() {
            return Err(io::Error::other(MemoryPreparationBlocked {
                observation: self.observation,
            }));
        }
        Ok(self)
    }
}

pub(crate) fn prepare_structured(context: &StructuredContext) -> io::Result<PreparedMemoryRequest> {
    let mut observation = MemoryObservation {
        mode: HistoryMode::Structured10,
        retrieval_status: Some(context.status),
        source_uris: context.source_uris.clone(),
        reference_tokens: 0,
        related_omission: context.related_omission,
    };
    let request = if observation.dispatch_allowed() {
        match context.request_text() {
            Ok((text, tokens)) => {
                observation.reference_tokens = tokens as u64;
                Some(text)
            }
            Err(error)
                if error.get_ref().is_some_and(|inner| {
                    inner.is::<super::structured::packing::RequestBudgetExceeded>()
                }) =>
            {
                observation.retrieval_status = Some(RetrievalStatus::EvidenceBudgetExceeded);
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if !observation.dispatch_allowed() {
        observation.source_uris.clear();
        observation.related_omission = None;
    }
    Ok(PreparedMemoryRequest {
        observation,
        text: request,
    })
}

pub(crate) fn prepare_strict(hits: &[ConversationHit]) -> io::Result<PreparedMemoryRequest> {
    let text = hits
        .iter()
        .map(ConversationHit::render)
        .collect::<Vec<_>>()
        .join("\n");
    let tokens = crate::budget::count_tokens(&text);
    if tokens > 768 {
        return Err(io::Error::other("取得した会話記憶が768tokensを超えました"));
    }
    let source_uris = hits
        .iter()
        .map(|hit| {
            format!(
                "conversation://{}?start={}&end={}",
                hit.source.id, hit.source.start_turn, hit.source.end_turn
            )
        })
        .collect();
    Ok(PreparedMemoryRequest {
        observation: MemoryObservation {
            mode: HistoryMode::Strict10,
            retrieval_status: None,
            source_uris,
            reference_tokens: tokens as u64,
            related_omission: None,
        },
        text: (!text.is_empty()).then_some(text),
    })
}
