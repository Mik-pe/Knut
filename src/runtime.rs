use serde::{Deserialize, Serialize};

use crate::{
    Action, Decision, DecisionInput, KnutError, ModelTier, RetrievalSource, Route, SystemOne,
    system_zero::{RuleVerdict, SystemZero},
};

const DEFAULT_CONFIDENCE_FLOOR: f32 = 0.75;

/// Which layer produced a routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    /// Deterministic fast path.
    SystemZero,
    /// Bounded-model judgment.
    SystemOne,
}

/// A full routing result: the decision, where it came from, and the action.
#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    pub source: DecisionSource,
    pub decision: Decision,
    pub action: Action,
}

pub struct Knut<S> {
    system_one: S,
    system_zero: SystemZero,
    confidence_floor: f32,
}

impl<S> Knut<S>
where
    S: SystemOne,
{
    pub fn new(system_one: S) -> Self {
        Self {
            system_one,
            system_zero: SystemZero::with_default_rules(),
            confidence_floor: DEFAULT_CONFIDENCE_FLOOR,
        }
    }

    pub fn with_confidence_floor(mut self, confidence_floor: f32) -> Self {
        self.confidence_floor = confidence_floor.clamp(0.0, 1.0);
        self
    }

    pub fn with_system_zero(mut self, system_zero: SystemZero) -> Self {
        self.system_zero = system_zero;
        self
    }

    /// Deterministic fast paths first; System One only when no rule fires.
    ///
    /// Decisions that come from System One are recorded in the System 0
    /// cache so an identical later input skips the model call.
    pub async fn route(&self, input: &DecisionInput) -> Result<Routed, KnutError> {
        if let Some(outcome) = self.system_zero.evaluate(input) {
            return match outcome.verdict {
                RuleVerdict::Decide(decision) => Ok(Routed {
                    source: DecisionSource::SystemZero,
                    action: self.action_for(decision.clone()),
                    decision,
                }),
                RuleVerdict::Blocked { reason } => Err(KnutError::Blocked { reason }),
                RuleVerdict::Pass => unreachable!("evaluate() never returns bare Pass"),
            };
        }

        let decision = self.system_one.decide(input).await?;
        self.system_zero.cache().store(input, &decision);

        Ok(Routed {
            source: DecisionSource::SystemOne,
            action: self.action_for(decision.clone()),
            decision,
        })
    }

    /// Route a fresh prompt with the given capabilities.
    pub async fn next(
        &self,
        prompt: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Result<Action, KnutError> {
        let input = DecisionInput::new(prompt, capabilities);
        Ok(self.route(&input).await?.action)
    }

    pub fn action_for(&self, decision: Decision) -> Action {
        if !decision.confidence.is_finite() || decision.confidence < self.confidence_floor {
            return Action::Generate(ModelTier::Reasoner);
        }

        match decision.route {
            Route::Clarify => Action::AskUser,
            Route::Retrieve => {
                Action::Retrieve(decision.retrieval.unwrap_or(RetrievalSource::Mixed))
            }
            Route::Act => decision
                .capability
                .map(|capability| Action::Tool { capability })
                .unwrap_or(Action::Generate(ModelTier::Reasoner)),
            Route::Generate => Action::Generate(decision.model_tier),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use serde_json::json;

    use crate::{DecisionInput, Risk, StaticSystemOne, SystemZero};

    use super::*;

    fn decision(route: Route, confidence: f32) -> Decision {
        Decision {
            route,
            confidence,
            retrieval: None,
            capability: None,
            model_tier: ModelTier::Fast,
            risk: Risk::Low,
            parallelizable: false,
        }
    }

    #[tokio::test]
    async fn low_confidence_escalates_to_reasoner() {
        let knut = Knut::new(StaticSystemOne::new(decision(Route::Generate, 0.4)));
        let action = knut.next("hello", vec![]).await.unwrap();

        assert_eq!(action, Action::Generate(ModelTier::Reasoner));
    }

    #[tokio::test]
    async fn confident_fast_generation_stays_fast() {
        let knut = Knut::new(StaticSystemOne::new(decision(Route::Generate, 0.95)));
        let action = knut.next("hello", vec![]).await.unwrap();

        assert_eq!(action, Action::Generate(ModelTier::Fast));
    }

    #[tokio::test]
    async fn act_without_capability_escalates() {
        let knut = Knut::new(StaticSystemOne::new(decision(Route::Act, 0.95)));
        let action = knut.next("do it", vec!["files".into()]).await.unwrap();

        assert_eq!(action, Action::Generate(ModelTier::Reasoner));
    }

    /// Counts how often System One is actually consulted.
    struct CountingSystemOne {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SystemOne for CountingSystemOne {
        async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Decision {
                route: Route::Retrieve,
                confidence: 0.9,
                retrieval: Some(crate::RetrievalSource::Files),
                capability: None,
                model_tier: ModelTier::Fast,
                risk: Risk::Low,
                parallelizable: false,
            })
        }
    }

    fn counting_knut() -> (Knut<CountingSystemOne>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Knut::new(CountingSystemOne {
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }

    #[tokio::test]
    async fn system_zero_reports_source() {
        let knut = Knut::new(StaticSystemOne::new(decision(Route::Generate, 0.95)));
        let routed = knut
            .route(&DecisionInput::new("weather", vec!["weather".into()]))
            .await
            .unwrap();

        assert_eq!(routed.source, DecisionSource::SystemZero);
        assert_eq!(
            routed.action,
            Action::Tool {
                capability: "weather".into()
            }
        );
        assert_eq!(routed.decision.route, Route::Act);
    }

    #[tokio::test]
    async fn system_one_decision_is_cached() {
        let (knut, calls) = counting_knut();
        let input = DecisionInput::new("please find files", vec![]);

        let first = knut.route(&input).await.unwrap();
        let second = knut.route(&input).await.unwrap();

        assert_eq!(first.source, DecisionSource::SystemOne);
        assert_eq!(second.source, DecisionSource::SystemZero);
        assert_eq!(first.action, second.action);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unavailable_capability_is_blocked() {
        let knut = Knut::new(StaticSystemOne::new(decision(Route::Act, 0.95)));
        let input = DecisionInput::new("weather", vec!["weather".into()])
            .with_state(json!({ "unavailable_capabilities": ["weather"] }));

        let err = knut.route(&input).await.unwrap_err();
        assert!(err.to_string().contains("unavailable"));
    }

    #[tokio::test]
    async fn empty_prompt_is_blocked() {
        let knut = Knut::new(StaticSystemOne::new(decision(Route::Generate, 0.95)));
        let err = knut.next("   ", vec![]).await.unwrap_err();

        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn non_matching_prompt_falls_through_to_system_one() {
        let (knut, calls) = counting_knut();
        let action = knut
            .next("what is the weather", vec!["weather".into()])
            .await
            .unwrap();

        assert_eq!(action, Action::Retrieve(crate::RetrievalSource::Files));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn system_zero_can_be_disabled() {
        let zero = SystemZero::empty();
        let input = DecisionInput::new("weather", vec!["weather".into()]);

        assert!(zero.evaluate(&input).is_none());
    }
}
