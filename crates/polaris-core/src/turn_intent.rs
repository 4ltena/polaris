//! What one user turn is, along several axes at once.
//!
//! The judge is asked six small questions about the same fixed state, not
//! one large one: an utterance kind, a work kind, how clear it is, how it
//! relates to what is already happening, and how the user expects it to
//! proceed, plus the reasoning effort the work needs. Each is a
//! [`Question::Choice`] the decider answers in one
//! request. Keeping them separate is what lets "a proposal that is also a
//! question" be both, and what stops one axis's uncertainty from being
//! read as another's.
//!
//! The route is then chosen here, by [`route_for`], from the answers. A
//! deterministic router is deliberate: the judge says what the turn is,
//! and what polaris does about it is a rule anybody can read, not a sixth
//! guess.
//!
//! Confidence is kept per axis and is **not** ambiguity. A judge that is
//! unsure whether this is a fix or a change has said nothing about whether
//! the request is clear; only the clarity axis says that.

use polaris_provider::decision::{Decider, DecisionError, Question};
use std::collections::BTreeMap;

macro_rules! axis {
    ($name:ident, $key:literal, $prompt:literal, { $($variant:ident => $value:literal, $why:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const KEY: &'static str = $key;

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }

            fn parse(value: &str) -> Option<Self> {
                match value { $($value => Some(Self::$variant),)+ _ => None }
            }

            fn question() -> Question {
                Question::Choice {
                    instructions: $prompt.into(),
                    criteria: BTreeMap::from([
                        $(($value.to_owned(), $why.to_owned())),+
                    ]),
                }
            }
        }
    };
}

axis!(Utterance, "utterance", "この発話は何をしているか", {
    Instruction => "instruction", "やることを指示している",
    Proposal => "proposal", "案を出していて、採否はまだ決まっていない",
    Question => "question", "答えを求めていて、作業は求めていない",
    Correction => "correction", "直前の理解や成果の誤りを正している",
    Approval => "approval", "示した案や計画を承認している",
    Feedback => "feedback", "評価や感想で、次の行動を指定していない",
    StatusRequest => "status_request", "今どうなっているかを訊いている",
    Pause => "pause", "止めるよう求めている",
    Resume => "resume", "再開するよう求めている",
    Cancel => "cancel", "取り消すよう求めている",
});

axis!(Work, "work", "どの種類の作業か", {
    BugFix => "bug_fix", "壊れている振る舞いを直す",
    FeatureAdd => "feature_add", "無かった機能を足す",
    FeatureChange => "feature_change", "ある機能の振る舞いを変える",
    Refactor => "refactor", "振る舞いを変えずに構造を直す",
    Investigation => "investigation", "調べて事実を出す",
    Verification => "verification", "既にあるものを確かめる",
    Documentation => "documentation", "文書を書く",
    Release => "release", "公開の工程",
    Configuration => "configuration", "設定を変える",
    Operation => "operation", "運用上の操作",
});

axis!(Clarity, "clarity", "何をすべきかが決まるか", {
    Clear => "clear", "そのまま実行できる",
    Inferable => "inferable", "既存のコードと決定から一意に決まる",
    MateriallyAmbiguous => "materially_ambiguous", "同等に妥当な選択肢が複数あり、決めないと違う完成形になる",
    Conflicting => "conflicting", "現在有効な決定と衝突している",
});

axis!(Relation, "relation", "今の作業との関係", {
    Continue => "continue", "今の作業の続き",
    ScopeExpand => "scope_expand", "今の作業の範囲を広げる",
    Replace => "replace", "今の作業を別のものへ置き換える",
    SideTopic => "side_topic", "別の話題",
});

axis!(Approach, "approach", "利用者が期待している進め方", {
    ExecuteNow => "execute_now", "すぐ実行してよい",
    DesignFirst => "design_first", "先に設計や計画を示す",
    AnswerOnly => "answer_only", "答えるだけでよい",
    AwaitUser => "await_user", "利用者の回答を待つ",
});

