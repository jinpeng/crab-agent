# Dogfood Log

记录每次用 crab 改 crab 的过程。最值钱的输入是**卡点**——模型哪里卡了、crab 哪里暴露缺陷。

格式：

## YYYY-MM-DD — <任务>
- 任务: <让 crab 做什么>
- 结果: 成功 / 部分 / 失败
- 权限确认次数: N
- edit 一次过 / 反复 N 次
- 卡点: <模型哪里卡住了>
- 暴露的 crab 缺陷: <要修的>
- commit: <sha 若有>

---

## 2026-10-14 — Session::len + test（Q2 验收）

- 任务: 给 Session 加 `pub fn len(&self) -> usize` 返回 entry 数 + 单元测试，跑测试确认通过
- 结果: **成功** — Q2 验收达成
- commit: f3fec8e
- （过程细节待补：权限确认次数 / edit 是否一次过 / 卡点）
