# 实现计划：MCP client（stdio，tools-only）

> **For agentic workers:** 逐 task TDD。依据 spec：`docs/superpowers/specs/2026-10-14-mcp-client-design.md`。

**Goal:** crab 作为 MCP client，spawn config 里 `enabled` 的 stdio MCP server，把它们的 tools 暴露为 crab 的 `Tool`（命名空间 `<server>__<tool>`），agent 可调用。启动失败容错，MCP 工具默认过权限门禁。

**Tech:** rmcp 3.5（features `client` + `transport-child-process`，default-features=false）、tokio process。

**前置：** config 多 provider（`Config`/`ProviderProfile` 在 config.rs）。`Tool` trait + `ToolRegistry`。红线不破（core 无终端 crate）。cargo 需代理。

---

## 接口契约

```rust
// crates/crab-core/src/mcp.rs

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]          // 缺省启用
    pub enabled: Option<bool>,
}

/// 一个连上的 MCP server（包装 rmcp client）。
pub struct McpClient { /* server_name, rmcp RunningService, gate */ }

impl McpClient {
    pub async fn spawn(name: &str, cfg: &McpServerConfig, gate: PermissionGate) -> Result<Self, McpError>;
    pub async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>, McpError>;
    pub async fn call(&self, tool: &str, args: serde_json::Value) -> Result<String, McpError>;
    pub async fn shutdown(self);
}

/// 把一个 MCP tool 包成 crab Tool。命名 `<server>__<tool>`。
pub struct McpTool { /* client: Arc<McpClient>, server, tool_name, description, schema */ }
```

`Tool::name()` 改为返回 `Cow<'static, str>`（内置工具 `Cow::Borrowed`，MCP 工具 `Cow::Owned`）。`ToolRegistry` key 改 `String`。

`Config` 加 `#[serde(default)] pub mcp: HashMap<String, McpServerConfig>`。

---

### Task 1: Tool::name() 改 Cow + Registry key 改 String（重构，无行为变化）

**Files:**
- Modify: `crates/crab-core/src/tool.rs`、6 个内置工具 + web_fetch、agent.rs 里的 Noop 测试工具

**这步纯重构**，为 MCP 工具（运行时名字）铺路。dogfood 让 crab 做。

- [x] **Step 1: 失败测试**——registry 能接受 String 名字的 tool

```rust
// tool.rs tests
#[tokio::test]
async fn registry_accepts_owned_string_names() {
    struct Dyn;
    #[async_trait]
    impl Tool for Dyn {
        fn name(&self) -> std::borrow::Cow<'static, str> { std::borrow::Cow::Owned("srv__tool".to_string()) }
        fn description(&self) -> &'static str { "d" }
        fn schema(&self) -> serde_json::Value { serde_json::json!({"type":"object"}) }
        async fn execute(&self, _a: serde_json::Value) -> Result<String, ToolError> { Ok("ok".into()) }
    }
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(Dyn));
    assert!(reg.contains("srv__tool"));
    assert_eq!(reg.execute("srv__tool", serde_json::json!({})).await.unwrap(), "ok");
}
```

- [x] **Step 2: 跑红**——`cargo test -p crab-core tool 2>&1 | grep -E 'error|test result'`，Expected: FAIL（name() 还是 &'static str）。

- [x] **Step 3: 改 trait + 全部实现 + Registry**

```rust
// tool.rs
use std::borrow::Cow;
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> Cow<'static, str>;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError>;
}

pub struct ToolRegistry { tools: HashMap<String, Box<dyn Tool>> }
impl ToolRegistry {
    pub fn register(&mut self, t: Box<dyn Tool>) { self.tools.insert(t.name().into_owned(), t); }
    pub fn contains(&self, name: &str) -> bool { self.tools.contains_key(name) }
    pub fn schemas(&self) -> Vec<serde_json::Value> { self.tools.values().map(|t| t.schema()).collect() }
    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String, ToolError> {
        match self.tools.get(name) {
            Some(t) => t.execute(args).await,
            None => Err(ToolError::UnknownTool(name.to_string())),
        }
    }
}
```

