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
}
