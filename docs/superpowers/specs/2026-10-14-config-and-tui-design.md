# 设计：配置文件 + TUI 美化

**日期**：2026-10-14
**状态**：待 review
**前置**：crab v0 功能完整（M1-M5，Q2 验收达成）。两个独立子系统，分开实现分开验证。

---

## 功能 1：`~/.crab/config.toml` 多 provider 配置

### 目标
用户级配置文件管理多个 provider profile，`--provider <name>` 切换。支持你"测多家 provider/模型"的需求。

### 配置文件形状

```toml
# ~/.crab/config.toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key = "sk-..."            # 可省略，省略时回退到环境变量
context_window = 128000       # 可选；用于状态行的上下文占用比例

[providers.kimi]
base_url = "https://api.moonshot.cn/v1"
model = "kimi-k2"
context_window = 131072

[providers.local]
base_url = "http://localhost:8080/v1"
model = "qwen"
# api_key 省略 → 走环境变量；本地常无需 key
```

### 解析优先级（高到低）

1. `--provider <name>` CLI flag 选定 profile（缺省用 `default_provider`）
2. profile 内的 `api_key`/`base_url`/`model`/`context_window`
3. 环境变量 `CRAB_API_KEY`/`CRAB_BASE_URL`/`CRAB_MODEL`/`CRAB_CONTEXT_WINDOW`（覆盖 profile 对应字段——密钥不落盘时有用）
4. 内置默认：`base_url=https://api.openai.com/v1`、`model=gpt-4o-mini`、无 context_window

`--provider mock` 是内置特殊 profile，不进 config。

### 接口（core）

```rust
// crates/crab-core/src/config.rs
pub struct ProviderProfile {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub context_window: Option<u64>,
}

pub struct Config {
    pub default_provider: Option<String>,
    pub providers: HashMap<String, ProviderProfile>,
}

impl Config {
    /// 读 ~/.crab/config.toml；文件不存在 = 空 Config（不报错）。
    pub fn load() -> Result<Self, ConfigError>;
    pub fn profile(&self, name: &str) -> Option<&ProviderProfile>;
}

/// 把 config profile + 环境变量 + CLI flag 合成最终 Credentials。
/// 替代/扩展现有 Credentials::resolve。
pub fn resolve_credentials(
    config: &Config,
    provider_name: Option<&str>,   // 来自 --provider flag
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedProvider, ProviderError>;

pub struct ResolvedProvider {
    pub base_url: String,
    pub model: String,
    pub api_key: String,             // 解析后必填（mock 除外）
    pub context_window: Option<u64>,
    pub provider_name: String,       // 用于状态行显示
}
```

**依赖注入**：`resolve_credentials` 收 `get_env` 闭包（沿用 M3 的 edition-2024 安全模式），`Config::load` 收可选路径参数便于测试。

### 错误处理
- `--provider foo` 但 config 无此 profile → 报错并列出可用 profile 名。
- 无 config 文件且 `--provider` 非 mock → 回退到纯环境变量解析（向后兼容现状）。
- TOML 解析错 → 报错带行号。

### 测试
- `resolve_credentials` 各优先级组合的单元测试（注入 env/config，不碰真实环境）。
- `--provider nonexistent` 报错列出可用名。
- 向后兼容：无 config 时纯 env 解析不变。

---

## 功能 2：TUI 美化（纯 crossterm，不上 ratatui）

### 目标
把纯文本流升级为有结构、有着色、有状态行的 agent 界面。参照 pi 的可落地启发：语义颜色 token、按可见宽度渲染。**不上 ratatui 全组件系统**（Q10）。

### 三件套

**A. 语义着色**（crossterm `SetForegroundColor` / `Stylize`）：
- 用户输入：bright/加粗
- assistant 文本：默认色
- 工具名：cyan
- 工具成功：green `✓`，失败：red `✗`
- 权限确认提示：yellow `⚠`
- 状态行：反色/暗淡背景

定义一个 `Theme` 结构（语义 token → crossterm Color），不硬编码颜色散落各处。这对应 pi 的语义 token 思路。

**B. 结构化工具块**：
```
  ● read  crates/crab-core/src/session.rs
    ✓ 1.2 KB
  ● bash  cargo test
    ✗ exit 1
      (output truncated, first 3 lines)
```
- 工具名彩色圆点 + 参数（截断）
- 结果状态行缩进显示
- 输出默认截断前 3 行（不折叠，YAGNI）

