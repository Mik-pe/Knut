use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::KnutError;

#[derive(Debug, Default)]
pub(super) struct WriteFailures(BTreeMap<(String, Option<String>), String>);

impl WriteFailures {
    pub(super) fn record(&mut self, tool: &str, arguments: &Value, error: &KnutError) {
        let scope = path_scope(arguments).or_else(|| {
            (!matches!(error, KnutError::InvalidArguments { .. }))
                .then(|| argument_scope(arguments))
        });
        self.0.insert(
            (tool.to_owned(), scope),
            crate::recovery::bounded_tail(&error.to_string(), 2048),
        );
    }

    pub(super) fn resolved(&mut self, tool: &str, arguments: &Value) {
        let scope = path_scope(arguments).unwrap_or_else(|| argument_scope(arguments));
        self.0.remove(&(tool.to_owned(), Some(scope)));
        // Missing or malformed target arguments cannot identify an operation.
        // A valid same-tool invocation repairs only that unscoped validation failure.
        self.0.remove(&(tool.to_owned(), None));
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}

fn path_scope(arguments: &Value) -> Option<String> {
    let path = arguments.get("path")?.as_str()?;
    if path.trim().is_empty() {
        return None;
    }
    let normalized: PathBuf = Path::new(path)
        .components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect();
    Some(format!("path:{}", normalized.to_string_lossy()))
}

fn argument_scope(arguments: &Value) -> String {
    format!(
        "arguments:{}",
        crate::content_hash(arguments.to_string().as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn successful_writes_resolve_only_the_same_file_or_operation() {
        let mut failures = WriteFailures::default();
        let error = KnutError::Tool("write failed".into());
        failures.record("write", &json!({"path":"a.txt", "content":"bad"}), &error);
        failures.resolved("write", &json!({"path":"b.txt", "content":"good"}));
        assert!(!failures.is_empty());
        failures.resolved("write", &json!({"path":"./a.txt", "content":"fixed"}));
        assert!(failures.is_empty());

        failures.record("save", &json!({"document":"first"}), &error);
        failures.resolved("save", &json!({"document":"other"}));
        assert!(!failures.is_empty());
        failures.resolved("save", &json!({"document":"first"}));
        assert!(failures.is_empty());
    }

    #[test]
    fn generic_malformed_arguments_can_be_repaired_without_clearing_targeted_failures() {
        let mut failures = WriteFailures::default();
        let error = KnutError::InvalidArguments {
            path: "$.text".into(),
            reason: "required".into(),
        };
        failures.record("save", &json!({}), &error);
        failures.resolved("other", &json!({"text":"fixed"}));
        assert!(!failures.is_empty());
        failures.resolved("save", &json!({"text":"fixed"}));
        assert!(failures.is_empty());

        failures.record("write", &json!({"path":"a.txt"}), &error);
        failures.resolved("write", &json!({"path":"b.txt", "text":"fixed"}));
        assert!(!failures.is_empty());
        failures.resolved("write", &json!({"path":"a.txt", "text":"fixed"}));
        assert!(failures.is_empty());
    }
}
