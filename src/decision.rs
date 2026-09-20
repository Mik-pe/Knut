use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Clarify,
    Retrieve,
    Act,
    Generate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalSource {
    Files,
    Memory,
    Web,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Fast,
    Standard,
    Reasoner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub route: Route,
    pub confidence: f32,
    pub retrieval: Option<RetrievalSource>,
    pub capability: Option<String>,
    pub model_tier: ModelTier,
    pub risk: Risk,
    pub parallelizable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionInput {
    pub prompt: String,
    pub capabilities: Vec<String>,
    pub state: Value,
}

impl DecisionInput {
    pub fn new(prompt: impl Into<String>, capabilities: Vec<String>) -> Self {
        Self {
            prompt: prompt.into(),
            capabilities,
            state: Value::Null,
        }
    }

    pub fn with_state(mut self, state: Value) -> Self {
        self.state = state;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    AskUser,
    Retrieve(RetrievalSource),
    Tool { capability: String },
    Generate(ModelTier),
}
