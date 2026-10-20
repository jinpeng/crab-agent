# 设计：crab-agent 可观测性方案（Log / Trace / Metrics；Evaluation 二期）

> 参考：DSH（`deepseek-ai/deepseek-harness`）可观测性设计（见
> `docs/notes/2026-10-19-deepseek-harness-observability.md`）+
> Confident AI《AI Agent Observability: Everything You Need to Know in 2026》
> （confident-ai.com/blog/ai-agent-observability）。
> 状态：已获批准（纯本地零网络；一期 Log+Trace+Metrics，二期 Evaluation）。

## 原则（借 DSH 的三段纪律，无插件架构）

1. **日志即真相**：session JSONL 是唯一事实源。观测事件与模型可见事件写同一
   文件、严格分离（`kind:"obs.*"`）；`--continue` 的上下文重建只读 message 类，
   obs 事件**永不进入模型上下文**。
2. **测量即回放**：在线只记原始事实（usage 四分、时间戳、duration）；精确统计
   由离线 replay 推导，在线路径不做计算。
3. **观测不改变系统**：零新增网络依赖、零模型调用、不影响主循环时延
   （obs 写入是顺序 append，同文件已开句柄）。

对应博客的概念映射：session = thread，turn = trace，llm_call/tool_call = span
（类型化 span：LLM span / Tool span）；permission gate 即最小 tool guard。

## Log——session JSONL v2

### 文件头

首行：`{"kind":"meta","format_version":2,"created":"<rfc3339>"}`。
代码常量 `SESSION_FORMAT_VERSION: u32 = 2` 是唯一事实源。
加载规则：无 meta 行 → 视为 v1（现有文件全部按此读）；writer 恒写当前版本。
v2 起每个新 session 首行写 meta。

### Entry 扩展

```rust
pub enum SessionEntry {
    Message { id, parent_id, message },          // 现有，不动
    Meta { format_version: u32, created: String },
    Obs(ObsEvent),
}

pub struct ObsEvent {
    pub turn_id: u64,
    pub ts: u64,            // unix epoch millis
    pub body: ObsBody,
}

pub enum ObsBody {
    TurnStart,
    TurnEnd { stop_reason: StopReason, interrupted: bool, llm_calls: u32, tool_calls: u32 },
    LlmCall {
        provider: String, model: String,
        duration_ms: u64,
        usage: Option<LlmUsage>,   // provider 上报才有；见下
        outcome: ObsOutcome,       // Ok | Error(String 截断128) | Aborted
    },
    ToolCall {
        tool: String,
        call_seq: u32,             // turn 内第几次工具调用
        args_digest: String,       // blake3 hex of canonical args json，8 字符截断
        result_digest: String,     // 同上，结果文本
        bytes_in: u64, bytes_out: u64,
        duration_ms: u64,
        outcome: ObsOutcome,
    },
}

pub struct LlmUsage { pub prompt: u32, pub completion: u32, pub cache_read: u32, pub cache_write: u32 }
```

serde tag 规则：entry 级 `kind`（`message|meta|obs`），obs body 内再 `obs`
子 tag（`turn_start|turn_end|llm_call|tool_call`）。

### 不重复存全量

tool args/result 已在 message 的 ToolUse/ToolResult 块里；obs 只存测量元数据 +
digest。trace 渲染与 metrics 回放时按 `(turn_id, call_seq)` join 回 message 取
全量。digest 用途：验证 join 完整性（digest 不匹配 = 日志截断/损坏，渲染时标注）。

### Usage 四分的来源

`AssistantTurn.usage: Option<(u32,u32)>` 扩展为 `Option<LlmUsage>`；SSE 管道
解析 provider 的 usage payload 时透传 cache 字段（DeepSeek/Kimi 均有 cache
hit 字段；缺失时为 0）。

### 写入点（crab-core）

- `Session::append` 保持只写 message；新增 `Session::append_obs(&ObsEvent)`，
  同文件 append。
- agent loop（`crates/crab-core/src/agent.rs`）：turn 边界发 `turn_start`/
  `turn_end`；每次模型调用完成发 `llm_call`；工具执行完成发 `tool_call`。
  写入失败仅 warn，不影响运行（观测不致命，同 MCP 容错哲学）。

## Trace——从日志推导，不单独存储

span 树 = message（ToolUse/ToolResult）+ obs 事件 join：

```
turn (trace)
├─ llm_call span(s)（duration、usage、outcome）
└─ tool_call span(s)（tool、duration、bytes、outcome；digest 校验 join）
```

- 渲染按 entry 顺序流式构造：见 `turn_start` 开 trace；`llm_call`/`tool_call`
  按时间序挂为 span（同一 turn 内 llm/tool 交替，平面列表即可，不强行嵌套）；
  `turn_end` 收尾。中断的 turn（有 start 无 end）渲染时标注 `incomplete`。
- join 校验：`tool_call.args_digest` vs message 中第 call_seq 个 ToolUse 的
  args json digest；不匹配标 `join-mismatch`。

### 新子命令 `crab trace <session.jsonl>`

- 输出文本树：每 turn 一节，span 行内联 `tool args摘要 → 结果摘要`（从 join 的
  message 取，截断 60 列）、duration、tokens、错误标红（ANSI）。
