//! The local typed decider, over a pinned SemIf helper.
//!
//! SemIf answers the same three shapes Jev does — [`Question::Noul`],
//! [`Question::Choice`], [`Question::Score`] — by reading the logits of the
//! option tokens rather than generating text. That is why it fits this
//! contract at all: nothing here parses prose, and a reply that does not
//! name the questions it answers is rejected rather than interpreted.
//!
//! The helper is a pinned process started by the installer, not a module
//! this crate imports and not a model id this crate resolves. It speaks
//! NDJSON: one request object per line in, one reply object per line out.
//! Keeping it out of process is what keeps Python and MLX out of the
//! user's environment, and what lets a protocol violation end the helper
//! without ending the run.
//!
//! **A local probability is not Jev's probability.** The two are calibrated
//! separately, and a threshold tuned against one is not carried to the
//! other. `SemIfDecider` returns the numbers the helper reports; where they
//! are compared against a gate, that gate is the caller's and belongs to
//! whichever judge produced them.

use super::{Answer, Decider, Decision, DecisionError, Question};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The helper this run may talk to.
///
/// Held as an absolute path to an existing file rather than a command
/// string: a decider that can be pointed at an arbitrary program by
/// configuration is a decider that can run one.
pub struct SemIfDecider {
    helper: PathBuf,
}

impl SemIfDecider {
    /// The decider for this machine, or `None` where the pinned helper is
    /// not installed.
    ///
    /// There is no installer step for it yet, so this is `None` until one
    /// places the helper. The call sites are written now so that the
    /// installer is the only thing left to add.
    pub fn available(helper: Option<&Path>) -> Option<Self> {
        let helper = helper?;
        (helper.is_absolute() && helper.is_file()).then(|| Self {
            helper: helper.to_owned(),
        })
    }

    /// The one line sent to the helper for a batch of questions.
    ///
    /// The shape is the contract, so it is built here and checked by a
    /// test rather than assembled at the call site.
    pub(crate) fn request_line(
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<String, DecisionError> {
        let body = serde_json::json!({ "state": state, "questions": questions });
        serde_json::to_string(&body).map_err(|_| DecisionError::TooLarge)
    }

    /// The reply, checked against the questions it claims to answer.
    ///
    /// Every question must be answered, no answer may name a question that
    /// was not asked, and a `Choice` must name one of its own options. A
    /// local model that drifts produces a rejected reply, not a decision
    /// nobody checked.
    pub(crate) fn parse_reply(
        line: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        #[derive(serde::Deserialize)]
        struct Wire {
            answers: BTreeMap<String, Answer>,
            #[serde(default)]
            input_tokens: Option<u64>,
            #[serde(default)]
            output_tokens: Option<u64>,
        }
        let wire: Wire = serde_json::from_str(line).map_err(|_| DecisionError::Malformed)?;
        let parsed = Decision {
            answers: wire.answers,
            input_tokens: wire.input_tokens,
            output_tokens: wire.output_tokens,
        };
        if parsed.answers.len() != questions.len() {
            return Err(DecisionError::Malformed);
        }
        for (name, answer) in &parsed.answers {
            let Some(question) = questions.get(name) else {
                return Err(DecisionError::Malformed);
            };
            let ok = match question {
                Question::Noul { .. } => answer.noul.is_some(),
                Question::Choice { criteria, .. } => answer
                    .choice
                    .as_deref()
                    .is_some_and(|chosen| criteria.contains_key(chosen)),
                Question::Score { .. } => answer.score.is_some(),
            };
            if !ok {
                return Err(DecisionError::Malformed);
            }
        }
        Ok(parsed)
    }
}

#[async_trait::async_trait]
impl Decider for SemIfDecider {
    fn name(&self) -> &str {
        "semif"
    }

    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let line = Self::request_line(state, questions)?;
        // ponytail: one helper per call. A 4B model reloads each time,
        // which is the whole reason the plan calls for one owner holding
        // one process for the run. Put that owner in front of this and
        // nothing below changes.
        let mut child = tokio::process::Command::new(&self.helper)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| DecisionError::Malformed)?;
        let mut stdin = child.stdin.take().ok_or(DecisionError::Malformed)?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|_| DecisionError::Malformed)?;
        stdin
            .shutdown()
            .await
            .map_err(|_| DecisionError::Malformed)?;
        drop(stdin);
        let stdout = child.stdout.take().ok_or(DecisionError::Malformed)?;
        let mut reply = String::new();
        BufReader::new(stdout)
            .read_line(&mut reply)
            .await
            .map_err(|_| DecisionError::Malformed)?;
        // The helper is not left behind on a bad reply.
        let _ = child.kill().await;
        Self::parse_reply(reply.trim(), questions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn questions() -> BTreeMap<String, Question> {
        BTreeMap::from([
            (
                "kind".to_owned(),
                Question::Choice {
                    instructions: "どの段から始めるか".into(),
                    criteria: BTreeMap::from([
                        ("brainstorm".to_owned(), "案を出す".to_owned()),
                        ("implement".to_owned(), "ファイルを変える".to_owned()),
                    ]),
                },
            ),
            (
                "weight".to_owned(),
                Question::Score {
                    instructions: "どれだけ重いか".into(),
                    criteria: vec!["軽い".into(), "重い".into()],
                },
            ),
        ])
    }

    fn reply(kind: &str) -> String {
        format!(
            r#"{{"answers":{{"kind":{{"choice":"{kind}"}},"weight":{{"score":1.25}}}},
                "input_tokens":10,"output_tokens":0}}"#
        )
    }

    /// A well-formed reply is accepted with both answers intact.
    #[test]
    fn a_reply_that_answers_every_question_is_accepted() {
        let decision = SemIfDecider::parse_reply(&reply("implement"), &questions()).unwrap();
        assert_eq!(
            decision.answers["kind"].choice.as_deref(),
            Some("implement")
        );
        assert_eq!(decision.answers["weight"].score, Some(1.25));
    }

    /// The three ways a local model drifts, each rejected rather than
    /// interpreted: an option nobody offered, a missing answer, and an
    /// answer to a question nobody asked.
    #[test]
    fn a_drifting_reply_is_rejected_rather_than_interpreted() {
        for line in [
            reply("deploy"),
            r#"{"answers":{"kind":{"choice":"implement"}}}"#.to_owned(),
            r#"{"answers":{"kind":{"choice":"implement"},"weight":{"score":1.0},
                "extra":{"noul":0.5}}}"#
                .to_owned(),
            "not json".to_owned(),
        ] {
            assert_eq!(
                SemIfDecider::parse_reply(&line, &questions()),
                Err(DecisionError::Malformed),
                "accepted a reply it should have refused: {line}"
            );
        }
    }

    /// The helper is a file on disk, named absolutely. A relative path, a
    /// missing file, or no path at all leaves the decider absent.
    #[test]
    fn the_decider_is_absent_without_a_pinned_helper() {
        assert!(SemIfDecider::available(None).is_none());
        assert!(SemIfDecider::available(Some(Path::new("semif-helper"))).is_none());
        assert!(SemIfDecider::available(Some(Path::new("/nonexistent/semif"))).is_none());
        let here = std::env::current_exe().unwrap();
        assert!(SemIfDecider::available(Some(&here)).is_some());
    }
}
