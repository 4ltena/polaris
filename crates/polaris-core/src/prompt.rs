//! The single place that assembles the set of things loaded every turn.
//!
//! Splices the constitution and environment info into the system prompt,
//! bundles it with the tool definitions to send, and turns it into
//! `AlwaysOn`.
//!
//! That the always-on context never exceeds 990 tokens, and never grows no
//! matter how many skills there are, is this design's central claim. For
//! that claim to be measurable, what production sends and what the tests
//! measure must be the same thing. This used to be assembled by `main.rs`,
//! with tests copying the same steps and assembling it separately. The two
//! silently drift apart, so a change adding one line for the skill catalog
//! to `main.rs` reached none of the tests (re-review's mutation N7).
//! Assembly is consolidated here into one path, and both `main.rs` and the
//! tests call the same `assemble_always_on`.

use polaris_tools::ToolSpec;

/// The system prompt that's always loaded. Cutting behavioral instructions
/// increases round trips and raises total cost, so don't cut this for the
/// sake of brevity. What may be cut is limited to structural duplication.
pub const SYSTEM_PROMPT: &str = "\
You are polaris, a coding agent. Read files and answer with what the code actually does.

Rules:
- State file paths as path:line so they can be opened directly.
- Never guess file contents. Read them.
- If the same error occurs three times in a row, stop and report it.
- Do not claim work is done without showing the command output that proves it.
- Read docs/filemap.md before tree searches only when filemap=ready.
- Verify counts and causal claims against real output before stating them.
- A cited path:line must actually support the claim it's attached to.
";

/// Assembles the context that's always loaded. Drops empty sections
/// heading and all.
///
/// The assembled result must be identical throughout the session. If this
/// changes every turn, the prompt cache's prefix shifts and the entire
/// history gets treated as uncached.
///
/// `constitution` is expected to already have been truncated to
/// `CONSTITUTION_LIMIT` by the caller (via `constitution::load` etc.), but
/// it's still run through `cap()` here too. Even if a future caller starts
/// passing untruncated raw text, the ceiling cannot be broken as long as it
/// goes through this guard. Input that's already been truncated is
/// unchanged, since `cap()` is idempotent.
///
/// `environment` is truncated to `ENVIRONMENT_LIMIT` for the same reason
/// and by the same mechanism. Both the cwd and the branch name have
/// lengths dictated by the disk / Git, and the caller
/// (`constitution::environment_block`) doesn't limit their length on its
/// own, so without an unconditional cap here, the always-on context's
/// ceiling could be broken by a single abnormally long cwd. To leave no
/// way around it short of going through the caller, this is aligned with
/// the same "cap here too, redundantly" design as the constitution.
pub(crate) fn build_system(constitution: &str, environment: &str) -> String {
    let constitution =
        crate::constitution::cap(constitution, crate::constitution::CONSTITUTION_LIMIT);
    let environment = crate::constitution::cap(environment, crate::constitution::ENVIRONMENT_LIMIT);
    let mut s = String::from(SYSTEM_PROMPT);
    if !constitution.is_empty() {
        s.push_str("\n## Project rules\n");
        s.push_str(&constitution);
        s.push('\n');
    }
    if !environment.is_empty() {
        s.push_str("\n## Environment\n");
        s.push_str(&environment);
        s.push('\n');
    }
    s
}

/// The set of things always sent every turn, without fail. Holds the
/// system prompt together with the tool definitions exposed for that turn
/// as a single unit.
///
/// The fields are private, and no means of mutating them is exposed
/// either. The only way to build one is through [`assemble_always_on`].
/// This isn't a matter of good manners — it follows from the very
/// invariant this type protects: if a caller could append anything to the
/// already-assembled system prompt, a path to "grow the always-on context"
/// would remain open outside `polaris-core`, and acceptance criterion 1's
/// claim that "no path exists to exceed the ceiling" would end up
/// depending on how `main.rs` happens to be written. With no public
/// constructor and no public mutator, that path is unrepresentable, not
/// merely something callers are expected to avoid. Any change that needs
/// to append something has no choice but to touch the inside of
/// [`assemble_always_on`], and that inside is measured by
/// `budget::tests::the_always_on_total_does_not_move_as_the_number_of_skills_grows`
/// with 0 / 15 / 100 skills.
#[derive(Debug, Clone)]
pub struct AlwaysOn {
    system: String,
    tools: Vec<ToolSpec>,
    skills_seen: usize,
    #[cfg(unix)]
    environment: String,
    #[cfg(unix)]
    agents_refresh: Option<crate::constitution::AgentsRefresh>,
}

