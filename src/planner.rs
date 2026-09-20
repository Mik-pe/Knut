use serde::Serialize;
use serde_json::Value;

use crate::model::{ExpectedArtifact, ModelRequest};
use crate::tool::ToolRegistry;
use crate::tree::{PlanError, PlanNode, validate_plan};
use crate::{ComputeCascade, KnutError, ModelTier, Verifier};

/// Hard structural limits for System Two output.
///
/// A plan that exceeds these is rejected before validation; there is no
/// configuration knob that turns them off.
pub const MAX_NODES: usize = 32;
pub const MAX_DEPTH: usize = 6;

/// What the planner needs beyond the raw prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanningContext {
    pub goal: String,
    pub capabilities: Vec<String>,
    /// Compact tool catalog: id + capability + description + side effect.
    pub tool_catalog: Vec<String>,
    pub constraints: Vec<String>,
}

impl PlanningContext {
    /// Build a context from the registry: only real, configured IDs.
    pub fn from_registry(goal: impl Into<String>, registry: &ToolRegistry) -> Self {
        let mut capabilities = registry.capabilities();
        let mut tool_catalog = Vec::new();

        for capability in &capabilities {
            for metadata in registry.tools_for_capability(capability) {
                tool_catalog.push(format!(
                    "{} [{}] {} ({:?})",
                    metadata.id, metadata.capability, metadata.description, metadata.side_effect
                ));
            }
        }
        capabilities.sort();

        Self {
            goal: goal.into(),
            capabilities,
            tool_catalog,
            constraints: vec![
                format!("at most {MAX_NODES} nodes"),
                format!("nesting depth at most {MAX_DEPTH}"),
                "use only listed tool ids under their own capability".to_owned(),
            ],
        }
    }
}

/// A validated plan plus the attempts it took to get there.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedPlan {
    pub plan: PlanNode,
    /// How many model rounds were spent (1 initial + up to 1 repair).
    pub rounds: usize,
    /// Structural stats for traces.
    pub node_count: usize,
    pub depth: usize,
}

/// The reason an invalid plan was rejected.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanRejection {
    /// The JSON did not parse into the node schema at all.
    NotPlanJson,
    /// Schema parsed but violated structural limits or references.
    Invalid { errors: Vec<String> },
}

/// How many nodes and how deep a plan is.
fn measure(plan: &PlanNode) -> (usize, usize) {
    fn walk(node: &PlanNode, depth: usize, nodes: &mut usize, max_depth: &mut usize) {
        *nodes += 1;
        *max_depth = (*max_depth).max(depth);
        for child in node.children() {
            walk(child, depth + 1, nodes, max_depth);
        }
    }

    let (mut nodes, mut max_depth) = (0, 0);
    walk(plan, 1, &mut nodes, &mut max_depth);
    (nodes, max_depth)
}

/// System Two: produce a validated plan, or fail loudly.
///
/// The planner is an escalation path, never the control loop. It gets the
/// goal, the real capability catalog, and hard constraints, and must
/// answer with plan JSON using only known IDs. Invalid output gets one
/// repair round with the concrete errors attached, then a hard
/// `PlanRejected` failure — the runtime never "just tries" a bad tree.
pub struct Planner {
    cascade: ComputeCascade,
}

impl Planner {
    pub fn new(cascade: ComputeCascade) -> Self {
        Self { cascade }
    }

    fn planning_request(
        context: &PlanningContext,
        repair_errors: Option<&[String]>,
    ) -> ModelRequest {
        let mut payload = serde_json::json!({
            "goal": context.goal,
            "capabilities": context.capabilities,
            "tools": context.tool_catalog,
            "constraints": context.constraints,
            "expected_format": {
                "type": "sequence|selector|parallel|tool|generate|verify|ask_user",
                "note": "single root node; tool nodes need capability + tool_id; \
                         verify nodes reference an existing node id"
            },
        });

        if let Some(errors) = repair_errors {
            payload["previous_errors"] = serde_json::to_value(errors).unwrap_or(Value::Null);
        }

        ModelRequest::new(
            "Produce a behavior-tree plan as JSON. \
             Use only the listed tool ids under their listed capabilities.",
            ExpectedArtifact::Json,
        )
        .with_input(payload)
    }

