# 设计：MCP client（stdio 子进程，只接 tools）

**日期**：2026-10-14
**状态**：待 review
**前置**：config 多 provider（已合并）。DESIGN §7 钦定 MCP 为唯一外部扩展机制。spec 依据 brainstorming 结论。

---

## 目标

crab 作为 MCP **client**，spawn config 里配的 stdio MCP server 子进程，把它们的 tools 暴露为 crab 的 `Tool`，agent 能像调内置工具一样调它们（例如 web_search server）。

## 范围（已定）

- **只接 MCP tools**——不接 resources / prompts / sampling / notifications。99% 用例是 tools（web search、数据库、API 包装）。YAGNI。
- **只 stdio 传输**——spawn 子进程，JSON-RPC over stdin/stdout。MCP 生态绝大多数是 stdio；HTTP/SSE 远程 server 留到以后。
- **每 server `enabled` 标志，缺省启用**。
- **启动失败容错**——某 server 连不上/握手失败：警告并跳过，crab 照常跑（不致命）。
- **权限**：MCP 工具默认全走 `PermissionGate`（同 bash 非只读命令），`--yolo` 全放行。crab 不知道 MCP 工具的危险性，默认确认最诚实。

## 配置形状

`~/.crab/config.toml` 在现有 provider 段之外加 `[mcp.<name>]`：

```toml
default_provider = "deepseek"

[providers.deepseek]
# ...

# MCP servers（stdio 子进程）
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/Users/you/code"]
enabled = true            # 缺省 true

[mcp.websearch]
command = "uvx"
args = ["mcp-web-search"]
# enabled 缺省 true

[mcp.heavy]
command = "some-server"
enabled = false           # 关掉，不启动
```

## 组件

```
crates/crab-core/src/mcp.rs
├── McpServerConfig { command, args, enabled }     // config 反序列化
├── McpClient { peer, server_name }                // 包装 rmcp RunningService
│   ├── async spawn(cfg) -> Result<Self>           // TokioChildProcess + serve + handshake
│   ├── async list_tools() -> Vec<McpToolMeta>     // 名称/描述/schema
│   └── async call(name, args) -> Result<String>   // 调工具 → 文本结果
└── McpTool { client: Arc<McpClient>, meta }       // 实现 crab Tool trait
    └── 包装一个 MCP tool 为 crab Tool，gate.check() 后转发 call
```

## 接入流程

1. **启动**（`build_agent`）：读 config 的 `[mcp.*]`，对 `enabled != false` 的 server 并发 `McpClient::spawn`。失败的打警告、跳过。
2. **工具发现**：每个连上的 server `list_tools()`，每个 MCP tool 包成 `McpTool` 注册进 `ToolRegistry`。
3. **调用**：agent 调工具 → `McpTool::execute` → `gate.check`（权限）→ `client.call` → 结果文本回灌。
4. **工具命名**：MCP tool 名可能和内置冲突。用 `<server>__<tool>` 命名空间（如 `websearch__search`），避免撞内置的 `read`/`bash`。
5. **关闭**：agent/进程退出时 `client.cancel()`（`McpClient` 实现 Drop 或显式 shutdown）。

## rmcp 用法（已勘察确认）

```rust
use rmcp::{ServiceExt, transport::{TokioChildProcess, ConfigureCommandExt}, model::CallToolRequestParams};
use tokio::process::Command;

let client = ().serve(TokioChildProcess::new(
    Command::new(cmd).configure(|c| { c.args(args); })
)?).await?;                                  // spawn + handshake
let tools = client.list_tools(Default::default()).await?;   // 发现
client.call_tool(CallToolRequestParams::new(name).with_arguments(args_obj)).await?;  // 调用
client.cancel().await?;                       // 关闭
```

features：`client` + `transport-child-process`。

## McpTool 的 Tool trait 实现

```rust
#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> ... // "<server>__<tool>"
    fn description(&self) -> ... // MCP tool 的 description
    fn schema(&self) -> serde_json::Value // MCP tool 的 inputSchema
    async fn execute(&self, args) -> Result<String, ToolError> {
        self.client.gate_check(self.name())?;   // 权限门禁
        let result = self.client.call(self.tool_name, args).await?;
        Ok(mcp_result_to_text(result))           // MCP content[] → 文本
    }
}
```

**问题**：`Tool::name()` 返回 `&'static str`，但 `<server>__<tool>` 是运行时构造的 String。需要改为返回 `String` 或 `Cow<str>`，或用 `&'static str` 的 leak。最干净：**`Tool::name()` 改为返回 `String`**（或 `Box<str>`）。这动 M2 的 trait——小改，所有内置工具同步改。

## 错误处理

- server spawn 失败（命令不存在）→ 警告 + 跳过
- 握手失败 → 警告 + 跳过
- tool 调用失败 → `ToolError::Exec`（回灌模型，让它重试或换方法）
- MCP 返回 `isError` content → 转成 ToolError

## 测试

- **Mock MCP server**：写一个最小的 stdio MCP server（用 rmcp server 端，或手写 JSON-RPC 循环）作为测试 fixture，验证 spawn/list/call 链路。
- 或用 rmcp 的 in-process transport（如果有）做无子进程测试。
- `McpTool` 的 name/description/schema 包装正确性。
- 门禁：`McpTool` 在 deny 时不发调用。
- 命名空间：`<server>__<tool>` 不撞内置。

## 明确不做

- resources / prompts / sampling / notifications / roots
- HTTP/SSE 传输
- MCP server 的 hot-reload / 运行时增删
- per-server `trusted` 标志（门禁一律过，`--yolo` 全放）

## 依赖

`rmcp = { version = "3.5", features = ["client", "transport-child-process"], default-features = false }`。

体积影响：rmcp 会加进 release 二进制，需实测（budget 12MiB，当前 8.24MiB）。

---

## Self-Review

- **Placeholder**：MCP content[]→text 的转换（`mcp_result_to_text`）需处理 text/image/resource 三种 content——v0 只取 text，image/resource 标注"[unsupported content]"，已明确不算占位。
- **一致性**：`<server>__<tool>` 命名空间贯穿；`enabled` 缺省 true；权限走 PermissionGate 与 bash 一致。
- **范围**：聚焦 stdio + tools，可控。
- **风险**：rmcp 的 `list_tools`/`call_tool` 返回类型细节、content[] 反序列化，在实现时按 rmcp 3.5 的实际 API 调整（已勘察主流程）。
- **`Tool::name()` 签名**：要改为返回 `String`（MCP 工具名运行时构造）——这是唯一动现有 trait 的点，需在实现时同步改 6 个内置工具。
