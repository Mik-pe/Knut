use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ArtifactRevision, CompletionRequirements, Evidence, KnutError, ToolRegistry};

pub const ASK_USER_TOOL_ID: &str = "harness_ask_user";

pub struct AskUserTool;

#[async_trait]
impl crate::Tool for AskUserTool {
    fn metadata(&self) -> crate::ToolMetadata {
        crate::ToolMetadata {
            id: ASK_USER_TOOL_ID.to_owned(),
            tool_version: "1".to_owned(),
            capability: "user".to_owned(),
            description: "Ask the user for missing information and wait for their answer."
                .to_owned(),
            input_schema: serde_json::json!({"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}),
            side_effect: crate::SideEffect::ReadOnly,
        }
    }

    async fn call(&self, input: Value) -> Result<Value, KnutError> {
        let question = input["question"].as_str().unwrap_or_default();
        if question.trim().is_empty() {
            return Err(KnutError::InvalidArguments {
                path: "question".to_owned(),
                reason: "question must not be empty".to_owned(),
            });
        }
        Ok(input)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRef {
    pub uri: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRecord {
    pub source: ResourceRef,
    pub description: String,
    pub content: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskOptions {
    pub instructions: String,
    pub tools: Option<Vec<String>>,
    pub requirements: CompletionRequirements,
    pub context: Vec<ContextRecord>,
}

impl TaskOptions {
    pub(crate) fn validate(&self, registry: &ToolRegistry) -> Result<(), KnutError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| KnutError::Tool(error.to_string()))?
            .len();
        if bytes > 128 * 1024 || self.context.len() > crate::MAX_CANDIDATES {
            return Err(KnutError::InvalidArguments {
                path: "options".to_owned(),
                reason: "task options exceed the 128 KiB or context-record limit".to_owned(),
            });
        }
        for record in &self.context {
            if record.source.uri.trim().is_empty() || record.source.revision.trim().is_empty() {
                return Err(KnutError::InvalidArguments {
                    path: "options.context".to_owned(),
                    reason: "context requires a resource URI and revision".to_owned(),
                });
            }
        }
        if let Some(ids) = &self.tools {
            registry.select(ids)?;
        }
        Ok(())
    }
}

pub struct ContextRead {
    pub capability: String,
    pub tool: String,
    pub input: Value,
}

pub trait ContextProvider: Send + Sync {
    fn instructions(&self) -> Result<String, KnutError>;
    fn reads(&self, prompt: &str) -> Vec<ContextRead>;
    fn record(&self, read: &ContextRead, output: &Value) -> Option<ContextRecord>;
}

#[async_trait]
pub trait CompletionMonitor: Send + Sync {
    fn requirements(&self) -> CompletionRequirements;
    fn verify_unchanged(&self) -> bool {
        true
    }
    fn current_revision(&self) -> Result<ArtifactRevision, KnutError>;
    async fn verify(&self, revision: &ArtifactRevision) -> Vec<Evidence>;
}

#[derive(Default)]
pub struct HarnessSetup {
    pub tools: ToolRegistry,
    pub instructions: String,
    pub completion: Option<Arc<dyn CompletionMonitor>>,
    pub context: Option<Arc<dyn ContextProvider>>,
}