    fn parse_plan(content: &str) -> Result<PlanNode, PlanRejection> {
        // The model may wrap the JSON in prose or fences; require a JSON
        // object somewhere in the response.
        let candidate = content.trim();
        let candidate = candidate
            .strip_prefix("```json")
            .and_then(|c| c.strip_suffix("```"))
            .map(str::trim)
            .unwrap_or(candidate);

        let value: Value =
            serde_json::from_str(candidate).map_err(|_| PlanRejection::NotPlanJson)?;

        serde_json::from_value(value).map_err(|_| PlanRejection::NotPlanJson)
    }

    fn check_limits(plan: &PlanNode) -> Result<(), Vec<String>> {
        let (nodes, depth) = measure(plan);
        let mut errors = Vec::new();
        if nodes > MAX_NODES {
            errors.push(format!("plan has {nodes} nodes; limit is {MAX_NODES}"));
        }
        if depth > MAX_DEPTH {
            errors.push(format!("plan depth {depth}; limit is {MAX_DEPTH}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Ask the reasoner for a plan and validate it end to end.
    ///
    /// Validation order: parse as plan JSON, enforce structural limits,
    /// then full semantic validation against the registry (unknown tools,
    /// wrong capabilities, dangling verify targets, unconfigured tiers).
    pub async fn plan(
        &self,
        context: &PlanningContext,
        registry: &ToolRegistry,
        tiers: &[ModelTier],
        verifier: &dyn Verifier,
    ) -> Result<ValidatedPlan, KnutError> {
        let request = Self::planning_request(context, None);

        let outcome = self
            .cascade
            .run(&request, ModelTier::Reasoner, verifier)
            .await?;

        let first_round = match Self::parse_plan(&outcome.response.content) {
            Ok(plan) => self.finish(plan, registry, tiers),
            Err(rejection) => {
                let errors = match rejection {
                    PlanRejection::NotPlanJson => {
                        vec!["response was not plan JSON".to_owned()]
                    }
                    PlanRejection::Invalid { errors } => errors,
                };
                Err(KnutError::PlanRejected { errors })
            }
        };

        match first_round {
            Ok(validated) => Ok(validated),
            Err(KnutError::PlanRejected { errors }) => {
                // One repair round, then hard fail.
                self.repair(context, registry, tiers, verifier, errors)
                    .await
            }
            Err(other) => Err(other),
        }
    }

    /// The single repair path: re-ask with the concrete errors attached.
    ///
    /// A structurally unparseable repair attempt fails immediately; a
    /// parseable one that still violates limits or validation fails with
    /// those errors. There is no second repair.
    async fn repair(
        &self,
        context: &PlanningContext,
        registry: &ToolRegistry,
        tiers: &[ModelTier],
        verifier: &dyn Verifier,
        errors: Vec<String>,
    ) -> Result<ValidatedPlan, KnutError> {
        let request = Self::planning_request(context, Some(&errors));
        let outcome = self
            .cascade
            .run(&request, ModelTier::Reasoner, verifier)
            .await?;

        let plan = match Self::parse_plan(&outcome.response.content) {
            Ok(plan) => plan,
            Err(_) => {
                // Keep the round-1 errors: the failure explains both rounds.
                let mut all = errors;
                all.push("repair attempt was not plan JSON".to_owned());
                return Err(KnutError::PlanRejected { errors: all });
            }
        };

        match self.finish(plan, registry, tiers) {
            Ok(mut validated) => {
                validated.rounds = 2;
                Ok(validated)
            }
            Err(KnutError::PlanRejected {
                errors: mut new_errors,
            }) => {
                // The failure explains both rounds.
                let mut all = errors;
                all.append(&mut new_errors);
                Err(KnutError::PlanRejected { errors: all })
            }
            Err(other) => Err(other),
        }
    }

    /// Shared tail: limits + semantic validation + stats.
    fn finish(
        &self,
        plan: PlanNode,
        registry: &ToolRegistry,
        tiers: &[ModelTier],
    ) -> Result<ValidatedPlan, KnutError> {
        if let Err(limit_errors) = Self::check_limits(&plan) {
            return Err(KnutError::PlanRejected {
                errors: limit_errors,
            });
        }

        validate_plan(&plan, registry, tiers).map_err(|err: PlanError| {
            KnutError::PlanRejected {
                errors: vec![err.to_string()],
            }
        })?;

        let (node_count, depth) = measure(&plan);
        Ok(ValidatedPlan {
            plan,
            rounds: 1,
            node_count,
            depth,
        })
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    use crate::model::{
        CascadeOutcome, Model, ModelIdentity, ModelRequest, ModelResponse, Usage,
        VerificationVerdict,
    };
    use crate::tool::{SideEffect, Tool, ToolMetadata};

    use super::*;

    /// Scripted reasoner: pops one response per cascade call.
    /// NOTE: the cascade stops escalating once a verdict is Sufficient,
    /// so one response per Sufficient-verdict call.
    struct ScriptedReasoner {
        responses: Mutex<Vec<String>>,
    }

    impl ScriptedReasoner {
        fn with(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedReasoner {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fake".to_owned(),
                model: "scripted".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, KnutError> {
            // Record whether repair errors were attached.
            if let Some(errors) = request.input.get("previous_errors") {
                assert!(errors.is_array());
            }

            let mut queue = self.responses.lock().unwrap();
            let content = if queue.is_empty() {
                "{\"type\":\"generate\"}".to_owned() // invalid: no id
            } else {
                queue.remove(0)
            };

            Ok(ModelResponse {
                content,
                identity: self.identity(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 10,
                },
                latency: std::time::Duration::ZERO,
            })
        }
    }

    struct AcceptAll;

    impl Verifier for AcceptAll {
        fn verify(&self, _r: &ModelResponse) -> VerificationVerdict {
            VerificationVerdict::Sufficient
        }
    }

    fn tool_registry() -> ToolRegistry {
        struct Fake;

        #[async_trait]
        impl Tool for Fake {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: "read_file".to_owned(),
                    tool_version: "1".to_owned(),
                    capability: "files".to_owned(),
                    description: "read a file".to_owned(),
                    input_schema: json!({ "type": "object" }),
                    side_effect: SideEffect::ReadOnly,
                }
            }

            async fn call(&self, _input: Value) -> Result<Value, KnutError> {
                Ok(json!({}))
            }
        }

        let mut registry = ToolRegistry::default();
        registry.register(Fake).unwrap();
        registry
    }

    fn planner_with(responses: Vec<String>) -> Planner {
        Planner::new(ComputeCascade::empty().with_reasoner(ScriptedReasoner::with(responses)))
    }

    fn good_plan_json() -> String {
        serde_json::to_string(&json!({
            "type": "sequence",
            "id": "root",
            "children": [
                {
                    "type": "tool",
                    "id": "fetch",
                    "capability": "files",
                    "tool_id": "read_file",
                    "input": { "path": "notes.txt" }
                },
                {
                    "type": "verify",
                    "id": "check",
                    "target": "fetch",
                    "artifact": "json"
                }
            ]
        }))
        .unwrap()
    }

    fn context() -> PlanningContext {
        PlanningContext::from_registry("summarize notes", &tool_registry())
    }

    #[tokio::test]
    async fn valid_plan_passes_in_one_round() {
        let planner = planner_with(vec![good_plan_json()]);

        let validated = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap();

        assert_eq!(validated.rounds, 1);
        assert_eq!(validated.node_count, 3);
        assert_eq!(validated.depth, 2);
        assert_eq!(validated.plan.id(), "root");
    }

    #[tokio::test]
    async fn invented_tool_ids_are_rejected_and_one_repair_is_offered() {
        let invented = json!({
            "type": "tool",
            "id": "fetch",
            "capability": "files",
            "tool_id": "delete_everything"
        })
        .to_string();

        let planner = planner_with(vec![invented, good_plan_json()]);

        let validated = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap();

        assert_eq!(validated.rounds, 2);
    }

    #[tokio::test]
    async fn second_invalid_plan_fails_hard_with_all_errors() {
        let invented = json!({
            "type": "tool",
            "id": "fetch",
            "capability": "files",
            "tool_id": "delete_everything"
        })
        .to_string();

        let planner = planner_with(vec![invented.clone(), invented]);

        let err = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap_err();

        match err {
            KnutError::PlanRejected { errors } => {
                assert!(
                    errors.len() >= 2,
                    "expected combined errors, got {errors:?}"
                );
                assert!(errors.iter().any(|e| e.contains("delete_everything")));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_repair_fails_immediately() {
        let planner = planner_with(vec![
            "I cannot produce JSON today, sorry".to_owned(),
            "still no json".to_owned(),
        ]);

        let err = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, KnutError::PlanRejected { .. }));
    }

    #[tokio::test]
    async fn oversized_plans_are_rejected_before_validation() {
        // 40 tool nodes over the limit of 32.
        let children: Vec<Value> = (0..40)
            .map(|i| {
                json!({
                    "type": "tool",
                    "id": format!("t{i}"),
                    "capability": "files",
                    "tool_id": "read_file",
                    "input": {}
                })
            })
            .collect();

        let plan = serde_json::to_string(&json!({
            "type": "parallel",
            "id": "root",
            "children": children
        }))
        .unwrap();

        let planner = planner_with(vec![plan]);

        let err = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap_err();

        match err {
            KnutError::PlanRejected { errors } => {
                // 40 children + the root: 41 nodes over the 32 limit.
                assert!(errors[0].contains("41 nodes"));
                assert!(errors[0].contains("32"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn too_deep_plans_are_rejected() {
        // Depth 7: sequence(1) -> selector(2) -> parallel(3) -> sequence(4)
        // -> selector(5) -> parallel(6) -> tool(7).
        let plan = serde_json::to_string(&json!({
            "type": "sequence", "id": "d1", "children": [{
                "type": "selector", "id": "d2", "children": [{
                    "type": "parallel", "id": "d3", "children": [{
                        "type": "sequence", "id": "d4", "children": [{
                            "type": "selector", "id": "d5", "children": [{
                                "type": "parallel", "id": "d6", "children": [{
                                    "type": "tool", "id": "deep_leaf",
                                    "capability": "files", "tool_id": "read_file",
                                    "input": {}
                                }]
                            }]
                        }]
                    }]
                }]
            }]
        }))
        .unwrap();

        let planner = planner_with(vec![plan]);

        let err = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap_err();

        match err {
            KnutError::PlanRejected { errors } => {
                assert!(errors[0].contains("depth"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn unconfigured_tiers_in_plans_are_rejected() {
        let plan = serde_json::to_string(&json!({
            "type": "generate",
            "id": "gen",
            "instruction": "hi",
            "tier": "fast"
        }))
        .unwrap();

        // Only Reasoner is configured for validation.
        let planner = planner_with(vec![plan]);

        let err = planner
            .plan(
                &context(),
                &tool_registry(),
                &[ModelTier::Reasoner],
                &AcceptAll,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, KnutError::PlanRejected { .. }));
    }

    #[test]
    fn context_lists_only_real_capabilities() {
        let context = context();

        assert_eq!(context.capabilities, vec!["files".to_owned()]);
        assert!(context.tool_catalog[0].contains("read_file"));
        assert!(context.constraints.iter().any(|c| c.contains("32")));
    }

    #[test]
    fn fenced_json_is_tolerated() {
        let fenced = format!("```json\n{}\n```", good_plan_json());
        let plan = Planner::parse_plan(&fenced);

        assert!(plan.is_ok());
    }

    /// The cascade's Sufficient verdict stops escalation, so the scripted
    /// model above answers exactly once per planning round.
    #[tokio::test]
    async fn cascade_returns_first_sufficient_response() {
        let cascade = ComputeCascade::empty().with_reasoner(ScriptedReasoner::with(vec![
            "first".to_owned(),
            "second".to_owned(),
        ]));

        let outcome: CascadeOutcome = cascade
            .run(
                &ModelRequest::new("x", ExpectedArtifact::Text),
                ModelTier::Reasoner,
                &AcceptAll,
            )
            .await
            .unwrap();

        assert_eq!(outcome.response.content, "first");
    }
}
