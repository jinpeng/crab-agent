# 实现计划：`~/.crab/config.toml` 多 provider 配置

> **For agentic workers:** 逐 task TDD。用 checkbox 追踪。依据 spec：`docs/superpowers/specs/2026-10-14-config-and-tui-design.md`。

**Goal:** 用户级 `~/.crab/config.toml` 管理多 provider profile，`--provider <name>` 切换，环境变量可覆盖。

**Tech:** toml 1.1、serde。core 加 `config.rs`；cli 的 `--provider` 从"类型"变"profile 名"。

**前置：** `Credentials::from_env(get_env, read_file)` 已是依赖注入形式（M3）。`OpenAiProvider::new(base_url, api_key, model)`。红线不破。cargo 需代理。

---

### Task 1: Config 结构 + TOML 加载

**Files:**
- Create: `crates/crab-core/src/config.rs`
- Modify: `crates/crab-core/src/lib.rs`（`pub mod config;`）、`Cargo.toml`（workspace + core 加 `toml`）

- [ ] **Step 1: workspace 根 + core 加 `toml = "1.1"`**

- [ ] **Step 2: 失败测试**

```rust
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
        let c = Config::from_str(TOML).unwrap();
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
        let c = Config::from_str("").unwrap();
        assert!(c.default_provider.is_none());
        assert!(c.profile("anything").is_none());
    }

    #[test]
    fn bad_toml_errors() {
        assert!(Config::from_str("[[[invalid").is_err());
    }
}
```

- [ ] **Step 3: 跑红** — `cargo test -p crab-core config 2>&1 | grep -E 'error|test result' | head -3`，Expected: FAIL。

- [ ] **Step 4: 实现 config.rs**

```rust
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
    pub fn from_str(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Load from an explicit path (tests) or default ~/.crab/config.toml. Missing file = empty.
    pub fn load_from(path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => {
                let home = std::env::var_os("HOME").ok_or_else(|| ConfigError::Parse("no HOME".into()))?;
                std::path::Path::new(&home).join(".crab/config.toml")
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::from_str(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    pub fn profile(&self, name: &str) -> Option<&ProviderProfile> {
        self.providers.get(name)
    }
}
```

- [ ] **Step 5: 跑绿 + Commit** — `git commit -m "feat(core): ~/.crab/config.toml profile parsing"`

---

### Task 2: resolve_credentials —— profile + env + flag 合成

**Files:**
- Modify: `crates/crab-core/src/config.rs`、`crates/crab-core/src/provider/auth.rs`

- [ ] **Step 1: 失败测试**

```rust
// config.rs tests
fn env(pairs: &[(&str,&str)]) -> std::collections::HashMap<String,String> {
    pairs.iter().map(|(k,v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn flag_selects_profile() {
    let c = Config::from_str(TOML).unwrap();
    let e = env(&[]);
    let r = resolve_credentials(&c, Some("local"), &|k| e.get(k).cloned()).unwrap();
    assert_eq!(r.base_url, "http://localhost:8080/v1");
    assert_eq!(r.provider_name, "local");
}

#[test]
fn default_provider_when_no_flag() {
    let c = Config::from_str(TOML).unwrap();
    let e = env(&[]);
    let r = resolve_credentials(&c, None, &|k| e.get(k).cloned()).unwrap();
    assert_eq!(r.provider_name, "deepseek");
    assert_eq!(r.api_key, "sk-x");
}

#[test]
fn env_overrides_profile_key() {
    let c = Config::from_str(TOML).unwrap();
    let e = env(&[("CRAB_API_KEY", "sk-env-override")]);
    let r = resolve_credentials(&c, None, &|k| e.get(k).cloned()).unwrap();
    assert_eq!(r.api_key, "sk-env-override");
}

#[test]
fn profile_without_key_falls_back_to_env() {
    let c = Config::from_str(TOML).unwrap();
    let e = env(&[("CRAB_API_KEY", "sk-env")]);
    let r = resolve_credentials(&c, Some("local"), &|k| e.get(k).cloned()).unwrap();
    assert_eq!(r.api_key, "sk-env");   // profile.api_key 为空 → env
}

#[test]
fn unknown_profile_lists_available() {
    let c = Config::from_str(TOML).unwrap();
    let e = env(&[]);
    let err = resolve_credentials(&c, Some("nope"), &|k| e.get(k).cloned()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("nope"));
    assert!(msg.contains("deepseek") && msg.contains("local"));
}

#[test]
fn no_config_falls_back_to_env_only() {
    let c = Config::default();
    let e = env(&[("CRAB_API_KEY","sk-env"),("CRAB_BASE_URL","http://x/v1"),("CRAB_MODEL","m")]);
    let r = resolve_credentials(&c, None, &|k| e.get(k).cloned()).unwrap();
    assert_eq!(r.base_url, "http://x/v1");
    assert_eq!(r.model, "m");
    assert_eq!(r.provider_name, "openai");   // 无 profile 时的默认名
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现**（在 config.rs 或 auth.rs）

```rust
use crate::provider::ProviderError;

