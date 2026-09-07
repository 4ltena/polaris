//! Opaque transport continuity scoped to one logical user turn.
//!
//! This state is deliberately neither serializable nor `Debug`: it may hold
//! a server-issued header and must never outlive the run loop that owns it.

use std::sync::Mutex;

use reqwest::header::HeaderValue;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAffinityMode {
    Off,
    On,
}

impl TurnAffinityMode {
    pub fn from_env() -> Result<Self, crate::ProviderError> {
        match std::env::var("POLARIS_TURN_AFFINITY") {
            Ok(value) if value == "on" => Ok(Self::On),
            Ok(value) if value == "off" => Ok(Self::Off),
            Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(crate::ProviderError::Decode(
                "POLARIS_TURN_AFFINITY は off または on を指定してください".into(),
            )),
            Err(std::env::VarError::NotPresent) => Ok(Self::Off),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// Request properties which make a server-issued turn state safe to reuse.
/// No implementation exposes or logs these values.
pub(crate) struct TurnIdentity {
    pub(crate) origin: String,
    pub(crate) account_id: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) cache_key: String,
}

impl TurnIdentity {
    fn matches(&self, other: &Self) -> bool {
        self.origin == other.origin
            && self.account_id == other.account_id
            && self.model == other.model
            && self.effort == other.effort
            && self.cache_key == other.cache_key
    }
}

struct StoredState {
    identity: TurnIdentity,
    value: HeaderValue,
}

/// A fresh instance is required for every `run_loop`.
#[derive(Default)]
pub struct TurnContext(Mutex<Option<StoredState>>);

impl TurnContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops the opaque state after a completed history compaction.
    pub fn clear(&self) {
        *self.0.lock().expect("turn context lock poisoned") = None;
    }

    pub(crate) fn take_matching(&self, identity: &TurnIdentity) -> Option<HeaderValue> {
        let mut state = self.0.lock().expect("turn context lock poisoned");
        match state.as_ref() {
            Some(stored) if stored.identity.matches(identity) => Some(stored.value.clone()),
            Some(_) => {
                // Never recover prior A after an A -> B -> A transition.
                *state = None;
                None
            }
            None => None,
        }
    }

    pub(crate) fn store_first(&self, identity: TurnIdentity, mut value: HeaderValue) {
        value.set_sensitive(true);
        let mut state = self.0.lock().expect("turn context lock poisoned");
        if state.is_none() {
            *state = Some(StoredState { identity, value });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(
        origin: &str,
        account_id: &str,
        model: &str,
        effort: Option<&str>,
        cache_key: &str,
    ) -> TurnIdentity {
        TurnIdentity {
            origin: origin.into(),
            account_id: account_id.into(),
            model: model.into(),
            effort: effort.map(str::to_owned),
            cache_key: cache_key.into(),
        }
    }

    #[test]
    fn first_state_wins_and_an_identity_change_forgets_it_permanently() {
        let context = TurnContext::new();
        let a = || identity("https://example.invalid", "a", "m", Some("medium"), "key");
        context.store_first(a(), HeaderValue::from_static("first"));
        context.store_first(a(), HeaderValue::from_static("second"));
        assert_eq!(
            context
                .take_matching(&a())
                .and_then(|value| value.to_str().ok().map(str::to_owned)),
            Some("first".into())
        );
        assert!(
            context
                .take_matching(&identity(
                    "https://example.invalid",
                    "b",
                    "m",
                    Some("medium"),
                    "key"
                ))
                .is_none()
        );
        assert!(context.take_matching(&a()).is_none());
    }

    #[test]
    fn every_routing_boundary_discards_the_state() {
        let base = || identity("https://one.invalid", "a", "m", Some("medium"), "key");
        let changes = [
            identity("https://two.invalid", "a", "m", Some("medium"), "key"),
            identity("https://one.invalid", "b", "m", Some("medium"), "key"),
            identity("https://one.invalid", "a", "other", Some("medium"), "key"),
            identity("https://one.invalid", "a", "m", Some("high"), "key"),
            identity("https://one.invalid", "a", "m", Some("medium"), "other-key"),
        ];
        for changed in changes {
            let context = TurnContext::new();
            context.store_first(base(), HeaderValue::from_static("opaque"));
            assert!(context.take_matching(&changed).is_none());
            assert!(
                context.take_matching(&base()).is_none(),
                "A -> B -> A recovered stale state"
            );
        }
    }

    #[test]
    fn clear_drops_state_without_recovering_it() {
        let context = TurnContext::new();
        let expected = identity("https://one.invalid", "a", "m", None, "key");
        context.store_first(
            identity("https://one.invalid", "a", "m", None, "key"),
            HeaderValue::from_static("opaque"),
        );
        context.clear();
        assert!(context.take_matching(&expected).is_none());
    }
}
