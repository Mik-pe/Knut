use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::policy::{ExecOutcome, ExecutionGate};
use crate::{KnutError, Risk};

/// What executing a tool does to the world.
///
/// Policy (issue #10) reads this class; tools merely declare it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// Pure read; safe to retry at will.
    ReadOnly,
    /// A write that tolerates replay.
    IdempotentWrite,
    /// A write that must not be silently repeated.
    NonIdempotentWrite,
}

/// Static description of a tool, cheap enough to hand to a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Globally unique tool ID.
    pub id: String,
    /// Implementation version; part of the execution fingerprint, so a
    /// changed implementation invalidates stale approvals and replays.
    pub tool_version: String,
    /// Capability this tool belongs to; the routing-level grouping.
    pub capability: String,
    /// One-line human/model-readable summary.
    pub description: String,
    /// JSON Schema (or schema-ish object) for the tool input.
    pub input_schema: Value,
    pub side_effect: SideEffect,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn metadata(&self) -> ToolMetadata;

    async fn call(&self, input: Value) -> Result<Value, KnutError>;
}

/// A tool as stored in the registry.
#[derive(Clone)]
struct ToolEntry {
    metadata: ToolMetadata,
    tool: Arc<dyn Tool>,
}

/// Capability-indexed tool registry.
///
/// Selection is hierarchical: pick a capability, then choose an exact tool
/// from that capability's bounded candidate set. Full input schemas are only
/// serialized for the candidates, never for the whole catalog.
#[derive(Default)]
pub struct ToolRegistry {
    by_capability: HashMap<String, Vec<ToolEntry>>,
}

impl ToolRegistry {
    /// Register a tool after freezing its metadata.
    ///
    /// Rejected at registration, never silently weakened later:
    /// - duplicate tool identity (same id in any capability),
    /// - input schemas outside the supported subset,
    /// - empty id or capability.
    pub fn register(&mut self, tool: impl Tool + 'static) -> Result<(), KnutError> {
        let metadata = tool.metadata();

        if metadata.id.trim().is_empty() || metadata.capability.trim().is_empty() {
            return Err(KnutError::InvalidArguments {
                path: "$".to_owned(),
                reason: "tool id and capability must be non-empty".to_owned(),
            });
        }

        // The empty object validates nothing but is a legal, if loose,
        // declared schema; anything unsupported is refused now.
        validate_arguments(&metadata.input_schema, &serde_json::json!({})).map_err(|_| {
            KnutError::InvalidArguments {
                path: "$".to_owned(),
                reason: format!(
                    "tool {:?} declares an input schema outside the supported subset",
                    metadata.id
                ),
            }
        })?;

        for entries in self.by_capability.values() {
            if entries.iter().any(|e| e.metadata.id == metadata.id) {
                return Err(KnutError::InvalidArguments {
                    path: "$".to_owned(),
                    reason: format!("duplicate tool identity {:?}", metadata.id),
                });
            }
        }

        self.by_capability
            .entry(metadata.capability.clone())
            .or_default()
            .push(ToolEntry {
                metadata,
                tool: Arc::new(tool),
            });

        Ok(())
    }

    /// All capability IDs, sorted.
    pub fn capabilities(&self) -> Vec<String> {
        let mut capabilities: Vec<_> = self.by_capability.keys().cloned().collect();
        capabilities.sort();
        capabilities
    }

