//! Message history. In M1, this is append-only — no compaction, no
//! persistence to disk. Persistence and resume land in M5.

use polaris_provider::{Message, ToolCall};

#[derive(Default, Clone)]
pub struct Session {
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_user(&mut self, content: &str) {
        self.messages.push(Message::user(content));
    }

    pub fn push_assistant(&mut self, content: &str) {
        self.messages.push(Message::assistant(content));
    }

    /// Records an assistant turn together with the tool calls it made.
    /// Under OpenAI's round-trip protocol, this message must remain in the
    /// history holding its own `tool_calls` before the tool results are sent.
    pub fn push_assistant_tool_calls(&mut self, content: &str, tool_calls: Vec<ToolCall>) {
        self.messages
            .push(Message::assistant_with_tool_calls(content, tool_calls));
    }

    /// Records a tool result, tying it to the id of the call it responds to.
    pub fn push_tool_result(&mut self, tool_call_id: &str, content: &str) {
        self.messages
            .push(Message::tool_result(tool_call_id, content));
    }
}