axis!(Effort, "effort", "このturnの作業に必要な推論量", {
    Low => "low", "機械的または局所的で、手順と正解がほぼ決まっている",
    Medium => "medium", "通常の調査や数fileの実装と、焦点を絞った検証が必要",
    High => "high", "複数componentの整合、難しい原因分析、または慎重な設計判断が必要",
    XHigh => "xhigh", "広範囲で相互依存が強く、失敗影響も大きい複数段階の推論が必要",
});

/// Where the turn goes. Chosen by [`route_for`], never asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Direct,
    InspectThenExecute,
    Brainstorm,
    Grilling,
    Specify,
    Review,
    Sidechat,
}

/// The rule that turns five answers into one route.
///
/// `grilling` is entered only where a decision the user can see is
/// genuinely open: the interface, the stored form, a public API, what is
/// sent, or what is permitted. An ordinary implementation detail is
/// `inferable`, and an inferable turn never asks.
pub fn route_for(
    utterance: Utterance,
    work: Work,
    clarity: Clarity,
    relation: Relation,
    approach: Approach,
) -> Route {
    use Clarity::*;
    use Utterance::*;
    // Control comes first: these are mechanical and never need a judge's
    // opinion about clarity.
    if matches!(utterance, Pause | Resume | Cancel) {
        return Route::Direct;
    }
    if matches!(utterance, Question | StatusRequest | Feedback) {
        return Route::Sidechat;
    }
    if relation == Relation::SideTopic {
        return Route::Sidechat;
    }
    // A decision that is open or contradicted is the one thing worth
    // stopping for, and only when the turn would otherwise change
    // something.
    let changes_things = matches!(
        work,
        Work::FeatureAdd
            | Work::FeatureChange
            | Work::BugFix
            | Work::Refactor
            | Work::Configuration
    );
    if changes_things && matches!(clarity, MateriallyAmbiguous | Conflicting) {
        return Route::Grilling;
    }
    if approach == Approach::AwaitUser {
        return Route::Grilling;
    }
    if utterance == Proposal && approach == Approach::DesignFirst {
        return Route::Brainstorm;
    }
    if approach == Approach::DesignFirst {
        return Route::Specify;
    }
    if approach == Approach::AnswerOnly {
        return Route::Sidechat;
    }
    match work {
        Work::Investigation | Work::Verification => Route::Review,
        Work::Documentation => Route::Specify,
        _ if clarity == Inferable => Route::InspectThenExecute,
        _ => Route::Direct,
    }
}

/// One question waiting on the user.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingClarification {
    pub question: String,
    /// The turn the question came from, so a restart does not ask again.
    pub turn: u64,
    /// Set once answered, so the same answer is not applied twice.
    pub answer_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ClarificationState {
    #[default]
    None,
    Pending {
        asked: Vec<PendingClarification>,
    },
    Resolved {
        asked: Vec<PendingClarification>,
    },
}

impl ClarificationState {
    /// Whether a change-making tool may be offered this turn.
    pub fn may_change_files(&self) -> bool {
        !matches!(self, Self::Pending { .. })
    }

    /// The questions still unanswered, so a restart re-asks none of them.
    pub fn unanswered(&self) -> Vec<&PendingClarification> {
        match self {
            Self::Pending { asked } => asked
                .iter()
                .filter(|item| item.answer_hash.is_none())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The state the conversation itself implies.
    ///
    /// Read rather than stored. A `-p` run that asks a question exits, and
    /// the `--resume` that answers it is a different process; keeping this
    /// in a field would mean a second thing to write, to load, and to
    /// disagree with the log. The log already holds both halves: the
    /// `clarify` call that asked, and whether a user turn came after it.
    pub fn from_history(messages: &[polaris_provider::Message]) -> Self {
        let Some((index, call)) = messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                message
                    .tool_calls
                    .iter()
                    .find(|call| call.name == CLARIFY)
                    .map(|call| (index, call))
            })
        else {
            return Self::None;
        };
        let answer = messages[index + 1..]
            .iter()
            .find(|message| message.role == polaris_provider::Role::User);
        let asked: Vec<PendingClarification> = questions_in(&call.arguments)
            .into_iter()
            .map(|question| PendingClarification {
                question,
                turn: index as u64,
                answer_hash: answer.map(|message| answer_hash(&message.content)),
            })
            .collect();
        if asked.is_empty() {
            return Self::None;
        }
        match answer {
            Some(_) => Self::Resolved { asked },
            None => Self::Pending { asked },
        }
    }
}

