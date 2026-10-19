# 设计：status 移至输入框下方（pi 式两行）

> 参照：pi coding agent 截图（`~/Desktop/Screenshot 2026-10-19 at 16.28.38.png`）。
> 状态：已获批准（分支缓存 2s 刷新 / token 会话累计）。

## Goal

把状态区从输入框**上方**移到**下方**，改成 pi 风格的两行：

```
<transcript>
┌─ 输入框（3 行，不动）────────┐
 ~/Experiments/crab-agent (main)          ← row 1：cwd + git branch
 ↑12.3k ↓45.6k CH20.5% idle  │ (deepseek) deepseek-chat   ← row 2：左状态 / 右模型
```

## 布局（view.rs）

`draw` 的 constraints 从 `[Min, Length(1), Length(3)]`（transcript/status/input）
改为 `[Min, Length(3), Length(1), Length(1)]`（transcript/input/cwd row/status row）。
`draw_transcript` 的行数计算 `h - 4` → `h - 5`。输入框（边框、spinner、
running title）完全不动。

## status.rs 重构

### 字段

```rust
pub struct StatusLine {
    provider_model: String,
    state: RunState,
    tokens_in: u64,    // 会话累计 input tokens（↓）
    tokens_out: u64,   // 会话累计 output tokens（↑）
    context_used: Option<u64>,   // 最近一次请求的 prompt_tokens
    context_window: Option<u64>,
    cwd: PathBuf,
    branch: Option<String>,      // git branch 缓存（异步任务刷新）
}
```

### 方法

- `add_usage(prompt_tokens: u32, completion_tokens: u32)`：累加到
  tokens_in/tokens_out；`context_used = Some(prompt_tokens)`（最近请求的
  prompt 大小才是"当前上下文真实占用"，pi 的 `20.5%/1.0M` 语义）
- `set_branch(Option<String>)`
- `format_cwd_row(width) -> String`：
  ` ~/Experiments/crab-agent (main) `；`branch` 为 None 时无 ` (main)` 段；
  超出宽度截断
- `format_status_row(width) -> String`：
  - 左段：`↑{kfmt(out)} ↓{kfmt(in)}` + `CH{pct}%`（无 window 时 `CH` 段省略，
    有 window 无 used 时也省略）+ state 词（idle/streaming/tool/confirm）
  - 右段：`(provider) model`——`provider_label` 形如 `deepseek/deepseek-chat`，
    直接作为右段（它就是 "(provider) model" 的紧凑形式）
  - 右对齐：`visible_width` 计算左右段显示宽度，中间空格填充到 `width`，
    超宽先截右段再截左段（保留箭头数字，丢 state 词和模型）
- `set_tokens_used` 删除（被 `add_usage` 取代）

### git_branch

```rust
/// `git branch --show-current` 的输出，非 git 目录 / detached / 失败 → None。
fn git_branch(cwd: &Path) -> Option<String>
```

### UiEvent / 刷新任务

- `UiEvent::BranchChanged(Option<String>)`（新变体）
- `tui::run()` spawn 异步任务：每 2s 调 `git_branch(cwd)`，与上次不同才发事件；
  用 `tokio::time::interval`
- `TuiState::apply_ui_event`（或现有 match）处理： `status.set_branch(...)`

### state.rs 接线

`StreamEvent::Usage { prompt_tokens, completion_tokens }` 分支改为
`status.add_usage(prompt_tokens, completion_tokens)`。

## 跳过的 pi 项（无对应数据，YAGNI）

`$` 成本、`(spam)` steering、`R24M`、`(auto)` compaction、effort（`· high`）。

## 错误处理

| 场景 | 行为 |
|---|---|
| 非 git 目录 | cwd 行无 branch 段 |
| detached HEAD | `--show-current` 输出空 → None |
| git 命令不存在/超时 | None（静默） |
| 无 context window 配置 | `CH` 段省略 |
| 极窄终端 | 先丢右段模型，再丢 state 词；数字箭头尽量保留 |

## 测试

- `add_usage`：累计正确；context_used 取最近一次 prompt
- `format_cwd_row`：有/无 branch；`~` 缩写；截断
- `format_status_row`：左右拼接宽度精确（含 CJK 宽字符用例）；CH 段有无
  window 两个分支；超宽截断顺序
- `git_branch`：tempdir `git init` → `Some`（无 commit 也有默认 branch）；
  非 git tempdir → `None`
- 既有 status 测试改造（set_tokens_used 调用点删除）

## Self-Review 备注

- `provider_label` 现状是 `format!("{}/{}", provider_name, model)`（`/model`
  命令处构造），天然等于 pi 右侧 `(provider) model` 的紧凑式，直接复用。
- branch 刷新任务在 `run()` 内 spawn、随 runtime 拆塔终止，无需显式 cancel。
- 2s 刷新是经验值：git 本地调用毫秒级，2s 足够及时且开销可忽略。
