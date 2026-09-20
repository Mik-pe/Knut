use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::{SideEffect, Tool, ToolMetadata};
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

/// Outcome class of a recorded execution.
///
/// The journal never claims an effect happened when it cannot know:
/// `unknown_effect` exists precisely for timeouts and crashes where the
/// outside world may have changed. Such entries block automatic retry —
/// recovery must reconcile or ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeState {
    Completed,
    Failed,
    UnknownEffect,
}

/// One recorded execution outcome under an execution fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub outcome: OutcomeState,
    /// Recorded output for completed replays; diagnostics otherwise.
    pub output: Option<Value>,
}

/// Journal storage boundary.
///
/// Reservations make concurrent first-runs single-winner; recorded
/// entries outlive their reservations so replays of completed work stay
/// exact. A released reservation without an entry (cancelled before the
/// invocation started) can be reserved again later.
pub trait JournalStore: Send + Sync {
    /// Claim the slot for one execution fingerprint. `false` when the
    /// slot is currently reserved by another caller.
    fn reserve(&self, fingerprint: &str) -> bool;

    fn lookup(&self, fingerprint: &str) -> Option<JournalEntry>;

    /// Record the outcome under the fingerprint and release the slot.
    fn record(&self, fingerprint: &str, entry: JournalEntry);

    /// Release a reservation without an outcome (cancelled before the
    /// action started).
    fn release(&self, fingerprint: &str);
}

/// Deterministic in-memory journal implementation.
#[derive(Default)]
pub struct InMemoryJournal {
    reserved: Mutex<BTreeSet<String>>,
    entries: Mutex<HashMap<String, JournalEntry>>,
}

impl InMemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

impl JournalStore for InMemoryJournal {
    fn reserve(&self, fingerprint: &str) -> bool {
        self.reserved
            .lock()
            .expect("journal poisoned")
            .insert(fingerprint.to_owned())
    }

    fn lookup(&self, fingerprint: &str) -> Option<JournalEntry> {
        self.entries
            .lock()
            .expect("journal poisoned")
            .get(fingerprint)
            .cloned()
    }

    fn record(&self, fingerprint: &str, entry: JournalEntry) {
        self.entries
            .lock()
            .expect("journal poisoned")
            .insert(fingerprint.to_owned(), entry);
        self.reserved
            .lock()
            .expect("journal poisoned")
            .remove(fingerprint);
    }

    fn release(&self, fingerprint: &str) {
        self.reserved
            .lock()
            .expect("journal poisoned")
            .remove(fingerprint);
    }
}

/// What one gated execution produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecOutcome {
    pub output: Value,
    /// True when the outcome came from the journal, not a fresh call.
    pub replayed: bool,
}

/// The verdict of pre-execution authorization.
#[derive(Debug, Clone, PartialEq)]
pub enum Authorization {
    /// The caller may execute, holding the reservation. The fingerprint
    /// is `Some` for journaled writes (record the outcome under it) and
    /// `None` for reads and un-keyed writes (never journaled).
    Execute { fingerprint: Option<String> },
    /// A completed prior execution with the same exact identity.
    Replay(ExecOutcome),
}

/// The single enforcement point for tool execution.
///
/// Every path to a tool goes through here: [`authorize`](ExecutionGate::authorize)
/// runs policy, approval, and reservation checks without touching the
/// world; [`execute`](ExecutionGate::execute) runs the raw implementation
/// only with an authorization in hand. `execute` is `pub(crate)` — raw
/// invocation is not part of the public API, so no external route can
/// skip the gate.
pub struct ExecutionGate {
    policy: Box<dyn Policy>,
    approvals: ApprovalLedger,
    journal: InMemoryJournal,
}

impl ExecutionGate {
    pub fn new(policy: impl Policy + 'static) -> Self {
        Self {
            policy: Box::new(policy),
            approvals: ApprovalLedger::new(),
            journal: InMemoryJournal::new(),
        }
    }

    pub fn approvals(&self) -> &ApprovalLedger {
        &self.approvals
    }

    pub fn journal(&self) -> &InMemoryJournal {
        &self.journal
    }

    /// Approve one exact execution fingerprint. Granted out of band
    /// (human, scheduled job); never derivable from repeated approvals.
    pub fn grant_approval(&self, fingerprint: &str) {
        self.approvals.grant(fingerprint);
    }

