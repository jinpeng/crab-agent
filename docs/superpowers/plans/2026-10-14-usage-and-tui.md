# 实现计划：token usage 前置 + TUI 美化

> **For agentic workers:** 逐 task TDD。依据 spec：`docs/superpowers/specs/2026-10-14-config-and-tui-design.md`。**依赖 config 计划**（状态行要 `context_window`/`provider_name`）。

**Goal:** (a) 管线捕获 token usage（StreamEvent::Usage）；(b) TUI 三件套——语义着色、结构化工具块、底部状态行。纯 crossterm，不上 ratatui。

**Tech:** crossterm 0.29、insta（快照）。

**前置：** `Provider::complete(messages, tools, on_event)` 已实时推 StreamEvent。DeepSeek 实测：最后一帧 `choices[0].finish_reason` + 顶层 `usage` **同帧**（不用 `stream_options`）。红线不破。cargo 需代理。

---

## 关键顺序陷阱（已勘察）

DeepSeek 把 `usage` 和 `finish_reason` 放**同一帧**。`complete` 在 `StreamEnd` 时 `break 'outer`——若 `parse_chunk` 只产 `StreamEnd` 漏掉同帧 usage 就丢了。**`parse_chunk` 必须同帧产 `Usage` + `StreamEnd` 两个事件**（Vec 返回支持）。Usage 事件必须先 push 进 accumulator/通道，再产 StreamEnd。

---

### Task 1: StreamEvent::Usage + parse_chunk 捕获 usage 帧

**Files:**
- Modify: `crates/crab-core/src/stream.rs`、`crates/crab-core/src/provider/sse.rs`

- [ ] **Step 1: 失败测试**

```rust
// sse.rs tests
#[test]
fn usage_frame_with_finish_reason_yields_both_events() {
    // DeepSeek 真实形状：finish_reason 与 usage 同帧
    let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":1,"total_tokens":10}}"#).unwrap();
    assert!(e.iter().any(|x| matches!(x, StreamEvent::Usage { prompt_tokens: 9, completion_tokens: 1, .. })));
    assert!(e.iter().any(|x| matches!(x, StreamEvent::StreamEnd { reason: StopReason::EndTurn })));
    // Usage 必须先于 StreamEnd（accumulator/通道顺序）
    let pos_usage = e.iter().position(|x| matches!(x, StreamEvent::Usage{..})).unwrap();
    let pos_end = e.iter().position(|x| matches!(x, StreamEvent::StreamEnd{..})).unwrap();
    assert!(pos_usage < pos_end);
}

#[test]
fn usage_null_frames_ignored() {
    // DeepSeek 中间帧 usage:null
    let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":null}"#).unwrap();
    assert_eq!(e, vec![StreamEvent::TextDelta("hi".into())]);
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现**——`StreamEvent` 加 `Usage { prompt_tokens: u32, completion_tokens: u32 }`；`parse_chunk` 在 finish_reason 之前先查顶层 `usage`（非 null）产 `Usage` 事件。

```rust
// stream.rs
pub enum StreamEvent {
    // ...existing...
    Usage { prompt_tokens: u32, completion_tokens: u32 },
}

// sse.rs，在 finish_reason 处理之前：
if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
    let pt = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
    let ct = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
    out.push(StreamEvent::Usage { prompt_tokens: pt, completion_tokens: ct });
}
```

`StreamAccumulator::push` 对 `Usage` 的处理：存到 accumulator（`last_usage: Option<(u32,u32)>`），`finish()` 时填进 `AssistantTurn`。`AssistantTurn` 加 `pub usage: Option<(u32,u32)>`。

- [ ] **Step 4: 跑绿 + Commit** — `git commit -m "feat(core): capture token usage from SSE stream"`

---

### Task 2: agent 透传 usage 为 AgentEvent::Usage

**Files:**
- Modify: `crates/crab-core/src/agent.rs`

- [ ] **Step 1: 失败测试**——agent 收到 provider 的 usage 后累计并发 `AgentEvent::Usage`

```rust
// agent.rs tests
#[tokio::test]
async fn agent_emits_usage_event() {
    let provider = MockProvider::new(vec![vec![
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Usage { prompt_tokens: 10, completion_tokens: 5 },
        StreamEvent::StreamEnd { reason: StopReason::EndTurn },
    ]]);
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(Noop));
    let dir = tempfile::tempdir().unwrap();
    let session = crate::session::Session::create(dir.path()).unwrap();
    let mut agent = Agent::new(Box::new(provider), tools, session, AgentConfig { max_turns: 5, ..Default::default() });
    let mut rx = agent.subscribe();
    agent.run("hi".into()).await.unwrap();
    let mut usage = None;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Usage { prompt_tokens, completion_tokens } = ev {
            usage = Some((prompt_tokens, completion_tokens));
        }
    }
    assert_eq!(usage, Some((10, 5)));
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现**——`AgentEvent` 加 `Usage { prompt_tokens: u32, completion_tokens: u32 }`；agent 的 forward task 把 `StreamEvent::Usage` 转成 `AgentEvent::Usage`（不止 TextDelta）。MockProvider 已通过 on_event 推 Usage（StreamEvent 透传）。

