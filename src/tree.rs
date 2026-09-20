use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    ComputeCascade, ExpectedArtifact, KnutError, ModelRequest, ModelTier, ToolRegistry, Verifier,
};

/// Explicit status of every node in a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Blocked,
}

/// The plan data Knut traverses.
///
/// Plans are pure data: there is no way to embed code in a node. Tool and
/// model references must validate against the registry/tier configuration
/// before execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanNode {
    /// All children in order; fails fast.
    Sequence { id: String, children: Vec<PlanNode> },
    /// First child that succeeds wins.
    Selector { id: String, children: Vec<PlanNode> },
    /// Join semantics: all children must succeed. v1 executes children to
    /// completion sequentially (deterministic, cooperative cancellation);
    /// the join outcome, not concurrency, is the contract.
    Parallel { id: String, children: Vec<PlanNode> },
    /// Call one validated tool under one capability.
    Tool {
        id: String,
        capability: String,
        tool_id: String,
        #[serde(default)]
        input: Value,
    },
    /// Produce text with the configured model for the tier.
    Generate {
        id: String,
        instruction: String,
        tier: ModelTier,
    },
    /// Check a previous node's output against a bounded, built-in check.
    Verify {
        id: String,
        /// Node whose output is verified.
        target: String,
        artifact: ExpectedArtifact,
    },
    /// Ask the user; blocked when no handler is configured.
    AskUser { id: String, question: String },
}

impl PlanNode {
    /// The node's stable identifier.
    pub fn id(&self) -> &str {
        match self {
            PlanNode::Sequence { id, .. }
            | PlanNode::Selector { id, .. }
            | PlanNode::Parallel { id, .. }
            | PlanNode::Tool { id, .. }
            | PlanNode::Generate { id, .. }
            | PlanNode::Verify { id, .. }
            | PlanNode::AskUser { id, .. } => id,
        }
    }

    /// Direct children (empty for leaves).
    pub fn children(&self) -> &[PlanNode] {
        match self {
            PlanNode::Sequence { children, .. }
            | PlanNode::Selector { children, .. }
            | PlanNode::Parallel { children, .. } => children,
            _ => &[],
        }
    }

    fn collect_ids(&self, ids: &mut Vec<String>) {
        ids.push(self.id().to_owned());
        for child in self.children() {
            child.collect_ids(ids);
        }
    }
}

/// Why a plan was rejected before execution.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    #[error("duplicate node id: {id:?}")]
    DuplicateId { id: String },

    #[error("composite node {id:?} has no children")]
    EmptyComposite { id: String },

    #[error("unknown tool {tool_id:?} in capability {capability:?}")]
    UnknownTool { capability: String, tool_id: String },

    #[error("model tier {tier:?} is not configured")]
    UnavailableTier { tier: ModelTier },

    #[error("node {id:?} references missing node {target:?}")]
    UnknownReference { id: String, target: String },

    #[error("AskUser node {id:?} has an empty question")]
    EmptyQuestion { id: String },
}

/// Validate a plan against the configured tools and model tiers.
///
/// Structure is a tree, so cycles are not representable; every reference
/// (Verify targets) must point at an existing node, tool references must
/// resolve in the registry, and generate tiers must be configured.
pub fn validate_plan(
    plan: &PlanNode,
    registry: &ToolRegistry,
    tiers: &[ModelTier],
) -> Result<(), PlanError> {
    let mut ids = Vec::new();
    plan.collect_ids(&mut ids);

    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        if !seen.insert(id.clone()) {
            return Err(PlanError::DuplicateId { id: id.clone() });
        }
    }

    validate_node(plan, registry, tiers, &seen)
}

