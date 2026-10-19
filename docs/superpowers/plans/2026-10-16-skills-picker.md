# `/skills` 交互式选择实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `/skills` 打开交互浮层列出全部 skill，Enter 选中后把 `/skill:<name> ` 填入输入框，由用户补参数回车（走既有 `/skill:` 通路）。

**Architecture:** `ModelPicker` 泛化改名为 `ListPicker`，`TuiState.picker` 改 `Option<Picker>` 两态枚举（Model/Skill），accessor 统一对外；`InputBuffer` 加 `insert_str`；skill 显示项构造为纯函数 `skill_picker`（可测）；Enter 回填行为抽纯函数 `skill_slash_line`。

**Tech:** 纯既有代码重组，无新依赖。截断用 `crate::width::truncate_to_width`（Unicode 安全）。

**Spec:** `docs/superpowers/specs/2026-10-16-skills-picker-design.md`

**前置：** skills 支持已合入 main（`5de0e23`）。工作分支从 main 切。

---

### Task 1: InputBuffer::insert_str + ListPicker 泛化 + Picker 枚举

**Files:**
- Modify: `crates/crab-cli/src/tui/input.rs`（insert_str）
- Modify: `crates/crab-cli/src/tui/state.rs`（ModelPicker→ListPicker、Picker 枚举、TuiState.picker 类型）
- Modify: `crates/crab-cli/src/tui/mod.rs`（ModelPicker::new 调用点、handle_picker_key 适配）
- Modify: `crates/crab-cli/src/tui/view.rs`（draw_picker 适配）

- [ ] **Step 1: 失败测试（input.rs）**

input.rs 测试模块追加：

```rust
    #[test]
    fn insert_str_inserts_at_cursor() {
        let mut b = InputBuffer::new();
        b.insert_str("foo");
        assert_eq!(b.text(), "foo");
        assert_eq!(b.cursor(), 3);
        b.move_home();
        b.insert_str("/skill:tdd ");
        assert_eq!(b.text(), "/skill:tdd foo");
        assert_eq!(b.cursor(), "/skill:tdd ".len());
    }
```

- [ ] **Step 2: 跑红**——`cargo test -p crab-cli insert_str 2>&1 | grep error`，Expected: 编译 FAIL（方法不存在）。

- [ ] **Step 3: 实现 insert_str（input.rs，放在 insert_char 之后）**

```rust
    /// Insert a string at cursor position, leaving the cursor after it.
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }
```

- [ ] **Step 4: state.rs 泛化**

`ModelPicker` 结构整体改名 `ListPicker`（字段、方法、`current.min(...)` 构造逻辑不变，注释去 provider 化），并在其后新增：

```rust
/// The active overlay picker: model profiles, or skills to load.
pub enum Picker {
    Model(ListPicker),
    /// Skill display items are "name — description" (description truncated);
    /// `names` holds the plain names, indexed like `list`, for the Enter action.
    Skill { list: ListPicker, names: Vec<String> },
}

impl Picker {
    pub fn items(&self) -> &[String] {
        match self {
            Picker::Model(l) => &l.items,
            Picker::Skill { list, .. } => &list.items,
        }
    }

    pub fn selected(&self) -> usize {
        match self {
            Picker::Model(l) => l.selected,
            Picker::Skill { list, .. } => list.selected,
        }
    }

    pub fn move_up(&mut self) {
        match self {
            Picker::Model(l) => l.move_up(),
            Picker::Skill { list, .. } => list.move_up(),
        }
    }

    pub fn move_down(&mut self) {
        match self {
            Picker::Model(l) => l.move_down(),
            Picker::Skill { list, .. } => list.move_down(),
        }
    }
}
```

`TuiState.picker: Option<ModelPicker>` → `Option<Picker>`。

- [ ] **Step 5: mod.rs 适配**

`dispatch_slash` 的 PickModel arm（现为 `state.picker = Some(state::ModelPicker::new(model_names.to_vec(), current))`）改为：

```rust
            state.picker = Some(state::Picker::Model(state::ListPicker::new(
                model_names.to_vec(),
                current,
            )));
```

`handle_picker_key`（按键部分）改为走 accessor：

```rust
    let Some(picker) = state.picker.as_mut() else {
        return;
    };
    match code {
        Up | Char('k') => picker.move_up(),
        Down | Char('j') => picker.move_down(),
        Enter => {
            // 分 variant 处理（Task 2 给 Skill 加分支；此处先只保留 Model）
            let is_model = matches!(picker, state::Picker::Model(_));
            if is_model {
                if let state::Picker::Model(l) = picker {
                    if let Some(name) = l.selected_name().map(str::to_string) {
                        state.picker = None;
                        let _ = cmd_tx.blocking_send(UiCommand::SetModel(name));
                    }
                }
            }
        }
        Esc | Char('q') => {
            state.picker = None;
        }
        _ => {}
    }
```

