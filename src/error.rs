use thiserror::Error;

/// Short, secret-free display form of an identity (fingerprint, key).
///
/// Identities embed arguments and can carry user content; error and
/// audit output shows only a bounded prefix so raw secrets never ride
/// into logs or approval prompts.
pub fn redacted(identity: &str) -> &str {
    let end = identity
        .char_indices()
        .nth(24)
        .map(|(i, _)| i)
        .unwrap_or(identity.len());
    &identity[..end]
}

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

    #[error("approval required: {reason} (approval key: {approval_key})", approval_key = redacted(approval_key.as_str()))]
    ApprovalRequired {
        reason: String,
        approval_key: String,
    },

    #[error("model escalation exhausted: {reason}")]
    ModelExhausted {
        reason: String,
        attempts: Vec<crate::model::ModelAttempt>,
    },

    #[error("plan rejected: {errors:?}")]
    PlanRejected { errors: Vec<String> },

    #[error("invalid arguments {path}: {reason}")]
    InvalidArguments { path: String, reason: String },

    #[error(
        "unknown effect for {}: the previous attempt may have partially applied; reconcile or ask, do not retry",
        redacted(fingerprint.as_str())
    )]
    UnknownEffect { fingerprint: String },

    #[error(
        "previous attempt failed for {}: {detail}",
        redacted(fingerprint.as_str())
    )]
    PreviousFailure { fingerprint: String, detail: String },

    #[error("execution already reserved for {}", redacted(fingerprint.as_str()))]
    ExecutionReserved { fingerprint: String },

    #[error(
        "replay key {key:?} is already bound to a different execution ({}); use a fresh key",
        redacted(expected.as_str())
    )]
    ReplayKeyConflict { key: String, expected: String },
}