**C. 底部状态行**（常驻一行）：
```
 deepseek/deepseek-chat · streaming · 1.2k tok · ctx 8.5k/128k (7%) · ~/code/crab-agent
```
- provider/model、状态（streaming/waiting tool/waiting confirm/idle）
- 本次 run 累计 token
- 上下文：已用/窗口（百分比）；无 context_window 配置则只显示绝对数
- 当前工作目录（`~` 缩写）

### 状态行渲染机制（纯 crossterm）

终端分"内容滚动区 + 底部 1 行状态行"：
- 用 `scroll region`（`SetScrollRegion(0, height-2)`）让上面内容滚动、底部固定。
- 状态行更新：保存光标 → 移到末行 → 写 → 恢复光标。
- 流式 TextDelta 照旧在内容区 print。

**宽度处理**（pi 启发）：状态行和工具块的截断按**可见列宽**算，不是字节/字符数。crossterm 有 ANSI，需剥离 escape 后量宽。加一个小的 `visible_width`/`truncate_to_width` 辅助（不引外部宽字符 crate，ASCII 为主先近似；wide-char 精确处理留 TODO）。

### token usage 前置（必须先做）

当前管线**不捕获 token usage**（StreamEvent 无 usage 变体、parse_chunk 不读、请求没开 `stream_options.include_usage`）。要显示 token 用量需：

1. `request_body` 加 `"stream_options": {"include_usage": true}`（OpenAI 协议）。
2. `parse_chunk` 识别 usage 帧（OpenAI 流式在末尾发一个 `choices:[]` + `usage` 的帧）。
3. `StreamEvent` 加 `Usage { prompt_tokens: u32, completion_tokens: u32 }` 变体。
4. agent 累计进一个 `usage` 计数，透传为 `AgentEvent::Usage{...}`。
5. render 层据此更新状态行。

**偏差处理**：DeepSeek 是否支持 `stream_options.include_usage` 需实测；不支持则状态行的 token 显示"不可用"，不报错。

### 接口

```rust
// crates/crab-cli/src/theme.rs
pub struct Theme { /* semantic tokens → Color */ }
impl Theme { pub fn dark() -> Self; /* v0 只有 dark */ }

// crates/crab-cli/src/render.rs（重写）
pub struct Renderer { /* theme, terminal size, status state */ }
impl Renderer {
    pub async fn run(&mut self, rx: Receiver<AgentEvent>);  // 事件驱动渲染
    fn render_status_line(&self);   // 底部状态行
    fn render_tool_block(...);      // 结构化工具块
}

// crates/crab-cli/src/status.rs
pub struct StatusLine {
    pub provider_model: String,
    pub state: RunState,            // Streaming | WaitingTool | WaitingConfirm | Idle
    pub tokens_used: u64,
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
    pub cwd: PathBuf,
}
```

### 测试
- `visible_width`/`truncate_to_width` 辅助的单测（ANSI 剥离、截断）。
- 状态行格式化（各字段组合、无 context_window 时降级）。
- 工具块渲染输出快照（insta）。
- usage 解析：fixture 一个含 usage 帧的 SSE 流。
- 端到端渲染测试仍用 MockProvider + assert_cmd（`--print` 模式输出包含 ANSI 序列断言）。

### 明确不做（YAGNI）
- 不上 ratatui / 无 alt-screen / 无 Markdown 渲染 / 无可折叠工具块 / 无 mouse / 无多主题（只 dark）。
- 这些参照 pi 但都超 v0 需求；dogfood 更充分后再说。

---

## 实现顺序与依赖

1. **config（功能 1）先做**——它独立、简单、TUI 的状态行需要 `context_window`。
2. **token usage 前置**——TUI 状态行的硬依赖。
3. **TUI 三件套**——着色 + 工具块 + 状态行。

**dogfood 方式**：按你的要求"用 crab-agent 完成"，实现这些功能时尽量让 crab 参与（read/edit/test）。crab 卡住的部分人补，记入 dogfood-log。

---

## Self-Review

- **Placeholder scan**：无 TBD；wide-char 精确宽度标注为 TODO（有意的降级，非占位）。
- **一致性**：config 的 `context_window` 与状态行的占用比例字段对齐；usage 前置与 StreamEvent/AgentEvent 演进一致。
- **范围**：两个独立子系统，各自可单独实现验证。TUI 控制在 crossterm 范围内（B 档），不碰 ratatui。
- **歧义**：`--provider` 从"选类型"变"选 profile 名"已明确；mock 保留为内置特殊 profile。
