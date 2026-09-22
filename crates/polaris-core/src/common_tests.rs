//! Black-box checks an implementer selects instead of writing.
//!
//! Most of what a change needs proved is the same six questions asked of a
//! different command: does it exit the way it should, does it refuse bad
//! input, does it say what it promised on stdout and stderr, does it leave
//! the files it said it would, does its structured output match a schema,
//! and does running it twice not break what the first run did. Written out
//! as test code each time, those become programs to maintain that all say
//! the same thing.
//!
//! So they are data here. The implementer picks the cases its change
//! actually needs and supplies the arguments; nothing is applied to every
//! program by default. Anything genuinely particular to one change — a
//! library's internals, a GUI, concurrency, recovery — stays a test in its
//! own language, because those are the ones a shared runner would get
//! wrong.
//!
//! The command is an argument vector, never a shell string: a case that
//! takes a string invites the quoting bug it was meant to catch. Sandbox,
//! write root, timeout and environment come from the caller's existing
//! boundary, so a case cannot quietly widen one.

use serde::{Deserialize, Serialize};

/// What one case checks. Adding a variant is a decision about what
/// belongs in the shared set, so the list is short on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "case")]
pub enum Case {
    /// The command exits with exactly this status.
    ExitsWith { status: i32 },
    /// The command refuses this input rather than acting on it: a
    /// non-zero exit, and nothing written where output was expected.
    RejectsInput {
        argv: Vec<String>,
        stdin: Option<String>,
    },
    /// stdout or stderr contains, or does not contain, a fixed string.
    /// Held to substrings deliberately — a case that matches whole output
    /// breaks on every wording change and teaches people to stop reading
    /// failures.
    Says {
        stream: Stream,
        contains: Vec<String>,
        absent: Vec<String>,
    },
    /// These paths exist after the run, relative to the write root.
    Creates { paths: Vec<String> },
    /// stdout parses as JSON and carries these pointers.
    StructuredOutput { pointers: Vec<String> },
    /// Running the same argv twice leaves the same files and the same
    /// exit status. Selected, never assumed: plenty of correct programs
    /// are not idempotent.
    RepeatsCleanly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One program and the cases chosen for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The command, as an argument vector.
    pub argv: Vec<String>,
    pub cases: Vec<Case>,
}

/// What a plan is not allowed to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// No command to run.
    EmptyArgv,
    /// A single string where an argument vector belongs — the shape that
    /// hides a quoting bug.
    ShellString(String),
    /// No cases, which is a plan that proves nothing.
    NoCases,
    /// A path that leaves the write root.
    EscapesWriteRoot(String),
}

impl Plan {
    /// Reject the shapes that would widen a boundary or prove nothing.
    ///
    /// The escape check is textual on purpose: it runs before anything is
    /// executed, and a plan naming `..` has no business reaching a
    /// canonicaliser to find out whether it happened to stay inside.
    pub fn check(&self) -> Result<(), PlanError> {
        let Some(program) = self.argv.first() else {
            return Err(PlanError::EmptyArgv);
        };
        if self.argv.len() == 1 && program.split_whitespace().count() > 1 {
            return Err(PlanError::ShellString(program.clone()));
        }
        if self.cases.is_empty() {
            return Err(PlanError::NoCases);
        }
        for case in &self.cases {
            let Case::Creates { paths } = case else {
                continue;
            };
            for path in paths {
                let p = std::path::Path::new(path);
                if p.is_absolute() || p.components().any(|c| c.as_os_str() == "..") {
                    return Err(PlanError::EscapesWriteRoot(path.clone()));
                }
            }
        }
        Ok(())
    }

    /// The cases named, for the record a run leaves behind.
    pub fn case_names(&self) -> Vec<&'static str> {
        self.cases
            .iter()
            .map(|case| match case {
                Case::ExitsWith { .. } => "exits_with",
                Case::RejectsInput { .. } => "rejects_input",
                Case::Says { .. } => "says",
                Case::Creates { .. } => "creates",
                Case::StructuredOutput { .. } => "structured_output",
                Case::RepeatsCleanly => "repeats_cleanly",
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(argv: &[&str], cases: Vec<Case>) -> Plan {
        Plan {
            argv: argv.iter().map(ToString::to_string).collect(),
            cases,
        }
    }

    /// A plan is an argument vector with at least one case, and nothing
    /// else gets to run.
    #[test]
    fn a_plan_must_be_a_command_with_something_to_prove() {
        assert_eq!(
            plan(&[], vec![Case::RepeatsCleanly]).check(),
            Err(PlanError::EmptyArgv)
        );
        assert_eq!(plan(&["true"], vec![]).check(), Err(PlanError::NoCases));
        assert!(
            plan(&["true"], vec![Case::ExitsWith { status: 0 }])
                .check()
                .is_ok()
        );
    }

    /// One string holding a whole command line is the shape that hides a
    /// quoting bug, so it is refused rather than split here.
    #[test]
    fn a_shell_string_is_not_an_argument_vector() {
        let got = plan(&["rm -rf /tmp/x"], vec![Case::RepeatsCleanly]).check();
        assert!(matches!(got, Err(PlanError::ShellString(_))), "{got:?}");
        // The same words as real arguments are fine.
        assert!(
            plan(&["rm", "-rf", "/tmp/x"], vec![Case::RepeatsCleanly])
                .check()
                .is_ok()
        );
    }

    /// A case cannot name a file outside the write root, by absolute path
    /// or by climbing out of it.
    #[test]
    fn a_case_cannot_name_a_file_outside_the_write_root() {
        for path in ["/etc/passwd", "../outside", "a/../../b"] {
            let got = plan(
                &["true"],
                vec![Case::Creates {
                    paths: vec![path.into()],
                }],
            )
            .check();
            assert!(
                matches!(got, Err(PlanError::EscapesWriteRoot(_))),
                "{path} was allowed: {got:?}"
            );
        }
        assert!(
            plan(
                &["true"],
                vec![Case::Creates {
                    paths: vec!["out/report.json".into()]
                }]
            )
            .check()
            .is_ok()
        );
    }

    /// The plan is data, so it round-trips and the record can name which
    /// cases ran without the runner being present.
    #[test]
    fn a_plan_round_trips_and_names_its_cases() {
        let original = plan(
            &["polaris", "--help"],
            vec![
                Case::ExitsWith { status: 0 },
                Case::Says {
                    stream: Stream::Stdout,
                    contains: vec!["Usage".into()],
                    absent: vec![],
                },
            ],
        );
        let text = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&text).unwrap(), original);
        assert_eq!(original.case_names(), ["exits_with", "says"]);
    }
}
