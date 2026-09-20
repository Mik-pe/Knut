use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::KnutError;
use crate::tool::SideEffect;

/// Transitions System One may choose after a meaningful node result.
///
/// The runtime supplies the allowed set; a choice outside it is a router
/// bug and falls back deterministically.
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
    Blocked,
}

/// Compact post-node state handed to the edge router.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeState {
    pub goal: String,
    pub current_node: String,
    pub result_summary: String,
    pub outcome: NodeOutcome,
    pub allowed: BTreeSet<EdgeChoice>,
}

impl EdgeState {
    /// The runtime-supplied allowed set for an outcome.
    ///
    /// Side-effecting nodes are never offered `Retry`: uncertainty about
    /// routing must not repeat a write. Success always offers exactly
    /// `Continue`/`Done`; blocked nodes cannot be retried into existence.
    pub fn allowed_for(outcome: NodeOutcome, side_effect: SideEffect) -> BTreeSet<EdgeChoice> {
        let mut allowed = BTreeSet::new();
        match outcome {
            NodeOutcome::Succeeded => {
                allowed.insert(EdgeChoice::Continue);
                allowed.insert(EdgeChoice::Done);
            }
            NodeOutcome::Failed { retriable } => {
                if retriable && side_effect == SideEffect::ReadOnly {
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
        allowed
    }
}

/// One edge decision with raw confidence for traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeJudgment {
    pub choice: EdgeChoice,
    pub confidence: f32,
}

/// Bounded-choice router over post-node state: the fast control layer.
#[async_trait]
pub trait EdgeRouter: Send + Sync {
    async fn choose(&self, state: &EdgeState) -> Result<EdgeJudgment, KnutError>;
}

/// Picks edges: mechanically when possible, System One otherwise.
///
/// - A singleton allowed set needs no judgment call.
/// - Low-confidence answers fall back to the deterministic safe edge
///   (`Done` after success, `EscalateModel` after failure/blocking)
///   instead of trusting a weak route.
/// - Choices outside the allowed set are ignored like weak answers.
pub struct EdgeSelector<R> {
    router: R,
    min_confidence: f32,
}

const DEFAULT_MIN_CONFIDENCE: f32 = 0.75;

impl<R: EdgeRouter> EdgeSelector<R> {
    pub fn new(router: R) -> Self {
        Self {
            router,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
        }
    }

    pub fn with_min_confidence(mut self, min_confidence: f32) -> Self {
        self.min_confidence = min_confidence.clamp(0.0, 1.0);
        self
    }

    /// The edge to take when no judgment can be trusted.
    fn safe_fallback(state: &EdgeState) -> EdgeChoice {
        if state.allowed.contains(&EdgeChoice::EscalateModel) {
            EdgeChoice::EscalateModel
        } else if state.allowed.contains(&EdgeChoice::Done) {
            EdgeChoice::Done
        } else {
            // Deterministic sets always contain one of the two above.
            state
                .allowed
                .iter()
                .next()
                .copied()
                .unwrap_or(EdgeChoice::Done)
        }
    }

    pub async fn select(&self, state: &EdgeState) -> Result<EdgeJudgment, KnutError> {
        // Mechanically determined: no call when the answer is forced.
        if state.allowed.len() == 1 {
            let choice = *state.allowed.iter().next().expect("singleton set");
            return Ok(EdgeJudgment {
                choice,
                confidence: 1.0,
            });
        }

        let judgment = self.router.choose(state).await?;

        let trusted = judgment.confidence.is_finite()
            && judgment.confidence >= self.min_confidence
            && state.allowed.contains(&judgment.choice);

        if trusted {
            Ok(judgment)
        } else {
            Ok(EdgeJudgment {
                choice: Self::safe_fallback(state),
                confidence: judgment.confidence,
            })
        }
    }
}

#[async_trait]
impl<T: EdgeRouter + ?Sized> EdgeRouter for &T {
    async fn choose(&self, state: &EdgeState) -> Result<EdgeJudgment, KnutError> {
        (**self).choose(state).await
    }
}

/// Total attempts allowed per step in the sequence runner (1 initial + 1
/// router-approved retry). The bound, not the router, ends loops.
pub const MAX_STEP_ATTEMPTS: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// Router with scripted answers; counts how often it was consulted.
    struct ScriptedRouter {
        answers: std::sync::Mutex<Vec<EdgeJudgment>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedRouter {
        fn new(answers: Vec<EdgeJudgment>) -> Self {
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
            Ok(answers.remove(0))
        }
    }

    fn judgment(choice: EdgeChoice, confidence: f32) -> EdgeJudgment {
        EdgeJudgment { choice, confidence }
    }

    fn state(outcome: NodeOutcome, side_effect: SideEffect) -> EdgeState {
        EdgeState {
            goal: "ship the thing".to_owned(),
            current_node: "step_1".to_owned(),
            result_summary: "tool errored".to_owned(),
            outcome,
            allowed: EdgeState::allowed_for(outcome, side_effect),
        }
    }

    #[test]
    fn write_nodes_are_never_offered_retry() {
        for effect in [SideEffect::IdempotentWrite, SideEffect::NonIdempotentWrite] {
            let allowed = EdgeState::allowed_for(NodeOutcome::Failed { retriable: true }, effect);

            assert!(!allowed.contains(&EdgeChoice::Retry));
            assert!(allowed.contains(&EdgeChoice::EscalateModel));
        }

        let read_only = EdgeState::allowed_for(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
        );
        assert!(read_only.contains(&EdgeChoice::Retry));
    }

    #[test]
    fn hard_failures_are_not_retriable_even_for_reads() {
        let allowed = EdgeState::allowed_for(
            NodeOutcome::Failed { retriable: false },
            SideEffect::ReadOnly,
        );

        assert!(!allowed.contains(&EdgeChoice::Retry));
    }

    #[test]
    fn success_offers_exactly_continue_or_done() {
        let allowed = EdgeState::allowed_for(NodeOutcome::Succeeded, SideEffect::ReadOnly);

        assert_eq!(allowed.len(), 2);
        assert!(allowed.contains(&EdgeChoice::Continue));
        assert!(allowed.contains(&EdgeChoice::Done));
    }

    #[tokio::test]
    async fn singleton_sets_skip_the_router_entirely() {
        // A blocked read-only step has three edges, so force a singleton
        // by using success in a runtime that removed Done.
        let mut state = state(NodeOutcome::Succeeded, SideEffect::ReadOnly);
        state.allowed.remove(&EdgeChoice::Done);

        let router = ScriptedRouter::new(vec![]);
        let selector = EdgeSelector::new(&router);

        let judgment = selector.select(&state).await.unwrap();

        assert_eq!(judgment.choice, EdgeChoice::Continue);
        assert_eq!(judgment.confidence, 1.0);
        assert_eq!(router.calls(), 0);
    }

    #[tokio::test]
    async fn router_choice_within_allowed_set_is_honored() {
        let state = state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, 0.9)]);
        let selector = EdgeSelector::new(&router);