- `--json` 输出结构化 span 树（供脚本消费）。

## Metrics——SQLite 聚合

- 存储：`.crab/metrics.sqlite`（WAL 模式），随 session 目录同层。
- 依赖：`rusqlite`（bundled sqlite，避免系统依赖；加入的是 **cli** crate，
  红线不破：core 无终端/存储 crate——metrics 汇总在 cli 层做）。
- 写入时机：`turn_end` 时由 cli 层（TUI/`--print` 共有路径）upsert 一行；
  纯 core 单测不产生 metrics。

### Schema

```sql
CREATE TABLE IF NOT EXISTS turns (
  session_id TEXT NOT NULL, turn_id INTEGER NOT NULL,
  ts INTEGER NOT NULL,                -- turn_start 的 epoch millis
  provider TEXT, model TEXT,
  prompt_tokens INTEGER, completion_tokens INTEGER,
  cache_read INTEGER, cache_write INTEGER,
  cost_usd REAL,                      -- 定价未知为 NULL
  llm_calls INTEGER, tool_calls INTEGER, tool_errors INTEGER,
  duration_ms INTEGER,                -- turn_start → turn_end
  interrupted INTEGER,
  PRIMARY KEY (session_id, turn_id)
);
CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts);
CREATE TABLE IF NOT EXISTS pricing (   -- 内置静态单价，启动时 seed
  model TEXT PRIMARY KEY,
  input_per_m REAL, output_per_m REAL,
  cache_read_per_m REAL, cache_write_per_m REAL
);
```

cost 计算：`prompt*input + completion*output + cache_read*cr + cache_write*cw`
（每百万 token 单价，结果保留 6 位小数）。定价来源 `pricing.toml`（workspace
根，随 release 更新）：deepseek-chat、deepseek-reasoner、kimi 系列首批入库；
查不到 → `cost_usd = NULL`（不阻塞）。

### 新子命令 `crab metrics [days]`

文本报告：
- 按日汇总：turns、token 三分合计、cost、tool 错误率、turn 时长 p50/p90；
- 按模型汇总（同期间）：调用数、token、cost 占比；
- `--json` 结构化输出。

聚合 SQL 直查 `turns`（数据量单机，不需要物化 daily 表——YAGNI）。

## Evaluation（二期，本次只设计不实现）

- **任务集** `.crab/evals/<name>/`：`task.toml`（prompt、provider、
  timeout_secs、fixture 目录复制到临时工作区）+ `check.sh`（agent 跑完后在
  工作区执行，退出码 0 = 通过；**确定性断言，零模型调用**）。
- `crab eval <name>`：tempdir 起 agent（复用 `--print` 路径，mock provider
  可用于冒烟）→ check.sh → 结果写 SQLite `eval_runs(name, ts, pass, session_id,
  duration_ms)`；支持 `crab eval` 列出历史通过率趋势。
- `crab eval promote <session.jsonl>`：从指定 session 生成任务骨架
  （trace-to-dataset：博客 feedback loop 的落地）——prompt 取自首条 user
  message，check.sh 留空模板由人补断言。
- **非目标**：LLM-as-judge、online eval、guardrails 新机制（permission gate
  已覆盖最小 tool guard；injection 检测不做）。

## 显式非目标（一期）

OTel/任何网络导出、插件化架构、TUI 内 dashboard/告警（metrics 是离线命令）、
obs 事件的高频采样（只记事实，不采样）。

## 错误处理

| 场景 | 行为 |
|---|---|
| v1 session 文件 | 按 v1 读，只读不写回；`--continue` 正常工作（无 obs 数据） |
| meta 行损坏/缺失但后续是 v2 格式 | 按内容尽力读，warn |
| obs append 失败（磁盘满/权限） | eprintln warn，运行继续 |
| digest join 不匹配 | trace 渲染标注 `join-mismatch`，不 panic |
| 未知模型定价 | `cost_usd=NULL`，metrics 报告该模型 cost 列显示 `-` |
| turn 崩溃无 turn_end | trace 标 `incomplete`；metrics 不计该 turn |
| metrics.sqlite 损坏 | `crab metrics` 报错并提示删除重建；不影响 agent 运行 |

## 测试

- session：v1 读兼容、v2 写读 roundtrip（含 meta 行）、`--continue` 上下文
  重建忽略 obs/meta（关键红线测试）
- usage 四分：SSE fixture 解析（含/不含 cache 字段）
- digest：args canonical json → blake3 稳定性、join 校验（匹配/不匹配）
- trace 渲染：多 turn fixture（正常/中断/digest 不匹配/纯 v1 文件）
- metrics：fixture turns 入库 → 按日/按模型聚合 SQL 断言；cost 计算（含
  cache 与未知模型 NULL）
- pricing.toml 解析与 seed 幂等

## Self-Review 备注

- `blake3` 新依赖进 core（digest 在 core 算）；`rusqlite` 进 cli。两者均不破
  "core 无终端 crate"红线；release 体积影响在 Task 完成时实测。
- obs 与 message 同文件 append：单句柄顺序写，无并发问题（agent loop 单点
  写入）；TUI 的 session 写路径同样经 `Session`。
- `turn_id` 由 agent loop 单调分配（每 session 从 0 起），与 EntryId 无关。
