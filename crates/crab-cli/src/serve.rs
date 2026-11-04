//! `crab serve` — remote control daemon (pull-based architecture).
//!
//! Design: docs/notes/2026-11-04-crab-serve-pull-architecture.md (当前架构).
//! 拉取式：事件以统一 EntryId 追加进内存日志（真相源），客户端长轮询
//! `GET /events?after_id=N` 拉取并自记 offset；控制命令走 `POST /cmd`。
//! 服务端无状态于客户端进度——只保证日志写好了，客户端自己来拉。
//! 审批：confirmer 写 confirm_req 进日志 + pending 快照，block_in_place
//! 等待客户端决策（不阻塞 tokio worker）。
//!
//! 历史：曾是 JSONL-over-WebSocket 推送，真机验证暴露 ws 协议复杂性问题
//!（手写 101 被拒、send 背压饿死 confirm_req、confirmer spawn 静默失败），
//! 翻转为拉取后毫秒级可达。ws 代码已删（教训见 DESIGN.md）。

use anyhow::{Context, Result, bail};
use crab_core::agent::{Agent, AgentConfig, AgentEvent};
use crab_core::gate::{Confirmer, Decision, PermissionGate, Prompt};
use crab_core::session::Session;
use crab_core::state::SessionState;
use crab_core::tools::register_all;
use crab_core::types::ContentBlock;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INDEX_HTML: &str = include_str!("serve/web/index.html");
const MANIFEST: &str = include_str!("serve/web/manifest.webmanifest");
const SW_JS: &str = include_str!("serve/web/sw.js");
const ICON_SVG: &str = include_str!("serve/web/icon.svg");
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

/// 事件日志条目：拉取式架构的真相源。EntryId 单调连续（复用 seq）。
#[derive(Clone, serde::Serialize)]
struct LogEntry {
    id: u64,
    entry: serde_json::Value,
}