    /// Metadata for every tool in one capability, sorted by tool ID.
    ///
    /// This is the bounded candidate set shown to the chooser.
    pub fn tools_for_capability(&self, capability: &str) -> Vec<ToolMetadata> {
        let mut entries: Vec<_> = self
            .by_capability
            .get(capability)
            .map(|entries| entries.iter().map(|e| e.metadata.clone()).collect())
            .unwrap_or_default();

        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// Top-k candidates within a capability, ranked by word overlap between
    /// the query and tool ID/description. Ties break by tool ID for
    /// determinism.
    pub fn top_k_candidates(&self, capability: &str, query: &str, k: usize) -> Vec<ToolMetadata> {
        let query_terms = terms(query);
        let mut scored: Vec<(usize, &ToolEntry)> = self
            .by_capability
            .get(capability)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| (score_entry(entry, &query_terms), entry))
                    .collect()
            })
            .unwrap_or_default();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.metadata.id.cmp(&b.1.metadata.id))
        });
        scored.truncate(k);

        scored
            .into_iter()
            .map(|(_, entry)| entry.metadata.clone())
            .collect()
    }

    /// Freeze the authoritative metadata for one registered tool.
    ///
    /// The only way to reach a tool's executable side is through
    /// [`ToolRegistry::invoke`]; lookup methods expose metadata alone, so
    /// discovery never grants an execution capability.
    pub fn find_exact(&self, capability: &str, tool_id: &str) -> Result<ToolMetadata, KnutError> {
        let entry = self.find_entry(capability, tool_id)?;
        Ok(entry.metadata.clone())
    }

    fn find_entry(&self, capability: &str, tool_id: &str) -> Result<&ToolEntry, KnutError> {
        let Some(entries) = self.by_capability.get(capability) else {
            return Err(KnutError::ToolNotFound(format!(
                "capability {capability:?} has no tools"
            )));
        };

        entries
            .iter()
            .find(|entry| entry.metadata.id == tool_id)
            .ok_or_else(|| {
                KnutError::ToolNotFound(format!(
                    "tool {tool_id:?} not found in capability {capability:?}"
                ))
            })
    }

    /// The single execution path: gate-checked, schema-validated, and
    /// audit-logged. `execute` is the raw implementation, reachable only
    /// from inside the crate after policy approval.
    /// Invoke without content preconditions (the common case for tools
    /// whose arguments reference nothing external).
    pub(crate) async fn invoke(
        &self,
        gate: &ExecutionGate,
        capability: &str,
        tool_id: &str,
        input: Value,
        idempotency_key: Option<String>,
        risk: Risk,
    ) -> Result<ExecOutcome, KnutError> {
        self.invoke_with_preconditions(
            gate,
            &InvocationRequest {
                capability,
                tool_id,
                input,
                preconditions: &crate::policy::ContentPreconditions::none(),
                idempotency_key,
                risk,
            },
        )
        .await
    }

    pub(crate) async fn invoke_with_preconditions(
        &self,
        gate: &ExecutionGate,
        invocation: &InvocationRequest<'_>,
    ) -> Result<ExecOutcome, KnutError> {
        let InvocationRequest {
            capability,
            tool_id,
            input,
            preconditions,
            idempotency_key,
            risk,
        } = invocation;
        let (capability, tool_id, input, idempotency_key, risk) = (
            *capability,
            *tool_id,
            input,
            idempotency_key.as_deref(),
            *risk,
        );
        // Unknown identity is a policy denial (issue #14: unknown
        // capabilities/tools are denied, not "not found" into an
        // escalate-elsewhere path). Metadata lookup stays available for
        // discovery; the execution route never exposes absence details.
        let Ok(entry) = self.find_entry(capability, tool_id) else {
            return Err(KnutError::PolicyDenied {
                reason: format!("tool {tool_id:?} is not available in capability {capability:?}"),
            });
        };

        // Supported-schema check against the registered schema.
        validate_arguments(&entry.metadata.input_schema, input).map_err(KnutError::from)?;

        let authorization = gate
            .authorize_with_preconditions(
                &entry.metadata,
                input,
                preconditions,
                idempotency_key,
                risk,
            )
            .await?;

        let outcome = match authorization {
            crate::policy::Authorization::Replay(outcome) => outcome,
            crate::policy::Authorization::Execute { fingerprint } => {
                gate.execute(entry.tool.as_ref(), fingerprint, input)
                    .await?
            }
        };

        // Audit line: identity and outcome class only, never payloads.
        tracing::info!(
            capability = %capability,
            tool_id = %tool_id,
            replayed = %outcome.replayed,
            "tool invocation authorized and executed"
        );

        Ok(outcome)
    }
}

/// Everything one gated invocation needs.
pub struct InvocationRequest<'a> {
    pub capability: &'a str,
    pub tool_id: &'a str,
    pub input: Value,
    pub preconditions: &'a crate::policy::ContentPreconditions,
    pub idempotency_key: Option<String>,
    pub risk: Risk,
}

/// The JSON Schema subset Knut validates: type, required, enum,
/// properties with recursion, and array items. Schemas using anything
/// else are rejected at registration instead of silently weakening
/// validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaError {
    pub path: String,
    pub reason: String,
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl From<SchemaError> for KnutError {
    fn from(e: SchemaError) -> Self {
        KnutError::InvalidArguments {
            path: e.path,
            reason: e.reason,
        }
    }
}

