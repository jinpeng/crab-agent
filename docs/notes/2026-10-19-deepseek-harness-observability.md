# DeepSeek Harness（dsh）可观测性方案分析

> 来源：`deepseek-ai/deepseek-harness` @ `687f282e`（2026-10-17，shallow clone）。
> 所有论断均引用仓库内一手文件（README / docs / 源码路径）。dsh 是一个 pi 式
> coding agent monorepo（pnpm workspace + Cordis 插件框架，"Everything is a
> Plugin"），本文只分析其可观测性（observability）设计，不做整体评测。

## TL;DR

dsh 的可观测性不是"加一个 tracing SDK"，而是一个**以持久化会话日志为唯一事实源**
（model-visible ⟺ logged）的分层体系：append-only 会话日志（带格式版本纪律）→
类型化事件总线 + 确定性 replay 测量（token-meter）→ 统一查询服务（session-query +
SQLite FTS5）→ 双通道 OTel 导出（product / session，采集侧授权+脱敏，severity 预映射
零配置告警）→ 编号化 postmortem 事故文档。核心思想：**观测数据不许改变被观测系统**
（测量无模型调用、请求深冻、遥测不进模型上下文），**消费侧共享同一次测量 fold**
（compaction、状态栏、遥测读同一个 token-meter 结果）。

## 一、第一性原则（从源码归纳）

1. **Model-visible ⟺ logged**——每个发往模型的请求都必须能从会话日志完整重建；
   agent-loop 构造的请求**深冻**（deep-frozen），监听器和适配器只能读、不能改写
   （`packages/llm/llm/README.md`、`packages/core/agent/src/runtime-types.ts`）。
   这让"调试线上问题"等价于"回放日志"。
2. **测量不改变系统**——token/context 测量通过 **replay 持久化会话日志**完成，
   确定性、零模型调用、零成本；compaction、占用显示、遥测共享同一结果
   （`packages/llm/token-meter/README.md`）。
3. **遥测数据永不进入模型请求**——"No reporting data reaches model requests"
   （`docs/subsystems/session-telemetry.md`）。
4. **文档即代码的产物**——事件生产/消费矩阵、Cordis API 目录、会话格式状态页
   全部由脚本从源码生成并有 freshness 校验（`docs/event-producer-consumer.md`
   头部的生成标记、`docs/session-format-status.md`）。

## 二、分层架构

```
L4  事故学习      docs/postmortem/0001..000N（编号事故复盘文档，双语）
L3  遥测导出      OTel 双通道：product-telemetry-otel / session-telemetry-otel
                （采集侧授权+脱敏；severity 预映射；4MB 请求上限；通道互不共享队列）
L2  查询检索      ctx.sessionQuery 统一服务（精确读/关系追踪/过滤）
                + session-query-sqlite（FTS5 全文索引）+ Web /export ZIP
L1  事件与投影    类型化事件总线（emit/serial/waterfall 三态 many-to-many）
                + session projections（tokenUsage / contextPressure / contextBreakdown）
L0  会话日志      append-only durable log；SESSION_FORMAT_VERSION 单一事实常量；
                finalization/release 双记录；迁移历史不可变（persistence-changes/）
```

### L0 会话日志——事实源的工程纪律

- 写者版本号是代码里唯一手工维护的常量
  （`packages/core/session/src/types.ts` 的 `SESSION_FORMAT_VERSION`），
  catalog 生成器校验迁移链可达（`docs/session-format-status.md`）。
- 格式演进有**两个独立记录**：finalization（兼容性基线，现 V4）与 release
  （已发布证据 + evidenceTag）；alpha/beta 发布同样产生持久化兼容义务。
- 每次格式变更留有不可变的 checkpoint 与历史 schema 文档——观测"数据结构本身
  的历史"。

### L1 事件与投影——可观测性的经络

- 事件矩阵由 `scripts/gen-doc-graphs.ts` 从源码生成，覆盖每个事件的声明位置、
  分发者、监听者（如 `agent/status`：`agent-loop` emit → acp /
  compaction-basic / goal-round-driver / session-controller 监听）
  （`docs/event-producer-consumer.md`）。
- 事件分三种投递模式：`emit`（异步广播）/ `serial`（顺序执行）/ `waterfall`
  （可改变结果的级联）。
- token-meter 注册三个**会话投影**供所有消费者读（`packages/llm/token-meter/README.md`）：
  - `tokenUsage`：`uncachedInputTokens` / `outputTokens` / `cacheReadTokens` /
    `cacheWriteTokens`（最终 assistant 消息的 usage 取代同次流式 usage；
    `llm/retry-started` 结束该作用域——**重试会计费为另一次尝试**）；
  - `contextPressure`：最新 provider 上报的 prompt 大小 + 下次请求投影 +
    context window；
  - `contextBreakdown`：启发式的 system/tools/message 三段构成。
- 测量语义细节：provider 上报 usage 仅在"请求 envelope 完全一致"时复用；否则对
  完整 envelope + surface 重新估算。图片按路由声明的视觉 token 计价，文件按
  "模型可见的句柄文本"计价。

