# Status 移至输入框下方实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 状态区从输入框上方移到下方，改 pi 式两行（cwd+branch / 左累计 tokens+CH%+state、右 provider/model），token 改为会话累计，branch 2s 异步刷新。

**Architecture:** `StatusLine` 重构字段（tokens_in/out 累计、branch）+ 两个 `format_*_row`；`view.rs` 布局从 `[Min,1,3]` 改 `[Min,3,1,1]`；`tui/mod.rs` 加 `UiEvent::BranchChanged` + 2s 刷新任务；`state.rs` 的 Usage 事件改调 `add_usage`。

**Tech:** 纯既有代码，无新依赖。宽度计算用 `crate::width::{visible_width, truncate_to_width}`（Unicode 安全）。

**Spec:** `docs/superpowers/specs/2026-10-19-status-below-input-design.md`

**前置：** 无。工作分支从 main 切。

---

### Task 1: status.rs 重构（字段 + add_usage + 两行 format + git_branch）+ state.rs 接线

**Files:**
- Modify: `crates/crab-cli/src/status.rs`
- Modify: `crates/crab-cli/src/tui/state.rs`（Usage 事件分支）

- [ ] **Step 1: 失败测试**——重写 status.rs 测试模块（旧 `format`/`set_tokens_used`/`ctx_segment` 测试全换）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::visible_width;

    fn home() -> PathBuf {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/home/user"))
    }

    fn full() -> StatusLine {
        StatusLine {
            provider_model: "deepseek/deepseek-chat".to_string(),
            state: RunState::Streaming,
            tokens_in: 45_600,
            tokens_out: 12_300,
            context_used: Some(8500),
            context_window: Some(128_000),
            cwd: home().join("projects/crab"),
            branch: Some("main".to_string()),
        }
    }

    #[test]
    fn kfmt_switches_to_k_at_a_thousand() {
        assert_eq!(kfmt(0), "0");
        assert_eq!(kfmt(500), "500");
        assert_eq!(kfmt(1200), "1.2k");
        assert_eq!(kfmt(128000), "128.0k");
    }

    #[test]
    fn add_usage_accumulates_and_context_takes_latest_prompt() {
        let mut s = StatusLine::new("m".into(), PathBuf::from("/"));
        s.add_usage(100, 20);
        s.add_usage(300, 40);
        assert_eq!(s.tokens_in, 400);
        assert_eq!(s.tokens_out, 60);
        assert_eq!(s.context_used, Some(300), "context tracks the latest prompt size");
    }

    #[test]
    fn cwd_row_shows_branch_in_parens() {
        let row = full().format_cwd_row(200);
        assert!(row.contains("~/projects/crab (main)"), "got {row:?}");
        assert!(row.starts_with(' ') && row.ends_with(' '));
    }

    #[test]
    fn cwd_row_without_branch_has_no_parens() {
        let mut s = full();
        s.branch = None;
        let row = s.format_cwd_row(200);
        assert!(row.contains("~/projects/crab"), "got {row:?}");
        assert!(!row.contains('('), "got {row:?}");
    }

    #[test]
    fn status_row_is_exactly_width_with_model_right_aligned() {
        let row = full().format_status_row(80);
        assert_eq!(visible_width(&row), 80, "got {row:?}");
        assert!(row.contains("↑12.3k ↓45.6k"), "got {row:?}");
        assert!(row.contains("CH7%"), "got {row:?}"); // 8500/128000 ≈ 6.6% → 7%
        assert!(row.contains("streaming"), "got {row:?}");
        assert!(row.trim_end().ends_with("deepseek/deepseek-chat"), "got {row:?}");
    }

    #[test]
    fn status_row_drops_model_when_narrow() {
        let row = full().format_status_row(24);
        assert!(visible_width(&row) <= 24);
        assert!(row.contains("↑12.3k"), "numbers survive longest: {row:?}");
        assert!(!row.contains("deepseek"), "got {row:?}");
    }

    #[test]
    fn status_row_omits_ch_without_window() {
        let mut s = full();
        s.context_window = None;
        let row = s.format_status_row(80);
        assert!(!row.contains("CH"), "got {row:?}");
    }

    #[test]
    fn git_branch_in_repo_and_outside() {
        if std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("git unavailable: skipping");
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(git_branch(repo.path()).is_some(), "a fresh repo has an unborn branch");

        let plain = tempfile::tempdir().unwrap();
        assert!(git_branch(plain.path()).is_none());
    }
}
```

注意：旧测试 `full_status_line_contains_every_segment`、`context_without_window_shows_absolute_count_and_no_percent` 等引用 `format`/`ctx_segment` 的全部删除替换。

- [ ] **Step 2: 跑红**——`cargo test -p crab-cli status 2>&1 | grep error`，Expected: 编译 FAIL（tokens_in/set_branch/format_cwd_row 等不存在）。

- [ ] **Step 3: 实现 status.rs**

`StatusLine` 字段与构造：

```rust
pub struct StatusLine {
    provider_model: String,
    state: RunState,
    tokens_in: u64,
    tokens_out: u64,
    context_used: Option<u64>,
    context_window: Option<u64>,
    cwd: PathBuf,
    branch: Option<String>,
}
```

`new()` 里 `tokens_in: 0, tokens_out: 0, branch: None`，其余不变。替换 `set_tokens_used` 为：

```rust
    /// Accumulate a response's token usage and refresh the context estimate.
    pub fn add_usage(&mut self, prompt_tokens: u32, completion_tokens: u32) {
        self.tokens_in += prompt_tokens as u64;
        self.tokens_out += completion_tokens as u64;
        // The last request's prompt size is the closest proxy for how much of
        // the context window is occupied right now.
        self.context_used = Some(prompt_tokens as u64);
    }

    /// Replace the cached git branch (refreshed by the TUI's async task).
    pub fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }
