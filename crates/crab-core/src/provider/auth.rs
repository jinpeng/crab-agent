use crate::provider::ProviderError;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct Credentials {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl Credentials {
    /// Priority: CRAB_* env > OPENAI_* env > ~/.crab/auth.json.
    pub fn resolve() -> Result<Self, ProviderError> {
        Self::from_env(&|k| std::env::var(k).ok(), &|p| {
            std::fs::read_to_string(p).ok()
        })
    }

    /// Dependency-injected core: `get_env`/`read_file` so tests don't touch the real
    /// process env (edition 2024 marks `set_var` unsafe for good reason).
    pub fn from_env(
        get_env: &dyn Fn(&str) -> Option<String>,
        read_file: &dyn Fn(&std::path::Path) -> Option<String>,
    ) -> Result<Self, ProviderError> {
        let mut api_key = get_env("CRAB_API_KEY").or_else(|| get_env("OPENAI_API_KEY"));
        let mut base_url = get_env("CRAB_BASE_URL")
            .or_else(|| get_env("OPENAI_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let mut model = get_env("CRAB_MODEL").unwrap_or_else(|| "gpt-4o-mini".into());

        if api_key.is_none()
            && let Some(home) = get_env("HOME")
        {
            let p = std::path::Path::new(&home).join(".crab/auth.json");
            if let Some(raw) = read_file(&p)
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            {
                api_key = v["api_key"].as_str().map(String::from);
                if let Some(b) = v["base_url"].as_str() {
                    base_url = b.into();
                }
                if let Some(m) = v["model"].as_str() {
                    model = m.into();
                }
            }
        }

        let api_key = api_key.filter(|k| !k.is_empty()).ok_or_else(|| {
            ProviderError::Network(
                "no API key: set CRAB_API_KEY / OPENAI_API_KEY or ~/.crab/auth.json".into(),
            )
        })?;
        Ok(Self {
            api_key,
            base_url,
            model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn no_file(_: &std::path::Path) -> Option<String> {
        None
    }

    #[test]
    fn crab_env_var_wins() {
        let e = env(&[("CRAB_API_KEY", "sk-env")]);
        let c = Credentials::from_env(&|k| e.get(k).cloned(), &no_file).unwrap();
        assert_eq!(c.api_key, "sk-env");
    }

    #[test]
    fn openai_env_fallback() {
        let e = env(&[("OPENAI_API_KEY", "sk-openai")]);
        let c = Credentials::from_env(&|k| e.get(k).cloned(), &no_file).unwrap();
        assert_eq!(c.api_key, "sk-openai");
    }

    #[test]
    fn base_url_default_and_override() {
        let e = env(&[("CRAB_API_KEY", "k")]);
        let c = Credentials::from_env(&|k| e.get(k).cloned(), &no_file).unwrap();
        assert_eq!(c.base_url, "https://api.openai.com/v1");

        let e2 = env(&[
            ("CRAB_API_KEY", "k"),
            ("CRAB_BASE_URL", "http://localhost:8080/v1"),
        ]);
        let c2 = Credentials::from_env(&|k| e2.get(k).cloned(), &no_file).unwrap();
        assert_eq!(c2.base_url, "http://localhost:8080/v1");
    }

    #[test]
    fn auth_json_fallback_when_no_env() {
        let e = env(&[("HOME", "/home/u")]);
        let auth_json =
            r#"{"api_key":"sk-file","base_url":"http://local/v1","model":"deepseek-chat"}"#;
        let c = Credentials::from_env(&|k| e.get(k).cloned(), &|p| {
            if p == std::path::Path::new("/home/u/.crab/auth.json") {
                Some(auth_json.to_string())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(c.api_key, "sk-file");
        assert_eq!(c.base_url, "http://local/v1");
        assert_eq!(c.model, "deepseek-chat");
    }

    #[test]
    fn missing_credentials_is_error() {
        let e = env(&[]);
        assert!(Credentials::from_env(&|k| e.get(k).cloned(), &no_file).is_err());
    }

    #[test]
    fn empty_key_is_treated_as_missing() {
        let e = env(&[("CRAB_API_KEY", "")]);
        assert!(Credentials::from_env(&|k| e.get(k).cloned(), &no_file).is_err());
    }
}