        let judgment = selector.select(&state).await.unwrap();

        assert_eq!(judgment.choice, EdgeChoice::Retry);
        assert_eq!(router.calls(), 1);
    }

    #[tokio::test]
    async fn low_confidence_falls_back_deterministically() {
        // After failure the safe fallback is EscalateModel.
        let failure_state = state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, 0.3)]);
        let selector = EdgeSelector::new(&router);

        let weak = selector.select(&failure_state).await.unwrap();

        assert_eq!(weak.choice, EdgeChoice::EscalateModel);

        // After success the safe fallback is Done (still in the set).
        let success_state = state(NodeOutcome::Succeeded, SideEffect::ReadOnly);
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Continue, 0.4)]);
        let selector = EdgeSelector::new(&router);

        let weak = selector.select(&success_state).await.unwrap();
        // EscalateModel is not allowed after success; Done is the safe one.
        assert_eq!(weak.choice, EdgeChoice::Done);
    }

    #[tokio::test]
    async fn out_of_set_choices_are_treated_as_weak() {
        let state = state(NodeOutcome::Succeeded, SideEffect::ReadOnly);
        // Router says Retry, but success never offers Retry.
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, 0.99)]);
        let selector = EdgeSelector::new(&router);

        let judgment = selector.select(&state).await.unwrap();

        assert_eq!(judgment.choice, EdgeChoice::Done);
    }

    #[tokio::test]
    async fn non_finite_confidence_is_never_trusted() {
        let state = state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::Retry, f32::NAN)]);
        let selector = EdgeSelector::new(&router);

        let judgment = selector.select(&state).await.unwrap();

        assert_eq!(judgment.choice, EdgeChoice::EscalateModel);
    }

    /// Multi-step fake workflow: a failed branch recovers through edge
    /// decisions without any generative model driving the loop.
    #[tokio::test]
    async fn fake_workflow_recovers_from_a_failed_branch() {
        use crate::tree::{CancelFlag, TreeExecutor};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct StaticTool;

        #[async_trait]
        impl crate::tool::Tool for StaticTool {
            fn metadata(&self) -> crate::tool::ToolMetadata {
                crate::tool::ToolMetadata {
                    id: "probe".to_owned(),
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
        registry.register(StaticTool);
        registry.register(WorkingTool);
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
        let executor = TreeExecutor::new(
            Arc::new(registry),
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

                let edge_state = EdgeState {
                    goal: "finish the fake workflow".to_owned(),
                    current_node: node.id().to_owned(),
                    result_summary: format!("{tool_name} attempt {attempts}"),
                    outcome,
                    allowed: EdgeState::allowed_for(outcome, SideEffect::ReadOnly),
                };

                let decision = selector.select(&edge_state).await.unwrap();
                transcript.push(format!("{}: {:?}", node.id(), decision.choice));

                match decision.choice {
                    EdgeChoice::Continue | EdgeChoice::AlternateBranch => break,
                    EdgeChoice::Done => return,
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
        // The router decided exactly once per executed attempt; no model
        // controlled the loop, only bounded edge choices.
        assert_eq!(router.calls(), 3);
    }

    #[tokio::test]
    async fn custom_floor_changes_the_trust_boundary() {
        let state = state(
            NodeOutcome::Failed { retriable: true },
            SideEffect::ReadOnly,
        );
        let router = ScriptedRouter::new(vec![judgment(EdgeChoice::AlternateBranch, 0.5)]);
        let selector = EdgeSelector::new(&router).with_min_confidence(0.5);

        let judgment = selector.select(&state).await.unwrap();

        assert_eq!(judgment.choice, EdgeChoice::AlternateBranch);
    }
}
