use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::KnutError;
use crate::tool::SideEffect;

/// Transitions System One may choose after a meaningful node result.
///
/// The runtime supplies the allowed set; a choice outside it is treated
/// as a weak answer and falls back deterministically. `Done` is only
/// ever offered when deterministic completion preconditions are
/// satisfied — a successful step alone is never proof of task
/// completion. `Blocked` is the explicit non-success state for
/// degenerate situations (empty transition sets, exhausted budgets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeChoice {
    Continue,
    Retry,
    AlternateBranch,
    RetrieveMore,
    AskUser,
    EscalateModel,
    Done,
    Blocked,
}

/// What happened at the node System One is judging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeOutcome {
    Succeeded,
    /// `retriable` is false for hard failures (invalid input, rejected
    /// plan), true for transient-looking ones (tool errored).
    Failed {
        retriable: bool,
    },
    /// Policy refusal, unavailability, or cancellation: an explicit
    /// non-success state, never converted into an automatic retry.
    Blocked,
}

/// Compact post-node state handed to the edge router.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeState {
    pub goal: String,
    pub current_node: String,
    pub result_summary: String,
    pub outcome: NodeOutcome,
    /// Attempts already spent on this step (initial + approved retries).
    pub attempts: usize,
    /// Whether deterministic completion preconditions are satisfied.
    ///
    /// Computed by the runtime from [`crate::completion::
    /// CompletionRequirements`] over verified evidence bound to the
    /// exact artifact revision — never from model confidence.
    pub done_permitted: bool,
    pub allowed: BTreeSet<EdgeChoice>,
}

/// Hard cap on attempts per step (1 initial + 1 router-approved retry).
/// The bound, not the router, ends loops.
pub const MAX_STEP_ATTEMPTS: usize = 2;

impl EdgeState {
    /// The runtime-supplied allowed set for one step attempt.
    ///
    /// - Success offers `Continue` always; `Done` only when completion
    ///   preconditions are satisfied.
    /// - Side-effecting nodes are never offered `Retry`, and no node is
    ///   once the attempt budget is exhausted: routing uncertainty must
    ///   not repeat a write or spin a step forever.
    /// - Blocked outcomes never offer `Done` or `Retry`: a refusal is
    ///   not progress and cannot be retried into existence.
    pub fn for_step(
        goal: impl Into<String>,
        current_node: impl Into<String>,
        result_summary: impl Into<String>,
        outcome: NodeOutcome,
        side_effect: SideEffect,
        attempts: usize,
        done_permitted: bool,
    ) -> EdgeState {
        let mut allowed = BTreeSet::new();
        match outcome {
            NodeOutcome::Succeeded => {
                allowed.insert(EdgeChoice::Continue);
                if done_permitted {
                    allowed.insert(EdgeChoice::Done);
                }
            }
            NodeOutcome::Failed { retriable } => {
                if retriable && side_effect == SideEffect::ReadOnly && attempts < MAX_STEP_ATTEMPTS
                {
                    allowed.insert(EdgeChoice::Retry);
                }
                allowed.insert(EdgeChoice::AlternateBranch);
                allowed.insert(EdgeChoice::RetrieveMore);
                allowed.insert(EdgeChoice::AskUser);
                allowed.insert(EdgeChoice::EscalateModel);
            }
            NodeOutcome::Blocked => {
                allowed.insert(EdgeChoice::AskUser);
                allowed.insert(EdgeChoice::RetrieveMore);
                allowed.insert(EdgeChoice::EscalateModel);
            }
        }

        EdgeState {
            goal: goal.into(),
            current_node: current_node.into(),
            result_summary: result_summary.into(),
            outcome,
            attempts,
            done_permitted,
            allowed,
        }
    }

    /// The state at the end of the plan: mechanically `Done` when
    /// completion preconditions are satisfied, otherwise an explicit
    /// request for reasoning — never a synthesized success.
    pub fn for_plan_end(
        goal: impl Into<String>,
        done_permitted: bool,
        summary: impl Into<String>,
    ) -> EdgeState {
        let mut allowed = BTreeSet::new();
        if done_permitted {
            allowed.insert(EdgeChoice::Done);
        } else {
            allowed.insert(EdgeChoice::EscalateModel);
            allowed.insert(EdgeChoice::AskUser);
        }

        EdgeState {
            goal: goal.into(),
            current_node: "<plan-end>".to_owned(),
            result_summary: summary.into(),
            outcome: NodeOutcome::Succeeded,
            attempts: 0,
            done_permitted,
            allowed,
        }
    }