/// The tool a run uses to stop and ask. Named here because both the gate
/// that offers it and the reader that reconstructs the state from the log
/// have to agree on the spelling.
pub const CLARIFY: &str = "clarify";

/// At most three, in the order the caller gave them, each non-empty after
/// trimming. Anything else in the argument object is ignored rather than
/// rejected: this reads a record that was already accepted once.
pub fn questions_in(arguments: &serde_json::Value) -> Vec<String> {
    arguments["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .take(MAX_QUESTIONS)
        .map(ToOwned::to_owned)
        .collect()
}

/// Dependency-ordered and few. Three is the spec's bound, and the reason
/// for a bound at all is that a run which asks everything it is unsure
/// about has moved the work to the user.
pub const MAX_QUESTIONS: usize = 3;

/// Identifies the answer without storing it. The point is only to tell one
/// answer from another across a restart, so the hash is short and the text
/// stays where it already is, in the conversation.
fn answer_hash(answer: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(answer.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What was decided about one turn, and by whom.
///
/// `decider` and `physical_attempts` are kept because a classification
/// nobody can attribute is a classification nobody can check. A turn the
/// judge never answered is [`TurnIntentV1::unavailable`], which is not the
/// same record as one it answered.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnIntentV1 {
    pub version: u8,
    pub turn: u64,
    pub utterance: Option<Utterance>,
    pub work: Option<Work>,
    pub clarity: Option<Clarity>,
    pub relation: Option<Relation>,
    pub approach: Option<Approach>,
    #[serde(default)]
    pub effort: Option<Effort>,
    pub route: Option<Route>,
    pub probabilities: BTreeMap<String, f64>,
    pub decider: Option<String>,
    pub physical_attempts: Option<u32>,
    /// Present exactly when the judge did not answer. The turn keeps its
    /// existing route; nothing here pretends a classification happened.
    pub unavailable: Option<String>,
}

impl TurnIntentV1 {
    /// The record for a turn the judge could not classify.
    pub fn unavailable(turn: u64, reason: &str) -> Self {
        Self {
            version: 1,
            turn,
            utterance: None,
            work: None,
            clarity: None,
            relation: None,
            approach: None,
            effort: None,
            route: None,
            probabilities: BTreeMap::new(),
            decider: None,
            physical_attempts: None,
            unavailable: Some(reason.to_owned()),
        }
    }

    pub fn classified(&self) -> bool {
        self.unavailable.is_none() && self.route.is_some()
    }
}

/// The six questions, asked about one fixed state in one request.
pub fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([
        (Utterance::KEY.to_owned(), Utterance::question()),
        (Work::KEY.to_owned(), Work::question()),
        (Clarity::KEY.to_owned(), Clarity::question()),
        (Relation::KEY.to_owned(), Relation::question()),
        (Approach::KEY.to_owned(), Approach::question()),
        (Effort::KEY.to_owned(), Effort::question()),
    ])
}

/// Classify one turn, or record that it could not be classified.
///
/// Never returns an error: a run whose judge is unreachable keeps its
/// existing route and says so in the record. Reporting a failure as a
/// classification is the one outcome this must not produce.
pub async fn classify(
    decider: Option<&dyn Decider>,
    state: &serde_json::Value,
    turn: u64,
) -> TurnIntentV1 {
    let Some(decider) = decider else {
        return TurnIntentV1::unavailable(turn, "classifier_unavailable");
    };
    let asked = questions();
    let observation = decider.observe(state, &asked).await;
    let decision = match observation.result {
        Ok(decision) => decision,
        Err(DecisionError::TooLarge) => {
            return TurnIntentV1::unavailable(turn, "classifier_input_too_large");
        }
        Err(_) => return TurnIntentV1::unavailable(turn, "classifier_unavailable"),
    };
    let pick = |key: &str| {
        decision
            .answers
            .get(key)
            .and_then(|answer| answer.choice.as_deref())
    };
    let (Some(utterance), Some(work), Some(clarity), Some(relation), Some(approach), Some(effort)) = (
        pick(Utterance::KEY).and_then(Utterance::parse),
        pick(Work::KEY).and_then(Work::parse),
        pick(Clarity::KEY).and_then(Clarity::parse),
        pick(Relation::KEY).and_then(Relation::parse),
        pick(Approach::KEY).and_then(Approach::parse),
        pick(Effort::KEY).and_then(Effort::parse),
    ) else {
        return TurnIntentV1::unavailable(turn, "classifier_answer_incomplete");
    };
    let mut probabilities = BTreeMap::new();
    for (key, answer) in &decision.answers {
        if let Some(chosen) = answer.choice.as_deref()
            && let Some(p) = answer.probabilities.get(chosen)
        {
            probabilities.insert(key.clone(), *p);
        }
    }
    TurnIntentV1 {
        version: 1,
        turn,
        utterance: Some(utterance),
        work: Some(work),
        clarity: Some(clarity),
        relation: Some(relation),
        approach: Some(approach),
        effort: Some(effort),
        route: Some(route_for(utterance, work, clarity, relation, approach)),
        probabilities,
        decider: Some(observation.identity.backend),
        physical_attempts: None,
        unavailable: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::decision::{Answer, Decision};

    struct Fixed(BTreeMap<String, String>);

    #[async_trait::async_trait]
    impl Decider for Fixed {
        async fn decide(
            &self,
            _state: &serde_json::Value,
            questions: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            // The judge is asked all six in one request, atomically.
            assert_eq!(questions.len(), 6, "the axes were not asked together");
            Ok(Decision {
                answers: self
                    .0
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            Answer {
                                noul: None,
                                choice: Some(value.clone()),
                                score: None,
                                probabilities: BTreeMap::from([(value.clone(), 0.9)]),
                                legend: BTreeMap::new(),
                                confidence: Some(0.8),
                            },
                        )
                    })
                    .collect(),
                input_tokens: Some(40),
                output_tokens: Some(0),
            })
        }
    }

    struct Broken;

    #[async_trait::async_trait]
    impl Decider for Broken {
        async fn decide(
            &self,
            _state: &serde_json::Value,
            _questions: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Err(DecisionError::Malformed)
        }
    }

    fn answers(values: [&str; 5]) -> Fixed {
        Fixed(BTreeMap::from([
            (Utterance::KEY.to_owned(), values[0].to_owned()),
            (Work::KEY.to_owned(), values[1].to_owned()),
            (Clarity::KEY.to_owned(), values[2].to_owned()),
            (Relation::KEY.to_owned(), values[3].to_owned()),
            (Approach::KEY.to_owned(), values[4].to_owned()),
            (Effort::KEY.to_owned(), "medium".to_owned()),
        ]))
    }

    #[tokio::test]
    async fn fallback_classification_records_the_effective_backend() {
        struct Observed(Fixed);
        #[async_trait::async_trait]
        impl Decider for Observed {
            fn name(&self) -> &str {
                "configured"
            }
            async fn decide(
                &self,
                state: &serde_json::Value,
                questions: &BTreeMap<String, Question>,
            ) -> Result<Decision, DecisionError> {
                self.0.decide(state, questions).await
            }
            async fn observe(
                &self,
                state: &serde_json::Value,
                questions: &BTreeMap<String, Question>,
            ) -> polaris_provider::decision::DecisionObservation {
                let mut observed = polaris_provider::decision::DecisionObservation::single(
                    self.identity(),
                    self.capabilities(),
                    self.decide(state, questions).await,
                    std::time::Duration::ZERO,
                );
                observed.identity.backend = "effective".into();
                observed
            }
        }
        let judge = Observed(answers([
            "instruction",
            "bug_fix",
            "clear",
            "continue",
            "execute_now",
        ]));
        let intent = classify(Some(&judge), &serde_json::Value::Null, 1).await;
        assert!(intent.classified());
        assert_eq!(intent.decider.as_deref(), Some("effective"));
    }

    async fn route(values: [&str; 5]) -> Route {
        let fixed = answers(values);
        let intent = classify(Some(&fixed), &serde_json::json!({}), 1).await;
        assert!(intent.classified(), "{intent:?}");
        intent.route.unwrap()
    }

    /// The cases the acceptance criteria name, each landing on several
    /// axes rather than one label.
    #[tokio::test]
    async fn the_named_cases_land_on_the_axes_they_should() {
        // A plain fix executes.
        assert_eq!(
            route(["instruction", "bug_fix", "clear", "continue", "execute_now"]).await,
            Route::Direct
        );
        // An ambiguous feature proposal — the request this was built for.
        assert_eq!(
            route([
                "proposal",
                "feature_add",
                "materially_ambiguous",
                "scope_expand",
                "design_first"
            ])
            .await,
            Route::Grilling
        );
        // A question is answered, not worked on.
        assert_eq!(
            route([
                "question",
                "investigation",
                "clear",
                "continue",
                "answer_only"
            ])
            .await,
            Route::Sidechat
        );
        assert_eq!(
            route([
                "status_request",
                "operation",
                "clear",
                "continue",
                "answer_only"
            ])
            .await,
            Route::Sidechat
        );
        assert_eq!(
            route(["correction", "bug_fix", "clear", "continue", "execute_now"]).await,
            Route::Direct
        );
        assert_eq!(
            route(["cancel", "operation", "clear", "continue", "execute_now"]).await,
            Route::Direct
        );
        assert_eq!(
            route([
                "instruction",
                "refactor",
                "clear",
                "side_topic",
                "execute_now"
            ])
            .await,
            Route::Sidechat
        );
    }

    /// Only a genuinely open decision asks. An ordinary implementation
    /// detail is `inferable`, and an inferable turn goes to work.
    #[tokio::test]
    async fn an_ordinary_detail_does_not_ask() {
        assert_eq!(
            route([
                "instruction",
                "feature_add",
                "inferable",
                "continue",
                "execute_now"
            ])
            .await,
            Route::InspectThenExecute
        );
        assert_eq!(
            route([
                "instruction",
                "feature_add",
                "clear",
                "continue",
                "execute_now"
            ])
            .await,
            Route::Direct
        );
        // Conflicting with a live decision is worth stopping for.
        assert_eq!(
            route([
                "instruction",
                "feature_change",
                "conflicting",
                "replace",
                "execute_now"
            ])
            .await,
            Route::Grilling
        );
    }

    /// A judge that is absent, unreachable or incomplete leaves a record
    /// that says so. None of them is a classification.
    #[tokio::test]
    async fn a_failure_is_never_stored_as_a_classification() {
        for (decider, reason) in [
            (None::<&dyn Decider>, "classifier_unavailable"),
            (Some(&Broken as &dyn Decider), "classifier_unavailable"),
        ] {
            let intent = classify(decider, &serde_json::json!({}), 7).await;
            assert!(!intent.classified());
            assert_eq!(intent.unavailable.as_deref(), Some(reason));
            assert!(intent.route.is_none());
        }
        // An answer that names an option nobody offered is incomplete,
        // not a route.
        let drifted = Fixed(BTreeMap::from([
            (Utterance::KEY.to_owned(), "deploy".to_owned()),
            (Work::KEY.to_owned(), "bug_fix".to_owned()),
            (Clarity::KEY.to_owned(), "clear".to_owned()),
            (Relation::KEY.to_owned(), "continue".to_owned()),
            (Approach::KEY.to_owned(), "execute_now".to_owned()),
            (Effort::KEY.to_owned(), "medium".to_owned()),
        ]));
        let intent = classify(Some(&drifted), &serde_json::json!({}), 8).await;
        assert_eq!(
            intent.unavailable.as_deref(),
            Some("classifier_answer_incomplete")
        );
    }

    /// While a question is outstanding no file-changing tool is offered,
    /// and a restart re-asks only what is still unanswered.
    #[test]
    fn a_pending_question_withholds_the_write_tools_and_is_asked_once() {
        let ask = |answer: Option<&str>| PendingClarification {
            question: "保存形式をどちらにするか".into(),
            turn: 3,
            answer_hash: answer.map(ToOwned::to_owned),
        };
        let pending = ClarificationState::Pending {
            asked: vec![ask(None), ask(Some("a1b2"))],
        };
        assert!(!pending.may_change_files());
        assert_eq!(
            pending.unanswered().len(),
            1,
            "an answered question was asked again"
        );

        let resolved = ClarificationState::Resolved {
            asked: vec![ask(Some("a1b2"))],
        };
        assert!(resolved.may_change_files());
        assert!(resolved.unanswered().is_empty());
        assert!(ClarificationState::None.may_change_files());
    }

    fn asked(questions: &[&str]) -> polaris_provider::Message {
        polaris_provider::Message {
            tool_calls: vec![polaris_provider::ToolCall {
                id: "c1".into(),
                name: CLARIFY.into(),
                arguments: serde_json::json!({ "questions": questions }),
            }],
            ..polaris_provider::Message::assistant("")
        }
    }

    /// The `-p` run that asks exits; the `--resume` that answers is a new
    /// process with an empty field. Everything this state needs is in the
    /// log, and these are the three readings of it.
    #[test]
    fn the_log_says_whether_a_question_is_still_outstanding() {
        let opening = polaris_provider::Message::user("設定をJSONで保存したい");
        assert_eq!(
            ClarificationState::from_history(std::slice::from_ref(&opening)),
            ClarificationState::None,
            "a conversation that never asked is not waiting"
        );

        let waiting = vec![opening.clone(), asked(&["TOMLとJSONのどちらにするか"])];
        let pending = ClarificationState::from_history(&waiting);
        assert!(!pending.may_change_files(), "{pending:?}");
        assert_eq!(pending.unanswered().len(), 1, "{pending:?}");
        assert_eq!(
            pending.unanswered()[0].question,
            "TOMLとJSONのどちらにするか"
        );

        let mut answered = waiting.clone();
        answered.push(polaris_provider::Message::user("JSONで"));
        let resolved = ClarificationState::from_history(&answered);
        assert!(resolved.may_change_files(), "{resolved:?}");
        assert!(
            resolved.unanswered().is_empty(),
            "the answered question was asked again: {resolved:?}"
        );
        let ClarificationState::Resolved { asked } = &resolved else {
            panic!("{resolved:?}");
        };
        assert!(asked[0].answer_hash.is_some(), "{resolved:?}");
    }

    /// Two answers that differ are two different records, and the same
    /// answer read twice is the same record. That is the whole job of the
    /// hash, and it is why the answer text itself is not copied here.
    #[test]
    fn the_answer_is_identified_without_being_stored() {
        let base = vec![
            polaris_provider::Message::user("設定を保存したい"),
            asked(&["どこへ置くか"]),
        ];
        let hash_of = |answer: &str| {
            let mut messages = base.clone();
            messages.push(polaris_provider::Message::user(answer));
            match ClarificationState::from_history(&messages) {
                ClarificationState::Resolved { asked } => asked[0].answer_hash.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(hash_of("~/.config へ"), hash_of("~/.config へ"));
        assert_ne!(hash_of("~/.config へ"), hash_of("プロジェクト直下へ"));
        assert!(!format!("{:?}", hash_of("~/.config へ")).contains(".config"));
    }

    /// Three is the bound and blank is not a question. A request that is
    /// all blanks asked nothing, and a state that says otherwise would
    /// hold the write tools back with no answer that gives them back —
    /// the empty `Pending` that was measured and removed on 2026-09-21.
    #[test]
    fn a_request_is_bounded_and_a_blank_is_not_a_question() {
        assert_eq!(
            questions_in(&serde_json::json!({"questions": ["a", "b", "c", "d"]})).len(),
            MAX_QUESTIONS
        );
        assert_eq!(
            questions_in(&serde_json::json!({"questions": ["  ", "", "  実質的な問い "]})),
            vec!["実質的な問い".to_owned()]
        );
        assert!(questions_in(&serde_json::json!({"questions": []})).is_empty());
        assert!(questions_in(&serde_json::json!({})).is_empty());
        let blank = vec![
            polaris_provider::Message::user("何かしてほしい"),
            asked(&["", "   "]),
        ];
        assert_eq!(
            ClarificationState::from_history(&blank),
            ClarificationState::None,
            "a request with no question in it left the run waiting"
        );
    }
}