注意 forward task 现在只转发 TextDelta，要加 Usage 分支。

- [ ] **Step 4: 跑绿 + Commit** — `git commit -m "feat(agent): emit AgentEvent::Usage from provider stream"`

---

### Task 3: Theme（语义着色 token）

**Files:**
- Create: `crates/crab-cli/src/theme.rs`

- [ ] **Step 1: 失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_styles_produce_ansi() {
        let t = Theme::dark();
        let s = t.tool_name("read");
        assert!(s.contains("read"));
        assert!(s.contains("\x1b["), "must contain ANSI escape");
    }

    #[test]
    fn distinct_roles_have_distinct_codes() {
        let t = Theme::dark();
        assert_ne!(t.tool_name("x"), t.error("x"));
        assert_ne!(t.success("x"), t.error("x"));
    }
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现**——用语义方法包装 crossterm `Stylize`

```rust
use crossterm::style::Stylize;

#[derive(Clone, Copy)]
pub struct Theme;

impl Theme {
    pub fn dark() -> Self { Self }
    pub fn user_input(&self, s: &str) -> String { s.bold().to_string() }
    pub fn tool_name(&self, s: &str) -> String { s.cyan().to_string() }
    pub fn success(&self, s: &str) -> String { s.green().to_string() }
    pub fn error(&self, s: &str) -> String { s.red().to_string() }
    pub fn warning(&self, s: &str) -> String { s.yellow().to_string() }
    pub fn dim(&self, s: &str) -> String { s.dark_grey().to_string() }
}
```

- [ ] **Step 4: 跑绿 + Commit** — `git commit -m "feat(cli): semantic color theme"`

---

### Task 4: 可见宽度辅助（ANSI 剥离 + 截断）

**Files:**
- Create: `crates/crab-cli/src/width.rs`

- [ ] **Step 1: 失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_strips_ansi() {
        assert_eq!(visible_width("\x1b[36mread\x1b[0m"), 4);
        assert_eq!(visible_width("plain"), 5);
    }

    #[test]
    fn truncate_to_width_keeps_ansi_intact() {
        let s = "\x1b[36mhello world\x1b[0m";
        let t = truncate_to_width(s, 5);
        assert_eq!(visible_width(&t), 5);
        assert!(t.contains("\x1b[0m"), "reset code preserved");
    }

    #[test]
    fn truncate_plain_short_string_noop() {
        assert_eq!(truncate_to_width("hi", 10), "hi");
    }
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现**——剥 ANSI 量宽 + 截断保留 escape。ASCII 近似（宽字符精确留 TODO）。

```rust
/// Visible column count, ignoring ANSI escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // skip until 'm' (SGR) — v0 handles color codes only
            for c2 in chars.by_ref() { if c2 == 'm' { break; } }
        } else {
            w += 1;  // ASCII approximation; wide chars = TODO
        }
    }
    w
}

/// Truncate to `max` visible columns, preserving ANSI codes and ending with reset.
pub fn truncate_to_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    let mut chars = s.chars().peekable();
    let mut had_ansi = false;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            had_ansi = true;
            out.push(c);
            for c2 in chars.by_ref() { out.push(c2); if c2 == 'm' { break; } }
            continue;
        }
        if w >= max { break; }
        out.push(c);
        w += 1;
    }
    if had_ansi && !out.ends_with("\x1b[0m") { out.push_str("\x1b[0m"); }
    out
}
```

- [ ] **Step 4: 跑绿 + Commit** — `git commit -m "feat(cli): visible-width helpers (ANSI-aware truncate)"`

---

### Task 5: 状态行（StatusLine + 底部固定渲染）

**Files:**
- Create: `crates/crab-cli/src/status.rs`

