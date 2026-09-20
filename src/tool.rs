use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::KnutError;

/// What executing a tool does to the world.
///
/// Policy (issue #10) reads this class; tools merely declare it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        let metadata = tool.metadata();
        self.by_capability
            .entry(metadata.capability.clone())
            .or_default()
            .push(ToolEntry {
                metadata,
                tool: Arc::new(tool),
            });
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

    /// Look up a tool by ID, but only inside the chosen capability.
    ///
    /// A model naming a real tool under the wrong capability is rejected:
    /// unknown IDs cannot be smuggled across capability boundaries.
    pub fn find_exact(&self, capability: &str, tool_id: &str) -> Result<Arc<dyn Tool>, KnutError> {
        let Some(entries) = self.by_capability.get(capability) else {
            return Err(KnutError::ToolNotFound(format!(
                "capability {capability:?} has no tools"
            )));
        };

        entries
            .iter()
            .find(|entry| entry.metadata.id == tool_id)
            .map(|entry| entry.tool.clone())
            .ok_or_else(|| {
                KnutError::ToolNotFound(format!(
                    "tool {tool_id:?} not found in capability {capability:?}"
                ))
            })
    }
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
        registry.register(tool("read", "files", "read a file"));
        registry.register(tool("write", "files", "write a file"));
        registry.register(tool("search", "web", "search the web"));

        assert_eq!(
            registry.capabilities(),
            vec!["files".to_owned(), "web".to_owned()]
        );
    }

    #[test]
    fn exact_lookup_respects_capability_boundaries() {
        let mut registry = ToolRegistry::default();
        registry.register(tool("read", "files", "read a file"));
        registry.register(tool("search", "web", "search the web"));

        assert!(registry.find_exact("files", "read").is_ok());
        assert!(registry.find_exact("web", "read").is_err());
        assert!(registry.find_exact("files", "missing").is_err());
    }

    #[tokio::test]
    async fn exact_lookup_returns_callable_tool() {
        let mut registry = ToolRegistry::default();
        registry.register(tool("read", "files", "read a file"));

        let found = registry.find_exact("files", "read").unwrap();
        let output = found.call(json!({ "path": "x" })).await.unwrap();

        assert_eq!(output, json!({ "tool": "read" }));
    }

    #[test]
    fn candidates_are_bounded_and_ranked() {
        let mut registry = ToolRegistry::default();
        for i in 0..30 {
            registry.register(tool(
                Box::leak(format!("tool_{i:02}").into_boxed_str()),
                "files",
                "generic file operation",
            ));
        }
        registry.register(tool("read_file", "files", "read a file from disk"));
        registry.register(tool("delete_file", "files", "delete a file from disk"));

        let candidates = registry.top_k_candidates("files", "read file", 3);

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].id, "read_file");

        let all = registry.tools_for_capability("files");
        assert_eq!(all.len(), 32);
    }

    #[test]
    fn metadata_carries_side_effect_class() {
        let mut registry = ToolRegistry::default();
        registry.register(FakeTool {
            id: "send",
            capability: "email",
            description: "send an email",
            side_effect: SideEffect::NonIdempotentWrite,
        });

        let candidates = registry.top_k_candidates("email", "send", 5);

        assert_eq!(candidates[0].side_effect, SideEffect::NonIdempotentWrite);
    }
}
