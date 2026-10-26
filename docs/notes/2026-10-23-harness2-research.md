# Harness²（google-research/harness2）调研

> 来源：`google-research/harness2` @ `main`（2026-10-06 创建，调研时 ~19.7k stars，
> shallow clone 于 2026-10-23）。所有论断均引用一手材料：仓库 README / docs / 源码、
> 论文摘要（arXiv:2610.14224）、项目官网 harness2.github.io。许可证 Apache-2.0，
> Python 3.12，Linux。

## TL;DR

Harness² 是 Google Research 的**递归式 agent harness 优化框架**：模型权重不动，
让另一个 agent 去改写"包住模型的那层系统"（system prompt / guardrails / memory /
tool guidance / subagents / skills / scripts / plugins 八个组件），并通过
**Propose–Probe–Compose** 对比式流程在没有 ground-truth 判分器的开放任务上验证
改写是否有效。两级递归：任务级（同一任务内改 harness）+ harnessing 级（把采纳的
harness 和经验教训存进按领域划分的持久 library，供后续任务参考）。在三个职业
基准（WorkBuddy / JobBench / LAB）、四种 model–harness 组合上，**单步**改进
GLM-5 与 Gemini 3.5 Flash，最高 +11.5 分（WorkBuddy），且随递归预算 k 增加继续涨。

## 它解决什么问题（论文摘要，arXiv:2610.14224）

1. **开放世界没有 ground-truth verifier**：测试任务在 harness 设计时没见过，
   在线改 harness 没有可靠信号。→ 解法：对比式 P–P–C，用执行轨迹的**行为对比**
   替代判分器。
2. **RSI（recursive self-improvement）过程高度任务特定、不泛化**。→ 解法：二级
   递归——把每次任务的经验蒸馏成"improvement harness"的更新。

## 核心机制（源码级）

### 任务级递归：Propose–Probe–Compose

`harness2/core/recursion.py`：

- **基线执行**：初始 harness 先跑 k 次参考执行，产生轨迹（RunView）。
- **Propose**：2k 个候选（k 个 invariant + k 个 domain-specific），每个候选是
  **一个完整的 agent 会话**，被授权直接编辑磁盘上的 harness 文件树——
  *"Your edits on disk ARE the proposal"*（`core/prompts.py` `_SURFACE_OPENCODE`）。
- **Probe**：每个候选 harness 各跑一次任务（`_roll_many`）。
- **Compose**：k 个独立的 aggregate 会话，每个都能看到**完整候选池 + 全部执行
  证据**，各自合成一个最终 harness（`run_parallel`，recursion.py:343）。
- **sequential 模式**（recursion.py:452）：k 轮嵌套的 P–P–C，把当前 harness 和
  累积证据带进下一轮（深度而非宽度）。

### 可编辑表面：八个组件 + 成本模型

`core/prompts.py` 把 harness 定义为一棵组件树，并按**运行时成本**给出明确排序
（`_SURFACE_OPENCODE`，prompts.py:95）：

1. `skills/<name>/SKILL.md` — on-demand，默认的新指令载体
2. `scripts/<name>.py` — 确定性操作，agent 手写错或慢的
3. `sub_agents/<name>/AGENT.md` — 只适用于"输入大、答案小"的有界子任务
4. `plugin/<name>.ts` — 唯一在 tool loop **内部**生效的组件："prose can be
   stopped reading; a hook cannot"；还零 always-on 成本
5. always-on 文本（systemprompt/guardrails/memory/tool_guidance）**最后考虑**，
   "加就要删别的来付账"

关键洞察：**always-on prose 是最贵的选项**（每个 token 都在和 agent 自己的输出
预算竞争），"最廉价的形态不是显而易见的那个"。

### 三条验证丢弃规则（同一 prompts.py）

提案在任何 rollout 前先过 validation，违反任一条**整个提案作废**：

1. 每个 skill 必须在 systemprompt/guardrails 里有**逐字出现目录名的 trigger
   行**（"most violated rule by a wide margin"）
2. always-on 文本禁止 Jinja 双大括号（StrictUndefined 渲染会直接杀掉任务）
3. 被点名的脚本必须随提案一起交付

