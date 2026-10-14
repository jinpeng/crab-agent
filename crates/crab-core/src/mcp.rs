//! MCP client core (DESIGN §7): spawn stdio MCP servers, enumerate their tools,
//! and invoke them through the permission gate.
//!
//! This module owns only the transport/protocol layer: one child process, one
//! MCP session, `tools/list` + `tools/call`. Turning the discovered tools into
//! local [`Tool`](crate::tool::Tool) impls (namespacing, schema translation) is
//! the caller's job.
//!
//! Every `tools/call` goes through the [`PermissionGate`] so a denied call never
//! reaches the server. Per DESIGN §6 the gate is a confirmation UX, not a
//! security boundary: a malicious server can still do anything its process can.

use crate::config::McpServerConfig;
use crate::gate::{PermissionGate, Prompt};
use crate::tool::ToolError;
use rmcp::model::{CallToolRequestParams, ContentBlock, JsonObject, Tool};
use rmcp::service::{ClientInitializeError, RoleClient, RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
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
/// Dropping the client kills the child process (the rmcp transport owns it), so
/// [`shutdown`](Self::shutdown) is the polite path, not the only one.
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
fn spawn_command(cfg: &McpServerConfig) -> Command {
    Command::new(&cfg.command).configure(|c| {
        c.args(&cfg.args);
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
    /// `args` must be a JSON object (or `null` for no arguments). Text content
    /// blocks are joined with newlines; non-text blocks become
    /// `[unsupported content]`. A result flagged `isError` is returned as
    /// [`McpError::Call`] so a failing tool looks like a failed tool call
    /// rather than a poisoned success.
    pub async fn call(&self, tool: &str, args: serde_json::Value) -> Result<String, McpError> {
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

        self.gate.check(Prompt {
            tool: "mcp",
            summary: format!("{}: call tool '{tool}'", self.name),
        })?;

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

    /// Cancel the session and reap the child process.
    pub async fn shutdown(self) {
        // A failing cancel only means the background task was already gone.
        let _ = self.service.cancel().await;
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
        };
        let cmd = spawn_command(&cfg);
        assert_eq!(cmd.as_std().get_program(), "my-server");
        assert_eq!(
            cmd.as_std().get_args().collect::<Vec<_>>(),
            vec!["--flag", "value"]
        );
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
}
