use anyhow::{Context, Result};
use crab_core::agent::{Agent, AgentConfig};
use crab_core::gate::{Confirmer, PermissionGate};
use crab_core::mcp::{McpClient, McpTool};
use crab_core::provider::Provider;
use crab_core::provider::openai::OpenAiProvider;
use crab_core::session::Session;
use crab_core::state::SessionState;
use crab_core::tool::ToolRegistry;
use crab_core::tools::register_all;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const BASE_PROMPT: &str = r#"You are crab, a coding agent. You have these tools: read, write, edit, bash, glob, grep, web_fetch.

Rules for editing files:
- ALWAYS read a file before editing or overwriting it. The edit/write tools refuse to touch a file you haven't read this session (freshness gate) — this protects against overwriting external changes.
- Use `edit` for changing existing files. Each edit's `old` must match the file EXACTLY once (no whitespace guessing). Batch multiple changes to the same file into one `edit` call's `edits` array.
- If `edit` fails with "no match", re-read the file and copy the exact text from the error hint.
- Use `write` only for new files, or full rewrites after a read.
- Prefer `glob`/`grep` to locate files and symbols before reading whole files.
- Use `web_fetch` to pull a URL's main article text (readability) when you need web docs or references.

Be precise and minimal. Verify changes compile or pass tests with `bash` when relevant."#;

pub struct AppConfig {
    pub yolo: bool,
    pub system: Option<String>,
    pub provider: Option<String>,
    /// true for --print (non-interactive: auto-approve), false for interactive (confirm).
    pub non_interactive: bool,
}

/// Everything startup assembles, handed to the CLI/TUI in one piece.
pub struct Startup {
    pub agent: Agent,
    pub system_prompt: String,
    pub resolved: crab_core::config::ResolvedProvider,
    /// Live MCP child processes: dropping this kills them, so it must outlive the run.
    pub mcp_clients: Vec<Arc<McpClient>>,
    pub gate: PermissionGate,
    pub config: crab_core::config::Config,
    pub skills: Vec<crab_core::skills::Skill>,
}

impl From<&crate::Cli> for AppConfig {
    fn from(c: &crate::Cli) -> Self {
        Self {
            yolo: c.yolo,
            system: c.system.clone(),
            provider: c.provider.clone(),
            non_interactive: c.print.is_some(),
        }
    }
}

pub fn build_system_prompt(cwd: &Path, extra: Option<String>) -> String {
    let mut sp = String::from(BASE_PROMPT);
    if let Ok(content) = std::fs::read_to_string(cwd.join("AGENTS.md")) {
        sp.push_str("\n\n# Project instructions (AGENTS.md)\n");
        sp.push_str(&content);
    }
    if let Some(e) = extra {
        sp.push_str("\n\n# Additional instructions\n");
        sp.push_str(&e);
    }
    sp
}

fn sessions_dir(cwd: &Path) -> PathBuf {
    cwd.join(".crab").join("sessions")
}

pub fn latest_session(dir: &Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    files.pop()
}

/// Assemble a ready-to-run agent.
///
/// The returned [`McpClient`]s are the live MCP children backing the MCP tools
/// registered in the agent's registry: dropping one closes the connection and
/// kills its process, so the caller **must** keep them alive for as long as the
/// agent may call those tools.
///
/// `confirmer` is optional: if `None`, the default behavior is used
/// (--yolo, --print, or a simple terminal confirmer). Pass `Some(confirmer)`
/// to override, e.g., for the TUI's channel-based confirmer.
pub async fn build_agent(
    cfg: &AppConfig,
    continue_: bool,
    confirmer: Option<Confirmer>,
) -> Result<Startup> {
    let cwd = std::env::current_dir().context("cwd")?;
    let skills = crab_core::skills::discover(&cwd, &std::env::home_dir().unwrap_or_default());
    let system_prompt = build_system_prompt(&cwd, cfg.system.clone());

    // Loaded once: the provider resolves its credentials from it and the
    // `[mcp.*]` sections below decide which servers to start.
    let config =
        crab_core::config::Config::load_from(None).map_err(|e| anyhow::anyhow!("config: {e}"))?;

    let (provider, resolved): (Box<dyn Provider>, crab_core::config::ResolvedProvider) =
        resolve_provider(&config, cfg.provider.as_deref())?;

    // Permission routing (Q9): --yolo all-allow; --print auto-approve (no one to ask);
    // interactive uses the provided confirmer (TUI's channel-based one).
    let gate = if cfg.yolo {
        PermissionGate::yolo()
    } else if cfg.non_interactive {
        PermissionGate::auto_approve()
    } else if let Some(c) = confirmer {
        PermissionGate::with_confirmer(c)
    } else {
        // Fallback (shouldn't normally happen, but provide a safe default)
        PermissionGate::auto_deny()
    };
    let mut tools = register_all(SessionState::default(), gate.clone());
    // MCP servers come up before the session is created: a server that fails to
    // start is only ever a warning, never a reason not to start crab.
    let mcp_clients = attach_mcp_servers(&config, gate.clone(), &mut tools).await;

    let dir = sessions_dir(&cwd);
    let session = if continue_ {
        let latest = latest_session(&dir).context("no prior session to continue")?;
        Session::load(&latest).with_context(|| format!("load {}", latest.display()))?
    } else {
        Session::create(&dir).context("create session")?
    };

    let agent = Agent::new(
        provider,
        tools,
        session,
        AgentConfig {
            system_prompt: system_prompt.clone(),
        },
    );
    Ok(Startup {
        agent,
        system_prompt,
        resolved,
        mcp_clients,
        gate,
        config,
        skills,
    })
}

