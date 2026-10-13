use anyhow::{Context, Result};
use crab_core::agent::{Agent, AgentConfig};
use crab_core::gate::PermissionGate;
use crab_core::provider::Provider;
use crab_core::provider::openai::OpenAiProvider;
use crab_core::session::Session;
use crab_core::state::SessionState;
use crab_core::tools::register_all;
use std::path::{Path, PathBuf};

pub const BASE_PROMPT: &str = "You are crab, a coding agent. Use the provided tools to read, edit, and run code. Be precise. When editing, match exact text.";

pub struct AppConfig {
    pub yolo: bool,
    pub system: Option<String>,
    pub provider: String,
}

impl From<&crate::Cli> for AppConfig {
    fn from(c: &crate::Cli) -> Self {
        Self {
            yolo: c.yolo,
            system: c.system.clone(),
            provider: c.provider.clone(),
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

pub fn build_agent(cfg: &AppConfig, continue_: bool) -> Result<(Agent, String)> {
    let cwd = std::env::current_dir().context("cwd")?;
    let system_prompt = build_system_prompt(&cwd, cfg.system.clone());

    let provider: Box<dyn Provider> = match cfg.provider.as_str() {
        "mock" => Box::new(crab_core::provider::MockProvider::new(vec![vec![
            crab_core::stream::StreamEvent::TextDelta("mock response".into()),
            crab_core::stream::StreamEvent::StreamEnd {
                reason: crab_core::types::StopReason::EndTurn,
            },
        ]])),
        _ => Box::new(OpenAiProvider::resolve().map_err(|e| anyhow::anyhow!("{e}"))?),
    };

    let gate = if cfg.yolo {
        PermissionGate::yolo()
    } else {
        PermissionGate::auto_approve()
    };
    let tools = register_all(SessionState::default(), gate);

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
            max_turns: 50,
            system_prompt: system_prompt.clone(),
        },
    );
    Ok((agent, system_prompt))
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
