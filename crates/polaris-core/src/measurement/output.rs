//! M8b-3: what a turn's output was made of. The split is mechanical — the
//! body a model wrote, and the arguments it passed to tools — because those
//! are the two things a reduction can act on separately. Intent, such as
//! whether a body was an answer or a progress note, is only recorded when
//! the transcript already says so; nothing here guesses from the wording.
use serde::Serialize;

/// The visible parts of one turn's output, counted with the same tokenizer
/// the context budget uses. This is a diagnostic over what can be seen, not
/// the billed amount: the provider's own output count is that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Parts {
    pub body_tokens: usize,
    /// The arguments passed to tools. A diff sits inside an edit tool's
    /// arguments, so it is part of this and never counted again beside it.
    pub tool_argument_tokens: usize,
    pub tool_calls: usize,
}

impl Parts {
    pub fn visible_tokens(&self) -> usize {
        self.body_tokens + self.tool_argument_tokens
    }
}

/// Counts the parts of one turn. `tool_arguments` are the arguments as they
/// were actually sent, since re-encoding them would count a different text.
pub fn parts(body: &str, tool_arguments: &[String]) -> Parts {
    Parts {
        body_tokens: crate::budget::count_tokens(body),
        tool_argument_tokens: tool_arguments
            .iter()
            .map(|arguments| crate::budget::count_tokens(arguments))
            .sum(),
        tool_calls: tool_arguments.len(),
    }
}

/// How the visible parts compare with what the provider billed as output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "recount", rename_all = "snake_case")]
pub enum Recount {
    /// The provider gave no output count, so there is nothing to compare.
    NoReportedOutput,
    Agrees {
        tokens: u64,
    },
    /// The two differ. Re-encoded JSON, split boundaries and reasoning that
    /// was never shown all cause this; the gap is left whole rather than
    /// spread across the parts, which would invent a breakdown.
    Differs {
        reported: u64,
        visible: u64,
        /// Positive when the provider billed more than can be seen.
        unexplained: i64,
    },
}

pub fn recount(parts: &Parts, reported_output: super::Reported) -> Recount {
    let Some(reported) = reported_output.known() else {
        return Recount::NoReportedOutput;
    };
    let visible = parts.visible_tokens() as u64;
    if visible == reported {
        return Recount::Agrees { tokens: reported };
    }
    Recount::Differs {
        reported,
        visible,
        unexplained: i64::try_from(reported).unwrap_or(i64::MAX)
            - i64::try_from(visible).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests;
