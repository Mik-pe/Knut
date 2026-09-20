use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::tool::{SideEffect, ToolRegistry};
use crate::{KnutError, Risk};

/// What policy concludes about a proposed execution.
///
/// Policy is ordinary Rust code. Models may propose; this decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Allowed,
    Denied { reason: String },
    ApprovalRequired { reason: String },
}

/// A proposed tool execution.
///
/// Note there is no side-effect field: the class is read from the tool's
/// registered metadata, so nothing on the request can misrepresent it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionRequest {
    pub capability: String,
    pub tool_id: String,
    pub input: Value,
    /// Replay identity for writes; required for non-idempotent ones.
    pub idempotency_key: Option<String>,
    /// Route-level risk judgment. Context for policy, never authority.
    pub risk: Risk,
}

/// Runtime policy hook: allow, deny, or demand approval.
pub trait Policy: Send + Sync {
    fn check(&self, request: &ExecutionRequest, side_effect: SideEffect) -> PolicyVerdict;
}

/// Declarative default policy: per side-effect class, allow / deny /
/// require approval. Unlisted classes are denied (closed by default).
#[derive(Debug, Clone, Default)]
pub struct SideEffectPolicy {
    pub allow: BTreeSet<SideEffect>,
    pub require_approval: BTreeSet<SideEffect>,
}

impl SideEffectPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow(mut self, effect: SideEffect) -> Self {
        self.allow.insert(effect);
        self
    }

    pub fn require_approval(mut self, effect: SideEffect) -> Self {
        self.require_approval.insert(effect);
        self
    }
}

impl Policy for SideEffectPolicy {
    fn check(&self, _request: &ExecutionRequest, side_effect: SideEffect) -> PolicyVerdict {
        if self.allow.contains(&side_effect) {
            PolicyVerdict::Allowed
        } else if self.require_approval.contains(&side_effect) {
            PolicyVerdict::ApprovalRequired {
                reason: format!("{side_effect:?} requires approval"),
            }
        } else {
            PolicyVerdict::Denied {
                reason: format!("{side_effect:?} is not permitted by policy"),
            }
        }
    }
}

/// Approvals are granted out of band (human, scheduled job) and recorded
/// here. Nothing the model controls can write to this ledger.
#[derive(Default)]
pub struct ApprovalLedger {
    approved: Mutex<BTreeSet<String>>,
}

impl ApprovalLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant approval for one execution key.
    pub fn grant(&self, key: &str) {
        self.approved
            .lock()
            .expect("approval ledger poisoned")
            .insert(key.to_owned());
    }

    pub fn revoke(&self, key: &str) {
        self.approved
            .lock()
            .expect("approval ledger poisoned")
            .remove(key);
    }

    pub fn is_approved(&self, key: &str) -> bool {
        self.approved
            .lock()
            .expect("approval ledger poisoned")
            .contains(key)
    }
}

/// Replay journal for writes: same idempotency key, recorded outcome.
#[derive(Default)]
pub struct WriteJournal {
    entries: Mutex<HashMap<String, Value>>,
}

impl WriteJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, key: &str, output: &Value) {
        self.entries
            .lock()
            .expect("write journal poisoned")
            .insert(key.to_owned(), output.clone());
    }

    pub fn replay(&self, key: &str) -> Option<Value> {
        self.entries
            .lock()
            .expect("write journal poisoned")
            .get(key)
            .cloned()
    }
}

/// What one gated execution produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecOutcome {
    pub output: Value,
    /// True when the outcome came from the journal, not a fresh call.
    pub replayed: bool,
}

/// The single enforcement point for tool execution.
///
/// Every path to a tool goes through here: unknown tools are denied,
/// side-effect class is taken from the registry (never the request),
/// denied and unapproved executions fail with explicit reasons, and
/// write replays return the journaled outcome instead of re-executing.
pub struct ExecutionGate {
    policy: Box<dyn Policy>,
    approvals: ApprovalLedger,
    journal: WriteJournal,
}

impl ExecutionGate {
    pub fn new(policy: impl Policy + 'static) -> Self {
        Self {
            policy: Box::new(policy),
            approvals: ApprovalLedger::new(),
            journal: WriteJournal::new(),
        }
    }

    /// The ledger key for a request; host code grants against this.
    pub fn approval_key(request: &ExecutionRequest) -> String {
        format!(
            "{}:{}:{}",
            request.capability,
            request.tool_id,
            request.idempotency_key.as_deref().unwrap_or("")
        )
    }

    pub fn approvals(&self) -> &ApprovalLedger {
        &self.approvals
    }

    pub fn journal(&self) -> &WriteJournal {
        &self.journal
    }

