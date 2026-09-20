use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;

use crate::KnutError;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn capability(&self) -> &str;

    async fn call(&self, input: Value) -> Result<Value, KnutError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn capabilities(&self) -> Vec<String> {
        let mut capabilities: Vec<_> = self
            .tools
            .values()
            .map(|tool| tool.capability().to_owned())
            .collect();

        capabilities.sort_unstable();
        capabilities.dedup();
        capabilities
    }
}