impl AlwaysOn {
    /// Trusted owner only: bind the original host sources, never a run copy.
    /// No timer is started. Failure on the initial read is not an empty profile.
    #[cfg(unix)]
    pub fn with_agents_refresh(
        mut self,
        source: crate::constitution::AgentsRefresh,
    ) -> std::io::Result<Self> {
        self.system = build_system(&source.read()?, &self.environment);
        self.agents_refresh = Some(source);
        Ok(self)
    }

    pub(crate) fn system_for_request(&self) -> std::io::Result<String> {
        #[cfg(unix)]
        if let Some(source) = &self.agents_refresh {
            return Ok(build_system(&source.read()?, &self.environment));
        }
        Ok(self.system.clone())
    }
    /// The system prompt to send.
    pub fn system(&self) -> &str {
        &self.system
    }

    /// The tool definitions to send.
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// This set's measured token count. Every budget test measures through
    /// this path.
    pub fn tokens(&self) -> usize {
        crate::budget::always_on_tokens(&self.system, &self.tools)
    }

    /// The number of skills handed in at assembly time.
    ///
    /// None of them are loaded into the always-on context at all (that
    /// they aren't loaded is this milestone's claim). The reason for
    /// keeping just the count anyway is to let a test like "the total
    /// doesn't move even when 100 skills are handed in" catch the no-op
    /// failure mode of actually only having had 0 skills handed in. B3's
    /// original defect was exactly "what was supposedly handed in never
    /// made it into the calculation", so this lets the assembling side
    /// itself state that it was actually received.
    pub fn skills_seen(&self) -> usize {
        self.skills_seen
    }
}

/// The single function that assembles the always-on context. Both
/// `main.rs` and the tests call this.
///
/// Takes `skills` and loads none of it. That is this function's claim —
/// the router (the `skill` tool) handles discovery, so there's no need to
/// surface a skill's name or description into the always-on context, and
/// as long as it isn't surfaced, the total doesn't depend on the number of
/// skills. It's taken as an argument so that tests can measure that it
/// isn't loaded: pass 3 inputs with different counts through the same
/// function and check that the resulting token counts match. A function
/// that took no argument would only be a tautology — evaluating the same
/// expression 3 times and comparing it to itself (B3's original defect).
pub fn assemble_always_on(
    constitution: &str,
    environment: &str,
    skills: &[polaris_skills::Skill],
) -> AlwaysOn {
    AlwaysOn {
        system: build_system(constitution, environment),
        tools: polaris_tools::all_specs(),
        skills_seen: skills.len(),
        #[cfg(unix)]
        environment: environment.to_owned(),
        #[cfg(unix)]
        agents_refresh: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constitution::CONSTITUTION_LIMIT;

    #[test]
    fn system_prompt_requires_verifying_quantitative_and_causal_claims() {
        assert!(SYSTEM_PROMPT.contains("Verify counts and causal claims"));
    }

    #[test]
    fn system_prompt_requires_citations_to_actually_support_the_claim() {
        assert!(SYSTEM_PROMPT.contains("must actually support the claim"));
    }

    #[test]
    fn caps_an_oversized_constitution_passed_directly() {
        // Confirm build_system itself carries this protection. Even if a
        // caller passes raw text and forgot to truncate it, the
        // constitution portion does not exceed the ceiling.
        let oversized = "rule".repeat(5000);
        let system = build_system(&oversized, "");

        let prefix = format!("{SYSTEM_PROMPT}\n## Project rules\n");
        let body = system
            .strip_prefix(&prefix)
            .expect("the Project rules section was not assembled")
            .strip_suffix('\n')
            .expect("missing trailing newline");

        let n = crate::budget::count_tokens(body);
        assert!(
            n <= CONSTITUTION_LIMIT,
            "the constitution portion exceeds the ceiling: {n} tokens"
        );
        assert!(
            !body.is_empty(),
            "must not return empty from non-empty input"
        );
    }

    #[test]
    fn skills_seen_reports_what_was_handed_in() {
        // `AlwaysOn::skills_seen` is the only clue the budget tests have
        // for confirming "were 100 really handed in", so pin down that it
        // reflects the input itself. An implementation that always returns
        // 0 fails here.
        let skills: Vec<polaris_skills::Skill> = (0..3)
            .map(|i| polaris_skills::Skill {
                name: format!("s{i}"),
                description: "description".into(),
                body: "body".into(),
                path: format!("/x/s{i}/SKILL.md").into(),
            })
            .collect();

        assert_eq!(assemble_always_on("", "", &[]).skills_seen(), 0);
        assert_eq!(assemble_always_on("", "", &skills).skills_seen(), 3);
    }

    #[test]
    fn already_capped_input_is_unchanged() {
        // Input that's already been truncated is unchanged, since cap() is idempotent.
        let already_capped = crate::constitution::cap("a fixed rule.", CONSTITUTION_LIMIT);
        let system = build_system(&already_capped, "");
        assert!(system.contains("a fixed rule."));
    }
}