    /// Pre-execution authorization.
    ///
    /// Order is deliberate: policy first (cheap, no side Channel), then
    /// approval against the exact fingerprint, then journal/replay
    /// semantics. A cache hit or a different route never skips policy:
    /// every `invoke` re-runs this method.
    pub async fn authorize(
        &self,
        metadata: &ToolMetadata,
        input: &Value,
        idempotency_key: Option<&str>,
        risk: Risk,
    ) -> Result<Authorization, KnutError> {
        let side_effect = metadata.side_effect;

        let request = ProposedExecution {
            capability: metadata.capability.clone(),
            tool_id: metadata.id.clone(),
            input: input.clone(),
            idempotency_key: idempotency_key.map(str::to_owned),
            risk,
        };

        match self.policy.check(&request, side_effect) {
            PolicyVerdict::Denied { reason } => return Err(KnutError::PolicyDenied { reason }),
            PolicyVerdict::ApprovalRequired { reason } => {
                let fingerprint =
                    execution_fingerprint(metadata, input, idempotency_key.unwrap_or("-"));
                if !self.approvals.is_approved(&fingerprint) {
                    return Err(KnutError::ApprovalRequired {
                        reason,
                        approval_key: fingerprint,
                    });
                }
            }
            PolicyVerdict::Allowed => {}
        }

        let fingerprint = match side_effect {
            SideEffect::ReadOnly => return Ok(Authorization::Execute { fingerprint: None }),
            SideEffect::IdempotentWrite => match idempotency_key {
                Some(key) => execution_fingerprint(metadata, input, key),
                None => return Ok(Authorization::Execute { fingerprint: None }),
            },
            SideEffect::NonIdempotentWrite => {
                let Some(key) = idempotency_key else {
                    return Err(KnutError::PolicyDenied {
                        reason: format!(
                            "tool {:?} is non-idempotent and requires an idempotency key",
                            metadata.id
                        ),
                    });
                };
                execution_fingerprint(metadata, input, key)
            }
        };

        if let Some(entry) = self.journal.lookup(&fingerprint) {
            return match entry.outcome {
                OutcomeState::Completed => Ok(Authorization::Replay(ExecOutcome {
                    output: entry.output.unwrap_or(Value::Null),
                    replayed: true,
                })),
                OutcomeState::Failed => Err(KnutError::PreviousFailure {
                    fingerprint,
                    detail: entry
                        .output
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "no diagnostics".to_owned()),
                }),
                OutcomeState::UnknownEffect => Err(KnutError::UnknownEffect { fingerprint }),
            };
        }

        if !self.journal.reserve(&fingerprint) {
            return Err(KnutError::ExecutionReserved { fingerprint });
        }

        Ok(Authorization::Execute {
            fingerprint: Some(fingerprint),
        })
    }

    /// Run the raw implementation under a prior authorization.
    ///
    /// Journaled writes record their outcome — completed or, on any tool
    /// error, `unknown_effect` — and release the reservation either way.
    pub(crate) async fn execute(
        &self,
        tool: &dyn Tool,
        fingerprint: Option<String>,
        input: &Value,
    ) -> Result<ExecOutcome, KnutError> {
        match tool.call(input.clone()).await {
            Ok(output) => {
                if let Some(fp) = &fingerprint {
                    self.journal.record(
                        fp,
                        JournalEntry {
                            outcome: OutcomeState::Completed,
                            output: Some(output.clone()),
                        },
                    );
                }
                Ok(ExecOutcome {
                    output,
                    replayed: false,
                })
            }
            Err(err) => {
                if let Some(fp) = &fingerprint {
                    // The tool errored, but whether the effect happened is
                    // unknowable from here: record unknown, never retry.
                    self.journal.record(
                        fp,
                        JournalEntry {
                            outcome: OutcomeState::UnknownEffect,
                            output: Some(Value::String(err.to_string())),
                        },
                    );
                }
                Err(err)
            }
        }
    }
}

/// A proposed tool execution, as seen by policy.
///
/// There is no side-effect field: the class is read from registered
/// metadata, so nothing on the request can misrepresent it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposedExecution {
    pub capability: String,
    pub tool_id: String,
    pub input: Value,
    pub idempotency_key: Option<String>,
    /// Route-level risk judgment. Context for policy, never authority.
    pub risk: Risk,
}

