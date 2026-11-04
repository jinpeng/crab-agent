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
use crab_core::gate::PermissionGate;
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

struct Shared {
    agent: Mutex<Agent>,
    workspace: String,
    tx: broadcast::Sender<EventRec>,
    replay: Mutex<std::collections::VecDeque<EventRec>>,
    seq: std::sync::atomic::AtomicU64,
    run_state: std::sync::Mutex<&'static str>,
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

pub async fn run(host: &str, port: u16) -> Result<()> {
    let ip = parse_bind(host)?;
    let token = Arc::new(device_token()?);
    let cwd = std::env::current_dir().context("cwd")?;

    // 与 TUI 同一个 Agent 装配。P0 简化：不挂 MCP（attach_mcp_servers 的
    // 子进程管理是独立切片）、gate=yolo（远程还没有审批通道，启动行明说）。
    let skills = crab_core::skills::discover(&cwd, &std::env::home_dir().unwrap_or_default());
    let system_prompt =
        crate::app::with_lessons(crate::app::build_system_prompt(&cwd, None, &skills), &cwd);
    let config =
        crab_core::config::Config::load_from(None).map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let (provider, resolved) = crate::app::resolve_provider(&config, None)?;
    let gate = PermissionGate::yolo();
    let tools = register_all(SessionState::default(), gate);
    let dir = cwd.join(".crab").join("sessions");
    let session = match crate::app::latest_session(&dir) {
        Some(p) => Session::load(&p).with_context(|| format!("load {}", p.display()))?,
        None => Session::create(&dir).context("create session")?,
    };
    let agent = Agent::new(
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
    );

    let shared = Arc::new(Shared {
        agent: Mutex::new(agent),
        workspace: cwd.display().to_string(),
        tx: broadcast::channel(256).0,
        replay: Mutex::new(std::collections::VecDeque::new()),
        seq: std::sync::atomic::AtomicU64::new(0),
        run_state: std::sync::Mutex::new("idle"),
    });

    // Agent 事件 → 协议事件（广播 + 回放缓冲）+ run_state 跟踪。
    {
        let shared = shared.clone();
        let mut rx = shared.agent.lock().await.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        match &ev {
                            AgentEvent::TurnStarted { .. } => shared.set_state("running"),
                            AgentEvent::Finished { .. } => shared.set_state("idle"),
                            _ => {}
                        }
                        if let Some(j) = event_json(&ev) {
                            shared.publish(j).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        shared
                            .publish(
                                json!({"kind":"warn","text":format!("event lag: dropped {n}")}),
                            )
                            .await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let addr = SocketAddr::new(ip, port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    eprintln!("crab serve v{VERSION}");
    eprintln!("  workspace: {}", shared.workspace);
    eprintln!("  url:       http://{addr}/?token={token}");
    eprintln!("  healthz:   http://{addr}/healthz");
    eprintln!(
        "  note: P0 has no TLS and no approval channel (gate=yolo) — trusted LAN/tailnet only"
    );

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
                "workspace": shared.workspace,
                "state": {"run_state": shared.state()},
                "note": "P0: no TLS, no approval channel (gate=yolo)",
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
                let mut agent = shared.agent.lock().await;
                let _ = agent.run(text).await;
            });
            Some(json!({"type":"response","ok":true,"data":{"disposition":"started"}}))
        }
        "abort" => {
            shared.agent.lock().await.abort();
            Some(json!({"type":"response","ok":true}))
        }
        _ => Some(err(format!(
            "unknown command {ty:?} (P0 supports: status, attach, prompt, abort)"
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
}
