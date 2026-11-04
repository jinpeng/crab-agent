//! `crab serve` — P0 remote control daemon (read-mostly).
//!
//! Design: docs/notes/2026-11-04-crab-mobile-app-design.md (P0 slice).
//! One workspace (cwd), JSONL-over-WebSocket control channel, static PWA
//! shell. The bind address must be a concrete local interface — never
//! 0.0.0.0 — and a device token gates both the page and the socket.
//!
//! P0 明确不做（后续切片）：TLS、配对 PIN、多 workspace、session 管理、
//! approve 通道（gate=yolo，启动行明说）。

use anyhow::{Context, Result, bail};
use crab_core::agent::{Agent, AgentConfig, AgentEvent};
use crab_core::gate::{Confirmer, Decision, PermissionGate, Prompt};
use crab_core::session::Session;
use crab_core::state::SessionState;
use crab_core::tools::register_all;
use crab_core::types::ContentBlock;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_HTML: &str = include_str!("serve/web/index.html");
/// 事件缓冲：断线重连 attach 时最多回放这么多条。
const REPLAY_CAP: usize = 1000;

#[derive(Clone)]
struct EventRec {
    seq: u64,
    event: Value,
}

/// confirmer/approve 需要的最小状态（可从 Shared 拆出独立测试）。
struct ConfirmHub {
    map: std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<Decision>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl ConfirmHub {
    fn new() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(0),
        }
    }
    fn insert(&self, tx: std::sync::mpsc::Sender<Decision>) -> u64 {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.map.lock().unwrap().insert(id, tx);
        id
    }
    fn remove(&self, id: u64) {
        self.map.lock().unwrap().remove(&id);
    }
    fn resolve(&self, id: u64, decision: Decision) -> bool {
        if let Some(tx) = self.map.lock().unwrap().remove(&id) {
            let _ = tx.send(decision);
            true
        } else {
            false
        }
    }
}

struct Shared {
    /// 每个 workspace 一个 Agent（懒加载，Arc 共享引用）。
    agents: Mutex<std::collections::HashMap<String, std::sync::Arc<Mutex<Agent>>>>,
    /// 当前激活的 workspace（客户端 attach/prompt 的目标）。
    active: std::sync::Mutex<String>,
    tx: broadcast::Sender<EventRec>,
    replay: Mutex<std::collections::VecDeque<EventRec>>,
    seq: std::sync::atomic::AtomicU64,
    run_state: std::sync::Mutex<&'static str>,
    confirms: ConfirmHub,
    workspaces: std::sync::Mutex<Vec<String>>,
    /// 重新装配 Agent 所需的最小集（Config 不可 Clone，存重建参数）。
    config: crab_core::config::Config,
    provider_name: Option<String>,
    gate: PermissionGate,
}