pub struct ResolvedProvider {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub context_window: Option<u64>,
    pub provider_name: String,
}

pub fn resolve_credentials(
    config: &Config,
    provider_name: Option<&str>,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedProvider, ProviderError> {
    // mock 是内置特殊 profile
    if provider_name == Some("mock") {
        return Ok(ResolvedProvider {
            base_url: String::new(), model: "mock".into(), api_key: String::new(),
            context_window: None, provider_name: "mock".into(),
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
            if avail.is_empty() { " (none; set CRAB_API_KEY env or add [providers.<name>])" } else { "" }
        ))
    })?;

    let base_url = get_env("CRAB_BASE_URL").or_else(|| get_env("OPENAI_BASE_URL"))
        .unwrap_or_else(|| profile.base_url.clone());
    let model = get_env("CRAB_MODEL").unwrap_or_else(|| profile.model.clone());
    let context_window = get_env("CRAB_CONTEXT_WINDOW")
        .and_then(|s| s.parse().ok())
        .or(profile.context_window);
    let api_key = get_env("CRAB_API_KEY").or_else(|| get_env("OPENAI_API_KEY"))
        .or_else(|| profile.api_key.clone())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ProviderError::Network(format!(
            "no API key for provider '{name}': set CRAB_API_KEY or api_key in the profile"
        )))?;

    Ok(ResolvedProvider { base_url, model, api_key, context_window, provider_name: name })
}
```

无 profile 且 name=="openai" 时的回退：`config.profile("openai")` 会 None → 报错。需要处理"无 config 文件 + 无 flag"的纯环境变量路径。调整：当 `provider_name` 为 None 且 config 为空时，回退到旧的环境变量行为（不经过 profile）。见 Step 4 修正。

- [ ] **Step 4: 处理向后兼容**——无 config 且无 flag 时，走纯 env（旧 `Credentials::from_env` 行为）。在 `resolve_credentials` 顶部：

```rust
// 向后兼容：config 为空（无 providers）且未指定 profile → 纯环境变量（旧行为）
if config.providers.is_empty() && provider_name.is_none() {
    let api_key = get_env("CRAB_API_KEY").or_else(|| get_env("OPENAI_API_KEY"))
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ProviderError::Network("no API key: set CRAB_API_KEY / OPENAI_API_KEY".into()))?;
    return Ok(ResolvedProvider {
        base_url: get_env("CRAB_BASE_URL").or_else(|| get_env("OPENAI_BASE_URL"))
            .unwrap_or_else(|| "https://api.openai.com/v1".into()),
        model: get_env("CRAB_MODEL").unwrap_or_else(|| "gpt-4o-mini".into()),
        api_key,
        context_window: get_env("CRAB_CONTEXT_WINDOW").and_then(|s| s.parse().ok()),
        provider_name: "openai".into(),
    });
}
```

放在 mock 分支之后、profile 查找之前。

- [ ] **Step 5: 跑绿 + Commit** — `git commit -m "feat(core): resolve_credentials profile+env+flag synthesis"`

---

### Task 3: CLI 接线 + auth.rs 收敛

**Files:**
- Modify: `crates/crab-cli/src/app.rs`（build_agent 用 resolve_credentials + OpenAiProvider::new）、`crates/crab-core/src/provider/auth.rs`（保留或收敛）

- [ ] **Step 1: app.rs 的 build_agent 改**——加载 config，按 `--provider` flag resolve，构造 OpenAiProvider（带 context_window 供状态行）。

```rust
pub fn build_agent(cfg: &AppConfig, continue_: bool) -> Result<(Agent, String, ResolvedProvider)> {
    // ...
    let provider: Box<dyn Provider> = match cfg.provider.as_str() {
        "mock" => Box::new(crab_core::provider::MockProvider::new(vec![vec![
            crab_core::stream::StreamEvent::TextDelta("mock response".into()),
            crab_core::stream::StreamEvent::StreamEnd { reason: crab_core::types::StopReason::EndTurn },
        ]])),
        name => {
            let config = crab_core::config::Config::load_from(None)?;
            let r = crab_core::config::resolve_credentials(&config, Some(name), &|k| std::env::var(k).ok())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Box::new(OpenAiProvider::new(r.base_url.clone(), r.api_key.clone(), r.model.clone()))
        }
    };
    // ... 返回 ResolvedProvider 给状态行
}
```

注意：`build_agent` 返回类型加 `ResolvedProvider`（状态行要 provider_name/model/context_window）。调用点（main/interactive）同步改。

`--provider` 默认值从 "openai" 改为不传（None）→ 让 config 的 default_provider 生效。cli.rs 的 `#[arg(default_value="openai")]` 改为 `Option<String>` 无默认。