- [ ] **Step 6: view.rs 适配**

`draw_picker` 改为从 `Picker` 取数据（title 按 variant 区分）：

```rust
fn draw_picker(f: &mut Frame, state: &TuiState) {
    let picker = match &state.picker {
        Some(p) => p,
        None => return,
    };
    let title = match picker {
        state::Picker::Model(_) => " Select model (↑↓/jk, Enter, Esc) ",
        state::Picker::Skill { .. } => " Select skill (↑↓/jk, Enter, Esc) ",
    };

    // Size to content, capped.
    let height = (picker.items().len() as u16 + 2).min(15);
    let width = picker
        .items()
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(10)
        .max(20) as u16
        + 4;
    let area = centered_rect_abs(width, height, f.area());
    f.render_widget(Clear, area);

    let selected = picker.selected();
    let lines: Vec<Line> = picker
        .items()
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if i == selected {
                Line::from(vec![Span::styled(
                    format!("> {name}"),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )])
            } else {
                Line::from(format!("  {name}"))
            }
        })
        .collect();

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );
    f.render_widget(paragraph, area);
}
```

（`use super::state::{Theme, TuiState};` 行改为 `use super::state::{Picker, Theme, TuiState};` 并在函数体用 `Picker::Model`/`Picker::Skill`；match 里的 `state::Picker` 前缀相应调整为直接 `Picker`。）

state.rs 测试模块里 `model_picker_navigation` 等用到 `ModelPicker` 的测试全部改 `ListPicker`：

```rust
    #[test]
    fn model_picker_navigation() {
        let items = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut p = ListPicker::new(items, 0);
        p.move_down();
        assert_eq!(p.selected, 1);
        p.move_down();
        p.move_down(); // clamps at the end
        assert_eq!(p.selected, 2);
        p.move_up();
        assert_eq!(p.selected, 1);
        assert_eq!(p.selected_name(), Some("b"));
    }
```

（以现有测试体为准，只替换类型名。）

- [ ] **Step 7: 跑绿全套件**——`cargo test --workspace 2>&1 | grep -E 'test result|error'`。

- [ ] **Step 8: Commit**

```bash
git add crates/crab-cli/src/tui/input.rs crates/crab-cli/src/tui/state.rs crates/crab-cli/src/tui/mod.rs crates/crab-cli/src/tui/view.rs
git commit -m "refactor(tui): generalize ModelPicker into ListPicker behind a Picker enum"
```

---

### Task 2: `/skills` 解析 + skill_picker 构造 + dispatch + Enter 回填

**Files:**
- Modify: `crates/crab-cli/src/tui/state.rs`（PickSkill 变体、parse_slash、skill_picker、skill_slash_line + 测试）
- Modify: `crates/crab-cli/src/tui/mod.rs`（dispatch PickSkill arm、handle_picker_key Enter Skill 分支）

- [ ] **Step 1: 失败测试（state.rs）**

```rust
    #[test]
    fn parse_skills_opens_picker() {
        assert_eq!(parse_slash("/skills"), Some(SlashCommand::PickSkill));
    }

    #[test]
    fn skill_slash_line_has_trailing_space() {
        assert_eq!(skill_slash_line("tdd"), "/skill:tdd ");
    }

    #[test]
    fn skill_picker_empty_when_no_skills() {
        assert!(skill_picker(&[]).is_none());
    }

    #[test]
    fn skill_picker_builds_display_items_and_names() {
        let mk = |name: &str, desc: &str| crab_core::skills::Skill {
            name: name.into(),
            description: desc.into(),
            dir: std::path::PathBuf::from("/x"),
            skill_md: std::path::PathBuf::from("/x/SKILL.md"),
        };
        let long = "y".repeat(200);
        let skills = vec![mk("tdd", "Test-driven development"), mk("rev", &long)];
        let picker = skill_picker(&skills).unwrap();
        let state::Picker::Skill { list, names } = picker else {
            panic!("must be a Skill picker");
        };
        assert_eq!(names, vec!["tdd".to_string(), "rev".to_string()]);
        assert_eq!(list.items[0], "tdd — Test-driven development");
        assert!(
            crate::width::display_width(&list.items[1]) <= 65,
            "long description must be truncated: {}",
            list.items[1]
        );
        assert!(list.items[1].starts_with("rev — yyy"));
    }
```

- [ ] **Step 2: 跑红**——`cargo test -p crab-cli skill_picker 2>&1 | grep error`，Expected: 编译 FAIL（PickSkill / skill_picker / skill_slash_line 不存在）。

- [ ] **Step 3: 实现（state.rs）**

`SlashCommand` 在 `Skill(String, Option<String>)` 之后加：

```rust
    /// Open the interactive skill picker (`/skills`).
    PickSkill,
```

`parse_slash` match 加（`"models"` 行之后）：

```rust
        "skills" => SlashCommand::PickSkill,
```

