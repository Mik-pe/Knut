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
    ComputeCascade, ExecutionGate, ExpectedArtifact, KnutError, ModelRequest, ModelTier, Risk,
    ToolRegistry, ValidatedPlan, Verifier,
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

/// The kind of value a node produces, used to type-check references
/// before anything executes (issue #19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A structured value (tool output).
    Json,
    /// Model-produced text.
    Text,
}

impl ArtifactKind {
    /// Whether a consumer expecting `self` can read a producer's kind.
    ///
    /// Text is readable as text; JSON is readable as JSON. A text
    /// artifact is never silently coerced into JSON: that is exactly the
    /// mismatch `Verify` exists to catch.
    pub fn accepts(self, produced: ArtifactKind) -> bool {
        self == produced
    }
}

/// A reference from a node's input to a prior node's artifact.
///
/// References are explicit and typed so validation can reject missing,
/// wrong-type and impossible-order (forward/self) references *before*
/// any effect runs. The referenced value is resolved from the artifact
/// store at execution time, so changing what an earlier node produced
/// changes exactly what a later node observes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// The node whose output is referenced.
    pub node: String,
    /// The kind this consumer requires.
    pub kind: ArtifactKind,
}

impl ArtifactRef {
    pub fn new(node: impl Into<String>, kind: ArtifactKind) -> Self {
        Self {
            node: node.into(),
            kind,
        }
    }
}

/// A place where a node wants a prior artifact substituted.
///
/// Serialized as `{"$ref": "node-id", "kind": "json"}` inside a tool
/// input: a model can build these, but it cannot make one refer to
/// something that does not exist or that runs later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRefSpec {
    #[serde(rename = "$ref")]
    pub node: String,
    pub kind: ArtifactKind,
}

/// Bounded store of node outputs, keyed by node id.
///
/// Large values live here rather than being re-embedded in the tree or
/// copied through every routing call. Values are bounded because plan
/// size and node count are bounded; the store never grows past the
/// validated plan.
#[derive(Debug, Clone, Default)]
pub struct ArtifactStore {
    values: BTreeMap<String, Value>,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, node: impl Into<String>, value: Value) {
        self.values.insert(node.into(), value);
    }

    pub fn get(&self, node: &str) -> Option<&Value> {
        self.values.get(node)
    }

    /// Build a store from already-produced outputs (executor view).
    pub fn from(values: &BTreeMap<String, Value>) -> Self {
        Self {
            values: values.clone(),
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.values.iter()
    }
}

/// Resolve the typed references inside one tool input.
///
/// Returns the input with every `$ref` replaced by the referenced
/// artifact. A missing reference is an error, never a null substitution.
pub fn resolve_input(input: &Value, store: &ArtifactStore) -> Result<Value, PlanError> {
    match input {
        Value::Object(map) => {
            // A ref is only a ref when it is the whole object: partial
            // merges would make the resolved arguments hard to reason
            // about and easy to mis-serialize.
            if map.len() == 2 && map.contains_key("$ref") && map.contains_key("kind") {
                let spec: ArtifactRefSpec =
                    serde_json::from_value(input.clone()).map_err(|_| PlanError::InvalidRef {
                        node: map
                            .get("$ref")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        reason: "malformed artifact reference".to_owned(),
                    })?;
                let value = store
                    .get(&spec.node)
                    .ok_or_else(|| PlanError::UnknownReference {
                        id: "<input>".to_owned(),
                        target: spec.node.clone(),
                    })?;
                let actual = match value {
                    Value::String(_) => ArtifactKind::Text,
                    _ => ArtifactKind::Json,
                };
                if !spec.kind.accepts(actual) {
                    return Err(PlanError::ReferenceType {
                        id: "<input>".to_owned(),
                        target: spec.node.clone(),
                        expected: spec.kind,
                        actual,
                    });
                }
                return Ok(value.clone());
            }
            let mut resolved = serde_json::Map::new();
            for (key, value) in map {
                resolved.insert(key.clone(), resolve_input(value, store)?);
            }
            Ok(Value::Object(resolved))
        }
        Value::Array(items) => {
            let mut resolved = Vec::with_capacity(items.len());
            for item in items {
                resolved.push(resolve_input(item, store)?);
            }
            Ok(Value::Array(resolved))
        }
        other => Ok(other.clone()),
    }
}

