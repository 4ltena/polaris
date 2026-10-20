//! Measurement of the always-on context. Numbers are backed by measurement,
//! never operated on by estimate.

use polaris_provider::openai::tool_wire_shape;
use polaris_tools::ToolSpec;

/// The ceiling on the always-on context.
pub const BUDGET_LIMIT: usize = 990;

/// The ceiling on the number of always-on tools.
pub const MAX_TOOLS: usize = 6;

/// Adaptive control is accounted separately from the unchanged baseline.
pub const STATE_SCHEMA_LIMIT: usize = 256;
pub const MCP_META_SCHEMA_LIMIT: usize = 256;
pub const MCP_SELECTED_SCHEMA_LIMIT: usize = 4096;
pub const MCP_CANDIDATE_LIMIT: usize = 768;

pub fn validate_mcp_meta_schema() -> std::io::Result<()> {
    let specs = [polaris_tools::mcp_catalog_spec()];
    for wire in [
        polaris_provider::openai::tool_wire_shape(&specs),
        polaris_provider::codex::tool_wire_shape(&specs),
    ] {
        if count_tokens(&serde_json::to_string(&wire).map_err(std::io::Error::other)?)
            > MCP_META_SCHEMA_LIMIT
        {
            return Err(std::io::Error::other(
                "MCP操作の定義が256tokensを超えました",
            ));
        }
    }
    Ok(())
}

/// Counts full production tool wire, including aliases and descriptions.
pub fn validate_mcp_tool_budget(tools: &[ToolSpec]) -> std::io::Result<()> {
    if tools.len() > 4 {
        return Err(std::io::Error::other("MCP toolの選択は4件以内です"));
    }
    for wire in [
        polaris_provider::openai::tool_wire_shape(tools),
        polaris_provider::codex::tool_wire_shape(tools),
    ] {
        if count_tokens(&serde_json::to_string(&wire).map_err(std::io::Error::other)?)
            > MCP_SELECTED_SCHEMA_LIMIT
        {
            return Err(std::io::Error::other(
                "MCP toolの定義が4096tokensを超えました",
            ));
        }
    }
    Ok(())
}

pub fn validate_state_schema() -> std::io::Result<()> {
    let specs = [polaris_tools::state_spec()];
    for wire in [
        polaris_provider::openai::tool_wire_shape(&specs),
        polaris_provider::codex::tool_wire_shape(&specs),
    ] {
        let text = serde_json::to_string(&wire).map_err(std::io::Error::other)?;
        if count_tokens(&text) > STATE_SCHEMA_LIMIT {
            return Err(std::io::Error::other(
                "state toolの定義が256tokensを超えました",
            ));
        }
    }
    Ok(())
}

/// Reference-token counts of the complete logical context in each provider's
/// production wire shape. These include history and JSON framing, and are not
/// billed token usage or transport-specific model/effort metadata.
#[derive(Debug, serde::Serialize)]
pub struct RequestContextTokens {
    pub system: usize,
    pub openai_context: usize,
    pub codex_context: usize,
    pub openai_tools: usize,
    pub codex_tools: usize,
    pub openai_state: usize,
    pub codex_state: usize,
}