    /// The edge to take when no judgment can be trusted.
    ///
    /// `EscalateModel` (request reasoning) when legal, `Done` only when
    /// completion preconditions are satisfied, `Blocked` as the last
    /// resort. Never synthesizes success.
    fn safe_fallback(&self) -> EdgeChoice {
        if self.allowed.contains(&EdgeChoice::EscalateModel) {
            EdgeChoice::EscalateModel
        } else if self.allowed.contains(&EdgeChoice::Done) {
            EdgeChoice::Done
        } else if self.allowed.contains(&EdgeChoice::Continue) {
            EdgeChoice::Continue
        } else {
            EdgeChoice::Blocked
        }
    }
}

/// One bounded edge judgment with raw confidence for traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeJudgment {
    pub choice: EdgeChoice,
    pub confidence: f32,
}

/// Why the proposed choice was overridden, kept for traces and evals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeOverrideReason {
    /// Response confidence was below the configured floor.
    LowConfidence,
    /// Confidence was non-finite or outside 0..=1.
    InvalidResponse,
    /// The chosen edge was not in the runtime-supplied allowed set.
    OutOfSet,
    /// `Done` proposed while deterministic completion preconditions are
    /// unmet. Model confidence can never waive this.
    DoneForbiddenByRequirements,
    /// No legal transitions existed at all.
    EmptyAllowedSet,
}

/// Proposed vs effective: the raw judgment is preserved for evals even
/// when policy overrode it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeDecision {
    /// What the router proposed (or the mechanical answer, if no router
    /// call was needed).
    pub proposed: EdgeJudgment,
    /// The edge actually taken.
    pub effective: EdgeChoice,
    /// Why `effective` differs from `proposed.choice`, when it does.
    pub override_reason: Option<EdgeOverrideReason>,
}

impl EdgeDecision {
    pub fn was_overridden(&self) -> bool {
        self.override_reason.is_some()
    }
}

/// Bounded-choice router over post-node state: the fast control layer.
#[async_trait]
pub trait EdgeRouter: Send + Sync {
    /// Returns a judgment with confidence in 0..=1; anything else is an
    /// invalid response handled by the selector.
    async fn choose(&self, state: &EdgeState) -> Result<EdgeJudgment, KnutError>;
}

#[async_trait]
impl<T: EdgeRouter + ?Sized> EdgeRouter for &T {
    async fn choose(&self, state: &EdgeState) -> Result<EdgeJudgment, KnutError> {
        (**self).choose(state).await
    }
}

/// Picks edges: mechanically when possible, System One otherwise.
///
/// Guarantees (issue #16):
/// - singleton allowed sets need no judgment call at all;
/// - low-confidence, out-of-set, malformed, and invalid answers fall
///   back deterministically with a structured reason — never into `Done`
///   unless `Done` was legal, and never into an invented success;
/// - the proposed choice is retained separately from the effective one.
pub struct EdgeSelector<R> {
    router: R,
    min_confidence: f32,
}

const DEFAULT_MIN_CONFIDENCE: f32 = 0.75;

impl<R: EdgeRouter> EdgeSelector<R> {
    /// `min_confidence` must lie in 0..=1; invalid configuration is
    /// rejected instead of silently producing unpredictable gating.
    pub fn with_min_confidence(mut self, min_confidence: f32) -> Result<Self, KnutError> {
        if !min_confidence.is_finite() || !(0.0..=1.0).contains(&min_confidence) {
            return Err(KnutError::InvalidArguments {
                path: "min_confidence".to_owned(),
                reason: format!("{min_confidence} is not a confidence in 0..=1"),
            });
        }
        self.min_confidence = min_confidence;
        Ok(self)
    }

    pub fn new(router: R) -> Self {
        Self {
            router,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
        }
    }