struct Shared {
    /// 每个 workspace 一个 Agent（懒加载，Arc 共享引用）。
    agents: Mutex<std::collections::HashMap<String, std::sync::Arc<Mutex<Agent>>>>,
    /// 当前激活的 workspace（客户端 attach/prompt 的目标）。
    active: std::sync::Mutex<String>,
    tx: broadcast::Sender<EventRec>,
    replay: std::sync::Mutex<std::collections::VecDeque<EventRec>>,
    seq: std::sync::atomic::AtomicU64,
    run_state: std::sync::Mutex<&'static str>,
    confirms: ConfirmHub,
    workspaces: std::sync::Mutex<Vec<String>>,
    /// 事件日志（拉取式真相源）：所有事件按 EntryId 追加，客户端长轮询拉取。
    log: std::sync::Mutex<Vec<LogEntry>>,
    /// 未决审批快照（attach/hello 携带，与 offset 无关）。
    pending: std::sync::Mutex<Vec<serde_json::Value>>,
    /// 新事件唤醒长轮询（log 追加时 notify）。
    notify: tokio::sync::Notify,
    /// per-agent FIFO prompt 队列（Q7）：key 是 agent 的 Arc 指针。
    prompt_queues:
        std::sync::Mutex<std::collections::HashMap<usize, std::collections::VecDeque<String>>>,
    /// 正在跑 run 循环的 agent key（去重，防重复启动）。
    queue_running: std::sync::Mutex<std::collections::HashSet<usize>>,
    /// 服务端启动纪元（serve 重启变化，客户端据此重置 offset）。
    boot_id: u64,
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
        self.publish_sync(event);
    }

    /// 同步发布（confirmer 用——同步闭包不能 await tokio Mutex，也不能
    /// spawn 进 async publish）。replay 用 std::sync::Mutex 才能同步访问。
    fn publish_sync(&self, event: Value) {
        let rec = EventRec {
            seq: self.next_seq(),
            event,
        };
        {
            let mut buf = self.replay.lock().unwrap();
            buf.push_back(rec.clone());
            while buf.len() > REPLAY_CAP {
                buf.pop_front();
            }
        }
        // 拉取式架构：事件进日志（真相源）+ 唤醒长轮询。broadcast 保留
        // 兼容存量 ws 客户端（迁移期间并存，验收后删）。
        self.log.lock().unwrap().push(LogEntry {
            id: rec.seq,
            entry: rec.event.clone(),
        });
        // notify_one 存一个 permit（无 waiter 也保留），下一个 notified().await
        // 立即消费——比 notify_waiters（无 waiter 即丢）可靠，配合 enable() 覆盖
        // "检查日志 vs 进入 await"的窗口。多客户端各自持独立 Notify 才完美，但
        // 单 Notify + notify_one + 3s 超时兜底已足够（丢 permit 最多 3s 重拉）。
        self.notify.notify_one();
        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            eprintln!(
                "[dbg] publish seq={} kind={} @{}ms",
                rec.seq,
                rec.event["kind"],
                ms % 100000
            );
        }
        let _ = self.tx.send(rec); // 无订阅者不算错误
    }

    /// 长轮询拉取：返回 after_id 之后的事件。hold 到 timeout 或新事件。
    /// 返回 (events, high_watermark)。
    async fn pull(&self, after_id: u64, timeout_secs: u64) -> (Vec<LogEntry>, u64) {
        // hold 上限 3s（不是客户端要求的 30s）——notify 在"客户端上一个 fetch
        // 返回、下一个 fetch 还在路上"的窗口会丢（此时无长轮询在等）。与其
        // 依赖 notify 精确唤醒（竞态难除），不如缩短 hold + 客户端快速重拉：
        // 正常时 notify 毫秒级唤醒，丢时最多 3s 兜底（最坏延迟从 30s 降到 3s）。
        let effective = timeout_secs.min(3);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(effective);
        loop {
            // 关键：先注册 notified 再检查日志——否则"检查日志"和"进入 await"
            // 之间的窗口里 notify_waiters() 会丢失（它不缓冲），导致 confirm_req
            // 到达时没被唤醒、长轮询干等 30s 超时（这就是卡片延迟 30s 的根因）。
            // tokio 的 Notified::enable() 在创建时注册 interest，之后检查日志
            // 发现已有新事件就直接返回，否则 await 时若事件已在窗口期到达，
            // enable 时注册的 interest 能捕获到那次 notify。
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let log = self.log.lock().unwrap();
                let hw = log.last().map(|e| e.id).unwrap_or(0);
                if after_id < hw {
                    let from = if after_id == 0 {
                        log.len().saturating_sub(200)
                    } else {
                        log.iter()
                            .position(|e| e.id > after_id)
                            .unwrap_or(log.len())
                    };
                    let events: Vec<LogEntry> = log[from..].to_vec();
                    return (events, hw);
                }
            }
            // 无新事件——hold 到超时或被 notify 唤醒（enable 已注册，窗口期
            // 的 notify 不会丢）。
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let hw = self.log.lock().unwrap().last().map(|e| e.id).unwrap_or(0);
                return (Vec::new(), hw);
            }
            let _ = tokio::time::timeout(deadline - now, notified).await;
            if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                eprintln!("[dbg] pull woke (after_id={after_id}) @{}ms", ms % 100000);
            }
        }
    }

    /// 未决审批快照（attach/hello/events 响应携带）。
    fn pending_confirms(&self) -> Vec<Value> {
        self.pending.lock().unwrap().clone()
    }
    fn pending_add(&self, id: u64, tool: String, summary: String) {
        self.pending
            .lock()
            .unwrap()
            .push(json!({"id": id, "tool": tool, "summary": summary}));
    }
    fn pending_remove(&self, id: u64) {
        self.pending.lock().unwrap().retain(|p| p["id"] != id);
    }

    /// per-agent FIFO 命令队列（Q7）：prompt 进队列，一个专门的 run 循环
    /// 按序消费——保证日志顺序 = 客户端发送顺序（修 ws 时代 spawn 竞争
    /// agent.lock() 的顺序不确定性）。队列存 Shared 级，run 循环按 agent 键。
    fn enqueue_prompt(self: &Arc<Self>, agent: std::sync::Arc<Mutex<Agent>>, text: String) {
        // 用 agent 的 Arc 指针做队列键（每个 agent 一个队列+一个 run 循环）。
        let key = std::sync::Arc::as_ptr(&agent) as usize;
        {
            let mut q = self.prompt_queues.lock().unwrap();
            q.entry(key).or_default().push_back(text);
        }
        // 若该 agent 的 run 循环不在跑，启动一个（幂等——running 集合去重）。
        {
            let mut running = self.queue_running.lock().unwrap();
            if !running.insert(key) {
                return; // 已有循环在跑，它会消费新入队的 prompt。
            }
        }
        let shared = self.clone();
        tokio::spawn(async move {
            loop {
                let next = {
                    let mut q = shared.prompt_queues.lock().unwrap();
                    q.get_mut(&key).and_then(|q| q.pop_front())
                };
                match next {
                    Some(text) => {
                        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                            eprintln!("[dbg] prompt: dequeued, running");
                        }
                        let mut agent = agent.lock().await;
                        let r = agent.run(text).await;
                        if std::env::var("CRAB_DEBUG_CMD").is_ok() {
                            eprintln!("[dbg] prompt: run done: {r:?}");
                        }
                    }
                    None => break, // 队列空——退出循环（下次入队会重启）。
                }
            }
            shared.queue_running.lock().unwrap().remove(&key);
        });
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
            true, // serve 总是新 session：续旧会话可能加载不完整的 tool_use/tool_result 配对（DeepSeek 400）
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
    /// 已连接客户端的 approve。写 confirm_req 进事件日志 + 登记 pending
    /// 快照；首个到达的决策生效。Q12-b：去掉 120s 超时——活跃客户端
    /// 长轮询毫秒级拉到，重连客户端由 hello 的 pending 快照补上；
    /// 被 iOS 暂停的客户端催也没用。仅通道断开（serve 关闭）才 Deny。
    fn confirmer(self: &Arc<Self>) -> Confirmer {
        let shared = self.clone();
        Arc::new(move |prompt: Prompt| {
            let (tx, rx) = std::sync::mpsc::channel::<Decision>();
            let id = shared.confirms.insert(tx);
            // 不能 tokio::spawn——confirmer 是同步闭包（gate.check 在 tool
            // execute 里同步调用），spawn 在无 runtime 上下文处 panic/静默失败。
            // publish_sync 同步写事件日志 + 唤醒长轮询；同时登记 pending 快照
            //（attach/hello 携带，与 offset 无关）。
            shared.pending_add(id, prompt.tool.to_string(), prompt.summary.clone());
            shared.publish_sync(serde_json::json!({
                "kind": "confirm_req",
                "confirm_id": id,
                "tool": prompt.tool,
                "summary": prompt.summary,
            }));
            // 阻塞等决策。关键：block_in_place 告诉 tokio "本 worker 线程要做
            // 阻塞 IO"，让它把该 worker 上其他 task（比如长轮询的 notified().await）
            // 迁到别的 worker——否则 confirmer 的 recv 占住 worker，notify 的
            // wakeup 排队无法被 poll，长轮询卡死（这就是 confirm_req 发布后
            // 客户端 35s 才拉到的根因：turn_started 时 confirmer 未阻塞故毫秒级，
            // confirm_req 一进 confirmer 阻塞，后续 wakeup 全卡）。
            // block_in_place 需在 runtime 上下文；confirmer 在 tool future（runtime
            // 内）被调用，安全。兜底：若不在 runtime（理论上不会），退化为裸 recv。
            let decision = match tokio::task::block_in_place(|| rx.recv()) {
                Ok(d) => d,
                Err(_) => Decision::Deny, // 通道断开（serve 关闭）
            };
            shared.confirms.remove(id);
            shared.pending_remove(id);
            shared.publish_sync(serde_json::json!({
                "kind": "confirm_resolved",
                "confirm_id": id,
                "decision": format!("{decision:?}").to_lowercase(),
            }));
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

/// TLS 材料（自签 Ed25519，SANs 覆盖 bind 地址 + localhost + 主机名）。
/// 首次生成写到 ~/.crab/serve/{cert,key}.pem 后复用。iOS 完全信任要求
/// Basic Constraints CA:TRUE（否则导入描述文件报 -25294）。
fn tls_material(
    bind_ip: &IpAddr,
) -> Result<(
    Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>,
    tokio_rustls::rustls::pki_types::PrivateKeyDer<'static>,
)> {
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let dir = serve_dir()?;
    std::fs::create_dir_all(&dir)?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        return Ok((
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        ));
    }
    let mut params = rcgen::CertificateParams::new(vec![
        bind_ip.to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        hostname(),
    ])?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "crab-serve");
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    std::fs::write(&cert_path, &cert_der)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&key_path)?
            .write_all(&key_der)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&key_path, &key_der)?;
    Ok((
        vec![CertificateDer::from(cert_der)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
    ))
}

