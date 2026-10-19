# 设计：`/skills` 交互式列表选择

> 前置：skills 支持已合入 main（`5de0e23`）。本设计在其上加 `/skills` 命令。
> 状态：已获批准（方案 1：泛化 ListPicker；选中后回填 `/skill:<name> ` 到输入框）。

## Goal

`/skills` 打开交互式浮层：列出全部已发现 skill（name + 截断的 description），
↑/↓（或 j/k）移动，Enter 选中后把 `/skill:<name> ` 填入输入框，由用户补参数、
回车发送（走既有 `parse_slash("/skill:...")` 通路）。Esc/q 关闭，输入框不动。

## 非目标（YAGNI）

- 搜索/过滤（skill 数量级：几十，滚动足够）
- 多选
- 选中后直接注入（那是 `/skill:name` 已有的行为）

## 架构

### state.rs

`ModelPicker` 泛化为 `ListPicker`（字段、方法不变，只改名；注释去"provider"化）：

```rust
pub struct ListPicker {
    pub items: Vec<String>,   // 显示文本
    pub selected: usize,
}
```

`TuiState.picker` 改为两态枚举（overlay 同时只开一个）：

```rust
pub enum Picker {
    Model(ListPicker),
    /// Skill 显示项是 "name — description"（description 截断），names 平行存纯
    /// 名字，Enter 时按 selected 下标回填 `/skill:<name> `。
    Skill { list: ListPicker, names: Vec<String> },
}
```

- `SlashCommand` 新增 `PickSkill`（`/skills` 解析）。
- `InputBuffer` 新增 `insert_str(&mut self, s: &str)`（在光标处插入；现有 API
  只有逐字符 insert_char，没有批量插入）。

### mod.rs（按键路由）

`handle_picker_key` 按 variant 分发：

- 移动（Up/Down/j/k）：两种 variant 共用，操作内层 ListPicker
- Enter：
  - `Picker::Model` → 关 picker，`UiCommand::SetModel(name)`（现状不变）
  - `Picker::Skill` → 关 picker，`input.clear()` + `insert_str("/skill:{name} ")`
    （name 来自平行 names 下标；不加 args，用户自己补）
- Esc/q：关 picker，输入框不动（现状不变）

### view.rs（绘制）

`draw_picker` 按 variant 取显示项和选中下标；宽度计算用显示项长度；标题区分
"Pick a model" / "Pick a skill"。

### dispatch_slash（mod.rs）

`SlashCommand::PickSkill`：

- skills 为空 → notice `"no skills found (looked in ~/.agents/skills and project .agents/skills)"`
- 否则构造 `Picker::Skill`：显示项 `format!("{} — {}", name, truncate(desc, 60))`
  （截断用 width.rs 的 `truncate_to_width`，宽字符安全），names 为纯 name 列表，
  初始选中 0。

## 数据流

```
/skills 回车 → parse_slash → SlashCommand::PickSkill
  → dispatch_slash：空→notice；非空→state.picker = Some(Picker::Skill{...})
  → handle_picker_key（overlay 优先路由）：
       Enter → state.picker = None; input.clear(); input.insert_str("/skill:tdd ")
  → 用户补参数、回车 → handle_key 常规 Submit 路径
       → parse_slash("/skill:tdd fix parser") → Skill("tdd", Some("fix parser"))
       → 既有注入逻辑（合入 main 的 Task 4）
```

## 错误处理

| 场景 | 行为 |
|---|---|
| 无 skills | notice 提示查找位置，不开浮层 |
| Enter 时 names/list 长度不一致 | 不可能（同一构造函数产生）；names.get(selected) 用 Option，取不到则静默关 picker |
| 输入框已有内容 | 直接覆盖（clear 后填）。语义：picker 选中 = 开始一条新命令 |

## 测试

- state.rs：`parse_slash("/skills")` → `PickSkill`；`InputBuffer::insert_str` 插入
  位置与光标行为；ListPicker 既有测试改名后照跑
- mod.rs（helper 纯逻辑）：skill 显示项构造（含长 description 截断）；Enter 回填
  行格式（`/skill:{name} ` 尾部空格）
- dispatch 行为若有可注入 seam 则测空 skills → notice；否则靠手动 smoke
- 全套件 + clippy -D warnings

## Self-Review 备注

- 显示项里 ` — ` 分隔后 description 可能含换行？frontmatter 单行解析保证
  description 无换行（parse_frontmatter 逐行），安全。
- `Picker::Skill` 用平行 `names` 而非从显示项解析名字：避免"名字含 — "的解析
  歧义，多存一份 Vec<String> 代价可忽略。
