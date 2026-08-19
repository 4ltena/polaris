//! Stop conditions. No automatic recovery is attempted. Continuing to spin
//! while broken is the most expensive outcome, so the decision is handed
//! back to the caller.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The same error occurred 3 times in a row.
    RepeatedError(String),
    /// The turn limit was reached.
    MaxTurns,
}

pub struct StopTracker {
    last_error: Option<String>,
    streak: u32,
    turns: u32,
    max_turns: u32,
}

impl StopTracker {
    pub fn new(max_turns: u32) -> Self {
        Self {
            last_error: None,
            streak: 0,
            turns: 0,
            max_turns,
        }
    }

    pub fn observe_error(&mut self, msg: &str) -> Option<StopReason> {
        if self.last_error.as_deref() == Some(msg) {
            self.streak += 1;
        } else {
            self.last_error = Some(msg.to_string());
            self.streak = 1;
        }
        if self.streak >= 3 {
            return Some(StopReason::RepeatedError(msg.to_string()));
        }
        None
    }

    /// Records a successful tool call and resets the consecutive-error
    /// streak. Without calling this, `last_error` / `streak` would only ever
    /// be updated from errors, so a sequence like
    /// `error, success, error, success, error` would be judged as "the same
    /// error occurred 3 times in a row" — even though it never actually
    /// occurred consecutively.
    pub fn observe_success(&mut self) {
        self.last_error = None;
        self.streak = 0;
    }

    pub fn observe_turn(&mut self) -> Option<StopReason> {
        self.turns += 1;
        if self.turns >= self.max_turns {
            return Some(StopReason::MaxTurns);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_after_three_identical_errors() {
        let mut t = StopTracker::new(100);
        assert!(t.observe_error("boom").is_none());
        assert!(t.observe_error("boom").is_none());
        assert!(matches!(
            t.observe_error("boom"),
            Some(StopReason::RepeatedError(_))
        ));
    }

    #[test]
    fn different_errors_reset_the_streak() {
        let mut t = StopTracker::new(100);
        t.observe_error("boom");
        t.observe_error("boom");
        assert!(t.observe_error("other").is_none());
        assert!(t.observe_error("other").is_none());
        assert!(matches!(
            t.observe_error("other"),
            Some(StopReason::RepeatedError(_))
        ));
    }

    #[test]
    fn success_between_errors_resets_the_streak_so_interleaved_errors_never_trip() {
        let mut t = StopTracker::new(100);
        for _ in 0..5 {
            assert!(t.observe_error("boom").is_none());
            t.observe_success();
        }
    }

    #[test]
    fn stops_at_max_turns() {
        let mut t = StopTracker::new(2);
        assert!(t.observe_turn().is_none());
        assert!(matches!(t.observe_turn(), Some(StopReason::MaxTurns)));
    }
}
