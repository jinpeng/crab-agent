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