/// Build a provider for the given profile name, resolving credentials the same
/// way startup does. Used by the TUI's `/model` to swap models mid-session.
/// Returns the provider and its resolved config (for the status line).
pub fn resolve_provider(
    config: &crab_core::config::Config,
    name: Option<&str>,
) -> Result<(Box<dyn Provider>, crab_core::config::ResolvedProvider)> {
    match name {
        Some("mock") => Ok((
            Box::new(crab_core::provider::MockProvider::new(vec![vec![
                crab_core::stream::StreamEvent::TextDelta("mock response".into()),
                crab_core::stream::StreamEvent::StreamEnd {
                    reason: crab_core::types::StopReason::EndTurn,
                },
            ]])),
            crab_core::config::ResolvedProvider {
                base_url: String::new(),
                model: "mock".into(),
                api_key: String::new(),
                context_window: None,
                provider_name: "mock".into(),
            },
        )),
        _ => {
            let r =
                crab_core::config::resolve_credentials(config, name, &|k| std::env::var(k).ok())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            let provider = Box::new(OpenAiProvider::new(
                r.base_url.clone(),
                r.api_key.clone(),
                r.model.clone(),
            ));
            Ok((provider, r))
        }
    }
}

/// Spawn every enabled `[mcp.*]` server and register its tools into `tools`.
///
/// Returns the live clients, which the caller must hold for the lifetime of the
/// run (see [`build_agent`]). Startup is best-effort by design: a spawn or
/// `tools/list` failure warns on stderr and skips that server, so one broken
/// server cannot keep crab from starting.
///
/// Servers start in name order so the warnings a user sees are deterministic.
async fn attach_mcp_servers(
    config: &crab_core::config::Config,
    gate: PermissionGate,
    tools: &mut ToolRegistry,
) -> Vec<Arc<McpClient>> {
    let mut clients = Vec::new();
    let mut names: Vec<&str> = config.mcp.keys().map(String::as_str).collect();
    names.sort_unstable();

    for name in names {
        let srv = &config.mcp[name];
        if !srv.is_enabled() {
            continue;
        }
        let client = match McpClient::spawn(name, srv, gate.clone()).await {
            Ok(client) => client,
            Err(e) => {
                eprintln!("[crab] mcp server '{name}': spawn failed: {e} (skipped)");
                continue;
            }
        };
        let listed = match client.list_tools().await {
            Ok(listed) => listed,
            Err(e) => {
                eprintln!("[crab] mcp server '{name}': list_tools failed: {e} (skipped)");
                continue;
            }
        };
        // One Arc per server: every wrapper shares the same child process, so a
        // server with five tools is still one process, one session.
        let client = Arc::new(client);
        for meta in &listed {
            tools.register(Box::new(McpTool::new(client.clone(), name, meta)));
        }
        clients.push(client);
    }

    clients
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_includes_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Rules\nAlways be terse.").unwrap();
        let sp = build_system_prompt(dir.path(), None);
        assert!(sp.contains("Always be terse"));
    }

    #[test]
    fn missing_agents_md_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let sp = build_system_prompt(dir.path(), None);
        assert!(!sp.is_empty());
    }

    #[test]
    fn system_flag_appended() {
        let dir = tempfile::tempdir().unwrap();
        let sp = build_system_prompt(dir.path(), Some("extra rule".into()));
        assert!(sp.contains("extra rule"));
    }
}

#[cfg(test)]
mod mcp_wiring_tests {
    use super::*;
    use crab_core::config::Config;

    fn built_in_registry() -> ToolRegistry {
        register_all(SessionState::default(), PermissionGate::auto_approve())
    }

    /// The headline invariant: a server that cannot be started is warned about
    /// and skipped, never a reason to fail startup. The built-in tools a
    /// `--print` run depends on must all still be registered.
    #[tokio::test]
    async fn unspawnable_server_is_skipped_not_fatal() {
        let config = Config::from_toml(
            r#"
[mcp.broken]
command = "/nonexistent/crab-mcp-server-that-is-not-here"
"#,
        )
        .unwrap();
        let mut tools = built_in_registry();
        let clients = attach_mcp_servers(&config, PermissionGate::auto_approve(), &mut tools).await;

        assert!(clients.is_empty(), "a failed server must not be kept alive");
        for name in ["read", "write", "edit", "bash", "glob", "grep", "web_fetch"] {
            assert!(tools.contains(name), "missing built-in tool {name}");
        }
    }

    /// `enabled = false` must never even try to spawn the process.
    #[tokio::test]
    async fn disabled_server_is_not_spawned() {
        let config = Config::from_toml(
            r#"
[mcp.off]
command = "/nonexistent/never-spawned"
enabled = false
"#,
        )
        .unwrap();
        let mut tools = built_in_registry();
        let clients = attach_mcp_servers(&config, PermissionGate::auto_approve(), &mut tools).await;
        assert!(clients.is_empty());
    }

    /// No `[mcp.*]` sections at all: nothing to attach, no panic.
    #[tokio::test]
    async fn no_servers_configured_attaches_nothing() {
        let mut tools = built_in_registry();
        let clients = attach_mcp_servers(
            &Config::default(),
            PermissionGate::auto_approve(),
            &mut tools,
        )
        .await;
        assert!(clients.is_empty());
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn base_prompt_teaches_freshness_gate() {
        let sp = build_system_prompt(std::path::Path::new("/nonexistent"), None);
        assert!(
            sp.to_lowercase().contains("read"),
            "must mention reading before editing"
        );
        assert!(sp.contains("edit") || sp.contains("Edit"));
        assert!(
            sp.contains("freshness") || sp.contains("haven't read"),
            "must explain the gate"
        );
    }
}
