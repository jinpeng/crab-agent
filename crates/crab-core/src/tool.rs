use async_trait::async_trait;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("execution failed: {0}")]
    Exec(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<&'static str, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, t: Box<dyn Tool>) {
        self.tools.insert(t.name(), t);
    }
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    pub fn schemas(&self) -> Vec<serde_json::Value> {
        self.tools.values().map(|t| t.schema()).collect()
    }
    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String, ToolError> {
        match self.tools.get(name) {
            Some(t) => t.execute(args).await,
            None => Err(ToolError::UnknownTool(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echo back input.text"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        }
        async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
            Ok(args["text"].as_str().unwrap_or("").to_string())
        }
    }

    #[tokio::test]
    async fn registry_dispatches_and_reports_unknown() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(Echo));
        let ok = reg
            .execute("echo", serde_json::json!({"text":"hi"}))
            .await
            .unwrap();
        assert_eq!(ok, "hi");
        let err = reg
            .execute("nope", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }
}