6 内置工具 + web_fetch + 所有 Noop 测试工具的 `fn name(&self) -> &'static str { "x" }` 改为 `fn name(&self) -> Cow<'static, str> { Cow::Borrowed("x") }`。schemas() 里 `"name": t.name()` 不变（Cow 可序列化为字符串）。

- [x] **Step 4: 跑绿全套件**——所有 `name()` 调用点（registry insert、schemas）适配。

- [x] **Step 5: Commit** — `git commit -m "refactor(core): Tool::name returns Cow, registry keys String (prep for MCP)"`

---

### Task 2: Config 加 `[mcp.*]` 段

**Files:**
- Modify: `crates/crab-core/src/config.rs`

- [x] **Step 1: 失败测试**

```rust
#[test]
fn parses_mcp_servers() {
    let c = Config::from_toml(r#"
default_provider = "deepseek"
[providers.deepseek]
base_url = "x"
model = "m"
api_key = "k"
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
[mcp.off]
command = "srv"
enabled = false
"#).unwrap();
    assert_eq!(c.mcp.len(), 2);
    let fs = &c.mcp["filesystem"];
    assert_eq!(fs.command, "npx");
    assert_eq!(fs.enabled, None);          // 缺省
    assert!(fs.is_enabled());              // 缺省 = true
    assert!(!c.mcp["off"].is_enabled());   // 显式 false
}

#[test]
fn no_mcp_section_is_empty() {
    let c = Config::from_toml("").unwrap();
    assert!(c.mcp.is_empty());
}
```

- [x] **Step 2: 跑红** → Expected: FAIL。

- [x] **Step 3: 实现**——`Config` 加 `mcp: HashMap<String, McpServerConfig>`，`McpServerConfig::is_enabled()` 返回 `self.enabled.unwrap_or(true)`。

- [x] **Step 4: 跑绿 + Commit** — `git commit -m "feat(core): config [mcp.*] server sections"`

---

### Task 3: McpClient——spawn + list_tools + call

**Files:**
- Create: `crates/crab-core/src/mcp.rs`
- Modify: `crates/crab-core/src/lib.rs`、`Cargo.toml`（workspace + core 加 rmcp）

**这步需要 rmcp 真实 API，最容易卡。让 crab 做，但给足 rmcp 用法样例（spec 里那段）。**

- [x] **Step 1: 加依赖**

`Cargo.toml` workspace：`rmcp = { version = "3.5", default-features = false, features = ["client", "transport-child-process"] }`；core 加 `rmcp.workspace = true`。

- [x] **Step 2: 失败测试**——用一个 in-process mock MCP server 验证 spawn/list/call

rmcp 测试子进程最好用真实的最小 server。用 rmcp server 端写一个 echo server 作为测试 fixture（`#[tokio::test]` 里 spawn 它）。或者更简单：写一个 shell 脚本当假 MCP server。最稳的是用 rmcp 自己起一个 in-process server。

crab 实现时决定测试策略，但必须有：spawn 成功 + list_tools 返回工具 + call 返回结果 + spawn 失败容错（命令不存在时 Err 不 panic）。

- [x] **Step 3: 实现 McpClient**

```rust
use rmcp::{ServiceExt, transport::{TokioChildProcess, ConfigureCommandExt}, model::CallToolRequestParams};
use tokio::process::Command;

pub async fn spawn(name: &str, cfg: &McpServerConfig, gate: PermissionGate) -> Result<Self, McpError> {
    let cmd = Command::new(&cfg.command).configure(|c| { c.args(&cfg.args); });
    let service = ().serve(TokioChildProcess::new(cmd)?).await?;  // spawn + handshake
    Ok(Self { server_name: name.into(), service, gate })
}
```

- [x] **Step 4: 跑绿 + Commit** — `git commit -m "feat(core): McpClient spawn/list/call over stdio"`

---

### Task 4: McpTool 包装 + register 进 ToolRegistry

**Files:**
- Modify: `crates/crab-core/src/mcp.rs`、`crates/crab-core/src/tools/mod.rs`（或新装配点）

- [x] **Step 1: 失败测试**——McpTool 的 name 是 `<server>__<tool>`，门禁 deny 时不调用

- [x] **Step 2: 实现 McpTool**