/// Collect the artifact references declared inside one input value.
pub fn collect_refs(input: &Value, refs: &mut Vec<ArtifactRef>) {
    match input {
        Value::Object(map) => {
            if map.len() == 2
                && map.contains_key("$ref")
                && map.contains_key("kind")
                && let Ok(spec) = serde_json::from_value::<ArtifactRefSpec>(input.clone())
            {
                refs.push(ArtifactRef::new(spec.node, spec.kind));
                return;
            }
            for value in map.values() {
                collect_refs(value, refs);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, refs);
            }
        }
        _ => {}
    }
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
    ///
    /// `input` may contain artifact references, so a node can be *given*
    /// an earlier node's output instead of only being told about it.
    Generate {
        id: String,
        instruction: String,
        tier: ModelTier,
        #[serde(default)]
        input: Value,
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

    #[error("node {id:?} references itself")]
    SelfReference { id: String },

    #[error(
        "node {id:?} would consume {target:?} before it has produced anything \
         (forward reference or unordered sibling)"
    )]
    ImpossibleOrder { id: String, target: String },

    #[error(
        "node {id:?} needs a {expected:?} artifact from {target:?}, but it produces {actual:?}"
    )]
    ReferenceType {
        id: String,
        target: String,
        expected: ArtifactKind,
        actual: ArtifactKind,
    },

    #[error("malformed artifact reference in {node:?}: {reason}")]
    InvalidRef { node: String, reason: String },

    #[error("AskUser node {id:?} has an empty question")]
    EmptyQuestion { id: String },
}

/// Validate a plan against the configured tools and model tiers.
///
/// Structure is a tree, so cycles are not representable; every reference
/// (Verify targets and typed artifact refs inside node inputs) must point
/// at a node that will have *already produced* its artifact when the
/// consumer runs, tool references must resolve in the registry, and
/// generate tiers must be configured.
///
/// Dependency order is checked by walking the plan in execution order and
/// tracking which ids have completed. `Parallel` children are treated
/// conservatively: a sibling's artifact is not assumed to exist, because
/// the join order is not a data contract.
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

    let mut produced = std::collections::HashMap::new();
    validate_node(plan, registry, tiers, &seen, &mut produced)
}

/// What kind of artifact a node produces, if any.
fn produced_kind(node: &PlanNode) -> Option<ArtifactKind> {
    match node {
        PlanNode::Tool { .. } => Some(ArtifactKind::Json),
        PlanNode::Generate { .. } => Some(ArtifactKind::Text),
        PlanNode::AskUser { .. } => Some(ArtifactKind::Text),
        // A check reports a status, not an artifact: nothing may consume
        // its "output".
        PlanNode::Verify { .. } => None,
        PlanNode::Sequence { .. } | PlanNode::Selector { .. } | PlanNode::Parallel { .. } => None,
    }
}

