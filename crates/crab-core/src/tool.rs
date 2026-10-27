use async_trait::async_trait;
use std::borrow::Cow;
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
    fn name(&self) -> Cow<'static, str>;
    /// Human-readable description for the model's tool list. A `Cow` rather
    /// than `&'static str` because tools discovered at runtime (e.g. MCP
    /// servers, whose descriptions ship with the listing) carry owned text.
    fn description(&self) -> Cow<'static, str>;
    fn schema(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, t: Box<dyn Tool>) {
        self.tools.insert(t.name().into_owned(), t);
    }
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    /// Full OpenAI-style function schemas: {name, description, parameters}.
    /// `Tool::schema()` yields only the parameters object; we wrap it here so every
    /// tool gets name+description without duplicating them. (DeepSeek 422 regression:
    /// function.name is required.)
    pub fn schemas(&self) -> Vec<serde_json::Value> {
        self.tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.schema(),
                })
            })
            .collect()
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
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("echo")
        }
        fn description(&self) -> Cow<'static, str> {
            Cow::Borrowed("echo back input.text")
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

    /// A tool whose name is built at runtime (e.g. an MCP server prefix) must
    /// register, be findable, and execute like any built-in tool.
    struct DynTool;

    #[async_trait]
    impl Tool for DynTool {
        fn name(&self) -> Cow<'static, str> {
            Cow::Owned("srv__tool".to_string())
        }
        fn description(&self) -> Cow<'static, str> {
            Cow::Borrowed("dynamically named")
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        }
        async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
            Ok(args["text"].as_str().unwrap_or("").to_string())
        }
    }

    #[tokio::test]
    async fn registry_accepts_runtime_constructed_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DynTool));
        assert!(reg.contains("srv__tool"));
        let out = reg
            .execute("srv__tool", serde_json::json!({"text":"hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
        assert_eq!(reg.schemas()[0]["name"], "srv__tool");
    }
}

#[cfg(test)]
mod size_probe {
    #[test]
    fn print_builtin_schema_sizes() {
        let reg = crate::tools::register_all(
            crate::state::SessionState::default(),
            crate::gate::PermissionGate::auto_approve(),
        );
        let mut total = 0;
        for s in reg.schemas() {
            let n = serde_json::to_string(&s).unwrap().len();
            total += n;
            println!("{:12} {n} bytes", s["name"]);
        }
        println!("TOTAL        {total} bytes");
    }
}