pub fn request_context_tokens(
    request: &polaris_provider::CompletionRequest,
) -> RequestContextTokens {
    let openai = polaris_provider::openai::build_body("", request, None);
    let codex = polaris_provider::codex::build_body("", request, None);
    let mut openai_context = serde_json::json!({"messages":openai["messages"]});
    let mut codex_context =
        serde_json::json!({"instructions":codex["instructions"], "input":codex["input"]});
    for (context, wire) in [(&mut openai_context, &openai), (&mut codex_context, &codex)] {
        if let Some(tools) = wire.get("tools") {
            context["tools"] = tools.clone();
        }
    }
    let state: Vec<_> = request
        .tools
        .iter()
        .filter(|tool| tool.name == "state")
        .cloned()
        .collect();
    let state_tokens = |wire: Vec<serde_json::Value>| {
        if wire.is_empty() {
            0
        } else {
            count_tokens(&serde_json::to_string(&wire).expect("tool wire is JSON"))
        }
    };
    RequestContextTokens {
        system: count_tokens(&request.system),
        openai_context: count_tokens(&openai_context.to_string()),
        codex_context: count_tokens(&codex_context.to_string()),
        openai_tools: openai
            .get("tools")
            .map_or(0, |tools| count_tokens(&tools.to_string())),
        codex_tools: codex
            .get("tools")
            .map_or(0, |tools| count_tokens(&tools.to_string())),
        openai_state: state_tokens(polaris_provider::openai::tool_wire_shape(&state)),
        codex_state: state_tokens(polaris_provider::codex::tool_wire_shape(&state)),
    }
}

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
    fn mcp_meta_and_owned_tool_wire_have_separate_measured_budgets() {
        let meta = [polaris_tools::mcp_catalog_spec()];
        for wire in [
            polaris_provider::openai::tool_wire_shape(&meta),
            polaris_provider::codex::tool_wire_shape(&meta),
        ] {
            let count = count_tokens(&serde_json::to_string(&wire).unwrap());
            println!("MCP meta tool wire: {count} reference tokens");
            assert!(count <= MCP_META_SCHEMA_LIMIT, "{count}");
        }
        let tool = ToolSpec {
            name: format!("mcp_{}", "a".repeat(60)),
            description: "fixture".into(),
            parameters: serde_json::json!({"type":"object"}),
        };
        assert!(validate_mcp_tool_budget(&vec![tool.clone(); 4]).is_ok());
        assert!(validate_mcp_tool_budget(&vec![tool.clone(); 5]).is_err());
        let mut oversized = tool;
        oversized.parameters["description"] =
            serde_json::json!("long schema description ".repeat(4096));
        assert!(validate_mcp_tool_budget(&[oversized]).is_err());
        assert_eq!(polaris_tools::all_specs().len(), MAX_TOOLS);
        assert!(
            polaris_tools::all_specs()
                .iter()
                .all(|tool| tool.name != "mcp_catalog")
        );
    }

    #[test]
    fn workflow_modes_measure_all_phases_and_full_wire_without_claiming_cost_savings() {
        use crate::workflow::{
            Phase, SessionWorkflow, WorkflowGatesV1, WorkflowMode, WorkflowStateV1,
        };
        use polaris_provider::{CompletionRequest, Message};
        let baseline = crate::prompt::assemble_always_on("", "", &[]);
        let config = crate::config::Config::default().workflow;
        let manual = SessionWorkflow::new(config.clone())
            .begin_turn(&[])
            .unwrap()
            .body
            .clone();
        let mut adaptive_config = config;
        adaptive_config.mode = WorkflowMode::Adaptive;
        let workflow = SessionWorkflow::restore(
            adaptive_config,
            WorkflowGatesV1 {
                phase_gate_schema_version: None,
                ..WorkflowGatesV1::default()
            },
            WorkflowStateV1::default(),
        )
        .unwrap();
        let turn = workflow.begin_turn(&[]).unwrap();
        let all_phases = polaris_skills::workflow_profile::recommended_bindings()
            .iter()
            .map(|skill| format!("### {}\n{}", skill.name, skill.body))
            .collect::<Vec<_>>()
            .join("\n");
        let mut history = vec![Message::user(
            "Implement the agreed task; retain constraints and report remaining work.",
        )];
        for phase in Phase::ALL {
            let mut prepared = turn
                .prepare_transition(phase, workflow.state().revision, &workflow.gates)
                .unwrap();
            prepared.begin_persist().unwrap();
            prepared.commit().unwrap();
            let profile = turn.profile(&workflow.gates).unwrap();
            assert!(profile.tokens <= 384);
            let mut keys = Vec::new();
            for (mode, body, adaptive) in [
                ("off", "", false),
                ("manual", manual.as_str(), false),
                ("all_phases_fixture", all_phases.as_str(), true),
                ("adaptive", profile.body.as_str(), true),
            ] {
                let mut tools = baseline.tools().to_vec();
                if adaptive {
                    tools.push(polaris_tools::state_spec());
                }
                let request = CompletionRequest {
                    system: format!("{}\n{body}", baseline.system()),
                    messages: history.clone(),
                    tools,
                };
                assert_eq!(
                    request.tools.iter().any(|tool| tool.name == "state"),
                    adaptive
                );
                let counts = request_context_tokens(&request);
                assert!(
                    counts.openai_context > counts.system && counts.codex_context > counts.system
                );
                for (transport, wire) in [
                    (
                        "openai",
                        polaris_provider::openai::build_body("fixture", &request, Some("medium")),
                    ),
                    (
                        "codex",
                        polaris_provider::codex::build_body("fixture", &request, Some("medium")),
                    ),
                ] {
                    println!(
                        "workflow_comparison phase={phase} mode={mode} transport={transport} full_fixture_wire={}",
                        count_tokens(&wire.to_string())
                    );
                }
                assert_eq!(counts.openai_state > 0, adaptive);
                assert_eq!(counts.codex_state > 0, adaptive);
                let key = polaris_provider::cache_key("fixture", Some("medium"), &request);
                let mut grown = CompletionRequest {
                    system: request.system.clone(),
                    messages: request.messages.clone(),
                    tools: request.tools.clone(),
                };
                grown.messages.push(Message::assistant(
                    "History grows without changing the prefix.",
                ));
                assert_eq!(
                    key,
                    polaris_provider::cache_key("fixture", Some("medium"), &grown)
                );
                keys.push(key);
            }
            assert_ne!(
                keys[1], keys[3],
                "adaptive context must invalidate the manual prefix key"
            );
            history.push(Message::assistant("Observed step completed."));
        }
    }

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

    /// Pins the exact always-on total, not just that it is under the limit.
    ///
    /// Every other budget test here asserts `<= BUDGET_LIMIT`. That catches
    /// the moment the ceiling breaks and nothing before it, so the context
    /// can drift from 632 to 989 one forgotten property description at a
    /// time without a single test going red — and the last commit before
    /// the break gets blamed for all of it. Pinning the number makes every
    /// change to the always-on context show up as a deliberate edit to this
    /// line, with the new value measured rather than assumed.
    ///
    /// Raising it is allowed. Raising it silently is not.
    #[test]
    fn the_always_on_total_is_pinned() {
        // 625 since 2026-09-20: `spawn` names implementer and reviewer.
        // Measured on a 15-turn task where the root did its own writing
        // and delegated once, to a type that does not exist — the only
        // delegation text it reads is this description, and it named
        // file-reader alone. 23 tokens a request against the whole
        // implementation staying on the parent's model.
        // 635 since 2026-09-20: one line of SYSTEM_PROMPT tells the root
        // to spawn its reading, writing and checking. It sits there and
        // not in a phase profile because the measured run never left the
        // `general` phase — an adaptive transition happens only when the
        // model calls `state` itself, and in 15 turns it never did, so
        // every phase-specific instruction was unreachable.
        // 655 since 2026-10-20: read supports directory discovery and the
        // prompt tells the model to discover unknown paths before reading.
        const PINNED: usize = 655;
        let n = crate::prompt::assemble_always_on("", "", &[]).tokens();
        assert_eq!(
            n, PINNED,
            "the always-on context moved from {PINNED} to {n} tokens. \
             If that was intended, update PINNED to {n} and say why in the commit."
        );
    }

    /// Pins what a root turn actually sends, which is not what
    /// `BUDGET_LIMIT` measures.
    ///
    /// `always_on_tokens` counts the six-tool baseline. A root turn appends
    /// `mcp_setup`, `setting` and `state` to `request_tools`
    /// (`agent::run_loop`), and none of those three has ever been counted
    /// against a limit — the 990 check looks at a list they are not in. So
    /// the number the repository quotes for its own always-on context is
    /// smaller than the number it sends, by the size of three schemas, and
    /// nothing fails when they grow.
    ///
    /// This does not assert a ceiling, because there is no agreed one for
    /// the root turn. It asserts the value, so that growth is visible and
    /// deliberate rather than invisible and free.
    #[test]
    fn the_root_turn_total_is_pinned() {
        // 995 until the `computer` tool joined the root-only set on
        // 2026-09-21, then 1,179, then this. The tool is what carries the
        // root prefix over the backend's 1,024-token cache minimum, which
        // polaris had been 29 tokens short of; the 110 tokens between 1,179
        // and here came back out of five descriptions that were explaining
        // themselves rather than telling the caller what to pass. 45 tokens
        // of margin is deliberate — enough that a project's own rules do
        // not push the prefix back under the threshold.
        const PINNED: usize = 1085;
        let mut tools = polaris_tools::all_specs();
        tools.push(polaris_tools::mcp_setup_spec());
        tools.push(polaris_tools::setting_spec());
        tools.push(polaris_tools::state_spec());
        tools.push(polaris_tools::computer_spec());

        let baseline = crate::prompt::assemble_always_on("", "", &[]);
        let n = always_on_tokens(baseline.system(), &tools);

        // Guards against a vacuous pass: the four root-only tools must
        // really be in the list being measured.
        assert_eq!(
            tools.len(),
            MAX_TOOLS + 4,
            "the root-only tools are missing"
        );
        assert!(
            n > baseline.tokens(),
            "the root turn must cost more than the baseline, not the same"
        );
        assert_eq!(
            n, PINNED,
            "the root turn moved from {PINNED} to {n} tokens. \
             If that was intended, update PINNED to {n} and say why in the commit."
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

    /// `dir_watch`/`gitignore`/`files_md` are harness-internal machinery
    /// that runs before/after a root tool call and never touch
    /// `AlwaysOn`/`assemble_always_on` — no new token source. That
    /// invariant is design, not runtime behavior, so there is nothing to
    /// toggle "on" and "off" here to compare; this pins it by showing the
    /// always-on total for the existing 6-tool configuration is stable
    /// (repeated measurement of the same inputs) and still within budget,
    /// exactly as it was before the files.md autogeneration feature
    /// existed.
    #[test]
    fn files_md_autogeneration_is_not_part_of_the_always_on_context() {
        let tools = polaris_tools::all_specs();
        let with_files_md_feature = always_on_tokens("system prompt placeholder", &tools);
        assert_eq!(
            with_files_md_feature,
            always_on_tokens("system prompt placeholder", &tools)
        );
        assert!(with_files_md_feature <= BUDGET_LIMIT);
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

#[cfg(test)]
mod state_budget {
    /// Pins the fixed input each workflow state adds.
    ///
    /// M17 measured `deliver` at 162 and reported the rest only as a range,
    /// so growth in any other state has been invisible. The always-on
    /// baseline and the root turn are pinned above; this pins the per-state
    /// part that rides on top of them, one number per state, for the same
    /// reason: growth should be deliberate, not free.
    #[test]
    fn every_state_profile_is_pinned() {
        const PINNED: [(&str, usize); 7] = [
            ("general", 126),
            ("brainstorm", 198),
            ("specify", 205),
            ("implement", 294),
            ("review", 284),
            ("verify", 218),
            ("deliver", 207),
        ];
        let core = include_str!("../../polaris-skills/resources/workflow/workflow-core/SKILL.md");
        let bodies: [(&str, &str); 7] = [
            ("general", ""),
            (
                "brainstorm",
                include_str!("../../polaris-skills/resources/workflow/brainstorm/SKILL.md"),
            ),
            (
                "specify",
                include_str!("../../polaris-skills/resources/workflow/specify/SKILL.md"),
            ),
            (
                "implement",
                include_str!("../../polaris-skills/resources/workflow/implement/SKILL.md"),
            ),
            (
                "review",
                include_str!("../../polaris-skills/resources/workflow/review/SKILL.md"),
            ),
            (
                "verify",
                include_str!("../../polaris-skills/resources/workflow/verify/SKILL.md"),
            ),
            (
                "deliver",
                include_str!("../../polaris-skills/resources/workflow/deliver/SKILL.md"),
            ),
        ];
        let measured: Vec<(&str, usize)> = bodies
            .iter()
            .map(|(name, body)| {
                // What a turn in this state actually carries: the always-on
                // core skill, the state's own skill, and the state line.
                let line = format!("phase={name} spec=missing verification=unknown grants=0\n");
                (
                    *name,
                    super::count_tokens(&format!("{core}\n{body}\n{line}")),
                )
            })
            .collect();
        let expected: Vec<(&str, usize)> = PINNED.to_vec();
        assert_eq!(
            measured, expected,
            "state profile tokens moved: {measured:?}"
        );
    }
}

#[cfg(test)]
mod phase_tools {
    /// What a read-only phase saves on every request. The six schemas are
    /// 496 of the 639 always-on tokens; `write`, `edit` and `spawn` are
    /// 287 of those, and a phase that cannot change a file can call none
    /// of them.
    #[test]
    fn a_read_only_phase_stops_paying_for_what_it_cannot_call() {
        let specs = polaris_tools::all_specs();
        let baseline = crate::prompt::assemble_always_on("", "", &[]);
        let all = super::always_on_tokens(baseline.system(), &specs);
        let kept = |allowed: &[&str]| {
            let subset: Vec<_> = specs
                .iter()
                .filter(|spec| allowed.contains(&spec.name.as_str()))
                .cloned()
                .collect();
            super::always_on_tokens(baseline.system(), &subset)
        };
        assert_eq!(all, 655);
        assert_eq!(kept(&["read", "bash", "skill"]), 382, "review");
        assert_eq!(kept(&["read", "skill"]), 316, "brainstorm");

        // Only an authorized Implement phase may expose mutation tools. The
        // runtime gate still checks the current contract at dispatch time.
        for phase in crate::workflow::Phase::ALL {
            let allowed = crate::agent::read_only_tools(phase);
            match phase {
                crate::workflow::Phase::Implement => {
                    assert!(
                        allowed.is_none(),
                        "{phase} may write with a current contract"
                    )
                }
                _ => {
                    let allowed = allowed.expect("read-only phase has an allowlist");
                    assert!(allowed.contains(&"state"), "{phase} lost its phase control");
                }
            }
        }
    }
}

#[cfg(test)]
mod breakdown {
    use super::*;

    /// Not an assertion — a way to see where the always-on tokens go, so a
    /// decision about the budget is made against the parts rather than the
    /// total. Run with `--nocapture`.
    #[test]
    fn what_the_root_only_tools_cost() {
        let baseline = crate::prompt::assemble_always_on("", "", &[]);
        let specs = polaris_tools::all_specs();
        let six = always_on_tokens(baseline.system(), &specs);
        for (name, spec) in [
            ("mcp_setup", polaris_tools::mcp_setup_spec()),
            ("setting", polaris_tools::setting_spec()),
            ("state", polaris_tools::state_spec()),
            ("computer", polaris_tools::computer_spec()),
        ] {
            let mut with = specs.clone();
            with.push(spec);
            println!(
                "  {name:<10} {}",
                always_on_tokens(baseline.system(), &with) - six
            );
        }
    }

    #[test]
    fn where_the_always_on_tokens_go() {
        let specs = polaris_tools::all_specs();
        let baseline = crate::prompt::assemble_always_on("", "", &[]);
        let empty: Vec<ToolSpec> = Vec::new();
        let system_only = always_on_tokens(baseline.system(), &empty);
        println!("system {system_only}");
        for spec in &specs {
            let one = vec![spec.clone()];
            let with = always_on_tokens(baseline.system(), &one);
            println!("  {:<14} {}", spec.name, with - system_only);
        }
        println!("total {}", always_on_tokens(baseline.system(), &specs));
    }
}