pub fn validate_arguments(schema: &Value, input: &Value) -> Result<(), SchemaError> {
    fn check(
        schema: &Value,
        input: &Value,
        path: &str,
        allow_non_object: bool,
    ) -> Result<(), SchemaError> {
        let err = |reason: &str| SchemaError {
            path: path.to_owned(),
            reason: reason.to_owned(),
        };

        let schema_type = schema
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| err("schema missing supported \"type\""))?;

        if schema_type != "object" {
            if allow_non_object && schema_type == "string" {
                // Leaf at the top level only; values below the root must
                // live in properties.
                return Ok(());
            }
            return Err(err(&format!("unsupported schema type {schema_type:?}")));
        }

        let Some(object) = input.as_object() else {
            return Err(err("expected a JSON object"));
        };

        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
        {
            if !object.contains_key(required) {
                return Err(err(&format!("missing required field {required:?}")));
            }
        }

        if let Some(enum_values) = schema.get("enum").and_then(Value::as_array)
            && !enum_values.contains(input)
        {
            return Err(err("value is not one of the enum options"));
        }

        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return Ok(());
        };

        for (key, field_schema) in properties {
            if let Some(value) = object.get(key) {
                let leaf_type = field_schema.get("type").and_then(Value::as_str);
                match leaf_type {
                    Some("string") | Some("number") | Some("boolean") => {
                        // Leaf check: only type compatibility, no recursion.
                        let ok = match leaf_type {
                            Some("string") => value.is_string(),
                            Some("number") => value.is_number(),
                            _ => value.is_boolean(),
                        };
                        if !ok {
                            return Err(err(&format!("field {key:?} has the wrong JSON type")));
                        }
                    }
                    Some("object") => {
                        check(field_schema, value, &format!("{path}.{key}"), false)?;
                    }
                    Some("array") => {
                        let items = field_schema
                            .get("items")
                            .ok_or_else(|| err("array schema missing supported \"items\""))?;
                        let Some(list) = value.as_array() else {
                            return Err(err(&format!("field {key:?} is not an array")));
                        };
                        for (i, item) in list.iter().enumerate() {
                            check(items, item, &format!("{path}.{key}[{i}]"), false)?;
                        }
                    }
                    other => {
                        return Err(err(&format!(
                            "unsupported schema type {}",
                            other.unwrap_or("<missing>")
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    check(schema, input, "$", true)
}

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn score_entry(entry: &ToolEntry, query_terms: &[String]) -> usize {
    let haystack = terms(&format!(
        "{} {}",
        entry.metadata.id, entry.metadata.description
    ));
    haystack
        .iter()
        .filter(|term| query_terms.contains(term))
        .count()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct FakeTool {
        id: &'static str,
        capability: &'static str,
        description: &'static str,
        side_effect: SideEffect,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                id: self.id.to_owned(),
                tool_version: "1".to_owned(),
                capability: self.capability.to_owned(),
                description: self.description.to_owned(),
                input_schema: json!({ "type": "object" }),
                side_effect: self.side_effect,
            }
        }

        async fn call(&self, _input: Value) -> Result<Value, KnutError> {
            Ok(json!({ "tool": self.id }))
        }
    }

    fn tool(id: &'static str, capability: &'static str, description: &'static str) -> FakeTool {
        FakeTool {
            id,
            capability,
            description,
            side_effect: SideEffect::ReadOnly,
        }
    }

    #[test]
    fn capabilities_are_sorted_and_unique() {
        let mut registry = ToolRegistry::default();
        registry
            .register(tool("read", "files", "read a file"))
            .unwrap();
        registry
            .register(tool("write", "files", "write a file"))
            .unwrap();
        registry
            .register(tool("search", "web", "search the web"))
            .unwrap();

        assert_eq!(
            registry.capabilities(),
            vec!["files".to_owned(), "web".to_owned()]
        );
    }

    #[test]
    fn exact_lookup_respects_capability_boundaries() {
        let mut registry = ToolRegistry::default();
        registry
            .register(tool("read", "files", "read a file"))
            .unwrap();
        registry
            .register(tool("search", "web", "search the web"))
            .unwrap();

        assert!(registry.find_exact("files", "read").is_ok());
        assert!(registry.find_exact("web", "read").is_err());
        assert!(registry.find_exact("files", "missing").is_err());
    }

    /// Discovery returns metadata only; execution goes through a gate.
    #[tokio::test]
    async fn lookup_grants_metadata_not_execution() {
        use crate::Risk;
        use crate::policy::ExecutionGate;

        let mut registry = ToolRegistry::default();
        registry
            .register(tool("read", "files", "read a file"))
            .unwrap();

        // Metadata lookup: identity and schema, no callable handle.
        let metadata = registry.find_exact("files", "read").unwrap();
        assert_eq!(metadata.id, "read");
        assert_eq!(metadata.side_effect, SideEffect::ReadOnly);

        // Execution requires a gate and flows through invoke.
        let gate = ExecutionGate::new(crate::SideEffectPolicy::new().allow(SideEffect::ReadOnly));
        let outcome = registry
            .invoke(
                &gate,
                "files",
                "read",
                json!({ "path": "x" }),
                None,
                Risk::Low,
            )
            .await
            .unwrap();

        assert_eq!(outcome.output, json!({ "tool": "read" }));
        assert!(!outcome.replayed);
    }

    #[test]
    fn candidates_are_bounded_and_ranked() {
        let mut registry = ToolRegistry::default();
        for i in 0..30 {
            registry
                .register(tool(
                    Box::leak(format!("tool_{i:02}").into_boxed_str()),
                    "files",
                    "generic file operation",
                ))
                .unwrap();
        }
        registry
            .register(tool("read_file", "files", "read a file from disk"))
            .unwrap();
        registry
            .register(tool("delete_file", "files", "delete a file from disk"))
            .unwrap();

        let candidates = registry.top_k_candidates("files", "read file", 3);

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].id, "read_file");

        let all = registry.tools_for_capability("files");
        assert_eq!(all.len(), 32);
    }

    #[test]
    fn metadata_carries_side_effect_class() {
        let mut registry = ToolRegistry::default();
        registry
            .register(FakeTool {
                id: "send",
                capability: "email",
                description: "send an email",
                side_effect: SideEffect::NonIdempotentWrite,
            })
            .unwrap();

        let candidates = registry.top_k_candidates("email", "send", 5);

        assert_eq!(candidates[0].side_effect, SideEffect::NonIdempotentWrite);
    }
}
