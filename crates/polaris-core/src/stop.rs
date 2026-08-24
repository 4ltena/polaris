//! Stop conditions. No automatic recovery is attempted. Continuing to spin
//! while broken is the most expensive outcome, so the decision is handed
//! back to the caller.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The same error occurred 3 times in a row.
    RepeatedError(String),
    /// The turn limit was reached.
    MaxTurns,
    /// A subagent's declared wall-clock budget (seconds) was exceeded.
    WallSeconds(u32),
    /// A subagent's output failed schema validation twice in a row.
    SchemaMismatch,
}

pub struct StopTracker {
    last_error: Option<String>,
    streak: u32,
    turns: u32,
    max_turns: u32,
    wall_seconds: Option<u32>,
    started: Option<std::time::Instant>,
    schema_mismatches: u32,
}

impl StopTracker {
    pub fn new(max_turns: u32) -> Self {
        Self {
            last_error: None,
            streak: 0,
            turns: 0,
            max_turns,
            wall_seconds: None,
            started: None,
            schema_mismatches: 0,
        }
    }

    /// subagent 用。壁時計の起点はこの呼び出し時点になる。
    pub fn with_wall_seconds(max_turns: u32, wall_seconds: u32) -> Self {
        Self {
            wall_seconds: Some(wall_seconds),
            started: Some(std::time::Instant::now()),
            ..Self::new(max_turns)
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

    /// 壁時計を持たないトラッカー(ルート)では常に `None`。
    pub fn observe_wall_clock(&mut self) -> Option<StopReason> {
        let (limit, started) = (self.wall_seconds?, self.started?);
        if started.elapsed().as_secs() >= u64::from(limit) {
            Some(StopReason::WallSeconds(limit))
        } else {
            None
        }
    }

    /// 2回連続の不一致で停止する。1回目は `None` を返し、呼び出し側が
    /// 検証エラーを添えて1回だけ再試行する運びになる。
    pub fn observe_schema_mismatch(&mut self) -> Option<StopReason> {
        self.schema_mismatches += 1;
        if self.schema_mismatches >= 2 {
            Some(StopReason::SchemaMismatch)
        } else {
            None
        }
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

    #[test]
    fn wall_clock_trips_after_the_declared_seconds() {
        let start = std::time::Instant::now();
        let mut t = StopTracker::with_wall_seconds(100, 0); // 0秒 = 即座に超過
        std::thread::sleep(std::time::Duration::from_millis(1));
        let r = t.observe_wall_clock();
        assert!(matches!(r, Some(StopReason::WallSeconds(0))));
        let _ = start; // 経過確認は表示上の意図のみ、アサーション自体は上のmatchesで完結
    }

    #[test]
    fn wall_clock_does_not_trip_before_the_declared_seconds() {
        let mut t = StopTracker::with_wall_seconds(100, 3600);
        assert_eq!(t.observe_wall_clock(), None);
    }

    #[test]
    fn a_tracker_without_wall_seconds_never_trips_on_wall_clock() {
        let mut t = StopTracker::new(100);
        assert_eq!(t.observe_wall_clock(), None);
    }

    #[test]
    fn schema_mismatch_trips_on_the_second_occurrence() {
        let mut t = StopTracker::new(100);
        assert_eq!(t.observe_schema_mismatch(), None);
        assert_eq!(
            t.observe_schema_mismatch(),
            Some(StopReason::SchemaMismatch)
        );
    }
}
