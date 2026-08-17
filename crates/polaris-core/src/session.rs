//! メッセージ履歴。M1 では追加のみで、圧縮もディスクへの永続化も持たない。
//! 永続化と再開は M5 で入れる。

use polaris_provider::{Message, ToolCall};

#[derive(Default)]
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

    /// アシスタントのターンを、それが行ったツール呼び出しとともに記録する。
    /// OpenAI の往復規約では、ツール結果を送る前にこのメッセージ自体が
    /// `tool_calls` を保持したまま履歴に残っていなければならない。
    pub fn push_assistant_tool_calls(&mut self, content: &str, tool_calls: Vec<ToolCall>) {
        self.messages
            .push(Message::assistant_with_tool_calls(content, tool_calls));
    }

    /// ツール結果を、それが応答する呼び出しの id と結び付けて記録する。
    pub fn push_tool_result(&mut self, tool_call_id: &str, content: &str) {
        self.messages
            .push(Message::tool_result(tool_call_id, content));
    }
}
