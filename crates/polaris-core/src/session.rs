//! In-memory request history with optional durable v2 storage.

use polaris_provider::{Message, ReasoningItem, ToolCall};

#[derive(Default, Clone)]
pub struct Session {
    pub messages: Vec<Message>,
    /// Configuration-provided examples; never real user turns or raw events.
    pub examples: Vec<Message>,
    pub persistence: Option<crate::session_store::PersistedSession>,
    pub strict_history: Option<std::sync::Arc<crate::conversation_memory::StrictHistory>>,
    pub summary_usage: Option<polaris_provider::UsageMeter>,
    /// Sticky write failure: a failed append must never lead to a later request.
    #[doc(hidden)]
    pub persistence_error: Option<String>,
    /// Optional v2 workflow runtime. Never inferred from model or tool text.
    pub workflow: Option<crate::workflow::SessionWorkflow>,
    /// Saves the full original history before a valid summary replaces it.
    pub before_compact: Option<std::sync::Arc<crate::compaction::ArchiveHook>>,
    /// Optional local override; no model window is inferred.
    pub compaction_threshold: Option<usize>,
    /// Automatic retry watermark; transient and not persisted with history.
    pub compaction_retry_tokens: Option<usize>,
    /// Explicitly configured durable tool-result storage; never serialized as credentials.
    pub tool_memory: Option<crate::tool_memory::ToolMemory>,
    /// Stops only root-triggered automatic `files.md` regeneration. Manual
    /// `spawn` remains available, and the default preserves legacy behavior.
    pub disable_files_md_auto_regenerate: bool,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim one automatic attempt per substantial increase in history.
    /// Set before awaiting so cancellation also cannot cause a tight retry loop.
    pub(crate) fn begin_compaction_attempt(&mut self) -> bool {
        let tokens = crate::compaction::session_tokens(&self.messages);
        if let Some(previous) = self.compaction_retry_tokens {
            let growth = crate::compaction::MIN_TOKENS_TO_SUMMARIZE.max(previous / 10);
            if tokens >= previous && tokens.saturating_sub(previous) < growth {
                return false;
            }
        }
        self.compaction_retry_tokens = Some(tokens);
        true
    }

    pub(crate) async fn compact_automatically(
        &mut self,
        provider: &dyn polaris_provider::Provider,
    ) -> Result<Option<crate::compaction::CompactionReport>, crate::compaction::CompactionError>
    {
        let previous_retry_tokens = self.compaction_retry_tokens;
        if !self.begin_compaction_attempt() {
            return Ok(None);
        }
        let outcome = crate::compaction::compact_with_usage(
            provider,
            &mut self.messages,
            self.before_compact.as_deref(),
        )
        .await;
        let usage = outcome.usage_report;
        if usage.reported_responses == 0
            && usage.missing_responses == 0
            && usage.failed_requests == 0
        {
            // No eligible range (or too little to summarize): no provider
            // attempt occurred, so a short new turn must remain eligible.
            self.compaction_retry_tokens = previous_retry_tokens;
        } else if matches!(outcome.result, Ok(Some(_))) {
            self.compaction_retry_tokens = None;
        }
        outcome.result
    }

    pub fn push_user(&mut self, content: &str) {
        if let Err(error) = self.recover_interrupted_tools() {
            self.persistence_error = Some(error.to_string());
            return;
        }
        self.push_message(Message::user(content), true);
    }

    /// Close only unresolved calls when the user resumes or begins a new turn.
    /// Their external effects are unknown; never retry a mutation automatically.
    pub fn recover_interrupted_tools(&mut self) -> std::io::Result<()> {
        self.check_persistence()?;
        let messages = if let Some(saved) = &self.persistence {
            let snapshot = saved.snapshot()?;
            snapshot
                .events
                .into_iter()
                .filter(|e| e.epoch == snapshot.state.epoch)
                .map(|e| e.message)
                .collect::<Vec<_>>()
        } else {
            self.messages.clone()
        };
        let mut pending = std::collections::BTreeSet::new();
        for message in &messages {
            for call in &message.tool_calls {
                pending.insert(call.id.clone());
            }
            if let Some(id) = &message.tool_call_id {
                pending.remove(id);
            }
        }
        if !pending.is_empty() {
            self.messages = messages;
        }
        for id in pending {
            self.push_tool_result(&id, "中断により結果は不明です。外部への影響を確認してから続行してください。自動再実行はしていません。");
            self.check_persistence()?;
        }
        Ok(())
    }

    pub fn push_assistant(&mut self, content: &str, reasoning: Vec<ReasoningItem>) {
        self.push_message(Message::assistant(content).with_reasoning(reasoning), false);
    }

