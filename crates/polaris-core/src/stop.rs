//! 停止条件。自動修復は行わない。壊れたまま回り続けるのが最も高くつくため、
//! 判断は呼び出し側へ返す。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 同一のエラーが 3 回続いた。
    RepeatedError(String),
    /// ターン数の上限に達した。
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
    fn stops_at_max_turns() {
        let mut t = StopTracker::new(2);
        assert!(t.observe_turn().is_none());
        assert!(matches!(t.observe_turn(), Some(StopReason::MaxTurns)));
    }
}