- [ ] **Step 1: 失败测试**——格式化各字段组合、无 context_window 降级

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_full_status() {
        let s = StatusLine {
            provider_model: "deepseek/deepseek-chat".into(),
            state: RunState::Streaming,
            tokens_used: 1200,
            context_used: Some(8500),
            context_window: Some(128000),
            cwd: std::path::PathBuf::from("/home/u/code/crab-agent"),
        };
        let line = s.format(120);
        assert!(line.contains("deepseek/deepseek-chat"));
        assert!(line.contains("streaming"));
        assert!(line.contains("1.2k"));
        assert!(line.contains("8.5k/128.0k"));
        assert!(line.contains("7%"));
        assert!(line.contains("crab-agent"));
    }

    #[test]
    fn no_context_window_shows_absolute_only() {
        let s = StatusLine {
            provider_model: "local/qwen".into(),
            state: RunState::Idle,
            tokens_used: 0,
            context_used: Some(500),
            context_window: None,
            cwd: std::path::PathBuf::from("/tmp"),
        };
        let line = s.format(120);
        assert!(line.contains("500"));
        assert!(!line.contains('%'));
    }

    #[test]
    fn truncates_to_terminal_width() {
        let s = StatusLine {
            provider_model: "a-very-long-provider-name/and-a-very-long-model-name".into(),
            state: RunState::Streaming, tokens_used: 0,
            context_used: Some(999999), context_window: Some(999999),
            cwd: std::path::PathBuf::from("/very/long/path/that/keeps/going/and/going"),
        };
        let line = s.format(40);
        assert!(crate::width::visible_width(&line) <= 40);
    }
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现**——StatusLine 结构 + `format(width)` 用 `truncate_to_width`，`render` 用 crossterm 保存光标/移末行/写/恢复 + scroll region。

```rust
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq)]
pub enum RunState { Idle, Streaming, WaitingTool, WaitingConfirm }

pub struct StatusLine {
    pub provider_model: String,
    pub state: RunState,
    pub tokens_used: u64,
    pub context_used: Option<u64>,
    pub context_window: Option<u64>,
    pub cwd: PathBuf,
}

impl StatusLine {
    pub fn format(&self, width: usize) -> String {
        let state = match self.state {
            RunState::Idle => "idle",
            RunState::Streaming => "streaming",
            RunState::WaitingTool => "tool",
            RunState::WaitingConfirm => "confirm",
        };
        let ctx = match (self.context_used, self.context_window) {
            (Some(u), Some(w)) if w > 0 => format!("ctx {}/{} ({}%)", kfmt(u), kfmt(w), u * 100 / w),
            (Some(u), _) => format!("ctx {}", kfmt(u)),
            _ => String::new(),
        };
        let cwd = self.cwd.display().to_string();
        let cwd = abbrev_home(&cwd);
        let line = format!(" {} · {} · {} tok · {} · {} ",
            self.provider_model, state, kfmt(self.tokens_used), ctx, cwd);
        crate::width::truncate_to_width(&line, width)
    }
}

fn kfmt(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

fn abbrev_home(p: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let h = home.to_string_lossy();
        if let Some(rest) = p.strip_prefix(h.as_ref()) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}
```

`render` 方法（crossterm scroll region + 光标控制）在 Task 6 的 Renderer 里接线。

- [ ] **Step 4: 跑绿 + Commit** — `git commit -m "feat(cli): StatusLine formatting"`

---

### Task 6: Renderer 重写——着色 + 工具块 + 状态行接线

**Files:**
- Modify: `crates/crab-cli/src/render.rs`

- [ ] **Step 1: 失败测试**——Renderer 把事件流转成着色输出（用 insta 快照或断言 ANSI 内容）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_block_formatting() {
        let t = crate::theme::Theme::dark();
        let block = format_tool_call(&t, "read", "crates/x.rs", Some("1.2 KB"), false);
        assert!(block.contains("read"));
        assert!(block.contains("crates/x.rs"));
        assert!(block.contains("✓"));
    }

    #[test]
    fn tool_error_block() {
        let t = crate::theme::Theme::dark();
        let block = format_tool_call(&t, "bash", "rm x", Some("no such file"), true);
        assert!(block.contains("✗"));
    }
}
```

- [ ] **Step 2: 跑红** — Expected: FAIL。

- [ ] **Step 3: 实现 Renderer**

```rust
use crate::status::{RunState, StatusLine};
use crate::theme::Theme;
use crate::width::{truncate_to_width, visible_width};
use crab_core::agent::AgentEvent;
use crossterm::{cursor, execute, terminal};
use std::io::Write;

pub struct Renderer {
    theme: Theme,
    status: StatusLine,
    width: usize,
    height: usize,
}

