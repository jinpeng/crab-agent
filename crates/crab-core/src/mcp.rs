//! MCP client core (DESIGN §7): spawn stdio MCP servers, enumerate their tools,
//! and invoke them through the permission gate.
//!
//! This module owns the transport/protocol layer — one child process, one MCP
//! session, `tools/list` + `tools/call` — and [`McpTool`], which wraps one
//! `tools/list` entry as a local [`Tool`](crate::tool::Tool) (namespacing,
//! schema translation, error mapping). Registering those wrappers with a
//! [`ToolRegistry`](crate::tool::ToolRegistry) is the caller's job.
//!
//! Every `tools/call` goes through the [`PermissionGate`] so a denied call never
//! reaches the server. Per DESIGN §6 the gate is a confirmation UX, not a
//! security boundary: a malicious server can still do anything its process can.

use crate::config::McpServerConfig;
use crate::gate::{PermissionGate, Prompt};
// The crab tool trait, aliased so it coexists with rmcp's `Tool` (a listing
// entry) in this module.
use crate::tool::{Tool as CrabTool, ToolError};
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ContentBlock, JsonObject, Tool};
use rmcp::service::{ClientInitializeError, RoleClient, RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use std::borrow::Cow;
use std::sync::Arc;
use tokio::process::Command;

/// Max time to wait for a server's `initialize` handshake. A process that never
/// speaks MCP must not block startup forever.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Failures raised while running an MCP server.
///
/// The three lifecycle stages are kept separate so callers (and users reading
/// the transcript) can tell "never started" from "started but not speaking MCP"
/// from "the call itself failed".
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The child process could not be started (e.g. command not found).
    #[error("failed to spawn MCP server '{server}': {source}")]
    Spawn {
        server: String,
        #[source]
        source: std::io::Error,
    },
    /// The process started but the MCP `initialize` handshake failed.
    #[error("MCP handshake with '{server}' failed: {source}")]
    Handshake {
        server: String,
        #[source]
        source: Box<ClientInitializeError>,
    },
    /// The server started but never completed `initialize` in time.
    #[error("MCP handshake with '{server}' timed out")]
    HandshakeTimeout { server: String },
    /// A request to the server failed, or the tool reported `isError`.
    /// The underlying rmcp error is rendered into `message`.
    #[error("MCP call to '{server}' failed: {message}")]
    Call { server: String, message: String },
    /// The permission gate refused the call (or its check itself failed).
    #[error(transparent)]
    Permission(#[from] ToolError),
}

/// A live stdio MCP session with one server.
///
/// **Callers must [`shutdown`](Self::shutdown) before dropping.** Plain drop does
/// NOT reliably kill the child process (the leak behind the "/quit leaves MCP
/// servers running" bug): the rmcp kill runs as a spawned task that exit-time
/// teardown can starve or never poll. `shutdown()` cancels the session while the
/// runtime is alive, which is what actually reaps the child.
pub struct McpClient {
    name: String,
    service: RunningService<RoleClient, ()>,
    gate: PermissionGate,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("name", &self.name)
            .field("closed", &self.service.is_closed())
            .finish_non_exhaustive()
    }
}

/// Build the child process for a server config. Kept separate from
/// [`McpClient::spawn`] so the command wiring is unit-testable without spawning.
///
/// Configured `env` pairs are layered on top of the inherited environment: a
/// server that needs a credential (`BRAVE_API_KEY`) reads it from its own
/// process env, and putting the value here keeps the key out of `args`, where
/// it would show up in `ps` output and in the permission-gate prompt.
fn spawn_command(cfg: &McpServerConfig) -> Command {
    Command::new(&cfg.command).configure(|c| {
        c.args(&cfg.args);
        for (key, value) in &cfg.env {
            c.env(key, value);
        }
    })
}