    pub async fn execute(
        &self,
        registry: &ToolRegistry,
        request: ExecutionRequest,
    ) -> Result<ExecOutcome, KnutError> {
        // Unknown tools cannot be smuggled through, whatever the route.
        let tool = registry
            .find_exact(&request.capability, &request.tool_id)
            .map_err(|_| KnutError::PolicyDenied {
                reason: format!(
                    "tool {:?} is not available in capability {:?}",
                    request.tool_id, request.capability
                ),
            })?;

        // The registry declares the side-effect class; requests cannot.
        let side_effect = tool.metadata().side_effect;

        match self.policy.check(&request, side_effect) {
            PolicyVerdict::Denied { reason } => return Err(KnutError::PolicyDenied { reason }),
            PolicyVerdict::ApprovalRequired { reason } => {
                let key = Self::approval_key(&request);
                if !self.approvals.is_approved(&key) {
                    return Err(KnutError::ApprovalRequired {
                        reason,
                        approval_key: key,
                    });
                }
            }
            PolicyVerdict::Allowed => {}
        }

        // Retry/replay rules: reads run fresh; writes with a key replay
        // from the journal; non-idempotent writes demand a key.
        let idempotency_key = match side_effect {
            SideEffect::ReadOnly => None,
            SideEffect::IdempotentWrite => request.idempotency_key.clone(),
            SideEffect::NonIdempotentWrite => match request.idempotency_key {
                Some(key) => Some(key),
                None => {
                    return Err(KnutError::PolicyDenied {
                        reason: format!(
                            "tool {:?} is non-idempotent and requires an idempotency key",
                            request.tool_id
                        ),
                    });
                }
            },
        };

        if let Some(key) = idempotency_key.as_deref()
            && let Some(output) = self.journal.replay(key)
        {
            return Ok(ExecOutcome {
                output,
                replayed: true,
            });
        }

        let output = tool.call(request.input.clone()).await?;

        if let Some(key) = idempotency_key.as_deref() {
            self.journal.record(key, &output);
        }

        Ok(ExecOutcome {
            output,
            replayed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::tool::{Tool, ToolMetadata};

    struct CountingTool {
        id: &'static str,
        capability: &'static str,
        side_effect: SideEffect,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                capability: self.capability.to_owned(),
                description: "counting test tool".to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: self.side_effect,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "call": n }))
        }
    }

    fn read_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        registry.register(CountingTool {
            id: "read",
            capability: "files",
            side_effect: SideEffect::ReadOnly,
            calls: AtomicUsize::new(0),
        });
        registry
    }

    fn write_registry(non_idempotent: bool) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        registry.register(CountingTool {
            id: "write",
            capability: "files",
            side_effect: if non_idempotent {
                SideEffect::NonIdempotentWrite
            } else {
                SideEffect::IdempotentWrite
            },
            calls: AtomicUsize::new(0),
        });
        registry
    }

    fn request(capability: &str, tool: &str) -> ExecutionRequest {
        ExecutionRequest {
            capability: capability.to_owned(),
            tool_id: tool.to_owned(),
            input: json!({}),
            idempotency_key: None,
            risk: Risk::Low,
        }
    }

    #[tokio::test]
    async fn reads_execute_and_are_never_journaled() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::ReadOnly));
        let registry = read_registry();

        let first = gate
            .execute(&registry, request("files", "read"))
            .await
            .unwrap();
        let second = gate
            .execute(&registry, request("files", "read"))
            .await
            .unwrap();

        assert!(!first.replayed && !second.replayed);
        assert_eq!(first.output, json!({ "call": 0 }));
        assert_eq!(second.output, json!({ "call": 1 }));
    }

    #[tokio::test]
    async fn denied_reads_fail_with_explicit_reason() {
        let gate = ExecutionGate::new(SideEffectPolicy::new()); // denies everything
        let registry = read_registry();

        let err = gate
            .execute(&registry, request("files", "read"))
            .await
            .unwrap_err();

        match err {
            KnutError::PolicyDenied { reason } => {
                assert!(reason.contains("not permitted"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn approval_required_until_the_host_grants_it() {
        let gate = ExecutionGate::new(
            SideEffectPolicy::new().require_approval(SideEffect::IdempotentWrite),
        );
        let registry = write_registry(false);

        let mut req = request("files", "write");
        req.idempotency_key = Some("w-1".to_owned());

        let err = gate.execute(&registry, req.clone()).await.unwrap_err();
        let approval_key = match err {
            KnutError::ApprovalRequired {
                reason,
                approval_key,
            } => {
                assert!(reason.contains("approval"));
                approval_key
            }
            other => panic!("unexpected error: {other:?}"),
        };

        // Host grants out of band; the same request now succeeds.
        gate.approvals().grant(&approval_key);
        let outcome = gate.execute(&registry, req).await.unwrap();
        assert!(!outcome.replayed);
    }

    #[tokio::test]
    async fn idempotent_writes_replay_from_the_journal() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::IdempotentWrite));
        let registry = write_registry(false);

        let mut req = request("files", "write");
        req.idempotency_key = Some("w-2".to_owned());

        let first = gate.execute(&registry, req.clone()).await.unwrap();
        let replay = gate.execute(&registry, req).await.unwrap();

        assert!(!first.replayed);
        assert!(replay.replayed);
        assert_eq!(first.output, replay.output);
    }

    #[tokio::test]
    async fn non_idempotent_writes_demand_a_key() {
        let gate =
            ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::NonIdempotentWrite));
        let registry = write_registry(true);

        let err = gate
            .execute(&registry, request("files", "write"))
            .await
            .unwrap_err();

        match err {
            KnutError::PolicyDenied { reason } => {
                assert!(reason.contains("idempotency key"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn different_keys_execute_fresh_but_same_key_replays() {
        let gate =
            ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::NonIdempotentWrite));
        let registry = write_registry(true);

        let mut first = request("files", "write");
        first.idempotency_key = Some("a".to_owned());
        let mut second = request("files", "write");
        second.idempotency_key = Some("b".to_owned());

        let out_a = gate.execute(&registry, first).await.unwrap();
        let out_b = gate.execute(&registry, second.clone()).await.unwrap();
        let out_b_again = gate.execute(&registry, second).await.unwrap();

        assert_ne!(out_a.output, out_b.output);
        assert!(out_b_again.replayed);
        assert_eq!(out_b.output, out_b_again.output);
    }

    /// The bypass invariant: denial cannot be routed around.
    #[tokio::test]
    async fn denial_survives_route_changes() {
        struct DenyTransfers;

        impl Policy for DenyTransfers {
            fn check(&self, request: &ExecutionRequest, _s: SideEffect) -> PolicyVerdict {
                if request.tool_id == "transfer" {
                    PolicyVerdict::Denied {
                        reason: "transfers are disabled".to_owned(),
                    }
                } else {
                    PolicyVerdict::Allowed
                }
            }
        }

        let gate = ExecutionGate::new(DenyTransfers);

        let mut registry = ToolRegistry::default();
        registry.register(CountingTool {
            id: "transfer",
            capability: "payments",
            side_effect: SideEffect::NonIdempotentWrite,
            calls: AtomicUsize::new(0),
        });
        registry.register(CountingTool {
            id: "transfer",
            capability: "admin",
            side_effect: SideEffect::ReadOnly,
            calls: AtomicUsize::new(0),
        });
        registry.register(CountingTool {
            id: "other",
            capability: "payments",
            side_effect: SideEffect::NonIdempotentWrite,
            calls: AtomicUsize::new(0),
        });

        // Route 1: the obvious capability.
        let err = gate
            .execute(&registry, request("payments", "transfer"))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));

        // Route 2: same tool under a different capability (read-only!).
        let err = gate
            .execute(&registry, request("admin", "transfer"))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));

        // Route 3: unknown tool under any capability is denied, not 404'd
        // into an "escalate and try elsewhere" path.
        let err = gate
            .execute(&registry, request("ghost", "transfer"))
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));

        // Route 4: policy allows a different tool in the same capability.
        let ok = gate
            .execute(
                &registry,
                ExecutionRequest {
                    idempotency_key: Some("ok-1".to_owned()),
                    ..request("payments", "other")
                },
            )
            .await;
        assert!(ok.is_ok());
    }

    /// The class cannot be misrepresented: the registry's declaration
    /// decides, whatever the request claims via risk or key.
    #[tokio::test]
    async fn side_effect_class_comes_from_the_registry_not_the_request() {
        // Policy allows only reads. The request cannot pretend the
        // non-idempotent write is one.
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::ReadOnly));
        let registry = write_registry(true);

        let mut req = request("files", "write");
        req.risk = Risk::Low; // "trust me"
        req.idempotency_key = Some("sneaky".to_owned());

        let err = gate.execute(&registry, req).await.unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));
    }

    #[test]
    fn verdicts_serialize_for_traces() {
        let allowed = serde_json::to_string(&PolicyVerdict::Allowed).unwrap();
        assert_eq!(allowed, "\"allowed\"");

        let denied = serde_json::to_string(&PolicyVerdict::Denied {
            reason: "no".to_owned(),
        })
        .unwrap();
        assert!(denied.contains("\"denied\""));
    }
}
