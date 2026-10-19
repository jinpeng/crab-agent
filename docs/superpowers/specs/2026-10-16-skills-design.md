# 设计：skills 支持

> 依据：DESIGN.md 后 v0 方向（"skills 思路"）；参考 pi 的 Agent Skills 实现。
> 状态：已获批准（范围 C + Agent Skills 目录 + pi 式 read 加载）。

## Goal

crab 支持 [Agent Skills spec](https://agentskills.io/specification) 格式的 skill：

1. 启动时扫描 skill 目录，把 name + description + 路径注入系统提示
2. 模型判断任务匹配时，用现有 `read` 工具自助加载 `SKILL.md` 并遵循其指令（零新工具）
3. TUI 支持 `/skill:name [args]`：强制加载，args 作为用户请求追加

## 范围

**做**：目录发现、frontmatter 解析、系统提示注入、`/skill:name args`。

**不做（YAGNI）**：`disable-model-invocation`、`/skills` 列表命令、`--print` 模式的
slash（`--print` 靠系统提示自助加载）、YAML 依赖。

## 模块划分（红线：core 无终端 crate）

新增 `crates/crab-core/src/skills.rs`：

```rust
pub struct Skill {
    pub name: String,        // frontmatter 的 name
    pub description: String, // frontmatter 的 description（路由用）
    pub dir: PathBuf,        // skill 目录（bundled 文件相对它解析）
    pub skill_md: PathBuf,   // dir/SKILL.md
}

/// home = ~/.agents/skills；cwd 向上逐层找 .agents/skills，
/// 停在 git repo 根（含）；无 repo 则走到文件系统根。同名先发现者优先 + 警告。
pub fn discover(cwd: &Path, home: &Path) -> Vec<Skill>;
```

### 发现规则

- 用户级：`$HOME/.agents/skills/*/SKILL.md`
- 项目级：从 `cwd` 逐层向上找 `.agents/skills/*/SKILL.md`，**停在 git repo 根**
  （含根本身）；无 repo 则走到文件系统根
- 目录去重：canonicalize 后相同目录只扫一次
- 同名冲突：先发现者保留，其余跳过 + 警告（与 pi 一致）
- 发现顺序（决定同名冲突谁赢）：项目级由内向外（近者优先），用户级最后
- 冲突解决在发现顺序上进行；返回前 Vec 按 name 排序，保证系统提示输出确定性

### frontmatter 解析

手写，不引 YAML 依赖。取 `SKILL.md` 开头 `---` ... `---` 之间逐行扫 `key: value`，
只认两个字段：

- `name` — 缺省用目录名（spec 允许实现宽松；pi 不强制，其他实现强制——取宽松 +
  无警告）
- `description` — **必填**，缺失或为空 → 整个 skill 跳过 + 警告（"declared skills
  without descriptions are not loaded"）

malformed 判定：无 frontmatter 块、非 UTF-8、description 缺失。一律跳过 + eprintln
警告（沿用 MCP 启动容错的风格），绝不致命。

## 系统提示注入

`build_system_prompt`（`crab-cli/src/app.rs`）末尾追加 section；**无 skills 时整个
section 省略**（省 token）：

```
# Skills
Available skills. When a task matches a skill's description, read its SKILL.md
at the given path with the read tool and follow it. Bundled files are relative
to the skill's directory.
- tdd — Test-driven development. Use when ... (at /Users/.../tdd/SKILL.md)
```

模型加载路径：系统提示看到 description 匹配 → `read` 对应 SKILL.md → 遵循指令 →
skill 内引用的 `references/xxx.md`、`scripts/foo.sh` 是相对 skill 目录的路径，模型
用 read/bash 自然解析（系统提示里告知 skill 目录位置）。

## `/skill:name args`

- `state.rs`：`SlashCommand::Skill(String, Option<String>)`；`parse_slash` 识别
  `skill:` 前缀：`/skill:tdd refactor parser` → name=`tdd`, args=`Some("refactor parser")`
- `mod.rs` 处理器：在 `build_agent` 返回的已发现 skills 里按 name 查找：
  - **命中**：注入一条用户消息（走正常用户消息通路，进 session 历史）：

    ```
    <Skill instructions: tdd (/path/to/tdd/SKILL.md)>
    {SKILL.md 全文}
    </Skill instructions>

    {args 作为用户请求；无 args 则省略尾段}
    ```
  - **未命中**：系统提示气泡 `unknown skill 'xxx'. Available: a, b, c`
- unknown-command 提示更新：`try /quit, /yolo, /model <name>, /skill:<name>`

### build_agent 返回结构

现 6-tuple 已臃肿，加 skills 顺势重构为结构体：

```rust
pub struct Startup {
    pub agent: Agent,
    pub system_prompt: String,
    pub resolved: ResolvedProvider,
    pub mcp_clients: Vec<Arc<McpClient>>,
    pub gate: PermissionGate,
    pub config: Config,
    pub skills: Vec<Skill>,
}
```

## 错误处理

| 场景 | 行为 |
|---|---|
| skill 目录不存在 | 空列表，正常启动 |
| SKILL.md malformed | 跳过 + 启动警告，不致命 |
| 同名冲突 | 先发现者赢 + 警告 |
| `/skill:unknown` | 气泡警告 + 列可用名，不中断会话 |
| 会话中 skill 文件被删 | read 工具自然报错，不特判 |

## 测试

**core（skills.rs）**：
- `discover` 用 tempdir 伪造 home + 项目目录树
- 覆盖：walk-up 多层、repo 根停止、无 repo 走到根、目录去重、同名冲突优先序、
  malformed 跳过（无 frontmatter / 无 description / 非 UTF-8）、name 缺省取目录名
- frontmatter 解析边界：CRLF、多余空行、值含冒号

**cli（app.rs / state.rs / mod.rs）**：
- 系统提示：有 skills 含 section + 每个 name/description/路径；无 skills 不含 "Skills"
- `parse_slash("/skill:tdd")` / `parse_slash("/skill:tdd fix parser")` / 无 args
- 注入消息格式：含 SKILL.md 全文、skill 路径、args 追加
- 未命中：notice 气泡含可用名
- 红线：`cargo tree -p crab-core` 无终端 crate

## Self-Review 备注

- frontmatter 解析刻意只认 `name`/`description` 两个字段，其余字段（license、
  metadata、allowed-tools 等）忽略不警告——YAGNI，pi 同款宽松。
- 项目级"停在 repo 根"的含义：repo 根的 `.agents/skills` 算，其祖先不算。无 repo
  时不设限（与 pi 文档语义一致）。