```

`ctx_segment` 替换为 `ch_segment`（compact pi-style）：

```rust
    /// `CH<pct>%`, or None when usage or the window is unknown.
    fn ch_segment(&self) -> Option<String> {
        let used = self.context_used?;
        let window = self.context_window.filter(|w| *w > 0)?;
        let pct = (used.saturating_mul(100) + window / 2) / window;
        Some(format!("CH{pct}%"))
    }
```

`format` 替换为两个 row 方法：

```rust
    /// Row 1 (below the input box): ` ~/path (branch) `.
    pub fn format_cwd_row(&self, width: usize) -> String {
        let mut label = self.cwd_label();
        if let Some(b) = &self.branch {
            label.push_str(&format!(" ({b})"));
        }
        truncate_to_width(&format!(" {label} "), width)
    }

    /// Row 2: left `↑out ↓in CH<pct>% <state>` and right `(provider) model`,
    /// right-aligned to `width`. When too narrow the model label is dropped
    /// and only the left part is kept (clipped).
    pub fn format_status_row(&self, width: usize) -> String {
        let mut left = format!("↑{} ↓{}", kfmt(self.tokens_out), kfmt(self.tokens_in));
        if let Some(ch) = self.ch_segment() {
            left.push(' ');
            left.push_str(&ch);
        }
        left.push(' ');
        left.push_str(self.state.word());

        let right = &self.provider_model;
        let total = visible_width(&left) + visible_width(right);
        if total + 3 <= width {
            // 2 edge spaces + at least 1 space between the segments.
            let middle = width - total - 2;
            return format!(" {left}{}{right} ", " ".repeat(middle));
        }
        truncate_to_width(&format!(" {left} "), width)
    }
```

文件底部加（`kfmt` 之后）：

```rust
/// `git branch --show-current` in `cwd`; None outside a git work tree, on a
/// detached HEAD, or when git is unavailable.
pub fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}
```

`use crate::width::truncate_to_width;` 改为 `use crate::width::{truncate_to_width, visible_width};`。

- [ ] **Step 4: 接线 state.rs**——Usage 事件分支：

```rust
            Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.status.add_usage(prompt_tokens, completion_tokens);
            }
```

（原 `set_tokens_used` + `set_context` 两行删除。）

- [ ] **Step 5: 跑绿**——`cargo test -p crab-cli status 2>&1 | grep -E 'test result|FAILED'`。Expected: 全过。

- [ ] **Step 6: Commit**

```bash
git add crates/crab-cli/src/status.rs crates/crab-cli/src/tui/state.rs
git commit -m "feat(tui): pi-style status rows — cumulative ↑↓ tokens, CH%, cwd+branch"
```

---

### Task 2: view.rs 布局——status 两行移到输入框下方

**Files:**
- Modify: `crates/crab-cli/src/tui/view.rs`

- [ ] **Step 1: 改 `draw`**——constraints 和绘制顺序：

```rust
pub fn draw(f: &mut Frame, state: &mut TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // Transcript
            Constraint::Length(3), // Input box
            Constraint::Length(1), // cwd row
            Constraint::Length(1), // status row
        ])
        .split(f.area());

    draw_transcript(f, state, chunks[0]);
    draw_input(f, state, chunks[1]);
    draw_status_rows(f, state, &chunks[2..]);

    // Draw confirm modal on top if active
    if state.confirm.is_some() {
        draw_confirm_modal(f, state);
    }

    // Draw model picker on top if open
    if state.picker.is_some() {
        draw_picker(f, state);
    }
}
```

- [ ] **Step 2: `draw_status` 替换为 `draw_status_rows`**

```rust
/// Draw the two status rows below the input box (pi-style): the cwd+branch
/// row, then the usage row (left) with the model label right-aligned.
fn draw_status_rows(f: &mut Frame, state: &TuiState, areas: &[Rect]) {
    let cwd = Line::styled(
        state.status.format_cwd_row(areas[0].width as usize),
        Theme::status(),
    );
    f.render_widget(Paragraph::new(cwd), areas[0]);
    let row = Line::styled(
        state.status.format_status_row(areas[1].width as usize),
        Theme::status(),
    );
    f.render_widget(Paragraph::new(row), areas[1]);
}
```

- [ ] **Step 3: transcript 行数**——`draw_transcript` 里 `let rows = h - 4; // status line (1) + input box (3)` 改为 `let rows = h - 5; // input box (3) + status rows (2)`。