### L2 查询检索——让日志可被消费

- `ctx.sessionQuery` 一个服务覆盖：精确日志读取、**关系追踪（relationship
  traces）**、过滤列表；SQLite FTS5 支撑全文搜索；"搜索结果与模型看到的对话
  历史一致"（`packages/session-query/README.md`）。
- Web 端 `/export` 可下载会话 ZIP。

### L3 遥测导出——OTel，但纪律严格

- 共享服务 `ctx.otel` 只是**通道工厂**：普通事件通道（按条数批量）与
  Session 日志通道（**按字节数限界**，单请求 4,000,000 字节上限）；
  "Mounting only the OTel service allocates no provider or transport"
  （`docs/subsystems/otel.md`）。
- **通道完全独立**：不共享队列、不共享请求；消费者持有并负责各自通道的
  shutdown 与外层截止时间。
- **采集侧治理**：Session 上报前做授权（canonical-prefix capture）与脱敏
  （redaction）；product 通道"不自动收集任何会话数据或标识符"，事件与字段由
  业务消费者显式选择（`docs/subsystems/product-telemetry.md`）。
- **记录结构刻意区分两类**（`docs/subsystems/session-telemetry.md`）：
  - `ledger`：会话日志的逐事件镜像，携带最小身份集（session.id、
    format_version、event.type、event.seq，可选 cwd/parent_id）；
  - `ops`：日志没有家的运维信号（`agent-error`、`shutdown`），**故意不带
    seq 类标识**，永远不会被误当 ledger 行。
- **severity 在采集侧预映射**：事件自身 outcome 标志（工具消息的 isError、
  turn/end 错误原因、agent-error）→ `error`，其余默认 `info`——
  "receiver can alert with zero configuration"。

### L4 事故学习

- `docs/postmortem/` 编号事故文档（0001 ACP 默认导出丢失 inject、0002 JS
  表达式禁用文件系统工具、0003 web agent GUI 反馈环……），双语维护。

## 三、对 crab 的可借鉴点

crab 现状：append-only JSONL session、SSE 管道的 Usage 事件、刚做的 pi 式
状态栏（↑↓ 累计 token、CH%、provider/model）。差距与机会按性价比排序：

| dsh 做法 | crab 现状 | 借鉴价值 |
|---|---|---|
| token-meter：**replay 日志做确定性测量**，compaction/状态栏/遥测共享 | 状态栏 context 用"最近一次 prompt_tokens"近似 | 高——crab 的 JSONL 就在本地，replay 成本极低，还能在启动时离线算出精确占用 |
| `tokenUsage` 四分（input/output/cacheRead/cacheWrite） | 只有 prompt+completion | 高——DeepSeek 有缓存命中字段， crab 已在 SSE 里丢过 usage（dogfood-log 有记录），顺手存全 |
| ledger/ops 双通道 + 采集侧脱敏 | 无遥测 | 中——crab 目前无遥测计划；若加，先抄"通道独立 + 上报前脱敏"两条纪律 |
| severity 预映射、零配置告警 | 无 | 低——单机 agent 无告警需求 |
| 事件矩阵文档由源码生成 | 无（DESIGN.md 手写） | 中——crab 事件少，手写够；规模上来再生成 |
| 会话格式版本纪律（V4 + 双记录 + 不可变迁移史） | session 无版本号 | 高——crab 的 session 格式已在演进（迟早要迁移），现在就加 `format_version` 字段成本最低 |
| postmortem 目录 | 有 dogfood-log（类似物，偏"过程+缺陷"） | 中——crab 的 dogfood-log 已覆盖；可加编号事故条目区分"事故"与"日常" |
| session-query（FTS5 全文检索会话） | 只有 `--continue` 读最新 | 低——YAGNI，等会话多到找不到再议 |
| 请求深冻保证可重建 | crab 的 edit 工具有 freshness gate（同思路：不可变事实保护） | 思想已一致 |

**一句话**：dsh 把可观测性做成了"日志即真相、测量即回放、导出即隔离"的三段
纪律；对 crab 最现实的一步是**把 Usage 四分字段落进 session JSONL + 给 session
格式加版本号**，为将来 replay 式精确统计铺路。

## 来源清单（均为 deepseek-harness 仓库内一手文件）

- `packages/llm/llm/README.md` —— model-visible ⟺ logged、深冻请求
- `packages/llm/token-meter/README.md` —— replay 测量、投影定义、usage 复用条件
- `docs/subsystems/otel.md` —— ctx.otel 通道工厂语义
- `docs/subsystems/session-telemetry.md` —— ledger/ops 记录结构、severity 预映射
- `docs/subsystems/product-telemetry.md` —— product 通道"显式选字段"原则
- `packages/session-query/README.md` —— 统一查询服务 + FTS5 + /export
- `docs/event-producer-consumer.md` —— 事件矩阵（生成文档）
- `docs/session-format-status.md` —— 会话格式版本与发布纪律
- `docs/postmortem/` —— 编号事故文档
- `docs/config-catalog.md` —— compaction 的 pressure 参数（token-meter 消费方）