helper（放在 `skill_message` 之后）：

```rust
/// The input-box line an Enter'd skill picker selection produces: `/skill:<name> `
/// with a trailing space so the user types straight into the args.
pub fn skill_slash_line(name: &str) -> String {
    format!("/skill:{name} ")
}

/// Build the skill picker overlay. None when no skills are discovered.
/// Display items are "name — description" (description truncated to 60 cells);
/// `names` parallels the items for the Enter action.
pub fn skill_picker(skills: &[crab_core::skills::Skill]) -> Option<Picker> {
    if skills.is_empty() {
        return None;
    }
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    let items: Vec<String> = skills
        .iter()
        .map(|s| format!("{} — {}", s.name, crate::width::truncate_to_width(&s.description, 60)))
        .collect();
    Some(Picker::Skill {
        list: ListPicker::new(items, 0),
        names,
    })
}
```

- [ ] **Step 4: 跑绿 state 部分**——`cargo test -p crab-cli tui::state 2>&1 | grep 'test result'`。

- [ ] **Step 5: 实现（mod.rs）**

`dispatch_slash` 加 arm（`SlashCommand::Skill` arm 之后）：

```rust
        state::SlashCommand::PickSkill => {
            match state::skill_picker(skills) {
                Some(picker) => state.picker = Some(picker),
                None => state.notice(
                    "no skills found (looked in ~/.agents/skills and project .agents/skills)",
                ),
            }
        }
```

`handle_picker_key` 的 Enter 分支改为分 variant：

```rust
        Enter => {
            let fill = match picker {
                state::Picker::Model(_) => None,
                state::Picker::Skill { names, .. } => names
                    .get(picker.selected())
                    .map(|n| state::skill_slash_line(n)),
            };
            if let Some(line) = fill {
                state.picker = None;
                state.input.clear();
                state.input.insert_str(&line);
            } else if let state::Picker::Model(l) = picker {
                if let Some(name) = l.selected_name().map(str::to_string) {
                    state.picker = None;
                    let _ = cmd_tx.blocking_send(UiCommand::SetModel(name));
                }
            }
        }
```

（借检查注意：`fill` 的 match 借用了 picker，`state.input` 操作前先结束借用——`fill` 是 owned String，match 表达式结束后借用即释放，安全。）

- [ ] **Step 6: 跑绿全套件 + clippy**

```bash
cargo test --workspace 2>&1 | grep -E 'test result|FAILED'
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
```

- [ ] **Step 7: Commit**

```bash
git add crates/crab-cli/src/tui/state.rs crates/crab-cli/src/tui/mod.rs
git commit -m "feat(tui): /skills picker — choose a skill, fills /skill:<name> into the input"
```

---

### Task 3: 收尾——全套件、文档、smoke

**Files:**
- Modify: `crates/crab-cli/src/tui/mod.rs`（Unknown 提示补 /skills）

- [ ] **Step 1: Unknown 提示更新**

`SlashCommand::Unknown` arm的提示改为：

```rust
            state.notice(format!(
                "unknown command /{name}  (try /quit, /yolo, /model <name>, /skills, /skill:<name>)"
            ));
```

- [ ] **Step 2: 全套件 + clippy + 红线**

```bash
export https_proxy=http://127.0.0.1:10808
cargo test --workspace 2>&1 | grep -cE 'test result: ok'   # Expected: 6
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
cargo tree -p crab-core | grep -iE 'crossterm|reedline|ratatui|clap' && echo VIOLATION || echo "core clean"
```

- [ ] **Step 3: 手动 smoke（人工）**——TUI 里 `/skills`：有空/无 skill 两种场景、Esc 取消、Enter 选中后输入框出现 `/skill:tdd `、补参数回车注入。记录结果。

- [ ] **Step 4: Commit**

```bash
git add crates/crab-cli/src/tui/mod.rs
git commit -m "docs(tui): mention /skills in the unknown-command hint"
```

---

## Self-Review

- **Spec 覆盖**：/skills 解析（Task 2）；空 skills notice（Task 2）；picker 构造/截断（Task 2 + 测试）；Enter 回填（Task 2）；Esc 不动输入框（Task 1 保留既有行为）；绘制标题区分（Task 1）；输入框覆盖语义（skill_slash_line + clear，Task 2）。✅
- **占位符**：无 TBD；全部代码步骤含完整代码。✅
- **类型一致**：`Picker::{Model, Skill{list, names}}`、`skill_picker(&[Skill]) -> Option<Picker>`、`skill_slash_line(&str) -> String`、`insert_str(&mut self, &str)` 前后一致；Task 1 定义的 accessor（items/selected/move_up/move_down）在 view/mod 使用一致。✅
- **坑**：`handle_picker_key` 里 `fill` 的借用需在 `state.input` 可变借用前结束（代码已按 owned String 处理）；view.rs 的 `use` 需补 `Picker`。
