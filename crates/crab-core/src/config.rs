use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderProfile {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Extra environment variables for the child process, e.g. the API key a
    /// server like Brave Search needs (`BRAVE_API_KEY`). These are added to the
    /// inherited environment, not instead of it.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

impl McpServerConfig {
    /// Servers are enabled unless explicitly disabled (`enabled = false`).
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderProfile>,
    #[serde(default)]
    pub mcp: std::collections::HashMap<String, McpServerConfig>,
    /// Auto-compact when the last prompt exceeds this fraction of the context
    /// window. Omitted = 0.8; 0 = disabled. Env CRAB_AUTO_COMPACT_THRESHOLD wins.
    #[serde(default)]
    pub auto_compact_threshold: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("bad config toml: {0}")]
    Parse(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Effective auto-compaction threshold: env > config file > 0.8 default
    /// (0 = disabled).
    pub fn auto_compact_threshold(&self) -> f64 {
        resolve_threshold(
            self.auto_compact_threshold,
            std::env::var("CRAB_AUTO_COMPACT_THRESHOLD").ok(),
        )
    }

    /// Load from explicit path (tests) or default ~/.crab/config.toml. Missing file = empty.
    pub fn load_from(path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => {
                let home =
                    std::env::var_os("HOME").ok_or_else(|| ConfigError::Parse("no HOME".into()))?;
                std::path::Path::new(&home).join(".crab/config.toml")
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::from_toml(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    pub fn profile(&self, name: &str) -> Option<&ProviderProfile> {
        self.providers.get(name)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub context_window: Option<u64>,
    pub provider_name: String,
}

/// Threshold resolution: env wins over the config file; default 0.8.
/// Broken env values fall through to the file value (fail-soft).
fn resolve_threshold(file: Option<f64>, env: Option<String>) -> f64 {
    if let Some(s) = env
        && let Ok(v) = s.parse()
    {
        return v;
    }
    file.unwrap_or(0.8)
}

/// Synthesize final provider from config profile + env + CLI flag.
/// Priority: flag > default_provider; per-field env > profile; api_key required (except mock).
pub fn resolve_credentials(
    config: &Config,
    provider_name: Option<&str>,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedProvider, crate::provider::ProviderError> {
    use crate::provider::ProviderError;

    if provider_name == Some("mock") {
        return Ok(ResolvedProvider {
            base_url: String::new(),
            model: "mock".into(),
            api_key: String::new(),
            // Mock has no profile to carry a window, but the env override
            // still applies (debugging auto-compaction with --provider mock).
            context_window: get_env("CRAB_CONTEXT_WINDOW").and_then(|s| s.parse().ok()),
            provider_name: "mock".into(),
        });
    }

    // Backward compat: empty config + no flag -> pure env (pre-config behavior).
    if config.providers.is_empty() && provider_name.is_none() {
        let api_key = get_env("CRAB_API_KEY")
            .or_else(|| get_env("OPENAI_API_KEY"))
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                ProviderError::Network("no API key: set CRAB_API_KEY / OPENAI_API_KEY".into())
            })?;
        return Ok(ResolvedProvider {
            base_url: get_env("CRAB_BASE_URL")
                .or_else(|| get_env("OPENAI_BASE_URL"))
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
            model: get_env("CRAB_MODEL").unwrap_or_else(|| "gpt-4o-mini".into()),
            api_key,
            context_window: get_env("CRAB_CONTEXT_WINDOW").and_then(|s| s.parse().ok()),
            provider_name: "openai".into(),
        });
    }

    let name = provider_name
        .map(String::from)
        .or_else(|| config.default_provider.clone())
        .unwrap_or_else(|| "openai".into());

    let profile = config.profile(&name).ok_or_else(|| {
        let mut avail: Vec<&str> = config.providers.keys().map(String::as_str).collect();
        avail.sort();
        ProviderError::Network(format!(
            "unknown provider '{name}'. available in ~/.crab/config.toml: {}{}",
            avail.join(", "),
            if avail.is_empty() {
                " (none; set CRAB_API_KEY env or add [providers.<name>])"
            } else {
                ""
            }
        ))
    })?;

    let base_url = get_env("CRAB_BASE_URL")
        .or_else(|| get_env("OPENAI_BASE_URL"))
        .unwrap_or_else(|| profile.base_url.clone());
    let model = get_env("CRAB_MODEL").unwrap_or_else(|| profile.model.clone());
    let context_window = get_env("CRAB_CONTEXT_WINDOW")
        .and_then(|s| s.parse().ok())
        .or(profile.context_window);
    let api_key = get_env("CRAB_API_KEY")
        .or_else(|| get_env("OPENAI_API_KEY"))
        .or_else(|| profile.api_key.clone())
        .filter(|k| !k.is_empty());
    // Local endpoints (localhost/127.0.0.1) commonly need no key — allow empty there.
    let is_local = base_url.contains("localhost") || base_url.contains("127.0.0.1");
    let api_key = match (api_key, is_local) {
        (Some(k), _) => k,
        (None, true) => String::new(),
        (None, false) => {
            return Err(ProviderError::Network(format!(
                "no API key for provider '{name}': set CRAB_API_KEY or api_key in the profile"
            )));
        }
    };

    Ok(ResolvedProvider {
        base_url,
        model,
        api_key,
        context_window,
        provider_name: name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_compact_threshold_defaults_and_overrides() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c.auto_compact_threshold, None);
        assert_eq!(resolve_threshold(c.auto_compact_threshold, None), 0.8); // 默认
        let c = Config::from_toml("auto_compact_threshold = 0.5").unwrap();
        assert_eq!(resolve_threshold(c.auto_compact_threshold, None), 0.5);
        let c = Config::from_toml("auto_compact_threshold = 0").unwrap();
        assert_eq!(resolve_threshold(c.auto_compact_threshold, None), 0.0); // 显式关闭
        // env 优先；坏值 fail-soft 落到 file
        assert_eq!(resolve_threshold(Some(0.5), Some("0.9".into())), 0.9);
        assert_eq!(resolve_threshold(Some(0.5), Some("junk".into())), 0.5);
    }

    const TOML: &str = r#"
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key = "sk-x"
context_window = 128000

[providers.local]
base_url = "http://localhost:8080/v1"
model = "qwen"
"#;

    #[test]
    fn parses_profiles() {
        let c = Config::from_toml(TOML).unwrap();
        assert_eq!(c.default_provider.as_deref(), Some("deepseek"));
        let d = c.profile("deepseek").unwrap();
        assert_eq!(d.base_url, "https://api.deepseek.com/v1");
        assert_eq!(d.model, "deepseek-chat");
        assert_eq!(d.api_key.as_deref(), Some("sk-x"));
        assert_eq!(d.context_window, Some(128000));
        let l = c.profile("local").unwrap();
        assert_eq!(l.api_key, None);
        assert_eq!(l.context_window, None);
    }

    #[test]
    fn empty_string_is_empty_config() {
        let c = Config::from_toml("").unwrap();
        assert!(c.default_provider.is_none());
        assert!(c.profile("anything").is_none());
    }

    #[test]
    fn bad_toml_errors() {
        assert!(Config::from_toml("[[[invalid").is_err());
    }

    #[test]
    fn parses_mcp_servers() {
        let c = Config::from_toml(
            r#"
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.off]
command = "some-server"
enabled = false
"#,
        )
        .unwrap();
        assert_eq!(c.mcp.len(), 2);

        let fs = c.mcp.get("filesystem").unwrap();
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args.len(), 3);
        assert_eq!(fs.args[1], "@modelcontextprotocol/server-filesystem");
        assert!(fs.is_enabled());

        let off = c.mcp.get("off").unwrap();
        assert_eq!(off.command, "some-server");
        assert!(off.args.is_empty());
        assert!(!off.is_enabled());
    }

    #[test]
    fn parses_mcp_server_env() {
        let c = Config::from_toml(
            r#"
[mcp.brave]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-brave-search"]
env = { BRAVE_API_KEY = "abc" }
"#,
        )
        .unwrap();

        let brave = c.mcp.get("brave").unwrap();
        assert_eq!(brave.env.len(), 1);
        assert_eq!(
            brave.env.get("BRAVE_API_KEY").map(String::as_str),
            Some("abc")
        );

        // Servers without an `env` table get an empty map, not a parse error.
        let fs = Config::from_toml(
            r#"
[mcp.filesystem]
command = "npx"
"#,
        )
        .unwrap();
        assert!(fs.mcp.get("filesystem").unwrap().env.is_empty());
    }

    #[test]
    fn no_mcp_section_is_empty() {
        let c = Config::from_toml("").unwrap();
        assert!(c.mcp.is_empty());
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use std::collections::HashMap;

    const TOML: &str = r#"
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key = "sk-x"
context_window = 128000

[providers.local]
base_url = "http://localhost:8080/v1"
model = "qwen"
"#;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn flag_selects_profile() {
        let c = Config::from_toml(TOML).unwrap();
        let e = env(&[]);
        let r = resolve_credentials(&c, Some("local"), &|k| e.get(k).cloned()).unwrap();
        assert_eq!(r.base_url, "http://localhost:8080/v1");
        assert_eq!(r.provider_name, "local");
    }

    #[test]
    fn default_provider_when_no_flag() {
        let c = Config::from_toml(TOML).unwrap();
        let e = env(&[]);
        let r = resolve_credentials(&c, None, &|k| e.get(k).cloned()).unwrap();
        assert_eq!(r.provider_name, "deepseek");
        assert_eq!(r.api_key, "sk-x");
    }

    #[test]
    fn env_overrides_profile_key() {
        let c = Config::from_toml(TOML).unwrap();
        let e = env(&[("CRAB_API_KEY", "sk-env-override")]);
        let r = resolve_credentials(&c, None, &|k| e.get(k).cloned()).unwrap();
        assert_eq!(r.api_key, "sk-env-override");
    }

    #[test]
    fn profile_without_key_falls_back_to_env() {
        let c = Config::from_toml(TOML).unwrap();
        let e = env(&[("CRAB_API_KEY", "sk-env")]);
        let r = resolve_credentials(&c, Some("local"), &|k| e.get(k).cloned()).unwrap();
        assert_eq!(r.api_key, "sk-env");
    }

    #[test]
    fn unknown_profile_lists_available() {
        let c = Config::from_toml(TOML).unwrap();
        let e = env(&[]);
        let err = resolve_credentials(&c, Some("nope"), &|k| e.get(k).cloned()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"));
        assert!(msg.contains("deepseek") && msg.contains("local"));
    }

    #[test]
    fn no_config_falls_back_to_env_only() {
        let c = Config::default();
        let e = env(&[
            ("CRAB_API_KEY", "sk-env"),
            ("CRAB_BASE_URL", "http://x/v1"),
            ("CRAB_MODEL", "m"),
        ]);
        let r = resolve_credentials(&c, None, &|k| e.get(k).cloned()).unwrap();
        assert_eq!(r.base_url, "http://x/v1");
        assert_eq!(r.model, "m");
        assert_eq!(r.provider_name, "openai");
    }
}