另有两条行为纪律：`KEEP_BASE` 是合法答案（"证据不支持的适应是回归"）；harness
只改 HOW，不改交付物的 WHAT SHAPE。

### Harnessing 级递归：library + reflection

`core/selection.py:402` `reflect_adoption`：采纳后把 harness 编辑、选择决策、轨迹、
交付物蒸馏成经验（`_distil`），存入 `Library`（`core/library.py`）。

- library **按领域分目录**（`domain_dir`），任务按序运行、只读更早的条目；
  按 benchmark / executor / mode / k / seed 隔离（docs/EXPERIMENTS.md）。
- **workspace fact vs procedure 之分**（prompts.py `GENERALIZE`，:295）：具体映射、
  阈值这类事实要标注"in this workspace..."；流程要写成领域通用——因为被采纳的
  harness 会被后续任务当**范例**读。从 library 拷贝组件时必须对照当前证据**重新
  验证**事实。

### 防作弊与诚实设计

- 选择阶段候选用**匿名 id**（selection.py `_anon_id`），按产出的交付物质量选，
  不按 harness 身份。
- 上下文构建断言**无泄漏路径**（`assert_no_leak_paths`）与**无 grader 判定**
  （`assert_no_grader_verdicts`，selection.py:441 附近）——评分器信息绝不进上下文。
- `Ledger.assert_identity`（recursion.py 尾部调用）：记录每次会话的 channel/model，
  配置漂移（换模型/换渠道）会**响亮的失败**而不是静默混数据。
- channel 与 model 必须**成对覆盖**（`spec.py resolve_channel_model`），单独覆盖
  任一个直接报错——"fails silently downstream" 的防御。

## 结果（README + 官网）

- WorkBuddy-Bench（Gemini 3.5 Flash / OpenCode）：72.4 → **77.5**（k=1）→
  **80.8**（k=5 parallel）。对比方法：ReasoningBank 73.4、Meta-Harness 76.4、
  Harness Scaling 75.3（Base 72.4；官网结果表）。
- JobBench **+4.5**，LAB 部分通过率 **+11.8pp**（README/摘要，单步）。
- 四种配置：GLM-5 / Gemini 3.5 Flash × OpenCode / Codex executor。

工程形态：executor substrate 抽象（`substrate/base|opencode|codex.py`），proposer
渠道支持 `claude_cli` / `opencode`（`spec.py CHANNELS`），solver/improver/judge
模型均经 env 配置（`config.py:99-102`）。基准：JobBench（HF 公开数据 + rubrics）、
LAB（法律，1,143 任务）、WorkBuddy-Bench（office/web/code 三子集，需 Docker）。

## 对 crab 的启示

1. **组件成本模型可以直接抄**：always-on system prompt 按字付费、skills on-demand、
   plugin 进 tool loop——crab 的 skills + system prompt 广告已有雏形，"加 always-on
   文本要删别的付账"是值得写进 DESIGN.md 的原则。
2. **edits-on-disk-are-the-proposal**：让改进 agent 直接改文件树，省掉提案格式
   的中间表示。crab 的自举（dogfood）已是人工版，P–P–C 是它的自动化闭环。
3. **无 verifier 时的对比式验证**：用轨迹行为对比替代判分器——对 crab 的
   auto-compact / 配置调参类"没有 ground truth"的决策有参照价值。
4. **事实 vs 流程的分层记忆**：library 里 workspace fact 标注来源、流程写成
   领域通用，且跨任务拷贝时必须重验证——比无差别往 memory 里塞教训更可靠。
5. **身份/泄漏断言**：ledger 的 channel-model 身份校验与 grader 信息隔离，是
   评测可信度的工程底线，与 crab 的 obs/ledger 思路同源。

## 引用

- 论文：https://arxiv.org/abs/2610.14224（摘要与结果声明）
- README：`google-research/harness2`（组件、安装、基准、结果声明）
- 源码：`harness2/core/{recursion,proposer,prompts,selection,library,spec,judge}.py`，
  `harness2/cli/{run_sweep,bridge}.py`，`harness2/config.py`
- 文档：`docs/EXPERIMENTS.md`（递归模式、library 隔离、成本），`docs/BENCHMARKS.md`
- 官网：https://harness2.github.io（WorkBuddy 完整结果表与对比方法）
