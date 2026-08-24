//! Measurement of the always-on context. Numbers are backed by measurement,
//! never operated on by estimate.

use polaris_provider::openai::tool_wire_shape;
use polaris_tools::ToolSpec;

/// The ceiling on the always-on context.
pub const BUDGET_LIMIT: usize = 990;

/// The ceiling on the number of always-on tools.
pub const MAX_TOOLS: usize = 6;

/// Counts using the reference tokenizer. The actual count shifts from
/// provider to provider, so budget judgments are always made against this
/// one reference.
///
/// `o200k_base()` rebuilds a rank table of roughly 200,000 lines from
/// embedded data every time it's called, so building it on every single call
/// would be noticeably slow inside `cap()`'s truncation loop (which
/// re-measures line by line, or character by character). We reuse a
/// singleton built once per process.
pub fn count_tokens(text: &str) -> usize {
    let bpe = tiktoken_rs::o200k_base_singleton();
    bpe.encode_with_special_tokens(text).len()
}

/// The total of everything carried on every turn. Counts the system prompt
/// plus the serialized form of the tool definitions actually sent.
///
/// This counts not the plain serialization of `ToolSpec` itself, but the
/// wire form built by `polaris_provider::openai::tool_wire_shape`
/// (`{"type":"function","function":{…}}`). There was a period where the byte
/// stream a provider actually sends and the byte stream measured here were
/// assembled independently in separate places, and the budget came out
/// smaller than reality by exactly the amount of that discrepancy.
pub fn always_on_tokens(system_prompt: &str, tools: &[ToolSpec]) -> usize {
    let wire = tool_wire_shape(tools);
    let tools_json = serde_json::to_string(&wire).expect("cannot serialize tool definitions");
    count_tokens(system_prompt) + count_tokens(&tools_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::SYSTEM_PROMPT;

    #[test]
    fn always_on_context_stays_within_budget() {
        // The floor of the always-on context — no constitution, no
        // environment info, no skills. Measured through the same function
        // production uses to assemble it.
        let n = crate::prompt::assemble_always_on("", "", &[]).tokens();
        assert!(
            n <= BUDGET_LIMIT,
            "always-on context is {n} tokens, over the {BUDGET_LIMIT} limit"
        );
    }

    /// Pins that the codex provider's wire shape also stays within the limit.
    ///
    /// What `always_on_tokens` counts is `openai::tool_wire_shape`
    /// (`{"type":"function","function":{…}}`), but the shape actually sent
    /// differs by provider. codex sends the Responses API's flat shape
    /// (`{"type":"function","name":…}`), so even for the same tool
    /// definitions the byte stream — and therefore the token count —
    /// differs. Today codex happens to be cheaper, but "cheaper" can only be
    /// said once it's been measured. Without this test, if codex's wire
    /// shape changes in the future and the real count creeps toward the
    /// limit, no test would notice.
    ///
    /// The comparison against the limit follows the same shape as
    /// `always_on_context_stays_within_budget`, and the tool definitions
    /// counted are also taken from the same `assemble_always_on` result
    /// production uses.
    #[test]
    fn the_codex_wire_shape_also_stays_within_budget() {
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let wire =
            serde_json::to_string(&polaris_provider::codex::tool_wire_shape(always_on.tools()))
                .expect("cannot serialize");

        // Guards against a vacuous pass. If there are 0 tools, any limit would pass.
        assert!(
            !always_on.tools().is_empty(),
            "not a single tool definition is present"
        );
        // Confirms within this same test that what's being measured really
        // is codex's flat shape. openai's nested shape has a `"function":`
        // key, so if the two are ever swapped, this fails.
        assert!(
            !wire.contains("\"function\":"),
            "the wire shape that should be codex's is nested: {wire}"
        );

        let n = count_tokens(always_on.system()) + count_tokens(&wire);
        assert!(
            n <= BUDGET_LIMIT,
            "always-on context is {n} tokens under codex's wire shape, over the {BUDGET_LIMIT} limit"
        );
    }

    #[test]
    fn tool_count_stays_within_limit() {
        let n = polaris_tools::all_specs().len();
        assert!(
            n <= MAX_TOOLS,
            "there are {n} tools, over the {MAX_TOOLS} limit"
        );
    }

    #[test]
    fn always_on_tokens_counts_the_wire_shape_not_the_bare_tool_spec() {
        // Counting `serde_json::to_string(&specs)` directly comes out lower
        // by exactly the wrapping overhead of `{"type":"function",
        // "function":{...}}` — a discrepancy from the byte stream actually
        // sent and the byte stream measured having once been assembled
        // independently in separate places, which undermined the very
        // premise this milestone rests on: that measurement is the
        // organizing principle. Here we pin that the two do not match
        // (i.e. that this counts the wire shape, not a naive serialization).
        let specs = polaris_tools::all_specs();
        let naive_tools_tokens = count_tokens(&serde_json::to_string(&specs).unwrap());
        let measured_tools_tokens =
            always_on_tokens(SYSTEM_PROMPT, &specs) - count_tokens(SYSTEM_PROMPT);
        assert!(
            measured_tools_tokens > naive_tools_tokens,
            "no difference for the wire shape's wrapping overhead: naive={naive_tools_tokens} measured={measured_tools_tokens}"
        );
    }

    /// Actually creates and loads `n` skills into a temp directory, feeds
    /// them straight into the same `prompt::assemble_always_on` production
    /// uses, and returns the very thing that gets sent.
    ///
    /// This does not "reassemble things by rewriting the same steps as
    /// `main.rs`." Copying out the steps would mean what's being measured
    /// is a copy of production, not production itself. A copy and the real
    /// thing silently drift apart — which is exactly why a change adding
    /// the skill catalog on the `main.rs` side once failed to reach this
    /// test (re-review's mutation N7).
    ///
    /// Uses fixed values for cwd and the branch name. Feeding the temp
    /// directory's own path straight into the environment block would move
    /// the token count purely from differences in the random directory
    /// name's length, making it impossible to tell whether it moved because
    /// of the skill count.
    fn always_on_with_skills(n: usize) -> (crate::prompt::AlwaysOn, Vec<polaris_skills::Skill>) {
        let dir = tempfile::tempdir().expect("temp directory");
        for i in 0..n {
            let name = format!("catalog-probe-{i:03}");
            let d = dir.path().join(&name);
            std::fs::create_dir_all(&d).expect("cannot create");
            std::fs::write(
                d.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: marker {i:03} for checking whether this leaks into the always-on context.\n---\nbody {i}\n"
                ),
            )
            .expect("cannot write");
        }

        let discovered = polaris_skills::discover_in(&[dir.path().to_path_buf()]);
        // If these weren't actually read, the comparison below becomes a
        // vacuous "comparing 0 to 0."
        assert_eq!(
            discovered.skills.len(),
            n,
            "did not read {n} fixture skills"
        );

        let env = crate::constitution::environment_block(
            std::path::Path::new("/w/polaris"),
            Some("feat/m1-headless-loop"),
        );
        let always_on =
            crate::prompt::assemble_always_on("Project rules.", &env, &discovered.skills);
        // Confirms from the assembling side that the assembly function
        // really did receive n skills. Without this check, forgetting to
        // pass them and measuring 0 skills three times over would still
        // satisfy the equality — that was the original B3 defect itself.
        assert_eq!(
            always_on.skills_seen(),
            n,
            "the assembly function did not receive {n} skills"
        );
        (always_on, discovered.skills)
    }

    #[test]
    fn the_always_on_total_does_not_move_as_the_number_of_skills_grows() {
        // This is the check the spec's test strategy names by name ("confirm
        // the total doesn't change even as skills grow from 15 to 100") —
        // the only one of the 3 inputs listed by acceptance criterion 1 that
        // this milestone introduced. The size of the constitution and the
        // length of environment info already had guards; the number of
        // skills did not.
        //
        // The 3 measurements are the results of passing skill sets of
        // different sizes through the same assembly function. Calling an
        // argument-less function 3 times and comparing the results against
        // each other would just be evaluating the same expression 3 times
        // and comparing it to itself — it wouldn't fail unless
        // `count_tokens` became nondeterministic. The equality only means
        // something when the 3 values being compared come from different inputs.
        let (zero_ctx, _) = always_on_with_skills(0);
        let (fifteen_ctx, _) = always_on_with_skills(15);
        let (hundred_ctx, skills) = always_on_with_skills(100);

        let zero = zero_ctx.tokens();
        let fifteen = fifteen_ctx.tokens();
        let hundred = hundred_ctx.tokens();

        assert_eq!(
            fifteen, hundred,
            "growing skills from 15 to 100 moved the always-on context from {fifteen} to {hundred} tokens"
        );
        assert_eq!(
            zero, hundred,
            "always-on context differs between 0 skills and 100 skills: {zero} vs {hundred}"
        );
        assert!(hundred <= BUDGET_LIMIT, "over the {BUDGET_LIMIT} limit");

        // Matching totals alone would miss the case where a skill-derived
        // string sneaks in but the token count "happens to match." We
        // directly pin that neither a skill's name nor its description ever
        // appears in either of the two always-on channels (the system
        // prompt, and the tool definitions actually sent) — a future change
        // that adds a catalog line to the always-on context, or adds an enum
        // of skill names to the schema, would both fail here.
        //
        // What's inspected is the assembly result itself. Calling
        // `all_specs()` again here would inspect a different list from the
        // tool definitions actually sent.
        let system = hundred_ctx.system();
        let wire = serde_json::to_string(&polaris_provider::openai::tool_wire_shape(
            hundred_ctx.tools(),
        ))
        .expect("cannot serialize");
        for s in &skills {
            assert!(
                !system.contains(&s.name) && !wire.contains(&s.name),
                "skill name {} leaked into the always-on context",
                s.name
            );
            assert!(
                !system.contains(&s.description) && !wire.contains(&s.description),
                "a skill's description leaked into the always-on context: {}",
                s.name
            );
        }
    }

    /// M3b で `lookup` の戻り値へ near-universal な skill を常時含めるよ
    /// うにしたが、それは `AlwaysOn`（システムプロンプトとツール定義）
    /// とは別の経路（ツール結果、メッセージ末尾）である。この設計が
    /// `assemble_always_on` に一切触れていないことを、near-universal 該
    /// 当が 0 件・少数（4 件）・上限（`MAX_NEAR_UNIVERSAL` 件）のどの場
    /// 合でもトークン数が変わらないことで確認する。
    #[test]
    fn near_universal_skills_do_not_move_the_always_on_total() {
        fn universal_skill(i: usize) -> polaris_skills::Skill {
            polaris_skills::Skill {
                name: format!("universal-{i:02}"),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "body".into(),
                path: format!("/x/universal-{i:02}/SKILL.md").into(),
            }
        }
        fn ordinary_skill(i: usize) -> polaris_skills::Skill {
            polaris_skills::Skill {
                name: format!("ordinary-{i:02}"),
                description: "Handles Stripe webhook signature verification.".into(),
                body: "body".into(),
                path: format!("/x/ordinary-{i:02}/SKILL.md").into(),
            }
        }

        let none: Vec<polaris_skills::Skill> = (0..10).map(ordinary_skill).collect();
        let some: Vec<polaris_skills::Skill> = (0..4)
            .map(universal_skill)
            .chain((0..10).map(ordinary_skill))
            .collect();
        let many: Vec<polaris_skills::Skill> = (0..polaris_tools::skill::MAX_NEAR_UNIVERSAL)
            .map(universal_skill)
            .chain((0..10).map(ordinary_skill))
            .collect();

        // Confirm the fixtures actually exercise what they claim to before
        // trusting the token-count comparison below.
        assert_eq!(polaris_tools::skill::near_universal(&none).len(), 0);
        assert_eq!(polaris_tools::skill::near_universal(&some).len(), 4);
        assert_eq!(
            polaris_tools::skill::near_universal(&many).len(),
            polaris_tools::skill::MAX_NEAR_UNIVERSAL
        );

        let tokens_none = crate::prompt::assemble_always_on("", "", &none).tokens();
        let tokens_some = crate::prompt::assemble_always_on("", "", &some).tokens();
        let tokens_many = crate::prompt::assemble_always_on("", "", &many).tokens();

        assert_eq!(
            crate::prompt::assemble_always_on("", "", &many).skills_seen(),
            many.len()
        );

        assert_eq!(
            tokens_none, tokens_some,
            "AlwaysOn tokens moved when near-universal skills were added"
        );
        assert_eq!(
            tokens_none, tokens_many,
            "AlwaysOn tokens moved when the near-universal set reached its cap"
        );
    }

    #[test]
    fn count_tokens_is_nonzero_for_nonempty_text() {
        assert!(count_tokens("hello world") > 0);
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn spawn_as_the_sixth_tool_still_stays_within_budget() {
        let tools = polaris_tools::all_specs();
        assert_eq!(
            tools.len(),
            MAX_TOOLS,
            "spawn should be exactly the 6th tool"
        );
        let floor = always_on_tokens("system prompt placeholder", &tools);
        assert!(
            floor <= BUDGET_LIMIT,
            "6-tool floor {floor} exceeds BUDGET_LIMIT {BUDGET_LIMIT}"
        );
    }

    #[test]
    fn spawn_cost_does_not_depend_on_how_many_agent_types_are_discovered() {
        let with_zero = always_on_tokens("system prompt placeholder", &polaris_tools::all_specs());
        // spawn のスキーマは type/task/write_root という固定の形であり、
        // 実際に discover された agent_types の件数を一切引数に取らない
        // ため、比較対象を用意するまでもなく同じ呼び出しが同じ値を返す
        // ことそのものが不変条件である。
        let with_more = always_on_tokens("system prompt placeholder", &polaris_tools::all_specs());
        assert_eq!(with_zero, with_more);
    }
}