    /// Records an assistant turn together with the tool calls it made.
    /// Under OpenAI's round-trip protocol, this message must remain in the
    /// history holding its own `tool_calls` before the tool results are sent.
    pub fn push_assistant_tool_calls(
        &mut self,
        content: &str,
        tool_calls: Vec<ToolCall>,
        reasoning: Vec<ReasoningItem>,
    ) {
        self.push_message(
            Message::assistant_with_tool_calls(content, tool_calls).with_reasoning(reasoning),
            false,
        );
    }

    /// Records a tool result, tying it to the id of the call it responds to.
    pub fn push_tool_result(&mut self, tool_call_id: &str, content: &str) {
        self.push_message(Message::tool_result(tool_call_id, content), false);
    }

    /// Few-shot and retrieved user-role text must pass false here; only real
    /// user input advances the durable turn counter.
    pub fn push_message(&mut self, message: Message, starts_turn: bool) {
        if self.persistence_error.is_some() {
            return;
        }
        if let Some(persistence) = &self.persistence
            && let Err(error) =
                persistence.append(message.clone(), starts_turn, self.workflow.as_ref())
        {
            self.persistence_error = Some(error.to_string());
            return;
        }
        self.messages.push(message);
    }

    pub fn check_persistence(&self) -> std::io::Result<()> {
        if let Some(error) = &self.persistence_error {
            return Err(std::io::Error::other(error.clone()));
        }
        if let Some(persistence) = &self.persistence {
            persistence.snapshot()?;
        }
        Ok(())
    }

    pub fn checkpoint(&self) -> std::io::Result<()> {
        self.check_persistence()?;
        if let Some(persistence) = &self.persistence {
            persistence.checkpoint(self.workflow.as_ref())?;
        }
        Ok(())
    }

    /// Restore raw messages from a verified attachment. Request-window
    /// preparation remains the agent's responsibility before its next send.
    pub fn attach(
        &mut self,
        persistence: crate::session_store::PersistedSession,
    ) -> std::io::Result<()> {
        let snapshot = persistence.snapshot()?;
        self.messages = snapshot
            .events
            .into_iter()
            .filter(|event| event.epoch == snapshot.state.epoch)
            .map(|event| event.message)
            .collect();
        self.persistence = Some(persistence);
        self.persistence_error = None;
        Ok(())
    }

    pub fn clear_history(&mut self) -> std::io::Result<()> {
        self.check_persistence()?;
        if let Some(persistence) = &self.persistence {
            persistence.clear()?;
        }
        self.messages.clear();
        self.compaction_retry_tokens = None;
        Ok(())
    }

    pub fn uses_strict_history(&self) -> std::io::Result<bool> {
        Ok(self
            .persistence
            .as_ref()
            .map(|saved| saved.snapshot())
            .transpose()?
            .is_some_and(|snapshot| {
                snapshot.state.history_mode == crate::conversation_state::HistoryMode::Strict10
            }))
    }

    /// Recover a pending expired turn before a resumed input changes its raw hash.
    pub async fn recover_history(&mut self) -> std::io::Result<()> {
        self.recover_interrupted_tools()?;
        if !self.uses_strict_history()? {
            return Ok(());
        }
        let history = self
            .strict_history
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10の埋め込み・要約backendがありません"))?;
        let saved = self
            .persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10には永続保存が必要です"))?;
        let recovered = history
            .recover_pending_observed(&saved.store, &saved.database, &|snapshot| {
                saved.accept_publication(snapshot)
            })
            .await?;
        saved.accept_prepared(&recovered.snapshot)?;
        Ok(())
    }

    /// Runs once at a real user boundary. Returned evidence is ephemeral and
    /// never appended as another user turn or fed into the raw archive.
    pub async fn prepare_history(&mut self) -> std::io::Result<Option<Message>> {
        self.check_persistence()?;
        if !self.uses_strict_history()? {
            return Ok(None);
        }
        let history = self
            .strict_history
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10の埋め込み・要約backendがありません"))?;
        let saved = self
            .persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10には永続保存が必要です"))?;
        let snapshot = saved.snapshot()?;
        let query = snapshot
            .events
            .iter()
            .rev()
            .find(|event| event.epoch == snapshot.state.epoch && event.starts_turn)
            .ok_or_else(|| std::io::Error::other("現在のユーザー入力がありません"))?
            .message
            .content
            .clone();
        let prepared = history
            .prepare_observed(&saved.store, &saved.database, &query, &|snapshot| {
                saved.accept_publication(snapshot)
            })
            .await?;
        saved.accept_prepared(&prepared.snapshot)?;
        let mut evidence = String::new();
        for hit in prepared.retrieval {
            if !evidence.is_empty() {
                evidence.push('\n');
            }
            evidence.push_str(&hit.render());
        }
        if crate::budget::count_tokens(&evidence) > 768 {
            return Err(std::io::Error::other(
                "取得した会話記憶が768tokensを超えました",
            ));
        }
        self.messages = prepared.messages;
        Ok((!evidence.is_empty()).then(|| Message::user(evidence)))
    }
}