impl McpClient {
    /// Spawn `cfg` as a child process and complete the MCP handshake.
    ///
    /// Returns [`McpError::Spawn`] if the process cannot be started and
    /// [`McpError::Handshake`] if it starts but never completes `initialize`.
    pub async fn spawn(
        name: &str,
        cfg: &McpServerConfig,
        gate: PermissionGate,
    ) -> Result<Self, McpError> {
        let transport =
            TokioChildProcess::new(spawn_command(cfg)).map_err(|source| McpError::Spawn {
                server: name.to_string(),
                source,
            })?;
        // The handshake must have a timeout: a process that isn't an MCP server (or a
        // hung one) would otherwise block forever (e.g. /bin/cat waits on stdin).
        let service = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| McpError::HandshakeTimeout {
                server: name.to_string(),
            })?
            .map_err(|source| McpError::Handshake {
                server: name.to_string(),
                source: Box::new(source),
            })?;
        Ok(Self {
            name: name.to_string(),
            service,
            gate,
        })
    }

    /// Configured server name (the key under `[mcp.*]`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// All tools the server exposes, following pagination cursors.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        self.service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Call {
                server: self.name.clone(),
                message: format!("tools/list failed: {e}"),
            })
    }

    /// Call one tool and flatten its result to plain text for the model.
    ///
    /// Gate-checked: a denied call never reaches the server. [`McpTool`] does
    /// not come through here — it runs the same gate itself so the prompt can
    /// name the server as well, then calls [`McpSession::invoke_raw`].
    pub async fn call(&self, tool: &str, args: serde_json::Value) -> Result<String, McpError> {
        self.check_gate(format!("{}: call tool '{tool}'", self.name))?;
        self.invoke_raw(tool, args).await
    }

    /// Cancel the session and reap the child process.
    pub async fn shutdown(self) {
        // A failing cancel only means the background task was already gone.
        let _ = self.service.cancel().await;
    }
}

/// What [`McpTool`] needs from a live session: the permission gate and the call.
///
/// A trait rather than a concrete [`McpClient`] so a wrapper's namespacing,
/// schema handling and error mapping stay unit-testable without spawning a
/// process. [`McpClient`] is the only production implementor; the two halves are
/// split because the gate must run *before* anything reaches the server, while
/// the prompt text is owned by whoever knows the server name.
#[async_trait]
pub trait McpSession: Send + Sync {
    /// Run the permission gate for one call. `summary` is the human-facing text
    /// the user confirms (e.g. `srv::echo {"text":"hi"}`).
    fn check_gate(&self, summary: String) -> Result<(), ToolError>;

    /// Invoke a tool and flatten its result to text. Gate-free by design: the
    /// caller must have run [`check_gate`](Self::check_gate) first.
    async fn invoke_raw(&self, tool: &str, args: serde_json::Value) -> Result<String, McpError>;
}

#[async_trait]
impl McpSession for McpClient {
    fn check_gate(&self, summary: String) -> Result<(), ToolError> {
        self.gate.check(Prompt {
            tool: "mcp",
            summary,
        })
    }

    /// `args` must be a JSON object (or `null` for no arguments). Text content
    /// blocks are joined with newlines; non-text blocks become
    /// `[unsupported content]`. A result flagged `isError` is returned as
    /// [`McpError::Call`] so a failing tool looks like a failed tool call
    /// rather than a poisoned success.
    async fn invoke_raw(&self, tool: &str, args: serde_json::Value) -> Result<String, McpError> {
        let arguments = match args {
            serde_json::Value::Object(obj) => obj,
            serde_json::Value::Null => JsonObject::new(),
            other => {
                return Err(McpError::Call {
                    server: self.name.clone(),
                    message: format!("arguments for '{tool}' must be a JSON object, got {other}"),
                });
            }
        };

        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| McpError::Call {
                server: self.name.clone(),
                message: format!("tools/call '{tool}' failed: {e}"),
            })?;

        let text = render_content(&result.content);
        if result.is_error.unwrap_or(false) {
            return Err(McpError::Call {
                server: self.name.clone(),
                message: if text.is_empty() {
                    format!("tool '{tool}' reported an error")
                } else {
                    text
                },
            });
        }
        Ok(text)
    }
}