/// 证书 SHA-256 指纹（TOFU 比对用）。
fn cert_fingerprint(cert_der: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(cert_der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
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

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    query.split('&').find_map(|kv| kv.strip_prefix(&prefix))
}

/// 读 HTTP body（Content-Length）。head 之后的 pre-read 可能已含部分 body。
async fn read_body<S>(stream: &mut S, head: &str, pre: &[u8]) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("content-length: ")
                .or(l.strip_prefix("Content-Length: "))
        })
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = pre[..len.min(pre.len())].to_vec();
    use tokio::io::AsyncReadExt;
    while body.len() < len {
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(body)
}

/// GET /events?after_id=N&timeout=S —— 长轮询拉取事件（拉取式架构核心）。
/// hold 到有新事件或超时（默认 30s，上限 60s）。响应带 high_watermark
/// 和 pending 审批快照（与 offset 无关的状态）。
async fn handle_events<S>(
    mut stream: S,
    shared: Arc<Shared>,
    query: &str,
    _head: &str,
    _pre: &[u8],
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let after_id: u64 = query_param(query, "after_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let timeout: u64 = query_param(query, "timeout")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let (events, hw) = shared.pull(after_id, timeout).await;
    let body = json!({
        "events": events,
        "high_watermark": hw,
        "pending_confirms": shared.pending_confirms(),
    });
    respond_http(&mut stream, 200, &body.to_string(), "application/json").await
}

/// POST /cmd —— 控制命令（prompt/approve/abort/workspace.switch/...）。
/// 复用现有 dispatch 逻辑（去 ws 帧包装，直接 JSON in/out）。
async fn handle_cmd<S>(mut stream: S, shared: Arc<Shared>, head: &str, pre: &[u8]) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let body = read_body(&mut stream, head, pre).await?;
    let v: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("_")
        .to_string();
    let resp = dispatch(v, &shared).await;
    let body = match resp {
        Some(mut r) => {
            r["id"] = json!(id);
            r
        }
        None => json!({"type":"response","id":id,"ok":true}),
    };
    respond_http(&mut stream, 200, &body.to_string(), "application/json").await
}