fn validate_node(
    node: &PlanNode,
    registry: &ToolRegistry,
    tiers: &[ModelTier],
    known_ids: &std::collections::HashSet<String>,
) -> Result<(), PlanError> {
    match node {
        PlanNode::Sequence { id, children }
        | PlanNode::Selector { id, children }
        | PlanNode::Parallel { id, children } => {
            if children.is_empty() {
                return Err(PlanError::EmptyComposite { id: id.clone() });
            }
            for child in children {
                validate_node(child, registry, tiers, known_ids)?;
            }
            Ok(())
        }
        PlanNode::Tool {
            id,
            capability,
            tool_id,
            ..
        } => {
            if registry.find_exact(capability, tool_id).is_err() {
                return Err(PlanError::UnknownTool {
                    capability: capability.clone(),
                    tool_id: tool_id.clone(),
                });
            }
            let _ = id;
            Ok(())
        }
        PlanNode::Generate { tier, .. } => {
            if !tiers.contains(tier) {
                return Err(PlanError::UnavailableTier { tier: *tier });
            }
            Ok(())
        }
        PlanNode::Verify { id, target, .. } => {
            if !known_ids.contains(target) {
                return Err(PlanError::UnknownReference {
                    id: id.clone(),
                    target: target.clone(),
                });
            }
            Ok(())
        }
        PlanNode::AskUser { id, question } => {
            if question.trim().is_empty() {
                return Err(PlanError::EmptyQuestion { id: id.clone() });
            }
            Ok(())
        }
    }
}

/// Answers AskUser nodes. Tests fake this; real UIs arrive later.
#[async_trait]
pub trait AskUserHandler: Send + Sync {
    /// `None` means the user could not be asked; the node blocks.
    async fn ask(&self, question: &str) -> Option<String>;
}

/// Cooperative cancellation flag checked at node boundaries.
pub type CancelFlag = Arc<AtomicBool>;

/// Result of one tree run.
///
/// Only executed nodes get a status; nodes never reached stay absent
/// (implicitly pending). `cancelled` marks a cooperative stop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeRunResult {
    pub statuses: BTreeMap<String, NodeStatus>,
    pub outputs: BTreeMap<String, Value>,
    pub cancelled: bool,
}

/// Executes validated plans against real or fake tools/models.
pub struct TreeExecutor {
    registry: Arc<ToolRegistry>,
    cascade: Arc<ComputeCascade>,
    verifier: Arc<dyn Verifier>,
    ask_handler: Option<Arc<dyn AskUserHandler>>,
}

impl TreeExecutor {
    pub fn new(
        registry: Arc<ToolRegistry>,
        cascade: Arc<ComputeCascade>,
        verifier: Arc<dyn Verifier>,
    ) -> Self {
        Self {
            registry,
            cascade,
            verifier,
            ask_handler: None,
        }
    }

    pub fn with_ask_handler(mut self, handler: Arc<dyn AskUserHandler>) -> Self {
        self.ask_handler = Some(handler);
        self
    }

    /// Run a plan that has passed [`validate_plan`].
    ///
    /// Validation happens again here for tool lookups only as defense in
    /// depth; structural errors are programmer errors and panic-free
    /// `Blocked`/`Failed` statuses.
    pub async fn run(
        &self,
        plan: &PlanNode,
        cancel: CancelFlag,
    ) -> Result<TreeRunResult, KnutError> {
        let mut result = TreeRunResult {
            statuses: BTreeMap::new(),
            outputs: BTreeMap::new(),
            cancelled: false,
        };

        let status = self
            .exec(
                plan,
                &mut result.statuses,
                &mut result.outputs,
                &cancel,
                &mut result.cancelled,
            )
            .await?;
        if !result.cancelled {
            result.statuses.insert(plan.id().to_owned(), status);
        }

        Ok(result)
    }

