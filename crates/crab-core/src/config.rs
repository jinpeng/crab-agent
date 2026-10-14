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
pub struct Config {
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderProfile>,
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
            context_window: None,
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
