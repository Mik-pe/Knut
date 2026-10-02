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

/// Sharing a router across a session must not require owning it: the
/// blanket impl keeps `Arc<dyn SystemOne>` a `SystemOne`.
#[async_trait]
impl<T: SystemOne + ?Sized> SystemOne for std::sync::Arc<T> {
    async fn decide(&self, input: &DecisionInput) -> Result<Decision, KnutError> {
        (**self).decide(input).await
    }
}

#[async_trait]
impl SystemOne for StaticSystemOne {
    async fn decide(&self, _input: &DecisionInput) -> Result<Decision, KnutError> {
        Ok(self.decision.clone())
    }
}