impl Renderer {
    pub fn new(status: StatusLine) -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        Self { theme: Theme::dark(), status, width: w as usize, height: h as usize }
    }

    /// Interactive: set scroll region so content scrolls above a fixed status line.
    pub fn setup(&self) {
        let mut out = std::io::stdout();
        let _ = execute!(out, terminal::SetScrollRegion(1, self.height as u16 - 1));
        self.draw_status();
    }

    pub fn teardown(&self) {
        let mut out = std::io::stdout();
        let _ = execute!(out, terminal::SetScrollRegion(1, self.height as u16));
        let _ = execute!(out, cursor::MoveTo(0, self.height as u16 - 1));
    }

    pub fn draw_status(&self) {
        let mut out = std::io::stdout();
        let line = self.status.format(self.width);
        let _ = execute!(out, cursor::SavePosition, cursor::MoveTo(0, self.height as u16 - 1));
        let _ = execute!(out, terminal::Clear(terminal::ClearType::CurrentLine));
        print!("{}", self.theme.dim(&line));
        let _ = execute!(out, cursor::RestorePosition);
        let _ = out.flush();
    }

    pub async fn run(&mut self, mut rx: tokio::sync::broadcast::Receiver<AgentEvent>) {
        let mut out = std::io::stdout();
        while let Ok(ev) = rx.recv().await {
            match ev {
                AgentEvent::TextDelta(s) => { print!("{s}"); let _ = out.flush(); }
                AgentEvent::ToolCallStarted { name, .. } => {
                    self.status.state = RunState::WaitingTool;
                    println!("\n{}", self.theme.tool_name(&format!("● {name}")));
                    self.draw_status();
                }
                AgentEvent::ToolCallFinished { is_error, result, .. } => {
                    let mark = if is_error { self.theme.error("✗") } else { self.theme.success("✓") };
                    println!("  {mark} {}", self.theme.dim(&first_line(&result, self.width)));
                    self.status.state = RunState::Streaming;
                    self.draw_status();
                }
                AgentEvent::Usage { prompt_tokens, completion_tokens } => {
                    self.status.tokens_used = (prompt_tokens + completion_tokens) as u64;
                    self.status.context_used = Some((prompt_tokens + completion_tokens) as u64);
                    self.draw_status();
                }
                AgentEvent::Finished { .. } => { println!(); break; }
                _ => {}
            }
        }
    }
}

fn first_line(s: &str, width: usize) -> String {
    truncate_to_width(s.lines().next().unwrap_or(""), width.saturating_sub(6))
}

pub fn format_tool_call(t: &Theme, name: &str, args: &str, result: Option<&str>, is_error: bool) -> String {
    let mut s = format!("  {} {}", t.tool_name(&format!("● {name}")), args);
    if let Some(r) = result {
        let mark = if is_error { t.error("✗") } else { t.success("✓") };
        s.push_str(&format!("\n    {mark} {}", r.lines().next().unwrap_or("")));
    }
    s
}
```

- [ ] **Step 4: 跑绿 + Commit** — `git commit -m "feat(cli): Renderer with theme, tool blocks, status line"`

---

### Task 7: 接线 + 端到端 + 完成标志

**Files:**
- Modify: `crates/crab-cli/src/main.rs`、`crates/crab-cli/src/interactive.rs`、`crates/crab-cli/src/app.rs`

- [ ] **Step 1: 接线**——interactive 用 `Renderer::new(status).setup()` + `run(rx)`；`--print` 保持简单流式（状态行在 print 模式可选关闭或单行）。build_agent 返回 `ResolvedProvider` 填 StatusLine 的 provider_model/context_window。

- [ ] **Step 2: e2e 测试**——`--print` 输出含 ANSI（assert_cmd 断言 `\x1b[`）或 mock 模式的渲染快照。

- [ ] **Step 3: 全套件绿 + 红线 + fmt/clippy**

```bash
export https_proxy=http://127.0.0.1:10808
cargo test --workspace 2>&1 | grep 'test result: ok'
cargo tree -p crab-core | grep -iE 'crossterm|reedline|ratatui|clap' && echo VIOLATION || echo "core clean"
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
```

- [ ] **Step 4: 手动 smoke**——`crab --provider deepseek`（交互），看状态行 + 着色 + 工具块。

- [ ] **Step 5: release 体积复查**（加了 crossterm 渲染逻辑）+ 更新 DESIGN/spec + Commit — `git commit -m "chore: TUI polish complete"`

---

## Self-Review

- **顺序陷阱**：usage 与 finish_reason 同帧 → parse_chunk 同帧产 Usage+StreamEnd，Usage 先（Task 1 Step 3 明确）。
- **一致性**：`StreamEvent::Usage{prompt_tokens,completion_tokens}` → `AgentEvent::Usage` 同名同字段；`StatusLine` 字段与 spec 的状态行对齐；`ResolvedProvider.context_window` 来自 config 计划。
- **坑**：DeepSeek 不用 `stream_options`（最后一帧自带 usage）；scroll region 在 `--print` 模式可能不需要（可关）；终端太窄时状态行截断；宽字符宽度是近似（TODO）。
- **范围**：纯 crossterm，无 ratatui/alt-screen/折叠/Markdown/多主题（YAGNI，spec 明确）。
