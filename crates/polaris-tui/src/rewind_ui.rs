//! TUI-only rewind interaction state. Checkpoint eligibility and file changes
//! are decided by the durable core owners, never by this key handler.

use std::time::{Duration, Instant};
use std::{collections::HashMap, path::Path};

use polaris_core::AgentEvent;

const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(750);

#[derive(Default)]
pub(crate) struct DoubleEscape {
    first: Option<Instant>,
}

impl DoubleEscape {
    /// Returns true only for a second idle Esc within the gesture window.
    pub(crate) fn press(&mut self, now: Instant) -> bool {
        if self
            .first
            .is_some_and(|first| now.saturating_duration_since(first) <= DOUBLE_ESCAPE_WINDOW)
        {
            self.first = None;
            true
        } else {
            self.first = Some(now);
            false
        }
    }

    pub(crate) fn cancel(&mut self) {
        self.first = None;
    }
}

#[derive(Default)]
pub(crate) struct EffectTracker {
    unknown: Option<String>,
    running_children: HashMap<String, usize>,
}

impl EffectTracker {
    pub(crate) fn observe(&mut self, event: &AgentEvent, restorable_path: impl Fn(&Path) -> bool) {
        match event {
            AgentEvent::ToolStarted { name, detail }
            | AgentEvent::SpawnActivity { name, detail, .. } => {
                self.observe_tool(name, detail, restorable_path);
            }
            AgentEvent::SpawnStarted { display_name, .. } => {
                *self
                    .running_children
                    .entry(display_name.clone())
                    .or_default() += 1;
            }
            AgentEvent::SpawnFinished { display_name, .. } => {
                if let Some(count) = self.running_children.get_mut(display_name) {
                    *count -= 1;
                    if *count == 0 {
                        self.running_children.remove(display_name);
                    }
                } else {
                    self.unknown = Some("A subagent ended without a verified start".into());
                }
            }
            _ => {}
        }
    }

    fn observe_tool(&mut self, name: &str, detail: &str, restorable_path: impl Fn(&Path) -> bool) {
        if self.unknown.is_some() {
            return;
        }
        match name {
            "read" | "skill" | "state" | "clarify" | "mcp_catalog" | "workflow_artifact"
            | "spawn" => {}
            "write" | "edit" => {
                let path = serde_json::from_str::<serde_json::Value>(detail)
                    .ok()
                    .and_then(|args| args.get("path")?.as_str().map(str::to_owned));
                if !path
                    .as_deref()
                    .is_some_and(|path| restorable_path(Path::new(path)))
                {
                    self.unknown = Some(format!(
                        "Cannot reconcile the {name} target with the restoration scope"
                    ));
                }
            }
            _ => self.unknown = Some(format!("Cannot verify external effects of {name}")),
        }
    }

    pub(crate) fn unknown_reason(&self) -> Option<String> {
        self.unknown.clone().or_else(|| {
            (!self.running_children.is_empty())
                .then(|| "A subagent has no verified termination".to_owned())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_only_on_two_nearby_escapes() {
        let start = Instant::now();
        let mut gesture = DoubleEscape::default();
        assert!(!gesture.press(start));
        assert!(gesture.press(start + Duration::from_millis(749)));
        assert!(!gesture.press(start + Duration::from_millis(750)));
    }

    #[test]
    fn late_or_interrupted_escape_starts_a_new_pair() {
        let start = Instant::now();
        let mut gesture = DoubleEscape::default();
        assert!(!gesture.press(start));
        assert!(!gesture.press(start + Duration::from_millis(751)));
        gesture.cancel();
        assert!(!gesture.press(start + Duration::from_millis(752)));
        assert!(gesture.press(start + Duration::from_millis(753)));
    }

    #[test]
    fn local_write_is_eligible_only_when_its_target_is_restorable() {
        let mut tracker = EffectTracker::default();
        tracker.observe(
            &AgentEvent::ToolStarted {
                name: "write".into(),
                detail: r#"{"path":"/project/src/main.rs","content":"x"}"#.into(),
            },
            |path| path.starts_with("/project/src"),
        );
        assert_eq!(tracker.unknown_reason(), None);
        tracker.observe(
            &AgentEvent::ToolStarted {
                name: "edit".into(),
                detail: r#"{"path":"/project/.git/config"}"#.into(),
            },
            |path| path.starts_with("/project/src"),
        );
        assert!(tracker.unknown_reason().is_some());
    }

    #[test]
    fn unknown_command_and_unfinished_child_block_rewind() {
        let mut tracker = EffectTracker::default();
        tracker.observe(
            &AgentEvent::SpawnStarted {
                display_name: "Sirius".into(),
                agent_type: "file-reader".into(),
                task: "inspect".into(),
            },
            |_| true,
        );
        assert!(tracker.unknown_reason().is_some());
        tracker.observe(
            &AgentEvent::SpawnFinished {
                display_name: "Sirius".into(),
                agent_type: "file-reader".into(),
                ok: true,
            },
            |_| true,
        );
        assert_eq!(tracker.unknown_reason(), None);
        tracker.observe(
            &AgentEvent::ToolStarted {
                name: "bash".into(),
                detail: r#"{"command":"curl example.com"}"#.into(),
            },
            |_| true,
        );
        assert!(tracker.unknown_reason().is_some());
    }

    #[test]
    fn duplicate_display_names_require_every_child_to_finish() {
        let mut tracker = EffectTracker::default();
        for _ in 0..2 {
            tracker.observe(
                &AgentEvent::SpawnStarted {
                    display_name: "Sirius".into(),
                    agent_type: "file-reader".into(),
                    task: "inspect".into(),
                },
                |_| true,
            );
        }
        tracker.observe(
            &AgentEvent::SpawnFinished {
                display_name: "Sirius".into(),
                agent_type: "file-reader".into(),
                ok: true,
            },
            |_| true,
        );
        assert!(tracker.unknown_reason().is_some());
    }
}
