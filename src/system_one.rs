use async_trait::async_trait;

use crate::{Decision, DecisionInput, KnutError};

#[async_trait]
pub trait SystemOne: Send + Sync {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError>;
}

#[derive(Debug, Clone)]
pub struct StaticSystemOne {
    decision: Decision,
}

impl StaticSystemOne {
    pub fn new(decision: Decision) -> Self {
        Self { decision }
    }
}

#[async_trait]
impl SystemOne for StaticSystemOne {
    async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
        Ok(self.decision.clone())
    }
}