/// One tool listed by an MCP server, exposed to the model as a crab
/// [`Tool`](crate::tool::Tool).
///
/// Names are namespaced `<server>__<tool>`: two servers may both expose
/// `search`, and the model has to be able to tell them apart. Wrapping spawns
/// nothing — several [`McpTool`]s share one [`McpClient`] (and therefore one
/// child process), which is why the client is an `Arc`.
pub struct McpTool {
    client: Arc<dyn McpSession>,
    server: String,
    tool_name: String,
    description: String,
    schema: serde_json::Value,
}

impl McpTool {
    /// Wrap one entry of `server`'s `tools/list` result.
    ///
    /// `client` is the shared live session (usually an `Arc<McpClient>`); it is
    /// dropped once the last wrapper is.
    pub fn new(client: Arc<dyn McpSession>, server: &str, meta: &Tool) -> Self {
        let server = server.to_string();
        let description = match meta.description.as_deref() {
            Some(d) => d.to_string(),
            // `description` is optional in MCP; the model still deserves a label.
            None => format!("MCP tool '{}' from server '{server}'", meta.name),
        };
        Self {
            client,
            server,
            tool_name: meta.name.to_string(),
            description,
            schema: meta.schema_as_json_value(),
        }
    }
}

#[async_trait]
impl CrabTool for McpTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!("{}__{}", self.server, self.tool_name))
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Owned(self.description.clone())
    }

    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        // Gate first (DESIGN §6/§7): a denied call must never reach the server,
        // and the user needs to see which server is asking before they answer.
        self.client
            .check_gate(format!("{}::{} {args}", self.server, self.tool_name))?;
        self.client
            .invoke_raw(&self.tool_name, args)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))
    }
}