```rust
pub struct McpTool {
    client: Arc<McpClient>,
    server: String,
    tool_name: String,
    description: String,
    schema: serde_json::Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!("{}__{}", self.server, self.tool_name))
    }
    fn description(&self) -> &'static str { Box::leak(self.description.clone().into_boxed_str()) }
    // description 也要 &'static str —— leak 或 trait 也改 Cow。见注意点。
    fn schema(&self) -> serde_json::Value { self.schema.clone() }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        self.client.gate.check(Prompt { tool: "mcp", summary: format!("{}::{} {:?}", self.server, self.tool_name, args) })
            .map_err(|e| e)?;
        self.client.call(&self.tool_name, args).await.map_err(|e| ToolError::Exec(e.to_string()))
    }
}
```

**注意**：`Tool::description()` 也是 `&'static str`，MCP 工具描述是运行时的。要么也改 `Cow<'static, str>`（推荐，一致性），要么 leak。计划里统一改成 `Cow`。

- [x] **Step 3: 跑绿 + Commit** — `git commit -m "feat(core): McpTool wraps MCP tools as crab Tool with namespacing + gate"`

---

### Task 5: 启动时装配 + CLI 接线

**Files:**
- Modify: `crates/crab-cli/src/app.rs`（build_agent 启动 enabled 的 MCP server）

- [x] **Step 1: build_agent 加 MCP 启动**

```rust
// register_all 之后：
let config = Config::load_from(None)?;
let mut mcp_clients = Vec::new();
for (name, srv) in &config.mcp {
    if !srv.is_enabled() { continue; }
    match McpClient::spawn(name, srv, gate.clone()).await {
        Ok(client) => {
            match client.list_tools().await {
                Ok(tools) => {
                    let client = Arc::new(client);
                    for t in tools {
                        registry.register(Box::new(McpTool::new(client.clone(), name, t)));
                    }
                    mcp_clients.push(client);
                }
                Err(e) => eprintln!("[crab] mcp server '{name}': list_tools failed: {e} (skipped)"),
            }
        }
        Err(e) => eprintln!("[crab] mcp server '{name}': spawn failed: {e} (skipped)"),
    }
}
```

`build_agent` 变 async（spawn 是 async）——调用点 main/interactive 改 `.await`。注意 eprintln 在 cli 层（app.rs 是 cli），不破 core 红线。

- [x] **Step 2: e2e 测试**——config 写一个假 MCP server（echo），`crab --print` 能列出/调用。

- [x] **Step 3: 跑绿 + Commit** — `git commit -m "feat(cli): spawn + register enabled MCP servers at startup"`

---

### Task 6: 完成标志 + 体积 + 文档

- [x] **Step 1: 全套件绿 + 红线 + release 体积**（rmcp 加了多少）

```bash
export https_proxy=http://127.0.0.1:10808
cargo test --workspace 2>&1 | grep 'test result: ok'
cargo tree -p crab-core | grep -iE 'crossterm|reedline|ratatui|clap' && echo VIOLATION || echo "core clean"
cargo build --release; ls -l target/release/crab | awk '{printf "%.2f MiB\n", $5/1048576}'
```

- [x] **Step 2: 手动 smoke**——config 配一个真 MCP server（如 filesystem），`crab --print "list files"` 看 agent 调用 MCP 工具。

- [x] **Step 3: 更新 spec/DESIGN + dogfood-log + Commit**

---

## Self-Review

- **`Tool::name()` 改 `Cow`**：spec 说改 `String`，计划改 `Cow<'static, str>` 更优（内置工具零成本 Borrowed）。`description()` 同样问题（MCP 描述是运行时）——统一改 `Cow`。这是计划对 spec 的一处细化。
- **一致性**：`<server>__<tool>` 命名贯穿；`is_enabled()` 缺省 true；门禁走 gate。
- **坑**：rmcp 的 `list_tools`/`call_tool` 返回类型和 content[] 反序列化按 rmcp 3.5 实际 API 调；测试用 in-process server 而非真子进程（快、稳）；`build_agent` 变 async 牵连调用点。
- **体积**：rmcp 会加进 release，Task 6 实测。
- **dogfood**：Task 1（重构）和 Task 3（McpClient）让 crab 做，给足 spec 里 rmcp 样例。Task 4 的 description Cow 改动是重点。
