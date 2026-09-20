use crate::{
    Action, Decision, DecisionInput, ModelTier, RetrievalSource, Route, SystemOne, KnutError,
};

const DEFAULT_CONFIDENCE_FLOOR: f32 = 0.75;

pub struct Knut<S> {
    system_one: S,
    confidence_floor: f32,
}

impl<S> Knut<S>
where
    S: SystemOne,
{
    pub fn new(system_one: S) -> Self {
        Self {
            system_one,
            confidence_floor: DEFAULT_CONFIDENCE_FLOOR,
        }
    }

    pub fn with_confidence_floor(mut self, confidence_floor: f32) -> Self {
        self.confidence_floor = confidence_floor.clamp(0.0, 1.0);
        self
    }

    pub async fn next(
        &self,
        prompt: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Result<Action, KnutError> {
        let input = DecisionInput::new(prompt, capabilities);
        let decision = self.system_one.decide(&input).await?;
        Ok(self.action_for(decision))
    }

    pub fn action_for(&self, decision: Decision) -> Action {
        if !decision.confidence.is_finite() || decision.confidence < self.confidence_floor {
            return Action::Generate(ModelTier::Reasoner);
        }

        match decision.route {
            Route::Clarify => Action::AskUser,
            Route::Retrieve => Action::Retrieve(
                decision.retrieval.unwrap_or(RetrievalSource::Mixed),
            ),
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
    use super::*;
    use crate::{Risk, StaticSystemOne};

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
}
