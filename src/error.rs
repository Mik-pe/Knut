use thiserror::Error;

#[derive(Debug, Error)]
pub enum KnutError {
    #[error("system one failed: {0}")]
    SystemOne(String),

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("tool failed: {0}")]
    Tool(String),

    #[error("blocked by deterministic rule: {reason}")]
    Blocked { reason: String },

    #[error("denied by policy: {reason}")]
    PolicyDenied { reason: String },

    #[error("approval required: {reason} (approval key: {approval_key})")]
    ApprovalRequired {
        reason: String,
        approval_key: String,
    },

    #[error("model escalation exhausted: {reason}")]
    ModelExhausted {
        reason: String,
        attempts: Vec<crate::model::ModelAttempt>,
    },
}