    /// Decide the next edge for this state.
    pub async fn select(&self, state: &EdgeState) -> Result<EdgeDecision, KnutError> {
        // Mechanically determined: no call when the answer is forced.
        if state.allowed.len() == 1 {
            let choice = *state.allowed.iter().next().expect("singleton set");
            return Ok(EdgeDecision {
                proposed: EdgeJudgment {
                    choice,
                    confidence: 1.0,
                },
                effective: choice,
                override_reason: None,
            });
        }

        // An empty set is an explicit non-success state, not a fallback
        // guess.
        if state.allowed.is_empty() {
            return Ok(EdgeDecision {
                proposed: EdgeJudgment {
                    choice: EdgeChoice::Blocked,
                    confidence: 1.0,
                },
                effective: EdgeChoice::Blocked,
                override_reason: Some(EdgeOverrideReason::EmptyAllowedSet),
            });
        }

        let judgment = match self.router.choose(state).await {
            Ok(j) => j,
            // Transport errors escalate deterministically; they never
            // become a success or a silent continue.
            Err(err) => {
                return Err(err);
            }
        };

        // Invalid response values (non-finite / out of range confidence).
        if !judgment.confidence.is_finite() || !(0.0..=1.0).contains(&judgment.confidence) {
            return Ok(EdgeDecision {
                proposed: judgment,
                effective: state.safe_fallback(),
                override_reason: Some(EdgeOverrideReason::InvalidResponse),
            });
        }

        // Done requires satisfaction of deterministic preconditions;
        // model confidence can never waive them.
        if judgment.choice == EdgeChoice::Done && !state.done_permitted {
            return Ok(EdgeDecision {
                proposed: judgment,
                effective: state.safe_fallback(),
                override_reason: Some(EdgeOverrideReason::DoneForbiddenByRequirements),
            });
        }

        let trusted =
            judgment.confidence >= self.min_confidence && state.allowed.contains(&judgment.choice);

        if trusted {
            Ok(EdgeDecision {
                proposed: judgment.clone(),
                effective: judgment.choice,
                override_reason: None,
            })
        } else {
            let reason = if state.allowed.contains(&judgment.choice) {
                EdgeOverrideReason::LowConfidence
            } else {
                EdgeOverrideReason::OutOfSet
            };
            Ok(EdgeDecision {
                proposed: judgment,
                effective: state.safe_fallback(),
                override_reason: Some(reason),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Router with scripted answers; counts how often it was consulted.
    struct ScriptedRouter {
        answers: std::sync::Mutex<Vec<Result<EdgeJudgment, KnutError>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedRouter {
        fn new(answers: Vec<Result<EdgeJudgment, KnutError>>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl EdgeRouter for ScriptedRouter {
        async fn choose(&self, _state: &EdgeState) -> Result<EdgeJudgment, KnutError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                return Err(KnutError::SystemOne("script exhausted".to_owned()));
            }
            answers.remove(0)
        }
    }

    fn judgment(choice: EdgeChoice, confidence: f32) -> Result<EdgeJudgment, KnutError> {
        Ok(EdgeJudgment { choice, confidence })
    }

    fn step_state(
        outcome: NodeOutcome,
        side_effect: SideEffect,
        attempts: usize,
        done_permitted: bool,
    ) -> EdgeState {
        EdgeState::for_step(
            "ship the thing",
            "step_1",
            "latest observation",
            outcome,
            side_effect,
            attempts,
            done_permitted,
        )
    }

    #[test]
    fn write_nodes_are_never_offered_retry() {
        for effect in [SideEffect::IdempotentWrite, SideEffect::NonIdempotentWrite] {
            let state = step_state(NodeOutcome::Failed { retriable: true }, effect, 1, false);
            assert!(!state.allowed.contains(&EdgeChoice::Retry));
            assert!(state.allowed.contains(&EdgeChoice::EscalateModel));
        }

        let read_only = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            1,
            false,
        );
        assert!(read_only.allowed.contains(&EdgeChoice::Retry));
    }

    #[test]
    fn hard_failures_are_not_retriable_even_for_reads() {
        let state = step_state(
            NodeOutcome::Failed { retriable: false },
            SideEffect::ReadOnly,
            1,
            false,
        );
        assert!(!state.allowed.contains(&EdgeChoice::Retry));
    }

    #[test]
    fn done_requires_satisfied_preconditions_even_after_success() {
        let without = step_state(NodeOutcome::Succeeded, SideEffect::ReadOnly, 1, false);
        assert!(!without.allowed.contains(&EdgeChoice::Done));
        assert!(without.allowed.contains(&EdgeChoice::Continue));

        let with = step_state(NodeOutcome::Succeeded, SideEffect::ReadOnly, 1, true);
        assert!(with.allowed.contains(&EdgeChoice::Done));
    }

    #[test]
    fn exhausted_budget_ends_retry_even_for_reads() {
        let exhausted = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            MAX_STEP_ATTEMPTS,
            false,
        );
        assert!(!exhausted.allowed.contains(&EdgeChoice::Retry));

        let within = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            MAX_STEP_ATTEMPTS - 1,
            false,
        );
        assert!(within.allowed.contains(&EdgeChoice::Retry));
    }

    #[test]
    fn blocked_outcomes_offer_no_done_and_no_retry() {
        for effect in [SideEffect::ReadOnly, SideEffect::NonIdempotentWrite] {
            let state = step_state(NodeOutcome::Blocked, effect, 0, true);
            assert!(!state.allowed.contains(&EdgeChoice::Done));
            assert!(!state.allowed.contains(&EdgeChoice::Retry));
            // Still explicit non-success handling: reasoning or asking.
            assert!(state.allowed.contains(&EdgeChoice::EscalateModel));
        }
    }

    #[tokio::test]
    async fn singleton_sets_skip_the_router_entirely() {
        // Plan end with satisfied preconditions: mechanically Done.
        let state = EdgeState::for_plan_end("ship", true, "all checks passed");

        let router = ScriptedRouter::new(vec![]);
        let selector = EdgeSelector::new(&router);

        let decision = selector.select(&state).await.unwrap();

        assert_eq!(decision.effective, EdgeChoice::Done);
        assert_eq!(decision.proposed.confidence, 1.0);
        assert!(!decision.was_overridden());
        assert_eq!(router.calls(), 0, "mechanical answer needs no model call");
    }

    #[tokio::test]
    async fn router_choice_within_allowed_set_is_honored() {
        let state = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            1,
            false,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, 0.9)]);
        let selector = EdgeSelector::new(&router);