- [ ] **Step 4: 跑绿**——`cargo test --workspace 2>&1 | grep -E 'test result|FAILED'`。

- [ ] **Step 5: Commit**

```bash
git add crates/crab-cli/src/tui/view.rs
git commit -m "feat(tui): move the status area below the input box"
```

---

### Task 3: UiEvent::BranchChanged + 2s git branch 刷新任务

**Files:**
- Modify: `crates/crab-cli/src/tui/mod.rs`

- [ ] **Step 1: UiEvent 加变体**（`enum UiEvent` 内，`ModelChanged` 之后）：

```rust
    /// Updated git branch for the status line (from the 2s refresher task).
    BranchChanged(Option<String>),
```

- [ ] **Step 2: run_tui_loop 的事件 match 加 arm**（`UiEvent::ModelChanged` arm 之后）：

```rust
                UiEvent::BranchChanged(branch) => state.status.set_branch(branch),
```

- [ ] **Step 3: run() 里 spawn 刷新任务**——在 `let agent_rx = agent.subscribe();` 之后（或 channel 创建后的任何位置）：

```rust
    // Refresh the git branch for the status line every 2s (only on change),
    // so the cwd row follows branch switches without blocking the draw thread.
    {
        let ui_tx = ui_tx.clone();
        let cwd = std::env::current_dir().unwrap_or_default();
        tokio::spawn(async move {
            let mut last: Option<String> = None;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.tick().await; // interval's first tick fires immediately; skip it
            loop {
                tick.tick().await;
                let branch = crate::status::git_branch(&cwd);
                if branch != last {
                    last = branch.clone();
                    if ui_tx.send(UiEvent::BranchChanged(branch)).is_err() {
                        break; // TUI gone
                    }
                }
            }
        });
    }
```

- [ ] **Step 4: 跑绿全套件 + clippy**

```bash
cargo test --workspace 2>&1 | grep -E 'test result|FAILED'
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
```

- [ ] **Step 5: Commit**

```bash
git add crates/crab-cli/src/tui/mod.rs
git commit -m "feat(tui): refresh the git branch for the status row every 2s"
```

---

### Task 4: 收尾验证

- [ ] **Step 1: 全套件 + clippy + 红线**

```bash
export https_proxy=http://127.0.0.1:10808
cargo test --workspace 2>&1 | grep -cE 'test result: ok'   # Expected: 6
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
cargo tree -p crab-core | grep -iE 'crossterm|reedline|ratatui|clap' && echo VIOLATION || echo "core clean"
```

- [ ] **Step 2: pty 视觉 smoke（人工或脚本）**——跑 TUI 看：输入框下方两行、cwd 带 `(branch)`、token 累计增长、右侧模型名。可复用之前的 python pty 脚本截图/目检。

---

## Self-Review

- **Spec 覆盖**：布局（Task 2）、两行格式（Task 1）、累计 token + context 最近值（Task 1 add_usage）、branch 检测与 2s 刷新（Task 1 git_branch + Task 3）、CH 段省略规则（Task 1 ch_segment + 测试）、窄终端截断（测试）、跳过的 pi 项（未进计划）。✅
- **占位符**：无 TBD；所有代码步骤含完整代码。✅
- **类型一致**：`add_usage(u32, u32)`（state.rs 的 Usage 字段是 u32）、`set_branch(Option<String>)`、`format_cwd_row/format_status_row(width) -> String`、`git_branch(&Path) -> Option<String>` 前后一致；`UiEvent::BranchChanged(Option<String>)` 发送/接收一致。✅
- **坑**：`format_status_row` 的宽度算术——`middle = width - total - 2`，条件 `total + 3 <= width` 保证 middle ≥ 1；测试用 `visible_width == 80` 精确锁定。`state.status` 在 run_tui_loop 的 drain match 里可变借用，新 arm 与既有一致。