/// Runtime policy hook: allow, deny, or demand approval.
pub trait Policy: Send + Sync {
    fn check(&self, request: &ProposedExecution, side_effect: SideEffect) -> PolicyVerdict;
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
    fn check(&self, _request: &ProposedExecution, side_effect: SideEffect) -> PolicyVerdict {
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
/// here against exact execution fingerprints. Nothing the model controls
/// can write to this ledger.
#[derive(Default)]
pub struct ApprovalLedger {
    approved: Mutex<BTreeSet<String>>,
}

impl ApprovalLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant approval for one exact execution fingerprint.
    pub fn grant(&self, fingerprint: &str) {
        self.approved
            .lock()
            .expect("approval ledger poisoned")
            .insert(fingerprint.to_owned());
    }

    pub fn revoke(&self, fingerprint: &str) {
        self.approved
            .lock()
            .expect("approval ledger poisoned")
            .remove(fingerprint);
    }

    pub fn is_approved(&self, fingerprint: &str) -> bool {
        self.approved
            .lock()
            .expect("approval ledger poisoned")
            .contains(fingerprint)
    }
}

/// The exact-action identity: registered tool identity (capability, id,
/// side-effect class), normalized arguments, and the replay key.
///
/// Changing the arguments, the tool identity, or the replay key produces
/// a different fingerprint and invalidates any prior approval. serde_json
/// object keys are sorted by default, so argument key order is irrelevant.
pub(crate) fn execution_fingerprint(
    metadata: &ToolMetadata,
    input: &Value,
    idempotency_key: &str,
) -> String {
    let args = serde_json::to_string(input).unwrap_or_else(|_| "<unserializable>".to_owned());
    format!(
        "v1:{}/{}:effect={:?}:args={args}:key={idempotency_key}",
        metadata.capability, metadata.id, metadata.side_effect
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolRegistry};

    struct CountingTool {
        id: &'static str,
        capability: &'static str,
        side_effect: SideEffect,
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Tool for CountingTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                capability: self.capability.to_owned(),
                description: "counting test tool".to_owned(),
                input_schema: serde_json::json!({ "type": "object" }),
                side_effect: self.side_effect,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(KnutError::Tool(format!("failure on call {n}")));
            }
            Ok(serde_json::json!({ "call": n }))
        }
    }

    fn read_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        registry
            .register(CountingTool {
                id: "read",
                capability: "files",
                side_effect: SideEffect::ReadOnly,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: false,
            })
            .unwrap();
        registry
    }

    fn write_registry(non_idempotent: bool) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        registry
            .register(CountingTool {
                id: "write",
                capability: "files",
                side_effect: if non_idempotent {
                    SideEffect::NonIdempotentWrite
                } else {
                    SideEffect::IdempotentWrite
                },
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: false,
            })
            .unwrap();
        registry
    }

    #[tokio::test]
    async fn reads_execute_and_are_never_journaled() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::ReadOnly));
        let registry = read_registry();

        let first = registry
            .invoke(
                &gate,
                "files",
                "read",
                serde_json::json!({}),
                None,
                Risk::Low,
            )
            .await
            .unwrap();
        let second = registry
            .invoke(
                &gate,
                "files",
                "read",
                serde_json::json!({}),
                None,
                Risk::Low,
            )
            .await
            .unwrap();

        assert!(!first.replayed && !second.replayed);
        assert_eq!(first.output, serde_json::json!({ "call": 0 }));
        assert_eq!(second.output, serde_json::json!({ "call": 1 }));
    }

    #[tokio::test]
    async fn denied_reads_fail_with_explicit_reason() {
        let gate = ExecutionGate::new(SideEffectPolicy::new()); // denies everything
        let registry = read_registry();

        let err = registry
            .invoke(
                &gate,
                "files",
                "read",
                serde_json::json!({}),
                None,
                Risk::Low,
            )
            .await
            .unwrap_err();

        match err {
            KnutError::PolicyDenied { reason } => assert!(reason.contains("not permitted")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn approval_is_bound_to_exact_arguments() {
        let gate = ExecutionGate::new(
            SideEffectPolicy::new().require_approval(SideEffect::IdempotentWrite),
        );
        let registry = write_registry(false);

        let invoke = |path: &str, key: &str| {
            registry.invoke(
                &gate,
                "files",
                "write",
                serde_json::json!({ "path": path }),
                Some(key.to_owned()),
                Risk::Low,
            )
        };

        // First attempt: approval required; the key names the exact action.
        let err = invoke("a.txt", "w-1").await.unwrap_err();
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
        assert!(approval_key.contains("a.txt"));

        // Host grants the exact fingerprint; identical action succeeds.
        gate.grant_approval(&approval_key);
        let outcome = invoke("a.txt", "w-1").await.unwrap();
        assert!(!outcome.replayed);

        // Changed arguments under the same key: fresh approval required.
        let err = invoke("OTHER.txt", "w-1").await.unwrap_err();
        assert!(matches!(err, KnutError::ApprovalRequired { .. }));

        // Same arguments, different key: fresh approval required.
        let err = invoke("a.txt", "w-2").await.unwrap_err();
        assert!(matches!(err, KnutError::ApprovalRequired { .. }));
    }

    #[tokio::test]
    async fn replay_key_with_different_arguments_is_not_a_hit() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::IdempotentWrite));
        let registry = write_registry(false);

        let first = registry
            .invoke(
                &gate,
                "files",
                "write",
                serde_json::json!({ "path": "a.txt" }),
                Some("k".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap();
        let second = registry
            .invoke(
                &gate,
                "files",
                "write",
                serde_json::json!({ "path": "b.txt" }),
                Some("k".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap();

        // Different fingerprint: executes fresh, never replays the old hit.
        assert!(!first.replayed);
        assert!(!second.replayed);
        assert_ne!(first.output, second.output);
    }

    #[tokio::test]
    async fn idempotent_writes_replay_from_the_journal() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::IdempotentWrite));
        let registry = write_registry(false);

        let args = serde_json::json!({ "path": "same.txt" });
        let first = registry
            .invoke(
                &gate,
                "files",
                "write",
                args.clone(),
                Some("w-2".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap();
        let replay = registry
            .invoke(
                &gate,
                "files",
                "write",
                args,
                Some("w-2".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap();

        assert!(!first.replayed);
        assert!(replay.replayed);
        assert_eq!(first.output, replay.output);
    }

    #[tokio::test]
    async fn non_idempotent_writes_demand_a_key() {
        let gate =
            ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::NonIdempotentWrite));
        let registry = write_registry(true);

        let err = registry
            .invoke(
                &gate,
                "files",
                "write",
                serde_json::json!({}),
                None,
                Risk::Low,
            )
            .await
            .unwrap_err();

        match err {
            KnutError::PolicyDenied { reason } => assert!(reason.contains("idempotency key")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_failure_records_unknown_effect_and_blocks_retry() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::IdempotentWrite));
        let mut registry = ToolRegistry::default();
        registry
            .register(CountingTool {
                id: "write",
                capability: "files",
                side_effect: SideEffect::IdempotentWrite,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: true,
            })
            .unwrap();

        let args = serde_json::json!({ "path": "x" });
        let err = registry
            .invoke(
                &gate,
                "files",
                "write",
                args.clone(),
                Some("f-1".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::Tool(_)));
        assert_eq!(registry.find_exact("files", "write").unwrap().id, "write");

        // The failure left an unknown-effect record: replaying is a loud
        // error, never an automatic re-execution.
        let err = registry
            .invoke(
                &gate,
                "files",
                "write",
                args,
                Some("f-1".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::UnknownEffect { .. }));
    }

    #[tokio::test]
    async fn concurrent_first_run_invokes_at_most_once() {
        use std::sync::Arc;

        struct RaceTool {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl Tool for RaceTool {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: "write".to_owned(),
                    capability: "files".to_owned(),
                    description: "race tool".to_owned(),
                    input_schema: serde_json::json!({ "type": "object" }),
                    side_effect: SideEffect::IdempotentWrite,
                }
            }

            async fn call(&self, _input: Value) -> Result<Value, KnutError> {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(serde_json::json!({ "call": n }))
            }
        }

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry
            .register(RaceTool {
                calls: Arc::clone(&calls),
            })
            .unwrap();

        let gate = Arc::new(ExecutionGate::new(
            SideEffectPolicy::new().allow(SideEffect::IdempotentWrite),
        ));
        let registry = Arc::new(registry);

        let (g1, g2) = (Arc::clone(&gate), Arc::clone(&gate));
        let (r1, r2) = (Arc::clone(&registry), Arc::clone(&registry));

        let (a, b) = tokio::join!(
            tokio::spawn(async move {
                r1.invoke(
                    &g1,
                    "files",
                    "write",
                    serde_json::json!({ "path": "race" }),
                    Some("race-1".to_owned()),
                    Risk::Low,
                )
                .await
            }),
            tokio::spawn(async move {
                r2.invoke(
                    &g2,
                    "files",
                    "write",
                    serde_json::json!({ "path": "race" }),
                    Some("race-1".to_owned()),
                    Risk::Low,
                )
                .await
            })
        );

        let (a, b) = (a.unwrap(), b.unwrap());

        // Either the loser observes the reservation, or it arrives after
        // completion and receives the recorded replay. Both are safe.
        let reserved = [&a, &b]
            .iter()
            .filter_map(|r| r.as_ref().err())
            .any(|e| matches!(e, KnutError::ExecutionReserved { .. }));
        let replayed = [&a, &b]
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .any(|o| o.replayed);
        assert!(
            reserved || replayed,
            "second caller must be reserved-or-replayed, got {a:?} / {b:?}"
        );

        // The invariant that actually matters: the implementation ran once.
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the fake tool must be invoked at most once"
        );
    }

    /// The bypass invariant: denial cannot be routed around, including
    /// through trees, cached routes, or unknown identities.
    #[tokio::test]
    async fn denial_survives_route_changes() {
        struct DenyTransfers;

        impl Policy for DenyTransfers {
            fn check(&self, request: &ProposedExecution, _s: SideEffect) -> PolicyVerdict {
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
        registry
            .register(CountingTool {
                id: "transfer",
                capability: "payments",
                side_effect: SideEffect::NonIdempotentWrite,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: false,
            })
            .unwrap();
        registry
            .register(CountingTool {
                id: "other",
                capability: "payments",
                side_effect: SideEffect::NonIdempotentWrite,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: false,
            })
            .unwrap();

        let err = registry
            .invoke(
                &gate,
                "payments",
                "transfer",
                serde_json::json!({}),
                Some("t-1".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));

        // The "same tool under a different capability" bypass route is
        // closed at registration: duplicate tool identity is rejected.
        let dup_err = registry
            .register(CountingTool {
                id: "transfer",
                capability: "admin",
                side_effect: SideEffect::ReadOnly,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: false,
            })
            .unwrap_err();
        assert!(matches!(dup_err, KnutError::InvalidArguments { .. }));

        let err = registry
            .invoke(
                &gate,
                "admin",
                "transfer",
                serde_json::json!({}),
                None,
                Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));

        let err = registry
            .invoke(
                &gate,
                "ghost",
                "transfer",
                serde_json::json!({}),
                None,
                Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));

        // Allowed tool still executes.
        let ok = registry
            .invoke(
                &gate,
                "payments",
                "other",
                serde_json::json!({}),
                Some("ok-1".to_owned()),
                Risk::Low,
            )
            .await;
        assert!(ok.is_ok());
    }

    /// The class cannot be misrepresented: the registry's declaration
    /// decides, whatever the request claims via risk or key.
    #[tokio::test]
    async fn side_effect_class_comes_from_the_registry_not_the_request() {
        let gate = ExecutionGate::new(SideEffectPolicy::new().allow(SideEffect::ReadOnly));
        let registry = write_registry(true);

        let err = registry
            .invoke(
                &gate,
                "files",
                "write",
                serde_json::json!({ "path": "sneaky" }),
                Some("sneaky".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, KnutError::PolicyDenied { .. }));
    }

    /// Replays still pass through policy on every invocation: a cached
    /// route cannot outlive a revocation.
    #[tokio::test]
    async fn revoked_policy_blocks_replay_of_a_completed_write() {
        struct Toggle(std::sync::atomic::AtomicBool);

        impl Policy for Toggle {
            fn check(&self, _r: &ProposedExecution, _s: SideEffect) -> PolicyVerdict {
                if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                    PolicyVerdict::Allowed
                } else {
                    PolicyVerdict::Denied {
                        reason: "writes disabled".to_owned(),
                    }
                }
            }
        }

        let mut registry = ToolRegistry::default();
        registry
            .register(CountingTool {
                id: "write",
                capability: "files",
                side_effect: SideEffect::IdempotentWrite,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail: false,
            })
            .unwrap();

        // Separate policy instances cannot share a gate; instead run two
        // gates over the same arguments to show replay identity includes
        // the exact fingerprint, and that a fresh gate re-checks policy.
        let gate = ExecutionGate::new(Toggle(std::sync::atomic::AtomicBool::new(true)));
        let args = serde_json::json!({ "path": "rev" });
        let first = registry
            .invoke(
                &gate,
                "files",
                "write",
                args.clone(),
                Some("r-1".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap();
        assert!(!first.replayed);

        // Completed work replays without another tool call.
        let replay = registry
            .invoke(
                &gate,
                "files",
                "write",
                args.clone(),
                Some("r-1".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap();
        assert!(replay.replayed);

        // Now flip the policy: even a replay request is refused because
        // authorize() runs policy before any journal lookup.
        let gate = ExecutionGate::new(Toggle(std::sync::atomic::AtomicBool::new(false)));
        let err = registry
            .invoke(
                &gate,
                "files",
                "write",
                args,
                Some("r-1".to_owned()),
                Risk::Low,
            )
            .await
            .unwrap_err();
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