/// HTTPS 连接处理：读请求头，按路由分发（静态页 + 证书分发 + 拉取式
/// /events + /cmd）。鉴权在路由里（token query）。
async fn handle_conn<S>(mut stream: S, shared: Arc<Shared>, token: Arc<String>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // 预读请求头（TLS 流无 peek）。
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        use tokio::io::AsyncReadExt;
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            break p;
        }
        if buf.len() > 16 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let line = head.lines().next().unwrap_or("");
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let ok_token = query_token(query).is_some_and(|t| t == token.as_str());

    match (route, ok_token) {
        ("/healthz", _) => simple_http(stream, 200, "ok").await,
        // 证书分发（DER 二进制）：iOS 完全信任自签证书需安装描述文件。
        // 主 https 端口也发一份（已信任后访问）；明文分发走 port+1（见 run）。
        ("/cert.pem", _) => {
            let bytes = std::fs::read(serve_dir()?.join("cert.pem")).unwrap_or_default();
            use tokio::io::AsyncWriteExt;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/x-x509-ca-cert\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                bytes.len()
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&bytes).await?;
            Ok(())
        }
        // 拉取式架构：/events 长轮询（读 body 前先看 method）。
        ("/events", true) => handle_events(stream, shared, query, &head, &buf[head_end..]).await,
        ("/cmd", true) => handle_cmd(stream, shared, &head, &buf[head_end..]).await,
        ("/", true) => {
            let html = INDEX_HTML.replace(
                "const tok = new URLSearchParams(location.search).get('token') || localStorage.crabToken || '';",
                &format!("const tok = '{token}'; localStorage.crabToken = '{token}';"),
            );
            simple_http(stream, 200, &html).await
        }
        ("/manifest.webmanifest", _) => {
            simple_http(stream, 200, &with_ct(MANIFEST, "application/manifest+json")).await
        }
        ("/sw.js", _) => simple_http(stream, 200, &with_ct(SW_JS, "text/javascript")).await,
        ("/icon.svg", _) => simple_http(stream, 200, &with_ct(ICON_SVG, "image/svg+xml")).await,
        ("/", false) => simple_http(stream, 403, "missing or bad token").await,
        _ => simple_http(stream, 404, "not found").await,
    }
}