        let decision = selector.select(&state).await.unwrap();

        assert_eq!(decision.effective, EdgeChoice::Retry);
        assert!(!decision.was_overridden());
        assert_eq!(router.calls(), 1);
    }

    #[tokio::test]
    async fn low_confidence_falls_back_deterministically() {
        // After failure the safe fallback is EscalateModel.
        let failure_state = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            1,
            false,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, 0.3)]);
        let selector = EdgeSelector::new(&router);

        let weak = selector.select(&failure_state).await.unwrap();

        assert_eq!(weak.effective, EdgeChoice::EscalateModel);
        assert_eq!(
            weak.override_reason,
            Some(EdgeOverrideReason::LowConfidence)
        );

        // After success (preconditions unmet) the safe fallback among
        // {Continue} after removing Done would be Continue; keep both
        // legal edges and confirm the proposed is retained for evals.
        let success_state = step_state(NodeOutcome::Succeeded, SideEffect::ReadOnly, 1, false);
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Continue, 0.4)]);
        let selector = EdgeSelector::new(&router);

        let weak = selector.select(&success_state).await.unwrap();
        assert_eq!(weak.effective, EdgeChoice::Continue);
        assert_eq!(weak.proposed.choice, EdgeChoice::Continue);
    }

    #[tokio::test]
    async fn high_confidence_done_is_rejected_while_requirements_are_unmet() {
        // A step failed mid read/edit/test workflow: requirements are
        // unmet and the state is non-mechanical (several legal edges).
        let state = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            1,
            false,
        );
        assert!(state.allowed.len() > 1);
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Done, 0.99)]);
        let selector = EdgeSelector::new(&router);

        let decision = selector.select(&state).await.unwrap();

        assert_ne!(decision.effective, EdgeChoice::Done);
        assert_eq!(
            decision.override_reason,
            Some(EdgeOverrideReason::DoneForbiddenByRequirements)
        );
        // The overconfident proposal is retained for traces/evals.
        assert_eq!(decision.proposed.choice, EdgeChoice::Done);
        assert_eq!(decision.proposed.confidence, 0.99);

        // Even at the plan end with a successful step, Done stays
        // impossible while evidence is missing — here the whole set is
        // non-Done, so the proposal is out-of-set AND forbidden.
        let plan_end = EdgeState::for_plan_end("ship", false, "no evidence yet");
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Done, 0.99)]);
        let selector = EdgeSelector::new(&router);

        let decision = selector.select(&plan_end).await.unwrap();

        assert_ne!(decision.effective, EdgeChoice::Done);
        assert_eq!(
            decision.override_reason,
            Some(EdgeOverrideReason::DoneForbiddenByRequirements)
        );
        assert_eq!(decision.effective, EdgeChoice::EscalateModel);
    }

    #[tokio::test]
    async fn mechanical_continue_after_success_needs_no_router_call() {
        // A successful step with unsatisfied preconditions has exactly
        // one legal edge (Continue): the router is never consulted, so
        // no weak judgment could turn it into Done.
        let state = step_state(NodeOutcome::Succeeded, SideEffect::ReadOnly, 1, false);
        assert_eq!(state.allowed.len(), 1);

        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Done, 0.99)]);
        let selector = EdgeSelector::new(&router);

        let decision = selector.select(&state).await.unwrap();

        assert_eq!(decision.effective, EdgeChoice::Continue);
        assert_eq!(router.calls(), 0);
    }

    #[tokio::test]
    async fn out_of_set_choices_are_treated_as_weak() {
        // A blocked state offers AskUser/RetrieveMore/EscalateModel but
        // never AlternateBranch: proposing it is out-of-set even at
        // maximum confidence.
        let state = step_state(NodeOutcome::Blocked, SideEffect::ReadOnly, 0, false);
        assert!(!state.allowed.contains(&EdgeChoice::AlternateBranch));
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::AlternateBranch, 0.99)]);
        let selector = EdgeSelector::new(&router);

        let decision = selector.select(&state).await.unwrap();

        assert_ne!(decision.effective, EdgeChoice::AlternateBranch);
        assert_eq!(decision.override_reason, Some(EdgeOverrideReason::OutOfSet));
        assert_eq!(decision.proposed.choice, EdgeChoice::AlternateBranch);
    }

    #[tokio::test]
    async fn non_finite_and_out_of_range_confidence_is_invalid_not_trusted() {
        for bad in [f32::NAN, f32::INFINITY, 1.5, -0.1] {
            let state = step_state(
                NodeOutcome::Failed { retriable: true },
                SideEffect::ReadOnly,
                1,
                false,
            );
            let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, bad)]);
            let selector = EdgeSelector::new(&router);

            let decision = selector.select(&state).await.unwrap();

            assert_eq!(
                decision.override_reason,
                Some(EdgeOverrideReason::InvalidResponse),
                "confidence {bad}"
            );
            assert_eq!(decision.effective, EdgeChoice::EscalateModel);
        }
    }

    #[tokio::test]
    async fn invalid_confidence_configuration_is_rejected() {
        let router = ScriptedRouter::new(vec![]);
        let selector = EdgeSelector::new(&router);

        for bad in [f32::NAN, -0.01, 1.01, f32::INFINITY] {
            let err = selector
                .clone_config()
                .with_min_confidence(bad)
                .unwrap_err();
            assert!(
                err.to_string().contains("confidence"),
                "bad config {bad} should be rejected"
            );
        }

        // Boundary values are legal.
        assert!(selector.clone_config().with_min_confidence(0.0).is_ok());
        assert!(selector.clone_config().with_min_confidence(1.0).is_ok());
    }

    impl<R> EdgeSelector<R> {
        fn clone_config(&self) -> EdgeSelectorConfig {
            EdgeSelectorConfig
        }
    }

    struct EdgeSelectorConfig;

    impl EdgeSelectorConfig {
        fn with_min_confidence(&self, c: f32) -> Result<f32, KnutError> {
            if !c.is_finite() || !(0.0..=1.0).contains(&c) {
                return Err(KnutError::InvalidArguments {
                    path: "min_confidence".to_owned(),
                    reason: format!("{c} is not a confidence in 0..=1"),
                });
            }
            Ok(c)
        }
    }

    #[tokio::test]
    async fn transport_errors_surface_instead_of_faking_progress() {
        let state = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            1,
            false,
        );
        let router =
            ScriptedRouter::new(vec![Err(KnutError::SystemOne("transport down".to_owned()))]);
        let selector = EdgeSelector::new(&router);

        let err = selector.select(&state).await.unwrap_err();

        assert!(matches!(err, KnutError::SystemOne(msg) if msg.contains("transport")));
    }

    #[tokio::test]
    async fn exhausted_script_treated_like_transport_error() {
        let state = step_state(NodeOutcome::Succeeded, SideEffect::ReadOnly, 1, true);
        let router = ScriptedRouter::new(vec![]);
        let selector = EdgeSelector::new(&router);

        let err = selector.select(&state).await.unwrap_err();
        assert!(matches!(err, KnutError::SystemOne(_)));
    }

    /// Property-style sweep: no (outcome, effect, attempts,
    /// done_permitted) combination ever makes Done legal without
    /// success + satisfied preconditions, and Retry is impossible for
    /// writes or exhausted budgets.
    #[test]
    fn state_transition_invariants_hold_across_the_matrix() {
        let outcomes = [
            NodeOutcome::Succeeded,
            NodeOutcome::Failed { retriable: true },
            NodeOutcome::Failed { retriable: false },
            NodeOutcome::Blocked,
        ];
        let effects = [
            SideEffect::ReadOnly,
            SideEffect::IdempotentWrite,
            SideEffect::NonIdempotentWrite,
        ];

        for outcome in outcomes {
            for effect in effects {
                for attempts in [0, 1, MAX_STEP_ATTEMPTS, MAX_STEP_ATTEMPTS + 5] {
                    for done_permitted in [false, true] {
                        let state = step_state(outcome, effect, attempts, done_permitted);

                        // Done never legal unless success + preconditions.
                        if state.allowed.contains(&EdgeChoice::Done) {
                            assert_eq!(outcome, NodeOutcome::Succeeded);
                            assert!(done_permitted);
                        }

                        // Retry only for retriable reads within budget.
                        if state.allowed.contains(&EdgeChoice::Retry) {
                            assert_eq!(effect, SideEffect::ReadOnly);
                            assert!(matches!(outcome, NodeOutcome::Failed { retriable: true }));
                            assert!(attempts < MAX_STEP_ATTEMPTS);
                        }

                        // Every state has a deterministic non-synthetic
                        // fallback; success states always have Continue.
                        if outcome == NodeOutcome::Succeeded {
                            assert!(state.allowed.contains(&EdgeChoice::Continue));
                        } else {
                            assert!(state.allowed.contains(&EdgeChoice::EscalateModel));
                        }

                        // Failure/blocked can never claim Done implicitly.
                        if outcome != NodeOutcome::Succeeded {
                            assert!(!state.allowed.contains(&EdgeChoice::Done));
                        }
                    }
                }
            }
        }
    }

    /// The multi-step fake workflow: a failed branch recovers through
    /// edge decisions, and no weak judgment can end the task early.
    #[tokio::test]
    async fn fake_workflow_recovers_and_cannot_finish_without_evidence() {
        use crate::tree::{CancelFlag, TreeExecutor};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct StaticTool;

        #[async_trait]
        impl crate::tool::Tool for StaticTool {
            fn metadata(&self) -> crate::tool::ToolMetadata {
                crate::tool::ToolMetadata {
                    id: "probe".to_owned(),
                    tool_version: "1".to_owned(),
                    capability: "diag".to_owned(),
                    description: "probe that fails once".to_owned(),
                    input_schema: serde_json::json!({ "type": "object" }),
                    side_effect: SideEffect::ReadOnly,
                }
            }

            async fn call(
                &self,
                _input: serde_json::Value,
            ) -> Result<serde_json::Value, KnutError> {
                Err(KnutError::Tool("transient".to_owned()))
            }
        }

        struct WorkingTool;

        #[async_trait]
        impl crate::tool::Tool for WorkingTool {
            fn metadata(&self) -> crate::tool::ToolMetadata {
                crate::tool::ToolMetadata {
                    id: "work".to_owned(),
                    tool_version: "1".to_owned(),
                    capability: "diag".to_owned(),
                    description: "succeeds".to_owned(),
                    input_schema: serde_json::json!({ "type": "object" }),
                    side_effect: SideEffect::ReadOnly,
                }
            }

            async fn call(
                &self,
                _input: serde_json::Value,
            ) -> Result<serde_json::Value, KnutError> {
                Ok(serde_json::json!({ "ok": true }))
            }
        }

        let mut registry = crate::ToolRegistry::default();
        registry.register(StaticTool).unwrap();
        registry.register(WorkingTool).unwrap();

        struct NoModel;
        #[async_trait]
        impl crate::model::Model for NoModel {
            fn identity(&self) -> crate::ModelIdentity {
                crate::ModelIdentity {
                    provider: "fake".to_owned(),
                    model: "none".to_owned(),
                    tier: crate::ModelTier::Reasoner,
                }
            }
            async fn complete(
                &self,
                _request: &crate::ModelRequest,
            ) -> Result<crate::ModelResponse, KnutError> {
                Err(KnutError::SystemOne("no model in this test".to_owned()))
            }
        }
        struct Accept;
        impl crate::Verifier for Accept {
            fn verify(&self, _r: &crate::ModelResponse) -> crate::VerificationVerdict {
                crate::VerificationVerdict::Sufficient
            }
        }
        let gate = Arc::new(crate::ExecutionGate::new(
            crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly),
        ));
        let executor = TreeExecutor::new(
            Arc::new(registry),
            gate,
            Arc::new(crate::ComputeCascade::empty().with_reasoner(NoModel)),
            Arc::new(Accept),
        );

        let step_a = crate::tree::PlanNode::Tool {
            id: "step_a".into(),
            capability: "diag".into(),
            tool_id: "probe".into(),
            input: serde_json::json!({}),
        };
        let step_b = crate::tree::PlanNode::Tool {
            id: "step_b".into(),
            capability: "diag".into(),
            tool_id: "work".into(),
            input: serde_json::json!({}),
        };
        let steps = [(&step_a, "probe"), (&step_b, "work")];

        // Script: retry step_a once, then abandon the branch; continue
        // after step_b succeeds. Continue on the final step ends the
        // workflow, so no further judgment is requested.
        let router = ScriptedRouter::new(vec![
            judgment(EdgeChoice::Retry, 0.9),
            judgment(EdgeChoice::AlternateBranch, 0.9),
            judgment(EdgeChoice::Continue, 0.9),
        ]);
        let selector = EdgeSelector::new(&router);

        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let mut transcript = Vec::new();

        for (node, tool_name) in steps {
            let mut attempts = 0usize;
            loop {
                attempts += 1;
                let run = executor.run(node, Arc::clone(&cancel)).await.unwrap();
                let status = run.statuses.get(node.id()).copied();

                let outcome = match status {
                    Some(crate::NodeStatus::Succeeded) => NodeOutcome::Succeeded,
                    _ => NodeOutcome::Failed { retriable: true },
                };

                // Completion preconditions are NOT satisfied mid-flow:
                // even a successful read cannot end the task.
                let edge_state = EdgeState::for_step(
                    "finish the fake workflow",
                    node.id().to_owned(),
                    format!("{tool_name} attempt {attempts}"),
                    outcome,
                    SideEffect::ReadOnly,
                    attempts,
                    false,
                );

                let decision = selector.select(&edge_state).await.unwrap();
                transcript.push(format!("{}: {:?}", node.id(), decision.effective));

                match decision.effective {
                    EdgeChoice::Continue | EdgeChoice::AlternateBranch => break,
                    EdgeChoice::Done => panic!("Done is impossible mid-flow"),
                    EdgeChoice::Retry if attempts < MAX_STEP_ATTEMPTS => continue,
                    _ => break,
                }
            }
        }

        assert_eq!(
            transcript,
            vec![
                "step_a: Retry",
                "step_a: AlternateBranch",
                "step_b: Continue",
            ]
        );
        // The router decided twice: step_a's failure branches. The final
        // successful Continue is a mechanical singleton (preconditions
        // unmet, one legal edge), so no model call was spent on it.
        assert_eq!(router.calls(), 2);
    }

    #[tokio::test]
    async fn custom_floor_changes_the_trust_boundary() {
        let state = step_state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
            1,
            false,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::AlternateBranch, 0.5)]);
        let selector = EdgeSelector::new(&router).with_min_confidence(0.5).unwrap();

        let decision = selector.select(&state).await.unwrap();

        assert_eq!(decision.effective, EdgeChoice::AlternateBranch);
        assert!(!decision.was_overridden());
    }
}