/// Flatten MCP content blocks into a single string.
fn render_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(t) => t.text.clone(),
            // Images/audio/embedded resources would be megabytes of base64;
            // the model gets a marker instead.
            _ => "[unsupported content]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolResult;
    use std::io::{BufRead, IsTerminal, Write};

    /// The lib test binary re-executes itself as a minimal stdio MCP server
    /// when this exact name is passed as the libtest filter (see
    /// `helper_command_args`). It is **not** a test: it is the child process's
    /// entry point, so it is `#[ignore]`d and the re-spawn passes `--ignored`
    /// (plus `--exact`) to select it. Without the ignore, a plain
    /// `cargo test -p crab-core mcp` would also match this name, run the body
    /// in-process, and block on stdin forever.
    ///
    /// The two guards below are belt and braces for a hand-run
    /// `cargo test -- --ignored`: the body only starts when the exact helper
    /// filter is on the command line, and never when stdin is a terminal.
    const HELPER_FILTER: &str = "mcp::tests::stdio_server_helper";

    #[ignore = "subprocess entry point: runs only when re-executed with the helper filter arg"]
    #[test]
    fn stdio_server_helper() {
        if !std::env::args().any(|a| a == HELPER_FILTER) || std::io::stdin().is_terminal() {
            return;
        }
        run_stdio_server();
    }

    fn helper_command_args() -> Vec<String> {
        [
            "--ignored",
            "--exact",
            HELPER_FILTER,
            "--quiet",
            "--nocapture",
            "--test-threads",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Hand-rolled JSON-RPC server: enough of the MCP spec to satisfy a real
    /// rmcp client over real stdio pipes (initialize -> initialize result,
    /// tools/list, tools/call). Non-JSON lines on stdin are ignored, so the
    /// libtest banner the child prints on stdout is harmless to the client.
    fn run_stdio_server() {
        let stdin = std::io::stdin();
        let mut out = std::io::stdout().lock();
        let mut protocol_version = "2024-11-05".to_string();

        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let id = msg.get("id").cloned();
            let method = msg
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            if let Some(pv) = msg
                .pointer("/params/protocolVersion")
                .and_then(|v| v.as_str())
            {
                protocol_version = pv.to_string();
            }

            let result = match method.as_str() {
                "initialize" => Some(serde_json::json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "crab-mcp-test-server", "version": "0.0.0" },
                })),
                "tools/list" => Some(serde_json::json!({
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo the given text back",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "text": { "type": "string" } },
                                "required": ["text"],
                            },
                        },
                        {
                            "name": "boom",
                            "description": "Always fails",
                            "inputSchema": { "type": "object" },
                        },
                    ],
                })),
                "tools/call" => {
                    let name = msg
                        .pointer("/params/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    match name {
                        "echo" => {
                            let text = msg
                                .pointer("/params/arguments/text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            Some(serde_json::json!({
                                "content": [{ "type": "text", "text": format!("echo: {text}") }],
                                "isError": false,
                            }))
                        }
                        "boom" => Some(serde_json::json!({
                            "content": [{ "type": "text", "text": "kaboom" }],
                            "isError": true,
                        })),
                        _ => None,
                    }
                }
                // Notifications (no id) get no reply.
                _ => None,
            };

            let Some(id) = id else { continue };
            let reply = match result {
                Some(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                None => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" },
                }),
            };
            if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }

    fn helper_config() -> McpServerConfig {
        let exe = std::env::current_exe().expect("test binary path");
        McpServerConfig {
            command: exe.to_string_lossy().into_owned(),
            args: helper_command_args(),
            enabled: Some(true),
            env: Default::default(),
        }
    }

    /// Spawn failure is the failure most likely to hit a user with a typo'd
    /// `command`, and it must be an `Err`, not a panic or a hang.
    #[tokio::test]
    async fn spawn_missing_command_errors() {
        let cfg = McpServerConfig {
            command: "/nonexistent/crab-mcp-server-that-is-not-here".into(),
            args: vec![],
            enabled: Some(true),
            env: Default::default(),
        };
        let err = McpClient::spawn("bogus", &cfg, PermissionGate::auto_approve())
            .await
            .expect_err("spawning a missing command must fail");
        match err {
            McpError::Spawn { server, .. } => assert_eq!(server, "bogus"),
            other => panic!("expected Spawn error, got {other:?}"),
        }
    }

    /// A process that starts but is not an MCP server must surface as a
    /// handshake failure rather than a hang. `/bin/cat` echoes nothing and
    /// exits on EOF, so the handshake cannot complete.
    #[cfg(unix)]
    #[tokio::test]
    async fn non_mcp_process_fails_handshake() {
        let cfg = McpServerConfig {
            command: "/bin/cat".into(),
            args: vec![],
            enabled: Some(true),
            env: Default::default(),
        };
        let err = McpClient::spawn("cat", &cfg, PermissionGate::auto_approve())
            .await
            .expect_err("cat is not an MCP server");
        // A non-MCP process never completes initialize: either a Handshake error or a
        // HandshakeTimeout, but never a hang and never success.
        assert!(
            matches!(
                err,
                McpError::Handshake { .. } | McpError::HandshakeTimeout { .. }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn command_uses_configured_program_and_args() {
        let cfg = McpServerConfig {
            command: "my-server".into(),
            args: vec!["--flag".into(), "value".into()],
            enabled: None,
            env: Default::default(),
        };
        let cmd = spawn_command(&cfg);
        assert_eq!(cmd.as_std().get_program(), "my-server");
        assert_eq!(
            cmd.as_std().get_args().collect::<Vec<_>>(),
            vec!["--flag", "value"]
        );
    }

    /// A server that needs a credential (Brave Search's `BRAVE_API_KEY`) gets it
    /// from its own environment, which is where the server reads it. Asserted on
    /// the built `Command` rather than on a running process: the point is the
    /// wiring, and spawning would need a real key.
    #[test]
    fn command_carries_configured_env() {
        let cfg = McpServerConfig {
            command: "my-server".into(),
            args: vec![],
            enabled: None,
            env: [
                ("BRAVE_API_KEY".to_string(), "abc".to_string()),
                ("OTHER_VAR".to_string(), "xyz".to_string()),
            ]
            .into_iter()
            .collect(),
        };

        let cmd = spawn_command(&cfg);
        // A `HashMap` has no order, so compare as a map rather than a sequence.
        let got: std::collections::HashMap<String, String> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.unwrap_or_default().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(got.get("BRAVE_API_KEY").map(String::as_str), Some("abc"));
        assert_eq!(got.get("OTHER_VAR").map(String::as_str), Some("xyz"));
        assert_eq!(got.len(), 2);
    }

    /// No `env` in the config must mean no override at all: the child inherits
    /// the parent's environment (which is how `PATH` keeps working).
    #[test]
    fn command_without_env_only_inherits() {
        let cfg = McpServerConfig {
            command: "my-server".into(),
            args: vec![],
            enabled: None,
            env: Default::default(),
        };
        let cmd = spawn_command(&cfg);
        assert_eq!(cmd.as_std().get_envs().count(), 0);
    }

    #[test]
    fn render_content_joins_text_and_marks_other_blocks() {
        let blocks = vec![
            ContentBlock::text("first"),
            ContentBlock::image("aGk=", "image/png"),
            ContentBlock::text("second"),
        ];
        assert_eq!(
            render_content(&blocks),
            "first\n[unsupported content]\nsecond"
        );
        assert_eq!(render_content(&[]), "");
    }

    #[test]
    fn error_result_is_an_error() {
        // `CallToolResult` is `#[non_exhaustive]`; the constructors are the
        // supported way to build one.
        let ok = CallToolResult::success(vec![ContentBlock::text("fine")]);
        assert!(!ok.is_error.unwrap_or(false));
        let bad = CallToolResult::error(vec![ContentBlock::text("nope")]);
        assert!(bad.is_error.unwrap_or(false));
    }

    /// Full round trip against the helper server: handshake, `tools/list`,
    /// `tools/call`, tool-level error, gate denial, shutdown.
    ///
    /// Ignored by default: the self-spawn round trip (the test binary re-execs itself
    /// as a stdio MCP server) is timing-sensitive under `cargo test` parallelism and
    /// can hang in constrained environments. Run it explicitly with
    /// `cargo test -p crab-core mcp -- --ignored --test-threads=1`. The deterministic
    /// spawn/handshake/gate/content tests run in the normal suite.
    #[ignore = "self-spawn round trip is timing-sensitive; run with --ignored --test-threads=1"]
    #[tokio::test]
    async fn round_trip_list_and_call() {
        let cfg = helper_config();
        let client = McpClient::spawn("helper", &cfg, PermissionGate::auto_approve())
            .await
            .expect("spawn + handshake with helper server");
        assert_eq!(client.name(), "helper");

        let tools = client.list_tools().await.expect("tools/list");
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["boom", "echo"]);
        assert_eq!(
            tools[0].description.as_deref(),
            Some("Echo the given text back")
        );

        let out = client
            .call("echo", serde_json::json!({"text": "hi"}))
            .await
            .expect("tools/call echo");
        assert_eq!(out, "echo: hi");

        let err = client
            .call("boom", serde_json::json!({}))
            .await
            .expect_err("isError result must map to Err");
        match err {
            McpError::Call { message, .. } => assert_eq!(message, "kaboom"),
            other => panic!("expected Call error, got {other:?}"),
        }

        let err = client
            .call("nope", serde_json::json!({}))
            .await
            .expect_err("unknown tool must map to Err");
        assert!(matches!(err, McpError::Call { .. }), "got {err:?}");

        client.shutdown().await;
    }

    #[tokio::test]
    async fn denied_call_never_reaches_the_server() {
        let cfg = helper_config();
        let client = McpClient::spawn("helper", &cfg, PermissionGate::auto_deny())
            .await
            .expect("spawn + handshake with helper server");
        let err = client
            .call("echo", serde_json::json!({"text": "hi"}))
            .await
            .expect_err("denied call must fail");
        assert!(matches!(err, McpError::Permission(_)), "got {err:?}");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        let cfg = helper_config();
        let client = McpClient::spawn("helper", &cfg, PermissionGate::auto_approve())
            .await
            .expect("spawn + handshake with helper server");
        let err = client
            .call("echo", serde_json::json!("not an object"))
            .await
            .expect_err("scalar arguments must be rejected");
        assert!(matches!(err, McpError::Call { .. }), "got {err:?}");
        client.shutdown().await;
    }

    // ---- McpTool: exercised against a mock session, so no server is spawned ----

    /// Stand-in for a live [`McpClient`]: records what was invoked, returns
    /// canned output, and runs a real gate so denial is exercised for real.
    struct MockSession {
        gate: PermissionGate,
        fail_with: Option<String>,
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl MockSession {
        fn approving() -> Self {
            Self {
                gate: PermissionGate::auto_approve(),
                fail_with: None,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, serde_json::Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl McpSession for MockSession {
        fn check_gate(&self, summary: String) -> Result<(), ToolError> {
            self.gate.check(Prompt {
                tool: "mcp",
                summary,
            })
        }

        async fn invoke_raw(
            &self,
            tool: &str,
            args: serde_json::Value,
        ) -> Result<String, McpError> {
            self.calls.lock().unwrap().push((tool.to_string(), args));
            match &self.fail_with {
                Some(message) => Err(McpError::Call {
                    server: "mock".into(),
                    message: message.clone(),
                }),
                None => Ok("mock output".into()),
            }
        }
    }

    /// A `tools/list` entry, built the way a server would send one.
    fn listing_entry(name: &str, description: Option<&str>) -> Tool {
        Tool::new_with_raw(
            Cow::Owned(name.to_string()),
            description.map(|d| Cow::Owned(d.to_string())),
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    fn wrapped(session: Arc<MockSession>, entry: &Tool) -> McpTool {
        McpTool::new(session, "srv", entry)
    }

    #[test]
    fn wrapped_tool_namespaces_name_and_keeps_metadata() {
        let tool = wrapped(
            Arc::new(MockSession::approving()),
            &listing_entry("echo", Some("Echo text back")),
        );

        assert_eq!(tool.name(), "srv__echo");
        assert_eq!(tool.description(), "Echo text back");
        assert_eq!(
            tool.schema(),
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        );
    }

    /// The stored description is handed back verbatim, even when a server ships
    /// text that looks like a schema or contains `__`.
    #[test]
    fn description_is_returned_verbatim() {
        let text = "weird__description {not: json}";
        let tool = wrapped(
            Arc::new(MockSession::approving()),
            &listing_entry("odd", Some(text)),
        );
        assert_eq!(tool.description(), text);
    }

    #[test]
    fn missing_description_gets_a_labelled_fallback() {
        let tool = wrapped(
            Arc::new(MockSession::approving()),
            &listing_entry("mystery", None),
        );
        let d = tool.description();
        assert!(d.contains("mystery") && d.contains("srv"), "got {d}");
    }

    /// Two servers exposing the same remote tool name must stay distinguishable.
    #[test]
    fn two_servers_can_expose_the_same_tool_name() {
        let entry = listing_entry("search", Some("Search"));
        let a = McpTool::new(Arc::new(MockSession::approving()), "alpha", &entry);
        let b = McpTool::new(Arc::new(MockSession::approving()), "beta", &entry);
        assert_eq!(a.name(), "alpha__search");
        assert_eq!(b.name(), "beta__search");
    }

    #[tokio::test]
    async fn execute_passes_args_through_and_maps_result() {
        let session = Arc::new(MockSession::approving());
        let tool = wrapped(
            session.clone(),
            &listing_entry("echo", Some("Echo text back")),
        );
        let out = tool
            .execute(serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out, "mock output");
        // The remote name goes on the wire un-namespaced.
        assert_eq!(
            session.calls(),
            vec![("echo".to_string(), serde_json::json!({"text": "hi"}))]
        );
    }

    #[tokio::test]
    async fn execute_denied_by_gate_never_reaches_the_server() {
        let session = Arc::new(MockSession {
            gate: PermissionGate::auto_deny(),
            fail_with: None,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let tool = wrapped(
            session.clone(),
            &listing_entry("echo", Some("Echo text back")),
        );
        let err = tool
            .execute(serde_json::json!({"text": "hi"}))
            .await
            .expect_err("a denied call must fail");
        assert!(matches!(err, ToolError::Exec(_)), "got {err:?}");
        assert!(err.to_string().contains("permission denied"), "got {err}");
        assert!(session.calls().is_empty(), "denied call reached the server");
    }

    #[tokio::test]
    async fn execute_maps_mcp_failure_to_tool_exec_error() {
        let session = Arc::new(MockSession {
            gate: PermissionGate::auto_approve(),
            fail_with: Some("kaboom".into()),
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let tool = wrapped(session, &listing_entry("boom", Some("Always fails")));
        let err = tool
            .execute(serde_json::json!({}))
            .await
            .expect_err("a server-side failure must surface");
        assert!(matches!(err, ToolError::Exec(_)), "got {err:?}");
        assert!(err.to_string().contains("kaboom"), "got {err}");
    }

    /// The prompt must name the server, the tool and the arguments: with several
    /// servers attached, `"call tool 'echo'"` would not tell the user which
    /// process is about to run.
    #[tokio::test]
    async fn gate_prompt_names_server_tool_and_args() {
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen2 = seen.clone();
        let session = Arc::new(MockSession {
            gate: PermissionGate::with_confirmer(Arc::new(move |p| {
                *seen2.lock().unwrap() = format!("{}|{}", p.tool, p.summary);
                crate::gate::Decision::Allow
            })),
            fail_with: None,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let tool = wrapped(session, &listing_entry("echo", Some("Echo text back")));
        tool.execute(serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        let prompt = seen.lock().unwrap().clone();
        assert!(prompt.starts_with("mcp|"), "got {prompt}");
        assert!(prompt.contains("srv::echo"), "got {prompt}");
        assert!(prompt.contains("\"text\":\"hi\""), "got {prompt}");
    }

    /// End to end at the registry level: a wrapped tool is found and dispatched
    /// under its namespaced name, with the schema the server sent.
    #[tokio::test]
    async fn wrapped_tool_registers_under_namespaced_name() {
        let entry = listing_entry("echo", Some("Echo text back"));
        let mut reg = crate::tool::ToolRegistry::new();
        reg.register(Box::new(wrapped(
            Arc::new(MockSession::approving()),
            &entry,
        )));

        assert!(reg.contains("srv__echo"));
        let schemas = reg.schemas();
        assert_eq!(schemas[0]["name"], "srv__echo");
        assert_eq!(schemas[0]["description"], "Echo text back");
        assert_eq!(
            schemas[0]["parameters"]["properties"]["text"]["type"],
            "string"
        );
        assert_eq!(
            reg.execute("srv__echo", serde_json::json!({"text": "hi"}))
                .await
                .unwrap(),
            "mock output"
        );
    }

    /// The real seam the mocks cannot reach: list a live server's tools, wrap
    /// them, and dispatch one through the wrapper (namespacing, schema
    /// translation, gate, error mapping). Ignored for the same reason as
    /// `round_trip_list_and_call`.
    #[ignore = "self-spawn round trip is timing-sensitive; run with --ignored --test-threads=1"]
    #[tokio::test]
    async fn round_trip_wrapped_tools() {
        let cfg = helper_config();
        let client = Arc::new(
            McpClient::spawn("helper", &cfg, PermissionGate::auto_approve())
                .await
                .expect("spawn + handshake with helper server"),
        );
        let entries = client.list_tools().await.expect("tools/list");

        {
            let wrapped: Vec<McpTool> = entries
                .iter()
                .map(|meta| McpTool::new(client.clone(), "helper", meta))
                .collect();
            let mut names: Vec<String> = wrapped.iter().map(|t| t.name().into_owned()).collect();
            names.sort_unstable();
            assert_eq!(names, vec!["helper__boom", "helper__echo"]);

            let echo = wrapped
                .iter()
                .find(|t| t.name() == "helper__echo")
                .expect("echo wrapper");
            assert_eq!(echo.description(), "Echo the given text back");
            assert_eq!(echo.schema()["properties"]["text"]["type"], "string");
            assert_eq!(
                echo.execute(serde_json::json!({"text": "hi"}))
                    .await
                    .expect("tools/call echo"),
                "echo: hi"
            );

            let boom = wrapped
                .iter()
                .find(|t| t.name() == "helper__boom")
                .expect("boom wrapper");
            let err = boom
                .execute(serde_json::json!({}))
                .await
                .expect_err("isError must map to a ToolError");
            assert!(matches!(err, ToolError::Exec(_)), "got {err:?}");
            assert!(err.to_string().contains("kaboom"), "got {err}");
        }

        // The wrappers must have shared one session, not replaced it: the last
        // owner can still shut it down politely.
        let client = Arc::try_unwrap(client).expect("wrappers still hold the client");
        client.shutdown().await;
    }

    /// A child that survives the exit path leaks server processes (the /quit and
    /// --print paths both drop the client without calling `shutdown`). Fixture:
    /// a minimal MCP server that completes the handshake then idles forever,
    /// writing its PID to $PIDFILE so the test can watch it.
    const SLEEPER_PY: &str = r#"
import json, os, sys, time
with open(os.environ["PIDFILE"], "w") as f:
    f.write(str(os.getpid()))

def reply(id_, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n")
    sys.stdout.flush()

while True:
    line = sys.stdin.readline()
    if not line:
        # daemon mode: release the inherited stdout pipe (else the test harness
        # hangs waiting for EOF) and keep living until explicitly killed
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, sys.stdout.fileno())
        while True:
            time.sleep(3600)
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if msg.get("method") == "initialize":
        reply(msg["id"], {"protocolVersion": "2024-11-05",
                          "capabilities": {"tools": {}},
                          "serverInfo": {"name": "sleeper", "version": "0"}})
    elif msg.get("method") == "tools/list":
        reply(msg["id"], {"tools": []})
"#;

    fn sleeper_config(dir: &tempfile::TempDir) -> Option<(McpServerConfig, String)> {
        if std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return None; // no python3: skip
        }
        let script = dir.path().join("sleeper.py");
        std::fs::write(&script, SLEEPER_PY).unwrap();
        let pidfile = dir.path().join("child.pid");
        let mut env = std::collections::HashMap::new();
        env.insert("PIDFILE".to_string(), pidfile.to_string_lossy().into_owned());
        let cfg = McpServerConfig {
            command: "python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            enabled: Some(true),
            env,
        };
        Some((cfg, pidfile.to_string_lossy().into_owned()))
    }

    fn child_pid(pidfile: &str) -> Option<String> {
        (0..50).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::fs::read_to_string(pidfile).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        })
    }

    fn pid_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// True once the child is gone, polling up to 5s.
    fn pid_died(pid: &str) -> bool {
        (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            !pid_alive(pid)
        })
    }

    /// Regression (the /quit MCP leak): shutdown must reap the child while the
    /// runtime is alive — every exit path is expected to call this instead of
    /// relying on drop.
    #[tokio::test]
    async fn shutdown_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let Some((cfg, pidfile)) = sleeper_config(&dir) else {
            eprintln!("python3 unavailable: skipping");
            return;
        };
        let client = McpClient::spawn("sleeper", &cfg, PermissionGate::auto_approve())
            .await
            .expect("sleeper must spawn and handshake");
        let pid = child_pid(&pidfile).expect("child must write its pid");

        client.shutdown().await;

        assert!(pid_died(&pid), "MCP child {pid} outlived shutdown");
    }
}