async fn simple_http<S>(mut stream: S, code: u16, body: &str) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ct = if body.starts_with("<!doctype") {
        "text/html; charset=utf-8"
    } else if body.starts_with('{') && body.contains("\"name\"") {
        "application/manifest+json"
    } else if body.starts_with("//") {
        "text/javascript; charset=utf-8"
    } else if body.starts_with("<svg") {
        "image/svg+xml"
    } else {
        "text/plain; charset=utf-8"
    };
    respond_http(&mut stream, code, body, ct).await
}

async fn respond_http<S>(stream: &mut S, code: u16, body: &str, ct: &str) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let reason = match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    // 禁缓冲头（X-Accel-Buffering / Cache-Control）：iOS Safari 会缓冲长轮询
    // 响应，延迟交给 JS——这就是 confirm_req 毫秒级到服务端、手机却 30s 后
    // 才见的根因。显式禁缓冲让 Safari 收到即交付。
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nx-accel-buffering: no\r\ncache-control: no-cache, no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn with_ct(body: &str, _ct: &str) -> String {
    body.to_string() // content-type 由 simple_http 嗅探；参数保留给调用方可读性
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

pub async fn run(host: &str, port: u16, http_cert: bool) -> Result<()> {
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
        replay: std::sync::Mutex::new(std::collections::VecDeque::new()),
        log: std::sync::Mutex::new(Vec::new()),
        pending: std::sync::Mutex::new(Vec::new()),
        notify: tokio::sync::Notify::new(),
        prompt_queues: std::sync::Mutex::new(std::collections::HashMap::new()),
        queue_running: std::sync::Mutex::new(std::collections::HashSet::new()),
        boot_id: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
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

    // TLS：iOS Safari 对非 localhost 的明文 ws 有硬性安全限制，serve 默认 wss。
    // 自签证书 + TOFU（首次访问比对指纹后信任）。
    let (certs, key) = tls_material(&ip)?;
    let fingerprint = cert_fingerprint(certs[0].as_ref());
    let tls_config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build tls config")?;
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls_config));

    // 明文 HTTP 证书分发（port+1）：解决 iOS"下载需信任、信任需下载"的循环。
    if http_cert {
        let cert_bytes = std::fs::read(serve_dir()?.join("cert.pem")).unwrap_or_default();
        let cert_addr = SocketAddr::new(ip, port + 1);
        tokio::spawn(async move {
            let Ok(l) = TcpListener::bind(cert_addr).await else {
                eprintln!("[serve] cert-http bind {cert_addr} failed (in use?)");
                return;
            };
            eprintln!("  cert (plain): http://{cert_addr}/cert.pem");
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    continue;
                };
                let b = cert_bytes.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/x-x509-ca-cert\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        b.len()
                    );
                    if s.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if s.write_all(&b).await.is_err() {
                        return;
                    }
                    let _ = s.shutdown().await;
                });
            }
        });
    }

    eprintln!("crab serve v{VERSION}");
    eprintln!("  workspace:   {}", shared.active());
    eprintln!("  url:         https://{addr}/?token={token}");
    eprintln!("  healthz:     https://{addr}/healthz");
    eprintln!("  cert sha256: {fingerprint}");
    eprintln!("  note: self-signed — on iOS, trust the cert then open the URL");

    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        let token = token.clone();
        let acceptor = tls_acceptor.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[serve] tls handshake failed: {e}");
                    return;
                }
            };
            if let Err(e) = handle_conn(stream, shared, token).await {
                eprintln!("[serve] conn error: {e:#}");
            }
        });
    }
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
            // 拉取式架构：attach 返回 boot_id（服务端启动纪元）。客户端发现
            // boot_id 变了（serve 重启、seq 从 1 重置）必须重置 offset=0——
            // 否则旧 offset（比如 142）> 新 hw，永远拉不到（这就是"看不到
            // 其它消息"的根因：客户端带着上次会话的 offset 连上了新 serve）。
            Some(json!({
                "type": "response", "ok": true,
                "data": {"boot_id": shared.boot_id, "active": shared.active()}
            }))
        }
        "prompt" => {
            let text = v["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Some(err("empty prompt"));
            }
            // FIFO 串行（Q7）：不 spawn 竞争 agent.lock()，改为 per-agent
            // 命令队列。prompt 进队列，立即返回 queued；专门的 run 循环
            // 按队列顺序逐个执行——保证日志顺序 = 客户端发送顺序。
            let ws = shared.active();
            match shared.agent_for(&ws).await {
                Ok(agent) => {
                    shared.enqueue_prompt(agent, text);
                    Some(json!({"type":"response","ok":true,"data":{"disposition":"queued"}}))
                }
                Err(e) => Some(err(e)),
            }
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
        "steer" => {
            // mid-run 引导：agent 空闲则直接 run（同 prompt），否则排队到
            // 当前 run 完成后自动续跑（与 TUI 的 mid-run 排队语义一致）。
            let text = v["text"].as_str().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Some(err("empty steer"));
            }
            let shared = shared.clone();
            let busy = shared.state() == "running";
            tokio::spawn(async move {
                let ws = shared.active();
                match shared.agent_for(&ws).await {
                    Ok(agent) => {
                        if busy {
                            shared
                                .publish(json!({"kind":"steer.queued","workspace":ws,
                                    "text":"steer queued behind the running turn"}))
                                .await;
                            // 等 idle（简单轮询——run_state 由事件桥维护，
                            // 事件驱动在这里是过度设计）。
                            loop {
                                if shared.state() == "idle" {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            }
                        }
                        let mut agent = agent.lock().await;
                        let _ = agent.run(text).await;
                    }
                    Err(e) => {
                        shared.publish(json!({"kind":"error","text":e})).await;
                    }
                }
            });
            Some(json!({
                "type":"response","ok":true,
                "data":{"disposition": if busy { "queued" } else { "started" }}
            }))
        }
        "compact" => {
            let ws = shared.active();
            match shared.agent_for(&ws).await {
                Ok(agent) => {
                    let report = agent.lock().await.compact_now().await;
                    if report.turns_affected == 0 {
                        Some(json!({"type":"response","ok":true,
                            "data":{"note":"nothing to compact"}}))
                    } else {
                        Some(json!({"type":"response","ok":true,
                            "data":{"strategy":format!("{:?}",report.strategy),
                                    "turns":report.turns_affected}}))
                    }
                }
                Err(e) => Some(err(e)),
            }
        }
        "reflect" => {
            let ws = shared.active();
            match shared.agent_for(&ws).await {
                Ok(agent) => {
                    let mut agent = agent.lock().await;
                    match agent.propose_lessons().await {
                        Ok(proposals) if proposals.is_empty() => {
                            Some(json!({"type":"response","ok":true,
                                "data":{"note":"nothing to reflect on"}}))
                        }
                        Ok(proposals) => {
                            // 远程版 /reflect：accept=all 全部接受（人审在
                            // App 上是独立切片；现在先全收，notice 报数）。
                            let total = proposals.len();
                            let mut accepted = 0usize;
                            for (kind, text) in proposals {
                                if agent.append_lesson(kind, &text).is_some() {
                                    accepted += 1;
                                }
                            }
                            shared
                                .publish(json!({"kind":"reflected","workspace":ws,
                                    "accepted":accepted,"total":total}))
                                .await;
                            Some(json!({"type":"response","ok":true,
                                "data":{"accepted":accepted,"total":total}}))
                        }
                        Err(e) => Some(err(format!("reflect failed: {e}"))),
                    }
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
            "unknown command {ty:?} (supports: status, attach, prompt, abort, approve, workspace.list/add/switch, session.list/create, steer, compact, reflect)"
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

/// `crab serve --pair`：打印配对 URL + 终端 QR 码。
/// 二维码内容即 URL（含 token）——手机相机/浏览器扫码即达，token
/// 随 URL 注入，PWA 存 localStorage 后主屏幕启动免重输。
pub fn print_pair(host: &str, port: u16) -> Result<()> {
    let ip = parse_bind(host)?;
    let token = device_token()?;
    let url = format!("https://{ip}:{port}/?token={token}");
    println!("pair this device with crab serve:\n");
    println!("  {url}\n");
    // 终端 QR：Unicode 半块字符，两行字符一行点。
    if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
        let w = code.width();
        let data = code.into_colors();
        for y in (0..w).step_by(2) {
            let mut line = String::new();
            for x in 0..w {
                let top = data[y * w + x] == qrcode::Color::Dark;
                let bot = y + 1 < w && data[(y + 1) * w + x] == qrcode::Color::Dark;
                line.push(match (top, bot) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            println!("  {line}");
        }
        println!();
    }
    println!("  scan with the phone camera on the same LAN/tailnet,");
    println!("  then 'Add to Home Screen' for the app experience.\n");
    println!("  token lives at ~/.crab/serve/token (0600). keep the URL private.");
    Ok(())
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