impl Shared {
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    async fn publish(&self, event: Value) {
        let rec = EventRec {
            seq: self.next_seq(),
            event,
        };
        {
            let mut buf = self.replay.lock().await;
            buf.push_back(rec.clone());
            while buf.len() > REPLAY_CAP {
                buf.pop_front();
            }
        }
        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
            eprintln!("[dbg] publish seq={} kind={}", rec.seq, rec.event["kind"]);
        }
        let _ = self.tx.send(rec); // 无订阅者不算错误
    }

    fn set_state(&self, s: &'static str) {
        *self.run_state.lock().unwrap() = s;
    }
    fn state(&self) -> String {
        self.run_state.lock().unwrap().to_string()
    }

    /// 当前激活 workspace。
    fn active(&self) -> String {
        self.active.lock().unwrap().clone()
    }

    /// 切换激活 workspace（必须已注册）。返回新激活名。
    /// 切换激活 workspace（必须已注册）。与 workspace.add 同款：先
    /// canonicalize——macOS 上 /var → /private/var 的符号链接会让
    /// 原名和注册名不一致。
    fn set_active(&self, name: &str) -> Result<String, String> {
        let canon = std::path::Path::new(name)
            .canonicalize()
            .map_err(|e| format!("cannot access {name}: {e}"))?
            .display()
            .to_string();
        if !self.workspaces.lock().unwrap().iter().any(|w| w == &canon) {
            return Err(format!("unknown workspace {canon:?} (run workspace.list)"));
        }
        *self.active.lock().unwrap() = canon.clone();
        Ok(canon)
    }

    /// 懒加载 workspace 的 Agent（首次访问时装配，之后复用）。
    /// 调用方持有 Arc<Mutex<Agent>>；事件桥由本函数在首次装配时挂好。
    async fn agent_for(
        self: &Arc<Self>,
        workspace: &str,
    ) -> Result<std::sync::Arc<Mutex<Agent>>, String> {
        {
            let map = self.agents.lock().await;
            if let Some(a) = map.get(workspace) {
                return Ok(a.clone());
            }
        }
        let cwd = std::path::PathBuf::from(workspace);
        let agent = assemble_agent(
            &cwd,
            &self.config,
            self.provider_name.as_deref(),
            &self.gate,
            false,
        )
        .map_err(|e| format!("assemble agent for {workspace}: {e}"))?;
        let agent = std::sync::Arc::new(Mutex::new(agent));
        // 事件桥必须在返回前就挂好——否则装配后立刻 run() 的早期事件
        //（TurnStarted）会在桥建立前发出，丢失。subscribe 在 spawn 之前
        // 同步完成（broadcast 只发给已存在的接收者）。
        let rx = agent.lock().await.subscribe();
        self.attach_event_bridge(workspace, rx);
        self.agents
            .lock()
            .await
            .insert(workspace.to_string(), agent.clone());
        Ok(agent)
    }

    /// 挂一个 Agent 的事件桥（agent_for 和 session.create 共用）。
    /// receiver 在 spawn 之前拿到——绝不在 spawn 里 subscribe（竞态丢事件）。
    fn attach_event_bridge(
        self: &Arc<Self>,
        workspace: &str,
        mut rx: broadcast::Receiver<AgentEvent>,
    ) {
        let shared = self.clone();
        let ws = workspace.to_string();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        match &ev {
                            AgentEvent::TurnStarted { .. } => shared.set_state("running"),
                            AgentEvent::Finished { .. } => shared.set_state("idle"),
                            _ => {}
                        }
                        if let Some(mut j) = event_json(&ev) {
                            j["workspace"] = json!(ws);
                            shared.publish(j).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        shared
                            .publish(json!({"kind":"warn","workspace":ws,
                                "text":format!("event lag: dropped {n}")}))
                            .await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// 远程审批的 Confirmer：阻塞（在 tool future 里被同步调用）等任意
    /// 已连接客户端的 approve。广播 confirm_req，首个到达的决策生效；
    /// 超时（120s）或通道断开一律 Deny——宁可错杀不可放行。
    fn confirmer(self: &Arc<Self>) -> Confirmer {
        let shared = self.clone();
        Arc::new(move |prompt: Prompt| {
            let (tx, rx) = std::sync::mpsc::channel::<Decision>();
            let id = shared.confirms.insert(tx);
            let shared2 = shared.clone();
            let p = prompt.clone();
            tokio::spawn(async move {
                shared2
                    .publish(serde_json::json!({
                        "kind": "confirm_req",
                        "confirm_id": id,
                        "tool": p.tool,
                        "summary": p.summary,
                    }))
                    .await;
            });
            // 超时即 Deny：客户端断线时工具不能无限挂起。
            let decision = rx
                .recv_timeout(std::time::Duration::from_secs(120))
                .unwrap_or(Decision::Deny);
            shared.confirms.remove(id);
            let shared3 = shared.clone();
            tokio::spawn(async move {
                shared3
                    .publish(serde_json::json!({
                        "kind": "confirm_resolved",
                        "confirm_id": id,
                        "decision": format!("{decision:?}").to_lowercase(),
                    }))
                    .await;
            });
            decision
        })
    }

    /// 客户端 approve：只对当前 pending 的 id 有效（防重放）。
    fn approve(&self, confirm_id: u64, decision: Decision) -> bool {
        self.confirms.resolve(confirm_id, decision)
    }

    fn workspace_names(&self) -> Vec<String> {
        self.workspaces.lock().unwrap().clone()
    }

    fn workspace_add(&self, path: String) -> Result<(), String> {
        let p = std::path::Path::new(&path);
        let canon = p
            .canonicalize()
            .map_err(|e| format!("cannot access {path}: {e}"))?;
        if !canon.is_dir() {
            return Err(format!("{path} is not a directory"));
        }
        let name = canon.display().to_string();
        let mut ws = self.workspaces.lock().unwrap();
        if !ws.contains(&name) {
            ws.push(name.clone());
            save_workspaces(&ws).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// 解析 bind 地址：必须是具体接口（loopback/LAN/tailnet），拒绝通配。
fn parse_bind(host: &str) -> Result<IpAddr> {
    let ip: IpAddr = host
        .parse()
        .with_context(|| format!("--host must be an IP literal, got {host:?}"))?;
    match ip {
        IpAddr::V4(v4) if v4 == Ipv4Addr::UNSPECIFIED => {
            bail!(
                "--host 0.0.0.0 would expose crab serve on every interface; pick a concrete LAN/tailnet IP"
            )
        }
        IpAddr::V6(v6) if v6.is_unspecified() => {
            bail!(
                "--host :: would expose crab serve on every interface; pick a concrete LAN/tailnet IP"
            )
        }
        _ => Ok(ip),
    }
}

fn serve_dir() -> Result<std::path::PathBuf> {
    let home = std::env::home_dir().context("home dir")?;
    Ok(home.join(".crab").join("serve"))
}

/// workspace registry 持久化（P1）：`~/.crab/serve/config.toml`，
/// TOML 里一个 `workspaces = [...]` 数组。丢失/损坏=空表（启动时 cwd
/// 永远自动注册），不报错——配置是增强，不是启动条件。
fn load_workspaces() -> Vec<String> {
    let path = match serve_dir() {
        Ok(d) => d.join("config.toml"),
        Err(_) => return Vec::new(),
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    toml::from_str::<toml::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("workspaces")?
                .as_array()?
                .iter()
                .map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn save_workspaces(list: &[String]) -> std::io::Result<()> {
    let dir = serve_dir().map_err(std::io::Error::other)?;
    std::fs::create_dir_all(&dir)?;
    let items = list
        .iter()
        .map(|w| format!("{w:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(dir.join("config.toml"), format!("workspaces = [{items}]\n"))
}

/// device token：32 字节随机 hex，首次生成写到 ~/.crab/serve/token（0600），
/// 之后复用。/dev/urandom 失败即报错——认证不可用比弱随机更糟。
fn device_token() -> Result<String> {
    let dir = serve_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let mut bytes = [0u8; 32];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?
            .write_all(token.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, &token)?;
    Ok(token)
}

/// 从 query string 提取 token 参数。
fn query_token(query: &str) -> Option<&str> {
    query.split('&').find_map(|kv| kv.strip_prefix("token="))
}

/// HTTP/WS 分流：tungstenite 的 accept_hdr_async 自己完成 101 握手，
/// 非 WS 路径走最小 HTTP 响应。鉴权在握手回调里（ws）或路由里（页面）。
async fn handle_conn(stream: TcpStream, shared: Arc<Shared>, token: Arc<String>) -> Result<()> {
    // peek 前 2KB 判断路径——不消耗字节，tungstenite 能看到完整握手。
    let mut peek = [0u8; 2048];
    let n = stream.peek(&mut peek).await?;
    if n == 0 {
        return Ok(());
    }
    let head = String::from_utf8_lossy(&peek[..n]);
    let line = head.lines().next().unwrap_or("");
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let ok_token = query_token(query).is_some_and(|t| t == token.as_str());

    match (route, ok_token) {
        ("/healthz", _) => simple_http(stream, 200, "ok").await,
        ("/ws", true) => {
            // clippy::result_large_err：tungstenite 握手错误类型大，但这条路
            // 径不构造 Err——ok 分支直接进 WS 会话。
            #[allow(clippy::result_large_err)]
            let cb = |_req: &Request, resp: Response| Ok(resp);
            let ws = tokio_tungstenite::accept_hdr_async(stream, cb).await?;
            handle_ws(ws, shared).await
        }
        ("/ws", false) => simple_http(stream, 403, "missing or bad token").await,
        ("/", true) => {
            let html = INDEX_HTML.replace(
                "const tok = new URLSearchParams(location.search).get('token') || '';",
                &format!("const tok = '{token}';"),
            );
            simple_http(stream, 200, &html).await
        }
        ("/", false) => simple_http(stream, 403, "missing or bad token").await,
        _ => simple_http(stream, 404, "not found").await,
    }
}

async fn simple_http(mut stream: TcpStream, code: u16, body: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let reason = match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let ct = if body.starts_with("<!doctype") {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

/// AgentEvent → 协议事件 JSON（协议文档 §组件二的事件面）。
fn event_json(ev: &AgentEvent) -> Option<Value> {
    Some(match ev {
        AgentEvent::TurnStarted { turn } => json!({"kind":"turn_started","turn":turn}),
        AgentEvent::TextDelta(s) => json!({"kind":"text_delta","text":s}),
        AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_write_tokens,
        } => json!({"kind":"usage","prompt":prompt_tokens,"completion":completion_tokens,
                    "cache_read":cache_read_tokens,"cache_write":cache_write_tokens}),
        AgentEvent::AssistantMessage(m) => {
            let text: String = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            json!({"kind":"assistant_message","text":text})
        }
        AgentEvent::ToolCallStarted { id, name } => {
            json!({"kind":"tool_call_started","id":id,"name":name})
        }
        AgentEvent::ToolCallFinished {
            id,
            result,
            is_error,
        } => json!({"kind":"tool_call_finished","id":id,"result":result,"is_error":is_error}),
        AgentEvent::Finished { reason } => {
            json!({"kind":"finished","reason":format!("{reason:?}")})
        }
        AgentEvent::Compacted {
            strategy,
            shed_tool_results,
            turns_affected,
            ..
        } => json!({"kind":"compacted","strategy":format!("{strategy:?}"),
                    "shed":shed_tool_results,"turns":turns_affected}),
    })
}

/// 装配一个 workspace 的 Agent（与 TUI 同路径：skills/lessons/provider/工具。
/// P0 起不挂 MCP——attach_mcp_servers 的子进程管理是独立切片）。
fn assemble_agent(
    cwd: &std::path::Path,
    config: &crab_core::config::Config,
    provider_name: Option<&str>,
    gate: &PermissionGate,
    new_session: bool,
) -> Result<Agent> {
    let skills = crab_core::skills::discover(cwd, &std::env::home_dir().unwrap_or_default());
    let system_prompt =
        crate::app::with_lessons(crate::app::build_system_prompt(cwd, None, &skills), cwd);
    let (provider, resolved) = crate::app::resolve_provider(config, provider_name)?;
    let tools = register_all(SessionState::default(), gate.clone());
    let dir = cwd.join(".crab").join("sessions");
    // new_session=true（session.create）强制开新文件；否则续最新（--continue 语义）。
    let session = if new_session {
        Session::create(&dir).context("create session")?
    } else {
        match crate::app::latest_session(&dir) {
            Some(p) => Session::load(&p).with_context(|| format!("load {}", p.display()))?,
            None => Session::create(&dir).context("create session")?,
        }
    };
    Ok(Agent::new(
        provider,
        tools,
        session,
        AgentConfig {
            system_prompt,
            context_window: resolved.context_window,
            auto_compact_threshold: config.auto_compact_threshold(),
            compactions: Vec::new(),
            mcp_allowlist: resolved.mcp_allowlist.clone(),
        },
    ))
}

pub async fn run(host: &str, port: u16) -> Result<()> {
    let ip = parse_bind(host)?;
    let token = Arc::new(device_token()?);
    let cwd = std::env::current_dir().context("cwd")?;

    let config =
        crab_core::config::Config::load_from(None).map_err(|e| anyhow::anyhow!("config: {e}"))?;
    // gate 的 confirmer 要在 register_all 前就位，但它依赖 shared（事件
    // 广播），shared 又依赖 agent——循环。解法是**延迟注入**：gate 先拿
    // 一个转发闭包，shared 建好后把真正的 confirmer 填进去。注入前
    // confirmer 一律 Deny（保守默认）。
    let late_confirmer: Arc<std::sync::Mutex<Option<Confirmer>>> =
        Arc::new(std::sync::Mutex::new(None));
    let gate = PermissionGate::with_confirmer({
        let late = late_confirmer.clone();
        Arc::new(move |p: Prompt| {
            match late.lock().unwrap().as_ref() {
                Some(c) => c(p),
                None => Decision::Deny, // serve 未就绪时所有写操作拒绝
            }
        })
    });

    // workspace registry：持久化的列表 + cwd（永远注册，去重）。
    let mut initial_ws = load_workspaces();
    let cwd_name = cwd.display().to_string();
    if !initial_ws.contains(&cwd_name) {
        initial_ws.push(cwd_name.clone());
        let _ = save_workspaces(&initial_ws); // 失败不阻塞启动
    }

    let shared = Arc::new(Shared {
        agents: Mutex::new(std::collections::HashMap::new()),
        active: std::sync::Mutex::new(cwd_name.clone()),
        tx: broadcast::channel(256).0,
        replay: Mutex::new(std::collections::VecDeque::new()),
        seq: std::sync::atomic::AtomicU64::new(0),
        run_state: std::sync::Mutex::new("idle"),
        confirms: ConfirmHub::new(),
        workspaces: std::sync::Mutex::new(initial_ws),
        config,
        provider_name: None,
        gate,
    });
    // 注入真正的远程审批 confirmer（注入前一律 Deny——保守默认）。
    *late_confirmer.lock().unwrap() = Some(shared.confirmer());

    let addr = SocketAddr::new(ip, port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    eprintln!("crab serve v{VERSION}");
    eprintln!("  workspace: {}", shared.active());
    eprintln!("  url:       http://{addr}/?token={token}");
    eprintln!("  healthz:   http://{addr}/healthz");
    eprintln!("  note: P1 has an approval channel but no TLS yet — trusted LAN/tailnet only");

    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, shared, token).await {
                eprintln!("[serve] conn error: {e:#}");
            }
        });
    }
}

// ---- WebSocket 会话 ----

async fn handle_ws<S>(ws: tokio_tungstenite::WebSocketStream<S>, shared: Arc<Shared>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut out, mut input) = ws.split();
    let mut events = shared.tx.subscribe();
    loop {
        tokio::select! {
            msg = input.next() => {
                let Some(msg) = msg else { break };
                let msg = msg?;
                if !msg.is_text() { continue; }
                let v: Value = serde_json::from_str(msg.to_text()?)?;
                let id = v["id"].as_str().map(str::to_string);
                if let Some(mut resp) = dispatch(v, &shared).await {
                    if let Some(id) = id { resp["id"] = json!(id); }
                    out.send(resp.to_string().into()).await?;
                }
            }
            rec = events.recv() => {
                match rec {
                    Ok(rec) => {
                        let frame = json!({"type":"event","seq":rec.seq,"event":rec.event});
                        out.send(frame.to_string().into()).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

/// 命令分发（P0 全集：status / attach / prompt / abort）。
async fn dispatch(v: Value, shared: &Arc<Shared>) -> Option<Value> {
    let ty = v["type"].as_str().unwrap_or("");
    match ty {
        "status" => Some(json!({
            "type": "response", "ok": true,
            "data": {
                "host": hostname(),
                "version": VERSION,
                "workspace": shared.active(),
                "state": {"run_state": shared.state()},
                "note": "P1: approval channel live; no TLS yet (P2)",
            }
        })),
        "attach" => {
            let after = v["after_seq"].as_u64();
            let buf = shared.replay.lock().await;
            let head_seq = buf.back().map(|r| r.seq).unwrap_or(0);
            let replay: Vec<Value> = buf
                .iter()
                .filter(|r| after.is_none_or(|a| r.seq > a))
                .map(|r| json!([r.seq, r.event]))
                .collect();
            Some(json!({
                "type": "response", "ok": true,
                "data": {"head_seq": head_seq, "replay": replay}
            }))
        }
        "prompt" => {
            let text = v["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Some(err("empty prompt"));
            }
            let shared = shared.clone();
            tokio::spawn(async move {
                let ws = shared.active();
                if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                    eprintln!("[dbg] prompt: ws={ws}");
                }
                match shared.agent_for(&ws).await {
                    Ok(agent) => {
                        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                            eprintln!("[dbg] prompt: agent ready, running");
                        }
                        let mut agent = agent.lock().await;
                        let r = agent.run(text).await;
                        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                            eprintln!("[dbg] prompt: run done: {r:?}");
                        }
                    }
                    Err(e) => {
                        shared.publish(json!({"kind":"error","text":e})).await;
                    }
                }
            });
            Some(json!({"type":"response","ok":true,"data":{"disposition":"started"}}))
        }
        "abort" => {
            let ws = shared.active();
            match shared.agent_for(&ws).await {
                Ok(agent) => {
                    agent.lock().await.abort();
                    Some(json!({"type":"response","ok":true}))
                }
                Err(e) => Some(err(e)),
            }
        }
        "approve" => {
            let id = v["confirm_id"].as_u64().unwrap_or(0);
            let decision = match v["decision"].as_str().unwrap_or("") {
                "allow" | "y" | "yes" => Decision::Allow,
                "allow_all" | "all" | "a" => Decision::AllowAll,
                "deny" | "n" | "no" => Decision::Deny,
                other => {
                    return Some(err(format!(
                        "bad decision {other:?} (allow|deny|allow_all)"
                    )));
                }
            };
            if shared.approve(id, decision) {
                Some(json!({"type":"response","ok":true}))
            } else {
                Some(err(format!(
                    "no pending confirm {id} (already resolved or unknown)"
                )))
            }
        }
        "workspace.list" => Some(json!({
            "type": "response", "ok": true,
            "data": {"workspaces": shared.workspace_names(),
                     "active": shared.active()}
        })),
        "workspace.switch" => {
            let name = v["name"].as_str().unwrap_or("");
            match shared.set_active(name) {
                Ok(active) => {
                    shared
                        .publish(json!({"kind":"workspace.switched","workspace":active}))
                        .await;
                    Some(json!({"type":"response","ok":true,"data":{"active":active}}))
                }
                Err(e) => Some(err(e.to_string())),
            }
        }
        "session.list" => {
            // 当前激活 workspace 的 session 列表（按修改时间倒序，前 20）。
            let dir = std::path::PathBuf::from(shared.active())
                .join(".crab")
                .join("sessions");
            let mut items: Vec<(String, u64)> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                        .filter_map(|e| {
                            let m = e.metadata().ok()?;
                            Some((
                                e.file_name().to_string_lossy().into_owned(),
                                m.modified()
                                    .ok()?
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .ok()?
                                    .as_secs(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            items.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
            items.truncate(20);
            let list: Vec<Value> = items
                .into_iter()
                .map(|(name, mtime)| json!({"file": name, "mtime": mtime}))
                .collect();
            Some(json!({"type":"response","ok":true,"data":{"sessions":list}}))
        }
        "session.create" => {
            // 新 session = 丢弃当前 Agent 并让下次 agent_for 重建一个（指向
            // 新文件）。简单可靠：直接换一个新 Agent（append-only 旧文件不动）。
            let ws = shared.active();
            let cwd = std::path::PathBuf::from(&ws);
            match assemble_agent(
                &cwd,
                &shared.config,
                shared.provider_name.as_deref(),
                &shared.gate,
                true,
            ) {
                Ok(new_agent) => {
                    let agent = std::sync::Arc::new(Mutex::new(new_agent));
                    let rx = agent.lock().await.subscribe();
                    shared.attach_event_bridge(&ws, rx);
                    shared.agents.lock().await.insert(ws.clone(), agent.clone());
                    shared
                        .publish(json!({"kind":"session.created","workspace":ws}))
                        .await;
                    Some(json!({"type":"response","ok":true}))
                }
                Err(e) => Some(err(e.to_string())),
            }
        }
        "workspace.add" => {
            let path = v["path"].as_str().unwrap_or("").to_string();
            match shared.workspace_add(path) {
                Ok(()) => {
                    let names = shared.workspace_names();
                    shared.publish(json!({"kind":"workspace.added"})).await;
                    Some(json!({"type":"response","ok":true,"data":{"workspaces":names}}))
                }
                Err(e) => Some(err(e)),
            }
        }
        _ => Some(err(format!(
            "unknown command {ty:?} (supports: status, attach, prompt, abort, approve, workspace.list/add/switch, session.list/create)"
        ))),
    }
}

fn err(msg: impl Into<String>) -> Value {
    json!({"type":"response","ok":false,"error":msg.into()})
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "crab-host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_rejects_wildcard() {
        assert!(parse_bind("0.0.0.0").is_err());
        assert!(parse_bind("::").is_err());
        assert!(parse_bind("127.0.0.1").is_ok());
        assert!(parse_bind("192.168.1.10").is_ok());
        assert!(parse_bind("not-an-ip").is_err());
    }

    #[test]
    fn query_token_extracts() {
        assert_eq!(query_token("token=abc&x=1"), Some("abc"));
        assert_eq!(query_token("x=1"), None);
    }

    #[test]
    fn event_json_shapes() {
        let e = AgentEvent::TextDelta("hi".into());
        assert_eq!(event_json(&e).unwrap()["kind"], "text_delta");
        let e = AgentEvent::Finished {
            reason: crab_core::types::StopReason::EndTurn,
        };
        assert_eq!(event_json(&e).unwrap()["kind"], "finished");
    }

    /// P1 核心：ConfirmHub 的插入/路由/移除 + confirmer 的广播与阻塞等待。
    /// 全流程不经过 LLM。
    #[tokio::test]
    async fn confirm_hub_routes_decision_and_rejects_replay() {
        let hub = ConfirmHub::new();
        let (tx, rx) = std::sync::mpsc::channel::<Decision>();
        let id = hub.insert(tx);
        // resolve 路由到等待中的 receiver。
        assert!(hub.resolve(id, Decision::Allow));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
            Decision::Allow
        );
        // 重放：同一 id 再 resolve 必须失败。
        assert!(!hub.resolve(id, Decision::Allow));
        // 未知 id 失败。
        assert!(!hub.resolve(999, Decision::Deny));
    }

    /// confirmer 的完整链路：广播 confirm_req → approve → 阻塞的调用方
    /// 拿到决策 → 广播 confirm_resolved。构造一个最小 Shared 替代品——
    /// confirmer 只依赖 confirms/tx/seq，workspaces/agent 不触碰。
    /// 由于 Rust 不允许部分构造，这里直接测 ConfirmHub + 手动复刻
    /// confirmer 的广播逻辑（共享同一份 publish 代码路径的语义）。
    #[tokio::test]
    async fn confirmer_logic_roundtrip() {
        let hub = ConfirmHub::new();
        let (tx_events, mut rx_events) = broadcast::channel::<EventRec>(64);
        let seq = std::sync::atomic::AtomicU64::new(0);

        // 复刻 confirmer 的主体（与 Shared::confirmer 同逻辑，注入 hub/tx/seq）。
        let hub2 = std::sync::Arc::new(hub);
        let hub3 = hub2.clone();
        let tx2 = tx_events.clone();
        let prompt = Prompt {
            tool: "bash",
            summary: "run: rm -rf /tmp/x".into(),
        };
        let handle = std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel::<Decision>();
            let id = hub2.insert(tx);
            let rec = EventRec {
                seq: seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
                event: serde_json::json!({"kind":"confirm_req","confirm_id":id,
                    "tool":prompt.tool,"summary":prompt.summary}),
            };
            let _ = tx2.send(rec);
            let decision = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or(Decision::Deny);
            hub2.remove(id);
            (id, decision)
        });

        // 等 confirm_req 广播。
        let rec = tokio::time::timeout(std::time::Duration::from_secs(5), rx_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.event["kind"], "confirm_req");
        let cid = rec.event["confirm_id"].as_u64().unwrap();
        assert_eq!(rec.event["tool"], "bash");

        // approve 路由。
        assert!(hub3.resolve(cid, Decision::Allow));
        let (id2, decision) = handle.join().unwrap();
        assert_eq!(id2, cid);
        assert_eq!(decision, Decision::Allow);
    }
}
