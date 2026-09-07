//! Optional per-provider spacing of model request dispatches.
use std::time::Duration;
use tokio::time::Instant;

use crate::ProviderError;

pub(crate) const INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    On,
}

impl Mode {
    pub(crate) fn from_env() -> Result<Self, ProviderError> {
        match std::env::var("POLARIS_CACHE_PACING") {
            Ok(v) => Self::parse(Some(&v)),
            Err(std::env::VarError::NotUnicode(_)) => Err(ProviderError::Decode(
                "POLARIS_CACHE_PACING は off または on を指定してください".into(),
            )),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
        }
    }
    fn parse(value: Option<&str>) -> Result<Self, ProviderError> {
        match value {
            Some("on") => Ok(Self::On),
            Some("off") | None => Ok(Self::Off),
            _ => Err(ProviderError::Decode(
                "POLARIS_CACHE_PACING は off または on を指定してください".into(),
            )),
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

pub(crate) struct Gate {
    mode: Mode,
    interval: Duration,
    started: Instant,
    last: tokio::sync::Mutex<Option<Instant>>,
}
impl Gate {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
            mode,
            interval: INTERVAL,
            started: Instant::now(),
            last: tokio::sync::Mutex::new(None),
        }
    }
    pub(crate) async fn dispatch(&self) -> Dispatch {
        if self.mode == Mode::Off {
            return self.stamp(Instant::now(), Duration::ZERO);
        }
        let queued = Instant::now();
        let (mut last, contended) = match self.last.try_lock() {
            Ok(guard) => (guard, false),
            Err(_) => (self.last.lock().await, true),
        };
        let scheduled = last
            .map(|t| self.interval.saturating_sub(t.elapsed()))
            .unwrap_or(Duration::ZERO);
        if !scheduled.is_zero() {
            tokio::time::sleep(scheduled).await;
        }
        let now = Instant::now();
        *last = Some(now);
        let waited = if scheduled.is_zero() && !contended {
            Duration::ZERO
        } else {
            now.duration_since(queued)
        };
        self.stamp(now, waited)
    }
    fn stamp(&self, now: Instant, wait: Duration) -> Dispatch {
        Dispatch {
            wait,
            offset: now.duration_since(self.started),
            started: now.into_std(),
            unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }
    pub(crate) fn interval(&self) -> Duration {
        if self.mode == Mode::On {
            self.interval
        } else {
            Duration::ZERO
        }
    }
}
pub(crate) struct Dispatch {
    pub(crate) wait: Duration,
    pub(crate) offset: Duration,
    pub(crate) started: std::time::Instant,
    pub(crate) unix_ms: u128,
}

#[cfg(test)]
impl Gate {
    pub(crate) fn with_interval(mode: Mode, interval: Duration) -> Self {
        Self {
            mode,
            interval,
            started: Instant::now(),
            last: tokio::sync::Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_is_immediate_and_consecutive_waits_but_idle_does_not() {
        let gate = Gate::with_interval(Mode::On, Duration::from_millis(20));
        assert!(gate.dispatch().await.wait.is_zero());
        let next = gate.dispatch().await;
        assert!(next.wait >= Duration::from_millis(15));
        tokio::time::sleep(Duration::from_millis(22)).await;
        assert!(gate.dispatch().await.wait.is_zero());
    }

    #[tokio::test]
    async fn concurrent_callers_are_spaced_by_the_shared_gate() {
        let gate = std::sync::Arc::new(Gate::with_interval(Mode::On, Duration::from_millis(15)));
        let (a, b, c) = tokio::join!(gate.dispatch(), gate.dispatch(), gate.dispatch());
        let mut offsets = [a.offset, b.offset, c.offset];
        offsets.sort();
        assert!(
            offsets
                .windows(2)
                .all(|p| p[1] - p[0] >= Duration::from_millis(15))
        );
        assert!(a.wait.max(b.wait).max(c.wait) >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn cancelled_wait_does_not_reserve_a_future_slot() {
        let gate = Gate::with_interval(Mode::On, Duration::from_millis(50));
        gate.dispatch().await;
        {
            let queued = gate.dispatch();
            tokio::pin!(queued);
            assert!(
                tokio::time::timeout(Duration::from_millis(5), &mut queued)
                    .await
                    .is_err()
            );
        } // Drop the underlying future, not just a Pin<&mut Future>.
        tokio::time::sleep(Duration::from_millis(52)).await;
        let next = tokio::time::timeout(Duration::from_millis(100), gate.dispatch())
            .await
            .expect("cancelled waiter released the lock");
        assert!(next.wait.is_zero());
    }

    #[tokio::test]
    async fn off_never_waits() {
        let gate = Gate::with_interval(Mode::Off, Duration::from_secs(60));
        assert!(gate.dispatch().await.wait.is_zero());
        assert!(gate.dispatch().await.wait.is_zero());
    }

    #[test]
    fn invalid_environment_values_are_rejected() {
        assert!(Mode::parse(Some("bad")).is_err());
        assert!(Mode::parse(Some("")).is_err());
        assert!(matches!(Mode::parse(None), Ok(Mode::Off)));
        assert!(matches!(Mode::parse(Some("off")), Ok(Mode::Off)));
        assert!(matches!(Mode::parse(Some("on")), Ok(Mode::On)));
    }
}