    /// Boxed entry point for recursive child execution.
    ///
    /// Async recursion requires boxing; all recursive call sites go
    /// through this wrapper so the future type stays finite.
    #[allow(clippy::type_complexity)]
    fn exec_boxed<'a>(
        &'a self,
        node: &'a PlanNode,
        statuses: &'a mut BTreeMap<String, NodeStatus>,
        outputs: &'a mut BTreeMap<String, Value>,
        cancel: &'a CancelFlag,
        cancelled: &'a mut bool,
    ) -> Pin<Box<dyn Future<Output = Result<NodeStatus, KnutError>> + Send + 'a>> {
        Box::pin(self.exec(node, statuses, outputs, cancel, cancelled))
    }

    /// Execute one node, recording status and output.
    ///
    /// Returns `NodeStatus::Pending` as a placeholder when cancellation
    /// fired; callers check the `cancelled` flag and discard it.
    async fn exec(
        &self,
        node: &PlanNode,
        statuses: &mut BTreeMap<String, NodeStatus>,
        outputs: &mut BTreeMap<String, Value>,
        cancel: &CancelFlag,
        cancelled: &mut bool,
    ) -> Result<NodeStatus, KnutError> {
        if cancel.load(Ordering::SeqCst) {
            *cancelled = true;
            return Ok(NodeStatus::Pending);
        }

        let status = match node {
            PlanNode::Sequence { children, .. } => {
                let mut joined = NodeStatus::Succeeded;
                for child in children {
                    let child_status = self
                        .exec_boxed(child, statuses, outputs, cancel, cancelled)
                        .await?;
                    if *cancelled {
                        return Ok(NodeStatus::Pending);
                    }
                    match child_status {
                        NodeStatus::Failed => {
                            joined = NodeStatus::Failed;
                            break;
                        }
                        NodeStatus::Blocked => {
                            joined = NodeStatus::Blocked;
                            break;
                        }
                        _ => {}
                    }
                }
                joined
            }
            PlanNode::Selector { children, .. } => {
                let mut joined = NodeStatus::Failed;
                for child in children {
                    let child_status = self
                        .exec_boxed(child, statuses, outputs, cancel, cancelled)
                        .await?;
                    if *cancelled {
                        return Ok(NodeStatus::Pending);
                    }
                    match child_status {
                        NodeStatus::Succeeded => {
                            joined = NodeStatus::Succeeded;
                            break;
                        }
                        NodeStatus::Blocked => {
                            joined = NodeStatus::Blocked;
                            break;
                        }
                        _ => {}
                    }
                }
                joined
            }
            PlanNode::Parallel { children, .. } => {
                // Join semantics: every child runs; failure dominates
                // blocking; success requires all.
                let mut any_failed = false;
                let mut any_blocked = false;
                for child in children {
                    let child_status = self
                        .exec_boxed(child, statuses, outputs, cancel, cancelled)
                        .await?;
                    if *cancelled {
                        return Ok(NodeStatus::Pending);
                    }
                    match child_status {
                        NodeStatus::Failed => any_failed = true,
                        NodeStatus::Blocked => any_blocked = true,
                        _ => {}
                    }
                }
                if any_failed {
                    NodeStatus::Failed
                } else if any_blocked {
                    NodeStatus::Blocked
                } else {
                    NodeStatus::Succeeded
                }
            }
            PlanNode::Tool {
                id,
                capability,
                tool_id,
                input,
            } => match self.registry.find_exact(capability, tool_id) {
                Err(_) => NodeStatus::Blocked,
                Ok(tool) => match tool.call(input.clone()).await {
                    Ok(output) => {
                        outputs.insert(id.clone(), output);
                        NodeStatus::Succeeded
                    }
                    Err(_) => NodeStatus::Failed,
                },
            },
            PlanNode::Generate {
                id,
                instruction,
                tier,
            } => {
                let request = ModelRequest::new(instruction.clone(), ExpectedArtifact::Text);
                match self
                    .cascade
                    .run(&request, *tier, self.verifier.as_ref())
                    .await
                {
                    Ok(outcome) => {
                        outputs.insert(id.clone(), Value::String(outcome.response.content));
                        NodeStatus::Succeeded
                    }
                    Err(_) => NodeStatus::Failed,
                }
            }
            PlanNode::Verify {
                id,
                target,
                artifact,
            } => {
                let verified = match (outputs.get(target), artifact) {
                    (Some(Value::String(text)), ExpectedArtifact::Text) => !text.is_empty(),
                    (Some(Value::String(text)), ExpectedArtifact::Json) => {
                        serde_json::from_str::<Value>(text).is_ok()
                    }
                    (Some(other), ExpectedArtifact::Text) => !other.is_null(),
                    (Some(other), ExpectedArtifact::Json) => {
                        // Tool outputs are already structured; any non-null
                        // value counts as the expected JSON artifact.
                        !other.is_null()
                    }
                    (None, _) => false,
                };
                let _ = id;
                if verified {
                    NodeStatus::Succeeded
                } else {
                    NodeStatus::Failed
                }
            }
            PlanNode::AskUser { id, question } => match &self.ask_handler {
                None => NodeStatus::Blocked,
                Some(handler) => match handler.ask(question).await {
                    Some(answer) => {
                        outputs.insert(id.clone(), Value::String(answer));
                        NodeStatus::Succeeded
                    }
                    None => NodeStatus::Blocked,
                },
            },
        };

        statuses.insert(node.id().to_owned(), status);
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::json;

    use crate::model::{Model, ModelIdentity, ModelRequest, ModelResponse, Usage};
    use crate::tool::{SideEffect, Tool, ToolMetadata};

    use super::*;

    struct StaticTool {
        id: &'static str,
        capability: &'static str,
        output: Value,
        fail: bool,
    }

    #[async_trait]
    impl Tool for StaticTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                capability: self.capability.to_owned(),
                description: "static test tool".to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: SideEffect::ReadOnly,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            if self.fail {
                Err(KnutError::Tool("static failure".to_owned()))
            } else {
                Ok(self.output.clone())
            }
        }
    }

    struct EchoModel {
        content: String,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl Model for EchoModel {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fake".to_owned(),
                model: "echo".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        async fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            *self.calls.lock().unwrap() += 1;
            Ok(ModelResponse {
                content: self.content.clone(),
                identity: self.identity(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct AcceptAll;

    impl Verifier for AcceptAll {
        fn verify(&self, _response: &crate::ModelResponse) -> crate::VerificationVerdict {
            crate::VerificationVerdict::Sufficient
        }
    }

    /// First N responses fail verification, then everything passes.
    struct RetryThenAccept {
        remaining_retries: Mutex<usize>,
    }

    impl Verifier for RetryThenAccept {
        fn verify(&self, _response: &crate::ModelResponse) -> crate::VerificationVerdict {
            let mut remaining = self.remaining_retries.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                crate::VerificationVerdict::Retry {
                    reason: "weak answer".to_owned(),
                }
            } else {
                crate::VerificationVerdict::Sufficient
            }
        }
    }

    struct AskingUser {
        answer: Option<String>,
    }

    #[async_trait]
    impl AskUserHandler for AskingUser {
        async fn ask(&self, _question: &str) -> Option<String> {
            self.answer.clone()
        }
    }

    fn registry_with(tools: Vec<StaticTool>) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::default();
        for tool in tools {
            registry.register(tool);
        }
        Arc::new(registry)
    }

    fn executor(registry: Arc<ToolRegistry>) -> TreeExecutor {
        let model = EchoModel {
            content: "{\"ok\": true}".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));
        TreeExecutor::new(registry, cascade, Arc::new(AcceptAll))
    }

    fn tool_node(id: &str, capability: &str, tool_id: &str) -> PlanNode {
        PlanNode::Tool {
            id: id.to_owned(),
            capability: capability.to_owned(),
            tool_id: tool_id.to_owned(),
            input: json!({}),
        }
    }

    fn good_registry() -> Arc<ToolRegistry> {
        registry_with(vec![
            StaticTool {
                id: "ok",
                capability: "files",
                output: json!({ "status": "done" }),
                fail: false,
            },
            StaticTool {
                id: "bad",
                capability: "files",
                output: json!({}),
                fail: true,
            },
        ])
    }

    #[tokio::test]
    async fn sequence_succeeds_only_when_all_children_succeed() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![tool_node("a", "files", "ok"), tool_node("b", "files", "ok")],
        };

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.outputs.get("a"), Some(&json!({ "status": "done" })));
    }

    #[tokio::test]
    async fn sequence_fails_fast_on_first_failure() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                tool_node("a", "files", "bad"),
                tool_node("b", "files", "ok"),
            ],
        };

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Failed));
        assert_eq!(result.statuses.get("a"), Some(&NodeStatus::Failed));
        // Fail fast: b never ran.
        assert!(!result.statuses.contains_key("b"));
    }

    #[tokio::test]
    async fn selector_stops_at_first_success() {
        let plan = PlanNode::Selector {
            id: "root".into(),
            children: vec![
                tool_node("a", "files", "bad"),
                tool_node("b", "files", "ok"),
                tool_node("c", "files", "ok"),
            ],
        };

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("a"), Some(&NodeStatus::Failed));
        assert_eq!(result.statuses.get("b"), Some(&NodeStatus::Succeeded));
        assert!(!result.statuses.contains_key("c"));
    }

    #[tokio::test]
    async fn selector_fails_when_every_child_fails() {
        let plan = PlanNode::Selector {
            id: "root".into(),
            children: vec![
                tool_node("a", "files", "bad"),
                tool_node("b", "files", "bad"),
            ],
        };

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Failed));
    }

    #[tokio::test]
    async fn parallel_runs_all_children_and_fails_on_any_failure() {
        let plan = PlanNode::Parallel {
            id: "root".into(),
            children: vec![
                tool_node("a", "files", "ok"),
                tool_node("b", "files", "bad"),
            ],
        };

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Failed));
        // Both ran: join semantics, unlike sequence fail-fast.
        assert_eq!(result.statuses.get("a"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("b"), Some(&NodeStatus::Failed));
    }

    #[tokio::test]
    async fn tool_node_blocks_when_tool_is_missing() {
        let plan = tool_node("a", "files", "missing");

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("a"), Some(&NodeStatus::Blocked));
    }

    #[tokio::test]
    async fn generate_then_verify_with_valid_json_succeeds() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "gen".into(),
                    instruction: "produce json".into(),
                    tier: ModelTier::Reasoner,
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "gen".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let result = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("gen"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("check"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Succeeded));
    }

    #[tokio::test]
    async fn verify_fails_on_invalid_json_from_generate() {
        let mut registry = ToolRegistry::default();
        registry.register(StaticTool {
            id: "ok",
            capability: "files",
            output: json!({}),
            fail: false,
        });
        let model = EchoModel {
            content: "definitely not json".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));
        let executor = TreeExecutor::new(Arc::new(registry), cascade, Arc::new(AcceptAll));

        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "gen".into(),
                    instruction: "produce json".into(),
                    tier: ModelTier::Reasoner,
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "gen".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let result = executor.run(&plan, no_cancel()).await.unwrap();

        assert_eq!(result.statuses.get("check"), Some(&NodeStatus::Failed));
        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Failed));
    }

    #[tokio::test]
    async fn generate_can_escalate_inside_a_tree_via_the_cascade() {
        // The verifier rejects the first response (fast tier) and accepts
        // the next, forcing a Fast -> Reasoner escalation mid-tree; the
        // reasoner echo model answers with valid JSON.
        let mut registry = ToolRegistry::default();
        registry.register(StaticTool {
            id: "ok",
            capability: "files",
            output: json!({}),
            fail: false,
        });
        let fast = EchoModel {
            content: "nope".to_owned(),
            calls: Mutex::new(0),
        };
        let reasoner = EchoModel {
            content: "{\"ok\": true}".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(
            ComputeCascade::empty()
                .with_fast(fast)
                .with_reasoner(reasoner),
        );
        let executor = TreeExecutor::new(
            Arc::new(registry),
            cascade,
            Arc::new(RetryThenAccept {
                remaining_retries: Mutex::new(1),
            }),
        );

        let generate = PlanNode::Generate {
            id: "gen".into(),
            instruction: "produce json".into(),
            tier: ModelTier::Fast,
        };

        let result = executor.run(&generate, no_cancel()).await.unwrap();

        assert_eq!(result.statuses.get("gen"), Some(&NodeStatus::Succeeded));
        assert_eq!(
            result.outputs.get("gen"),
            Some(&Value::String("{\"ok\": true}".to_owned()))
        );
    }

    #[tokio::test]
    async fn ask_user_blocks_without_handler_and_succeeds_with_one() {
        let plan = PlanNode::AskUser {
            id: "ask".into(),
            question: "Which file?".into(),
        };

        let blocked = executor(good_registry())
            .run(&plan, no_cancel())
            .await
            .unwrap();
        assert_eq!(blocked.statuses.get("ask"), Some(&NodeStatus::Blocked));

        let mut registry = ToolRegistry::default();
        registry.register(StaticTool {
            id: "ok",
            capability: "files",
            output: json!({}),
            fail: false,
        });
        let model = EchoModel {
            content: "x".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));
        let executor = TreeExecutor::new(Arc::new(registry), cascade, Arc::new(AcceptAll))
            .with_ask_handler(Arc::new(AskingUser {
                answer: Some("notes.txt".to_owned()),
            }));

        let answered = executor.run(&plan, no_cancel()).await.unwrap();
        assert_eq!(answered.statuses.get("ask"), Some(&NodeStatus::Succeeded));
        assert_eq!(
            answered.outputs.get("ask"),
            Some(&Value::String("notes.txt".to_owned()))
        );
    }

    #[tokio::test]
    async fn cancellation_stops_between_nodes() {
        struct CancelAfterCall {
            flag: CancelFlag,
        }

        #[async_trait]
        impl Tool for CancelAfterCall {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: "cancelme".to_owned(),
                    capability: "files".to_owned(),
                    description: "sets the cancel flag".to_owned(),
                    input_schema: json!({ "type": "object" }),
                    side_effect: SideEffect::ReadOnly,
                }
            }

            async fn call(&self, _input: Value) -> Result<Value, KnutError> {
                self.flag.store(true, Ordering::SeqCst);
                Ok(json!({}))
            }
        }

        let flag: CancelFlag = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::default();
        registry.register(CancelAfterCall {
            flag: Arc::clone(&flag),
        });
        registry.register(StaticTool {
            id: "ok",
            capability: "files",
            output: json!({}),
            fail: false,
        });
        let model = EchoModel {
            content: "x".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));

        let executor = TreeExecutor::new(Arc::new(registry), cascade, Arc::new(AcceptAll));
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                tool_node("trigger", "files", "cancelme"),
                tool_node("after", "files", "ok"),
            ],
        };

        let result = executor.run(&plan, Arc::clone(&flag)).await.unwrap();

        assert!(result.cancelled);
        assert_eq!(result.statuses.get("trigger"), Some(&NodeStatus::Succeeded));
        assert!(!result.statuses.contains_key("after"));
        assert!(!result.statuses.contains_key("root"));
    }

    fn no_cancel() -> CancelFlag {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn validation_rejects_duplicate_ids() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![tool_node("a", "files", "ok"), tool_node("a", "files", "ok")],
        };

        let err = validate_plan(&plan, &good_registry(), &[ModelTier::Reasoner]).unwrap_err();
        assert_eq!(err, PlanError::DuplicateId { id: "a".to_owned() });
    }

    #[test]
    fn validation_rejects_empty_composites() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![],
        };

        let err = validate_plan(&plan, &good_registry(), &[ModelTier::Reasoner]).unwrap_err();
        assert_eq!(
            err,
            PlanError::EmptyComposite {
                id: "root".to_owned()
            }
        );
    }

    #[test]
    fn validation_rejects_unknown_tools() {
        let plan = tool_node("a", "files", "missing");

        let err = validate_plan(&plan, &good_registry(), &[ModelTier::Reasoner]).unwrap_err();
        assert_eq!(
            err,
            PlanError::UnknownTool {
                capability: "files".to_owned(),
                tool_id: "missing".to_owned(),
            }
        );
    }

    #[test]
    fn validation_rejects_unavailable_model_tiers() {
        let plan = PlanNode::Generate {
            id: "gen".into(),
            instruction: "hi".into(),
            tier: ModelTier::Fast,
        };

        let err = validate_plan(&plan, &good_registry(), &[ModelTier::Reasoner]).unwrap_err();
        assert_eq!(
            err,
            PlanError::UnavailableTier {
                tier: ModelTier::Fast
            }
        );
    }

    #[test]
    fn validation_rejects_references_to_missing_nodes() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "gen".into(),
                    instruction: "hi".into(),
                    tier: ModelTier::Reasoner,
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "ghost".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let err = validate_plan(&plan, &good_registry(), &[ModelTier::Reasoner]).unwrap_err();
        assert_eq!(
            err,
            PlanError::UnknownReference {
                id: "check".to_owned(),
                target: "ghost".to_owned(),
            }
        );
    }

    #[test]
    fn validation_accepts_a_well_formed_plan() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                tool_node("a", "files", "ok"),
                PlanNode::Generate {
                    id: "gen".into(),
                    instruction: "summarize".into(),
                    tier: ModelTier::Reasoner,
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "gen".into(),
                    artifact: ExpectedArtifact::Text,
                },
            ],
        };

        assert!(validate_plan(&plan, &good_registry(), &[ModelTier::Reasoner]).is_ok());
    }

    #[test]
    fn plans_are_serializable_for_traces_and_replay() {
        let plan = PlanNode::Selector {
            id: "root".into(),
            children: vec![
                tool_node("a", "files", "ok"),
                PlanNode::AskUser {
                    id: "ask".into(),
                    question: "fallback?".into(),
                },
            ],
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"type\":\"selector\""));

        let round: PlanNode = serde_json::from_str(&json).unwrap();
        assert_eq!(round, plan);

        let run = TreeRunResult {
            statuses: BTreeMap::from([("root".to_owned(), NodeStatus::Succeeded)]),
            outputs: BTreeMap::new(),
            cancelled: false,
        };
        let run_json = serde_json::to_string(&run).unwrap();
        assert!(run_json.contains("\"succeeded\""));
    }

    #[tokio::test]
    async fn hand_authored_tree_executes_deterministically() {
        let make_plan = || PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Selector {
                    id: "pick".into(),
                    children: vec![
                        tool_node("try_cache", "files", "bad"),
                        tool_node("try_disk", "files", "ok"),
                    ],
                },
                PlanNode::Generate {
                    id: "summarize".into(),
                    instruction: "summarize the result".into(),
                    tier: ModelTier::Reasoner,
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "summarize".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let executor = executor(good_registry());
        let first = executor.run(&make_plan(), no_cancel()).await.unwrap();
        let second = executor.run(&make_plan(), no_cancel()).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first.statuses.get("root"), Some(&NodeStatus::Succeeded));
        assert_eq!(first.statuses.get("try_cache"), Some(&NodeStatus::Failed));
        assert_eq!(first.statuses.get("try_disk"), Some(&NodeStatus::Succeeded));
    }
}