fn validate_node(
    node: &PlanNode,
    registry: &ToolRegistry,
    tiers: &[ModelTier],
    known_ids: &std::collections::HashSet<String>,
    produced: &mut std::collections::HashMap<String, ArtifactKind>,
) -> Result<(), PlanError> {
    match node {
        PlanNode::Sequence { children, .. } => {
            if children.is_empty() {
                return Err(PlanError::EmptyComposite {
                    id: node.id().to_owned(),
                });
            }
            for child in children {
                validate_node(child, registry, tiers, known_ids, produced)?;
                if let Some(kind) = produced_kind(child) {
                    produced.insert(child.id().to_owned(), kind);
                }
            }
            Ok(())
        }
        PlanNode::Selector { children, .. } => {
            if children.is_empty() {
                return Err(PlanError::EmptyComposite {
                    id: node.id().to_owned(),
                });
            }
            // Only one branch runs, but which one is unknown at validation
            // time; a later consumer may reference any branch's artifact
            // only if every branch produces a compatible kind.
            let mut branch_kinds: Option<std::collections::HashMap<String, ArtifactKind>> = None;
            for child in children {
                let mut branch = produced.clone();
                validate_node(child, registry, tiers, known_ids, &mut branch)?;
                if let Some(kind) = produced_kind(child) {
                    branch.insert(child.id().to_owned(), kind);
                }
                branch_kinds = Some(match branch_kinds {
                    None => branch,
                    Some(previous) => previous
                        .into_iter()
                        .filter(|(id, kind)| branch.get(id) == Some(kind))
                        .collect(),
                });
            }
            if let Some(kinds) = branch_kinds {
                // A later consumer cannot rely on a selector's output
                // unless every branch produced it.
                for (id, kind) in kinds {
                    if produced.get(&id) != Some(&kind) {
                        // Do not advertise; leave the pre-selector state.
                        continue;
                    }
                }
            }
            Ok(())
        }
        PlanNode::Parallel { children, .. } => {
            if children.is_empty() {
                return Err(PlanError::EmptyComposite {
                    id: node.id().to_owned(),
                });
            }
            // Siblings may not consume each other's artifacts: the join
            // order is an implementation detail, not a data contract.
            // Each branch is validated against the snapshot taken
            // *before* the parallel, and the union is merged only after
            // every branch has been checked.
            let mut branches = Vec::new();
            for child in children {
                let mut for_this_branch = produced.clone();
                validate_node(child, registry, tiers, known_ids, &mut for_this_branch)?;
                if let Some(kind) = produced_kind(child) {
                    for_this_branch.insert(child.id().to_owned(), kind);
                }
                branches.push(for_this_branch);
            }
            // Every branch runs and the join waits for all of them, so
            // afterwards a consumer may reference any branch's artifact
            // (but not rely on cross-branch consumption, just rejected).
            for branch in branches {
                produced.extend(branch);
            }
            Ok(())
        }
        PlanNode::Tool {
            id,
            capability,
            tool_id,
            input,
        } => {
            if registry.find_exact(capability, tool_id).is_err() {
                return Err(PlanError::UnknownTool {
                    capability: capability.clone(),
                    tool_id: tool_id.clone(),
                });
            }
            validate_refs(id, input, known_ids, produced)?;
            Ok(())
        }
        PlanNode::Generate {
            id, tier, input, ..
        } => {
            if !tiers.contains(tier) {
                return Err(PlanError::UnavailableTier { tier: *tier });
            }
            validate_refs(id, input, known_ids, produced)?;
            Ok(())
        }
        PlanNode::Verify { id, target, .. } => {
            if !known_ids.contains(target) {
                return Err(PlanError::UnknownReference {
                    id: id.clone(),
                    target: target.clone(),
                });
            }
            // A verify node observes its target, so the target must have
            // run: a check cannot precede the thing it checks.
            if !produced.contains_key(target) {
                return Err(PlanError::ImpossibleOrder {
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

/// Reject artifact references that are missing, mistyped or impossible.
///
/// The producing node must be *earlier in execution order* and declared
/// as producing a compatible kind. A hand-authored plan and a
/// model-produced plan pass through exactly this function.
fn validate_refs(
    id: &str,
    input: &Value,
    known_ids: &std::collections::HashSet<String>,
    produced: &std::collections::HashMap<String, ArtifactKind>,
) -> Result<(), PlanError> {
    let mut refs = Vec::new();
    collect_refs(input, &mut refs);

    for reference in refs {
        if reference.node == id {
            return Err(PlanError::SelfReference { id: id.to_owned() });
        }
        if !known_ids.contains(&reference.node) {
            return Err(PlanError::UnknownReference {
                id: id.to_owned(),
                target: reference.node.clone(),
            });
        }
        let Some(actual) = produced.get(&reference.node) else {
            return Err(PlanError::ImpossibleOrder {
                id: id.to_owned(),
                target: reference.node.clone(),
            });
        };
        if !reference.kind.accepts(*actual) {
            return Err(PlanError::ReferenceType {
                id: id.to_owned(),
                target: reference.node.clone(),
                expected: reference.kind,
                actual: *actual,
            });
        }
    }
    Ok(())
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
    gate: Arc<ExecutionGate>,
    cascade: Arc<ComputeCascade>,
    verifier: Arc<dyn Verifier>,
    ask_handler: Option<Arc<dyn AskUserHandler>>,
}

impl TreeExecutor {
    /// `gate` is mandatory: tree execution cannot exist without an
    /// authorization boundary, so there is no gate-less constructor.
    pub fn new(
        registry: Arc<ToolRegistry>,
        gate: Arc<ExecutionGate>,
        cascade: Arc<ComputeCascade>,
        verifier: Arc<dyn Verifier>,
    ) -> Self {
        Self {
            registry,
            gate,
            cascade,
            verifier,
            ask_handler: None,
        }
    }

    pub fn with_ask_handler(mut self, handler: Arc<dyn AskUserHandler>) -> Self {
        self.ask_handler = Some(handler);
        self
    }

    /// Run a validated plan.
    ///
    /// Only a [`ValidatedPlan`] is accepted: the type *is* the proof that
    /// limits and semantic validation ran, so no arbitrary `PlanNode`
    /// (from model output, a test fixture, or hand-written code) can
    /// reach execution without them. Structural re-checks below are
    /// defense in depth.
    pub async fn run(
        &self,
        validated: &ValidatedPlan,
        cancel: CancelFlag,
    ) -> Result<TreeRunResult, KnutError> {
        let plan = &validated.plan;
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
            } => {
                // Resolve typed references against what earlier nodes
                // actually produced. An unresolvable reference is a
                // failure, never a silently-empty argument.
                let resolved = match resolve_input(input, &ArtifactStore::from(outputs)) {
                    Ok(resolved) => resolved,
                    Err(_) => return Ok(NodeStatus::Failed),
                };
                // Single execution path: availability, argument schema,
                // policy, approval, and journal semantics all live in the
                // gate. A node never calls an implementation directly.
                match self
                    .registry
                    .invoke(&self.gate, capability, tool_id, resolved, None, Risk::Low)
                    .await
                {
                    Ok(outcome) => {
                        outputs.insert(id.clone(), outcome.output);
                        NodeStatus::Succeeded
                    }
                    // Unavailable tool or refused authorization: blocked.
                    Err(KnutError::ToolNotFound(_))
                    | Err(KnutError::PolicyDenied { .. })
                    | Err(KnutError::ApprovalRequired { .. })
                    | Err(KnutError::ExecutionReserved { .. })
                    | Err(KnutError::UnknownEffect { .. }) => NodeStatus::Blocked,
                    // Bad arguments or tool failure: failed.
                    Err(_) => NodeStatus::Failed,
                }
            }
            PlanNode::Generate {
                id,
                instruction,
                tier,
                input,
            } => {
                // The referenced artifacts *are* the input the model
                // sees: a read result must actually reach generation.
                let resolved = match resolve_input(input, &ArtifactStore::from(outputs)) {
                    Ok(resolved) => resolved,
                    Err(_) => return Ok(NodeStatus::Failed),
                };
                let mut request = ModelRequest::new(instruction.clone(), ExpectedArtifact::Text);
                if !resolved.is_null() {
                    request = request.with_input(resolved);
                }
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
                tool_version: "1".to_owned(),
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

    /// Records the exact input of every request, so a test can assert
    /// what a referenced artifact actually delivered to the model.
    struct RecordingModel {
        content: String,
        seen: Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl Model for RecordingModel {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fake".to_owned(),
                model: "recording".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            self.seen.lock().unwrap().push(request.input.clone());
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

    /// A tool whose output the test controls at call time.
    struct FixtureTool {
        id: &'static str,
        capability: &'static str,
        output: Value,
    }

    #[async_trait]
    impl Tool for FixtureTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                tool_version: "1".to_owned(),
                capability: self.capability.to_owned(),
                description: "fixture".to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: crate::SideEffect::ReadOnly,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            Ok(self.output.clone())
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
            registry.register(tool).unwrap();
        }
        Arc::new(registry)
    }

    fn test_gate() -> Arc<ExecutionGate> {
        Arc::new(ExecutionGate::new(
            crate::SideEffectPolicy::new()
                .allow(crate::SideEffect::ReadOnly)
                .allow(crate::SideEffect::IdempotentWrite)
                .allow(crate::SideEffect::NonIdempotentWrite),
        ))
    }

    fn executor(registry: Arc<ToolRegistry>) -> TreeExecutor {
        let model = EchoModel {
            content: "{\"ok\": true}".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));
        TreeExecutor::new(registry, test_gate(), cascade, Arc::new(AcceptAll))
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
            .run(&validated(plan, &good_registry()), no_cancel())
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
            .run(&validated(plan, &good_registry()), no_cancel())
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
            .run(&validated(plan, &good_registry()), no_cancel())
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
            .run(&validated(plan, &good_registry()), no_cancel())
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
            .run(&validated(plan, &good_registry()), no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Failed));
        // Both ran: join semantics, unlike sequence fail-fast.
        assert_eq!(result.statuses.get("a"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("b"), Some(&NodeStatus::Failed));
    }

    #[test]
    fn unknown_tools_cannot_reach_execution_at_all() {
        // #19: an unknown tool is a *validation* failure now, so this
        // plan never becomes a `ValidatedPlan` and never executes. The
        // executor's `Blocked` status remains as defense in depth for a
        // tool that disappears between validation and execution.
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

    #[tokio::test]
    async fn generate_then_verify_with_valid_json_succeeds() {
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "gen".into(),
                    instruction: "produce json".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({}),
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "gen".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let result = executor(good_registry())
            .run(&validated(plan, &good_registry()), no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("gen"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("check"), Some(&NodeStatus::Succeeded));
        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Succeeded));
    }

    #[tokio::test]
    async fn verify_fails_on_invalid_json_from_generate() {
        let mut registry = ToolRegistry::default();
        registry
            .register(StaticTool {
                id: "ok",
                capability: "files",
                output: json!({}),
                fail: false,
            })
            .unwrap();
        let model = EchoModel {
            content: "definitely not json".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));
        let executor = TreeExecutor::new(
            Arc::new(registry),
            test_gate(),
            cascade,
            Arc::new(AcceptAll),
        );

        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "gen".into(),
                    instruction: "produce json".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({}),
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "gen".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let result = executor
            .run(&validated(plan, &good_registry()), no_cancel())
            .await
            .unwrap();

        assert_eq!(result.statuses.get("check"), Some(&NodeStatus::Failed));
        assert_eq!(result.statuses.get("root"), Some(&NodeStatus::Failed));
    }

    #[tokio::test]
    async fn generate_can_escalate_inside_a_tree_via_the_cascade() {
        // The verifier rejects the first response (fast tier) and accepts
        // the next, forcing a Fast -> Reasoner escalation mid-tree; the
        // reasoner echo model answers with valid JSON.
        let mut registry = ToolRegistry::default();
        registry
            .register(StaticTool {
                id: "ok",
                capability: "files",
                output: json!({}),
                fail: false,
            })
            .unwrap();
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
        let registry = Arc::new(registry);
        let executor = TreeExecutor::new(
            Arc::clone(&registry),
            test_gate(),
            cascade,
            Arc::new(RetryThenAccept {
                remaining_retries: Mutex::new(1),
            }),
        );

        let generate = PlanNode::Generate {
            id: "gen".into(),
            instruction: "produce json".into(),
            tier: ModelTier::Fast,
            input: json!({}),
        };

        let result = executor
            .run(&validated(generate, &registry), no_cancel())
            .await
            .unwrap();

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
            .run(&validated(plan.clone(), &good_registry()), no_cancel())
            .await
            .unwrap();
        assert_eq!(blocked.statuses.get("ask"), Some(&NodeStatus::Blocked));

        let mut registry = ToolRegistry::default();
        registry
            .register(StaticTool {
                id: "ok",
                capability: "files",
                output: json!({}),
                fail: false,
            })
            .unwrap();
        let model = EchoModel {
            content: "x".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));
        let executor = TreeExecutor::new(
            Arc::new(registry),
            test_gate(),
            cascade,
            Arc::new(AcceptAll),
        )
        .with_ask_handler(Arc::new(AskingUser {
            answer: Some("notes.txt".to_owned()),
        }));

        let answered = executor
            .run(&validated(plan, &good_registry()), no_cancel())
            .await
            .unwrap();
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
                    tool_version: "1".to_owned(),
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
        registry
            .register(CancelAfterCall {
                flag: Arc::clone(&flag),
            })
            .unwrap();
        registry
            .register(StaticTool {
                id: "ok",
                capability: "files",
                output: json!({}),
                fail: false,
            })
            .unwrap();
        let model = EchoModel {
            content: "x".to_owned(),
            calls: Mutex::new(0),
        };
        let cascade = Arc::new(ComputeCascade::empty().with_reasoner(model));

        let registry = Arc::new(registry);
        let executor = TreeExecutor::new(
            Arc::clone(&registry),
            test_gate(),
            cascade,
            Arc::new(AcceptAll),
        );
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                tool_node("trigger", "files", "cancelme"),
                tool_node("after", "files", "ok"),
            ],
        };

        let result = executor
            .run(&validated(plan, &registry), Arc::clone(&flag))
            .await
            .unwrap();

        assert!(result.cancelled);
        assert_eq!(result.statuses.get("trigger"), Some(&NodeStatus::Succeeded));
        assert!(!result.statuses.contains_key("after"));
        assert!(!result.statuses.contains_key("root"));
    }

    fn no_cancel() -> CancelFlag {
        Arc::new(AtomicBool::new(false))
    }

    /// Validate a plan the way production does before execution.
    fn validated(plan: PlanNode, registry: &Arc<ToolRegistry>) -> crate::ValidatedPlan {
        validate_plan(
            &plan,
            registry,
            &[ModelTier::Fast, ModelTier::Standard, ModelTier::Reasoner],
        )
        .unwrap_or_else(|err| panic!("test plan failed validation: {err}"));
        crate::ValidatedPlan::from_validated(plan, 1).unwrap()
    }

    // --- artifact bindings (#19) ----------------------------------------

    /// Build the read -> summarize fragment with a typed reference.
    fn read_then_summarize(note: Value) -> (Arc<ToolRegistry>, PlanNode) {
        let mut registry = ToolRegistry::default();
        registry
            .register(FixtureTool {
                id: "read_note",
                capability: "files",
                output: note,
            })
            .unwrap();
        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Tool {
                    id: "note".into(),
                    capability: "files".into(),
                    tool_id: "read_note".into(),
                    input: json!({}),
                },
                PlanNode::Generate {
                    id: "summarize".into(),
                    instruction: "summarize the note".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({ "note": { "$ref": "note", "kind": "json" } }),
                },
            ],
        };
        (Arc::new(registry), plan)
    }

    #[tokio::test]
    async fn changing_the_read_result_changes_the_exact_model_input() {
        // The acceptance test for #19: the read result is what the model
        // sees, and changing it changes exactly that input.
        let model_seen = |note: Value| async move {
            let (registry, plan) = read_then_summarize(note);
            let model = Arc::new(RecordingModel {
                content: "summary".into(),
                seen: Mutex::new(Vec::new()),
            });
            let cascade = Arc::new(ComputeCascade::empty().with_reasoner(Arc::clone(&model)));
            let executor = TreeExecutor::new(
                Arc::clone(&registry),
                test_gate(),
                cascade,
                Arc::new(AcceptAll),
            );

            executor
                .run(&validated(plan, &registry), no_cancel())
                .await
                .unwrap();

            let seen = model.seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "one model call");
            seen[0].clone()
        };

        let first = model_seen(json!({ "content": "alpha" })).await;
        let second = model_seen(json!({ "content": "beta" })).await;

        assert_eq!(first, json!({ "note": { "content": "alpha" } }));
        assert_ne!(first, second);
        assert_eq!(second, json!({ "note": { "content": "beta" } }));
    }

    #[test]
    fn missing_wrong_type_and_impossible_order_references_are_rejected() {
        let (registry, _) = read_then_summarize(json!({}));

        // Missing: references a node that does not exist.
        let missing = PlanNode::Sequence {
            id: "root".into(),
            children: vec![PlanNode::Generate {
                id: "summarize".into(),
                instruction: "x".into(),
                tier: ModelTier::Reasoner,
                input: json!({ "note": { "$ref": "nope", "kind": "json" } }),
            }],
        };
        assert!(matches!(
            validate_plan(&missing, &registry, &[ModelTier::Reasoner]).unwrap_err(),
            PlanError::UnknownReference { .. }
        ));

        // Impossible order: the consumer runs before its producer.
        let forward = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "summarize".into(),
                    instruction: "x".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({ "note": { "$ref": "note", "kind": "json" } }),
                },
                PlanNode::Tool {
                    id: "note".into(),
                    capability: "files".into(),
                    tool_id: "read_note".into(),
                    input: json!({}),
                },
            ],
        };
        assert!(matches!(
            validate_plan(&forward, &registry, &[ModelTier::Reasoner]).unwrap_err(),
            PlanError::ImpossibleOrder { .. }
        ));

        // Self reference.
        let selfish = PlanNode::Generate {
            id: "loop".into(),
            instruction: "x".into(),
            tier: ModelTier::Reasoner,
            input: json!({ "me": { "$ref": "loop", "kind": "text" } }),
        };
        assert!(matches!(
            validate_plan(&selfish, &registry, &[ModelTier::Reasoner]).unwrap_err(),
            PlanError::SelfReference { .. }
        ));

        // Parallel siblings cannot consume each other.
        let siblings = PlanNode::Parallel {
            id: "root".into(),
            children: vec![
                PlanNode::Generate {
                    id: "a".into(),
                    instruction: "x".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({}),
                },
                PlanNode::Generate {
                    id: "b".into(),
                    instruction: "y".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({ "peer": { "$ref": "a", "kind": "text" } }),
                },
            ],
        };
        assert!(matches!(
            validate_plan(&siblings, &registry, &[ModelTier::Reasoner]).unwrap_err(),
            PlanError::ImpossibleOrder { .. }
        ));
    }

    #[test]
    fn wrong_type_reference_is_rejected_before_any_effect() {
        // A tool producing JSON referenced where the consumer demands
        // text is caught at *validation*: the plan never becomes
        // executable, so no tool call happens at all.
        let mut registry = ToolRegistry::default();
        registry
            .register(FixtureTool {
                id: "read_note",
                capability: "files",
                output: json!({ "content": "alpha" }),
            })
            .unwrap();
        let registry = Arc::new(registry);

        let plan = PlanNode::Sequence {
            id: "root".into(),
            children: vec![
                PlanNode::Tool {
                    id: "note".into(),
                    capability: "files".into(),
                    tool_id: "read_note".into(),
                    input: json!({}),
                },
                PlanNode::Generate {
                    id: "summarize".into(),
                    instruction: "x".into(),
                    tier: ModelTier::Reasoner,
                    input: json!({ "note": { "$ref": "note", "kind": "text" } }),
                },
            ],
        };

        let err = validate_plan(&plan, &registry, &[ModelTier::Reasoner]).unwrap_err();
        assert_eq!(
            err,
            PlanError::ReferenceType {
                id: "summarize".to_owned(),
                target: "note".to_owned(),
                expected: ArtifactKind::Text,
                actual: ArtifactKind::Json,
            }
        );
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
            input: json!({}),
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
                    input: json!({}),
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
                    input: json!({}),
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
                    input: json!({}),
                },
                PlanNode::Verify {
                    id: "check".into(),
                    target: "summarize".into(),
                    artifact: ExpectedArtifact::Json,
                },
            ],
        };

        let executor = executor(good_registry());
        let first = executor
            .run(&validated(make_plan(), &good_registry()), no_cancel())
            .await
            .unwrap();
        let second = executor
            .run(&validated(make_plan(), &good_registry()), no_cancel())
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.statuses.get("root"), Some(&NodeStatus::Succeeded));
        assert_eq!(first.statuses.get("try_cache"), Some(&NodeStatus::Failed));
        assert_eq!(first.statuses.get("try_disk"), Some(&NodeStatus::Succeeded));
    }
}
