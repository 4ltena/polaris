//! In-memory request history with optional durable v2 storage.

use polaris_provider::{Message, ReasoningItem, ToolCall};

#[derive(Default, Clone)]
pub struct Session {
    /// Installed by root entry points only; new child sessions do not inherit it.
    #[doc(hidden)]
    pub request_prompt: Option<crate::prompt::AlwaysOn>,
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
    /// Desktop-only unmodified turn output, independent of request projections.
    /// The trusted worker initializes this; it is never a replacement for disk publication.
    pub desktop_transcript: Option<DesktopTranscript>,
}

#[derive(Default, Clone)]
pub struct DesktopTranscript {
    messages: Vec<Message>,
}
impl DesktopTranscript {
    pub(crate) fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

/// Deterministic request-only annotations for a user-authorized continuation.
/// Missing tool outcomes are explicitly unknown and never trigger reexecution.
/// The durable raw history remains unchanged, including its interrupted calls.
pub fn desktop_request_history(raw: &[Message]) -> std::io::Result<Vec<Message>> {
    use polaris_provider::Role;
    let mut projected = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for message in raw {
        if message.role != Role::Tool {
            for id in pending.drain(..) {
                projected.push(Message::tool_result(&id,
                    "中断により結果は不明です。外部への影響を確認してから続行してください。自動再実行はしていません。"));
            }
        }
        match message.role {
            Role::Assistant => {
                if message.tool_call_id.is_some() {
                    return Err(std::io::Error::other("invalid assistant history"));
                }
                for call in &message.tool_calls {
                    if call.id.is_empty() || pending.contains(&call.id) {
                        return Err(std::io::Error::other("invalid tool call history"));
                    }
                    pending.push(call.id.clone());
                }
            }
            Role::Tool => {
                if !message.tool_calls.is_empty() {
                    return Err(std::io::Error::other("invalid tool result history"));
                }
                let Some(index) = message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| pending.iter().position(|p| p == id))
                else {
                    return Err(std::io::Error::other("orphan tool result history"));
                };
                pending.remove(index);
            }
            Role::User => {
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    return Err(std::io::Error::other("invalid user history"));
                }
            }
        }
        projected.push(message.clone());
    }
    for id in pending {
        projected.push(Message::tool_result(&id,
            "中断により結果は不明です。外部への影響を確認してから続行してください。自動再実行はしていません。"));
    }
    Ok(projected)
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
        if let Some(transcript) = &mut self.desktop_transcript {
            transcript.messages.push(message);
            // Validate before cloning into the mutable request view. Partial
            // tool groups are legitimate until completion, but user injection
            // or an oversized turn prevents further provider/tool dispatch.
            if let Err(error) = crate::desktop_store::validate_turn_suffix(
                &transcript.messages,
                polaris_desktop_protocol::run_state::Observation::Cancelled,
            ) {
                transcript.messages.pop();
                self.persistence_error = Some(error.to_string());
                return;
            }
            self.messages
                .push(transcript.messages.last().unwrap().clone());
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

#[cfg(test)]
mod desktop_transcript_tests {
    //! Raw output must survive request-only retention and refuse excess before dispatch.
    use super::*;
    #[test]
    fn interrupted_request_projection_closes_unknown_calls_without_changing_raw() {
        let raw = vec![
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "missing".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"do not repeat"}),
                }],
            ),
            Message::user("continue after inspecting effects"),
        ];
        let projected = desktop_request_history(&raw).unwrap();
        assert_eq!(raw.len(), 2);
        assert_eq!(projected.len(), 3);
        assert_eq!(projected[1].tool_call_id.as_deref(), Some("missing"));
        assert!(projected[1].content.contains("結果は不明"));
        assert_eq!(projected[2].content, raw[1].content);
        assert!(desktop_request_history(&[Message::tool_result("orphan", "value")]).is_err());
    }
    #[test]
    fn request_projection_does_not_replace_original_tool_output() {
        let mut session = Session::new();
        session.desktop_transcript = Some(Default::default());
        session.push_assistant_tool_calls(
            "",
            vec![ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"file"}),
            }],
            vec![],
        );
        session.push_tool_result("call", "full original evidence");
        session.messages[1].content = "retained excerpt".into();
        session.push_assistant("done", vec![]);
        assert_eq!(
            session.desktop_transcript.unwrap().messages[1].content,
            "full original evidence"
        );
    }
    #[test]
    fn oversized_or_user_suffix_stops_further_messages_without_truncating() {
        for bad in [
            Message::user("injected"),
            Message::assistant("\0".repeat(22_000)),
        ] {
            let mut session = Session::new();
            session.desktop_transcript = Some(Default::default());
            session.push_message(bad, false);
            assert!(session.check_persistence().is_err());
            session.push_assistant("must not continue", vec![]);
            assert!(session.messages.is_empty());
            assert!(session.desktop_transcript.unwrap().messages.is_empty());
        }
    }
}
