use crate::{
    herdr::Core,
    install::agent_directory,
    media,
    model::{Durable, PROTOCOL, RemoteTerminal, Session},
    outbox::{Delivery, Outbox},
    relay::{self, RemoteRelay},
    sessions::import_known_sessions,
    store::Store,
};
use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify, RwLock, mpsc, oneshot};

pub type Routes = Arc<Mutex<Durable>>;
struct Peer {
    session: Session,
    sender: mpsc::Sender<Value>,
    ready: bool,
}
struct Pending {
    session: String,
    generation: String,
    reply: oneshot::Sender<Result<Value>>,
}
#[derive(Clone)]
pub struct Hub {
    peers: Arc<RwLock<HashMap<String, Peer>>>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    registrations: Arc<Notify>,
    pub routes: Routes,
    store: Store,
    outbox: Outbox,
    events: mpsc::Sender<BridgeEvent>,
    token: Arc<String>,
    relay_key: Arc<Mutex<Option<zeroize::Zeroizing<[u8; 32]>>>>,
    core: Arc<RwLock<Option<Core>>>,
}
pub enum BridgeEvent {
    Registered(Session),
    Disconnected(String, String),
    Interaction {
        session: String,
        generation: String,
        message: Value,
    },
}
impl Hub {
    pub async fn start(
        store: Store,
        outbox: Outbox,
        events: mpsc::Sender<BridgeEvent>,
    ) -> Result<Self> {
        let mut bytes = [0; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut routes = store.durable()?;
        if !routes.recent_sessions_initialized && routes.recent_sessions.is_empty() {
            if let Some(dirs) = directories::BaseDirs::new() {
                let agent = agent_directory(dirs.home_dir());
                routes = tokio::task::spawn_blocking(move || {
                    routes.recent_sessions_initialized =
                        import_known_sessions(&agent, &mut routes).is_ok();
                    routes
                })
                .await?;
            }
        }
        let hub = Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            registrations: Arc::new(Notify::new()),
            routes: Arc::new(Mutex::new(routes)),
            store,
            outbox,
            events,
            token: Arc::new(token),
            relay_key: Arc::new(Mutex::new(None)),
            core: Arc::new(RwLock::new(None)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        hub.store.save("bridge.json", &json!({"protocol":PROTOCOL,"pid":std::process::id(),"port":listener.local_addr()?.port(),"token":*hub.token}))?;
        let app = Router::new()
            .route("/session", get(upgrade))
            .with_state(hub.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(hub)
    }
    pub async fn sessions(&self) -> Vec<Session> {
        let mut sessions: Vec<_> = self
            .peers
            .read()
            .await
            .values()
            .filter(|p| p.ready)
            .map(|p| p.session.clone())
            .collect();
        sessions.sort_by_key(|s| s.slot.trim_start_matches('s').parse::<u64>().unwrap_or(0));
        sessions
    }
    pub async fn session(&self, id: &str) -> Option<Session> {
        self.peers
            .read()
            .await
            .get(id)
            .filter(|p| p.ready)
            .map(|p| p.session.clone())
    }
    pub(crate) async fn wait_for_reload(&self, previous: &Session, token: &str) -> Option<Session> {
        let wait = async {
            loop {
                let registered = self.registrations.notified();
                tokio::pin!(registered);
                registered.as_mut().enable();
                if let Some(session) = self.session(&previous.id).await.filter(|s| {
                    s.generation != previous.generation && s.reload_token.as_deref() == Some(token)
                }) {
                    return session;
                }
                registered.await;
            }
        };
        tokio::time::timeout(Duration::from_secs(15), wait)
            .await
            .ok()
    }
    pub async fn action(&self, session: &Session, mut message: Value) -> Result<Value> {
        if session
            .remote
            .as_ref()
            .is_some_and(|remote| !self.outbox.matches_scope(&remote.scope))
        {
            bail!("QQ 绑定已变化，请刷新 Sessions");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let peers = self.peers.read().await;
        let sender = peers
            .get(&session.id)
            .filter(|p| p.ready && p.session.generation == session.generation)
            .context("原 Pi 已离线或重新连接，请刷新 Sessions")?
            .sender
            .clone();
        drop(peers);
        let (reply, receive) = oneshot::channel();
        self.pending.lock().await.insert(
            id.clone(),
            Pending {
                session: session.id.clone(),
                generation: session.generation.clone(),
                reply,
            },
        );
        message["type"] = json!("action");
        message["requestId"] = json!(id);
        if sender.send(message).await.is_err() {
            self.pending.lock().await.remove(&id);
            bail!("Pi 已断开，操作未发送");
        }
        let result = tokio::time::timeout(Duration::from_secs(12), receive).await;
        self.pending.lock().await.remove(&id);
        result
            .context("Pi 未确认操作，请查看终端或状态后再重试")?
            .context("Pi 连接已关闭，操作结果未确认")?
    }
    pub async fn save_routes(&self) -> Result<()> {
        self.store.save("routing.json", &*self.routes.lock().await)
    }
    pub fn supervise_remote(&self, core: Arc<RwLock<Option<Core>>>) {
        relay::RemoteSessions::start(self.clone(), core);
    }
    pub(crate) async fn use_core(&self, core: Core) {
        *self.core.write().await = Some(core);
    }
    pub(crate) fn matches_scope(&self, scope: &str) -> bool {
        self.outbox.matches_scope(scope)
    }
    pub(crate) async fn relay_master_key(&self) -> Result<zeroize::Zeroizing<[u8; 32]>> {
        let mut key = self.relay_key.lock().await;
        if key.is_none() {
            *key = Some(relay::load_key(&self.store)?);
        }
        Ok(key.as_ref().unwrap().clone())
    }
    pub(crate) async fn relay_key(&self, id: &str) -> Result<zeroize::Zeroizing<[u8; 32]>> {
        let key = self.relay_master_key().await?;
        Ok(zeroize::Zeroizing::new(relay::key_for(&key, id)))
    }
    pub(crate) async fn relay(&self, remote: &RemoteTerminal) -> Result<RemoteRelay> {
        let routes = self.routes.lock().await;
        if !self.outbox.matches_scope(&remote.scope) {
            bail!("QQ 绑定已变化");
        }
        routes
            .remote_relays
            .get(&remote.relay)
            .filter(|relay| {
                relay.workspace.core_id == remote.core_id
                    && relay.workspace.machine_id.as_deref() == Some(remote.machine_id.as_str())
                    && relay.terminal.as_deref() == Some(remote.terminal.as_str())
            })
            .cloned()
            .context("远程 Pi 连接已变化，请刷新 Sessions")
    }
    pub(crate) async fn register_remote(
        &self,
        message: &Value,
        sender: mpsc::Sender<Value>,
        remote: RemoteTerminal,
    ) -> Result<Session> {
        self.register(message, sender, Some(remote)).await
    }
    async fn register(
        &self,
        message: &Value,
        sender: mpsc::Sender<Value>,
        remote: Option<RemoteTerminal>,
    ) -> Result<Session> {
        if remote.is_none()
            && (message["protocol"] != PROTOCOL
                || message["token"].as_str() != Some(self.token.as_str()))
        {
            bail!("客户端认证失败");
        }
        let mut session: Session = serde_json::from_value(message.clone())?;
        if session.id.is_empty()
            || session.id.len() > 200
            || session.session_id.is_empty()
            || session.session_id.contains('/')
            || session.pid == 0
            || session.id != format!("{}:{}", session.session_id, session.pid)
        {
            bail!("Pi 注册身份无效");
        }
        if session.cwd.len() > 4096
            || session.label.len() > 1000
            || session.terminal.as_ref().is_some_and(|s| s.len() > 100)
        {
            bail!("Pi 注册信息过长");
        }
        if let Some(remote) = &remote {
            session.id = format!("{}/{}", remote.machine_id, session.id);
            session.terminal = Some(remote.address());
            session.terminal_socket = None;
            session.session_file = session
                .session_file
                .filter(|p| p.to_str().is_some_and(|s| s.starts_with('/')));
        } else {
            session.terminal = session.terminal.filter(|address| !address.contains('/'));
            session.session_file = session.session_file.filter(|path| path.is_absolute());
        }
        session.remote = remote;
        session.reload_token = session
            .reload_token
            .filter(|token| uuid::Uuid::parse_str(token).is_ok());
        session.generation = uuid::Uuid::new_v4().to_string();
        {
            let mut routes = self.routes.lock().await;
            if session.remote.as_ref().is_some_and(|remote| {
                !routes.remote_relays.contains_key(&remote.relay)
                    || !self.outbox.matches_scope(&remote.scope)
            }) {
                bail!("远程 Pi 连接已取消");
            }
            if !routes.slots.contains_key(&session.id) {
                routes.next_slot += 1;
                let slot = format!("s{}", routes.next_slot);
                routes.slots.insert(session.id.clone(), slot);
            }
            session.slot = routes.slots[&session.id].clone();
            routes.remember_session(&session, false);
            self.store.save("routing.json", &*routes)?;
        }
        self.peers.write().await.insert(
            session.id.clone(),
            Peer {
                session: session.clone(),
                sender,
                ready: false,
            },
        );
        Ok(session)
    }
    pub(crate) async fn registration_name(&self, session: &Session) -> Option<String> {
        self.routes.lock().await.names.get(&session.id).cloned()
    }
    pub(crate) async fn announce_registration(&self, session: &Session) {
        let mut peers = self.peers.write().await;
        let Some(peer) = peers
            .get_mut(&session.id)
            .filter(|peer| peer.session.generation == session.generation)
        else {
            return;
        };
        peer.ready = true;
        drop(peers);
        self.registrations.notify_waiters();
        let events = self.events.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let _ = events.send(BridgeEvent::Registered(session)).await;
        });
    }
    pub(crate) async fn handle_reply(&self, session: &Session, message: Value) -> Option<Value> {
        let request_id = message.get("requestId").cloned();
        let result = self.handle(session, message).await;
        let request_id = request_id?;
        match result {
            Ok(Some(mut value)) => {
                value["type"] = json!("result");
                value["requestId"] = request_id;
                value["ok"] = json!(true);
                Some(value)
            }
            Err(error) => Some(
                json!({"type":"result","requestId":request_id,"ok":false,"error":error.to_string()}),
            ),
            Ok(None) => None,
        }
    }
    pub(crate) async fn update_remote(&self, session: &Session, meta: &Value) -> Result<()> {
        let current = self.session(&session.id).await.context("Pi 已断开")?;
        let mut value = json!({"type":"update"});
        if let Some(path) = meta["sessionFile"].as_str() {
            if current.session_file.as_deref() != Some(std::path::Path::new(path)) {
                value["sessionFile"] = json!(path);
            }
        }
        if meta["state"].as_str() != Some(current.state.as_str()) {
            value["state"] = meta["state"].clone();
        }
        if meta["label"].as_str() != Some(current.label.as_str()) {
            value["label"] = meta["label"].clone();
            value["name"] = meta["name"].clone();
        }
        if meta["runtime"] != current.runtime {
            value["runtime"] = meta["runtime"].clone();
        }
        self.handle(session, value).await?;
        Ok(())
    }
    async fn handle(&self, session: &Session, message: Value) -> Result<Option<Value>> {
        if !self
            .peers
            .read()
            .await
            .get(&session.id)
            .is_some_and(|p| p.ready && p.session.generation == session.generation)
        {
            bail!("Pi 连接已替换");
        }
        match message["type"].as_str().unwrap_or_default() {
            "update" => {
                let (recent, used_model) = {
                    let mut peers = self.peers.write().await;
                    let peer = peers
                        .get_mut(&session.id)
                        .filter(|p| p.session.generation == session.generation)
                        .context("Pi 连接已替换")?;
                    let started = peer.session.state == "idle" && message["state"] == "working";
                    let previous_model = message
                        .get("runtime")
                        .and_then(|_| peer.session.model_key());
                    let session_file = message["sessionFile"]
                        .as_str()
                        .map(std::path::PathBuf::from)
                        .filter(|path| {
                            if peer.session.remote.is_some() {
                                path.to_str().is_some_and(|s| s.starts_with('/'))
                            } else {
                                path.is_absolute()
                            }
                        });
                    let path_changed =
                        session_file.is_some() && peer.session.session_file != session_file;
                    if path_changed {
                        peer.session.session_file = session_file;
                    }
                    if let Some(label) = message["label"].as_str() {
                        peer.session.label = label.chars().take(140).collect();
                    }
                    if let Some(state) = message["state"].as_str() {
                        peer.session.state = state.chars().take(50).collect();
                    }
                    if let Some(runtime) = message.get("runtime") {
                        peer.session.runtime = runtime.clone();
                    }
                    let used_model = if started || message.get("runtime").is_some() {
                        peer.session
                            .model_key()
                            .filter(|current| started || Some(current) != previous_model.as_ref())
                    } else {
                        None
                    };
                    let recent = (started || path_changed || message["label"].is_string())
                        .then(|| (peer.session.clone(), started));
                    (recent, used_model)
                };
                let name = message["name"].as_str();
                if recent.is_some() || name.is_some() || used_model.is_some() {
                    let mut routes = self.routes.lock().await;
                    if let Some((current, started)) = recent {
                        routes.remember_session(&current, started);
                    }
                    if let Some(name) = name {
                        if name.is_empty() {
                            routes.names.remove(&session.id);
                        } else {
                            routes
                                .names
                                .insert(session.id.clone(), name.chars().take(70).collect());
                        }
                    }
                    if let Some(model) = used_model {
                        routes.record_model(model);
                    }
                    self.store.save("routing.json", &*routes)?;
                }
                Ok(None)
            }
            "action_result" => {
                let request = message["requestId"].as_str().unwrap_or_default();
                let mut pending = self.pending.lock().await;
                if pending
                    .get(request)
                    .is_some_and(|p| p.session == session.id && p.generation == session.generation)
                {
                    let p = pending.remove(request).unwrap();
                    let result = if message["ok"] == true {
                        Ok(message)
                    } else {
                        Err(anyhow!(
                            "{}",
                            message["reason"].as_str().unwrap_or("Pi 拒绝操作")
                        ))
                    };
                    let _ = p.reply.send(result);
                }
                Ok(None)
            }
            "notify" => {
                let mut attachment = None;
                let scope = session.remote.as_ref().map(|remote| remote.scope.as_str());
                for (key, kind) in [
                    ("imagePath", "image"),
                    ("videoPath", "video"),
                    ("filePath", "file"),
                ] {
                    if let Some(path) = message[key].as_str() {
                        if attachment.is_some() {
                            bail!("每次只发送一个附件");
                        }
                        let staged = if session.remote.is_some() {
                            let core = self
                                .core
                                .read()
                                .await
                                .clone()
                                .context("Herdr 正在连接，请稍后重试")?;
                            tokio::time::timeout(
                                Duration::from_secs(120),
                                relay::get_attachment(&core, session, path, kind, self),
                            )
                            .await
                            .context("远程附件传输超时")??
                        } else {
                            let path = std::path::Path::new(path);
                            if !path.is_absolute() {
                                bail!("附件路径必须是绝对路径");
                            }
                            media::stage(path, kind).await?
                        };
                        attachment = Some(staged);
                    }
                }
                let raw = message["text"].as_str().unwrap_or_default();
                if raw.chars().count() > 19000 {
                    bail!("消息超过 19000 字符限制");
                }
                let current = self
                    .session(&session.id)
                    .await
                    .filter(|s| s.generation == session.generation)
                    .context("Pi 连接已替换")?;
                let text = format!(
                    "[{} · {}]\n{}",
                    current.slot,
                    crate::model::clean(&current.label, 100),
                    raw
                );
                let receipt = self
                    .outbox
                    .enqueue(
                        Delivery::Notification { scope },
                        &text,
                        Some(&session.id),
                        None,
                        false,
                        attachment,
                    )
                    .await?;
                Ok(Some(receipt))
            }
            "status" => Ok(Some(
                json!({"sessions":self.sessions().await.len(),"delivery":self.outbox.summary().await}),
            )),
            "question_open" | "question_close" | "permission_open" | "permission_close" => {
                self.events
                    .send(BridgeEvent::Interaction {
                        session: session.id.clone(),
                        generation: session.generation.clone(),
                        message,
                    })
                    .await
                    .map_err(|_| anyhow!("客户端已关闭"))?;
                Ok(None)
            }
            _ => bail!("不支持的本机协议请求"),
        }
    }
    pub(crate) async fn disconnect_relay(&self, relay: &str) {
        let sessions: Vec<_> = self
            .peers
            .read()
            .await
            .values()
            .filter(|p| {
                p.session
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.relay == relay)
            })
            .map(|p| p.session.clone())
            .collect();
        for session in sessions {
            self.disconnect(&session).await;
        }
    }
    pub(crate) async fn disconnect(&self, session: &Session) {
        let mut peers = self.peers.write().await;
        if peers
            .get(&session.id)
            .is_some_and(|p| p.session.generation == session.generation)
        {
            peers.remove(&session.id);
        }
        drop(peers);
        let mut pending = self.pending.lock().await;
        let ids: Vec<_> = pending
            .iter()
            .filter(|(_, p)| p.session == session.id && p.generation == session.generation)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(p) = pending.remove(&id) {
                let _ = p.reply.send(Err(anyhow!("Pi 连接已关闭，操作结果未确认")));
            }
        }
        let _ = self
            .events
            .send(BridgeEvent::Disconnected(
                session.id.clone(),
                session.generation.clone(),
            ))
            .await;
    }
}
async fn upgrade(
    State(hub): State<Hub>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if headers.contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.max_message_size(65_536)
        .max_frame_size(65_536)
        .on_upgrade(move |socket| serve(hub, socket))
        .into_response()
}
async fn serve(hub: Hub, mut socket: WebSocket) {
    let registration = tokio::time::timeout(Duration::from_secs(6), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = registration else {
        return;
    };
    let Ok(message) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    if message["type"] != "register" {
        return;
    }
    let (tx, mut rx) = mpsc::channel(64);
    let Ok(session) = hub.register(&message, tx, None).await else {
        return;
    };
    let name = hub.registration_name(&session).await;
    if socket.send(Message::Text(json!({"type":"result","requestId":message["requestId"],"ok":true,"slot":session.slot,"name":name,"generation":session.generation,"protocol":PROTOCOL}).to_string().into())).await.is_err() { hub.disconnect(&session).await; return; }
    hub.announce_registration(&session).await;
    let (mut write, mut read) = socket.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut alive = true;
    loop {
        tokio::select! {
            message = rx.recv() => {
                let Some(message) = message else { break; };
                if write.send(Message::Text(message.to_string().into())).await.is_err() { break; }
            },
            _ = heartbeat.tick() => {
                if !alive { break; } alive = false;
                if write.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            },
            message = read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        alive = true;
                        let Ok(message) = serde_json::from_str::<Value>(&text) else { break; };
                        if let Some(reply) = hub.handle_reply(&session, message).await {
                            if write.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
                        }
                    },
                    Some(Ok(Message::Pong(_))) => alive = true,
                    Some(Ok(Message::Ping(p))) => { alive = true; if write.send(Message::Pong(p)).await.is_err() { break; } },
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break, _ => {},
                }
            }
        }
    }
    hub.disconnect(&session).await;
}