- [ ] **Step 2: 失败测试（cli）**——`--provider nonexistent` 报错列出可用名（assert_cmd，临时 HOME 写一个 config）

```rust
#[test]
fn unknown_provider_lists_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "default_provider=\"a\"\n[providers.a]\nbase_url=\"http://x\"\nmodel=\"m\"\napi_key=\"k\"\n").unwrap();
    let mut cmd = assert_cmd::Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider").arg("zzz").arg("--print").arg("hi")
        .current_dir(dir.path())
        .env("HOME", dir.path())   // 让 config 从临时 HOME 读
        .assert().failure()
        .stderr(predicates::str::contains("zzz").and(predicates::str::contains("a")));
}
```

需确认 config 从 `$HOME/.crab/config.toml` 读（Task 1 load_from 用 HOME）。

- [ ] **Step 3: 跑绿 + Commit** — `git commit -m "feat(cli): --provider selects config profile"`

---

### Task 4: 完成标志 + 文档

- [ ] **Step 1: 全套件绿 + 红线**

```bash
export https_proxy=http://127.0.0.1:10808
cargo test --workspace 2>&1 | grep 'test result: ok'
cargo tree -p crab-core | grep -iE 'crossterm|reedline|ratatui|clap' && echo VIOLATION || echo "core clean"
```

- [ ] **Step 2: 手动 smoke**——写一个 `~/.crab/config.toml`，`crab --provider deepseek --print "hi"` 和 `crab --provider kimi --print "hi"` 各跑一次。

- [ ] **Step 3: 更新 spec/DESIGN + Commit** — `git commit -m "chore: config profiles complete"`

---

## Self-Review

- **一致性**：`ResolvedProvider.provider_name` 供状态行（功能 2 用）。`resolve_credentials` 依赖注入 `get_env`（不碰真实 env）。
- **坑**：`--provider` 默认值从 "openai" 改为 `Option<String>`（让 config default 生效）；`build_agent` 返回类型加 `ResolvedProvider`；向后兼容分支（无 config 纯 env）必须在 profile 查找前。
- **范围**：只做功能 1。token usage 前置与 TUI 在另一份计划。
