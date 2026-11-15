use crate::{
    media,
    model::{Durable, PROTOCOL, Session},
    outbox::Outbox,
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
        let hub = Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            registrations: Arc::new(Notify::new()),
            routes: Arc::new(Mutex::new(store.durable()?)),
            store,
            outbox,
            events,
            token: Arc::new(token),
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
            .map(|p| p.session.clone())
            .collect();
        sessions.sort_by_key(|s| s.slot.trim_start_matches('s').parse::<u64>().unwrap_or(0));
        sessions
    }
    pub async fn session(&self, id: &str) -> Option<Session> {
        self.peers.read().await.get(id).map(|p| p.session.clone())
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
        let id = uuid::Uuid::new_v4().to_string();
        let peers = self.peers.read().await;
        let sender = peers
            .get(&session.id)
            .filter(|p| p.session.generation == session.generation)
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
    async fn register(&self, message: &Value, sender: mpsc::Sender<Value>) -> Result<Session> {
        if message["protocol"] != PROTOCOL || message["token"].as_str() != Some(self.token.as_str())
        {
            bail!("客户端认证失败");
        }
        let mut session: Session = serde_json::from_value(message.clone())?;
        if session.id.is_empty()
            || session.id.len() > 200
            || session.session_id.is_empty()
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
        session.reload_token = session
            .reload_token
            .filter(|token| uuid::Uuid::parse_str(token).is_ok());
        session.generation = uuid::Uuid::new_v4().to_string();
        {
            let mut routes = self.routes.lock().await;
            if !routes.slots.contains_key(&session.id) {
                routes.next_slot += 1;
                let slot = format!("s{}", routes.next_slot);
                routes.slots.insert(session.id.clone(), slot);
            }
            session.slot = routes.slots[&session.id].clone();
            self.store.save("routing.json", &*routes)?;
        }
        self.peers.write().await.insert(
            session.id.clone(),
            Peer {
                session: session.clone(),
                sender,
            },
        );
        self.registrations.notify_waiters();
        Ok(session)
    }
    async fn handle(&self, session: &Session, message: Value) -> Result<Option<Value>> {
        if !self
            .peers
            .read()
            .await
            .get(&session.id)
            .is_some_and(|p| p.session.generation == session.generation)
        {
            bail!("Pi 连接已替换");
        }
        match message["type"].as_str().unwrap_or_default() {
            "update" => {
                let mut peers = self.peers.write().await;
                if let Some(peer) = peers.get_mut(&session.id) {
                    if let Some(label) = message["label"].as_str() {
                        peer.session.label = label.chars().take(140).collect();
                    }
                    if let Some(state) = message["state"].as_str() {
                        peer.session.state = state.chars().take(50).collect();
                    }
                    if let Some(runtime) = message.get("runtime") {
                        peer.session.runtime = runtime.clone();
                    }
                    if let Some(name) = message["name"].as_str() {
                        let mut routes = self.routes.lock().await;
                        if name.is_empty() {
                            routes.names.remove(&session.id);
                        } else {
                            routes
                                .names
                                .insert(session.id.clone(), name.chars().take(70).collect());
                        }
                        self.store.save("routing.json", &*routes)?;
                    }
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
                for (key, kind) in [
                    ("imagePath", "image"),
                    ("videoPath", "video"),
                    ("filePath", "file"),
                ] {
                    if let Some(path) = message[key].as_str() {
                        if attachment.is_some() {
                            bail!("每次只发送一个附件");
                        }
                        let path = std::path::Path::new(path);
                        if !path.is_absolute() {
                            bail!("附件路径必须是绝对路径");
                        }
                        attachment = Some(media::stage(path, kind).await?);
                    }
                }
                let raw = message["text"].as_str().unwrap_or_default();
                if raw.chars().count() > 19000 {
                    bail!("消息超过 19000 字符限制");
                }
                let current = self
                    .session(&session.id)
                    .await
                    .unwrap_or_else(|| session.clone());
                let text = format!(
                    "[{} · {}]\n{}",
                    current.slot,
                    crate::model::clean(&current.label, 100),
                    raw
                );
                let receipt = self
                    .outbox
                    .enqueue(None, &text, Some(&session.id), None, false, attachment)
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
    async fn disconnect(&self, session: &Session) {
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
    let Ok(session) = hub.register(&message, tx).await else {
        return;
    };
    let name = hub.routes.lock().await.names.get(&session.id).cloned();
    if socket.send(Message::Text(json!({"type":"result","requestId":message["requestId"],"ok":true,"slot":session.slot,"name":name,"generation":session.generation,"protocol":PROTOCOL}).to_string().into())).await.is_err() { hub.disconnect(&session).await; return; }
    let _ = hub
        .events
        .send(BridgeEvent::Registered(session.clone()))
        .await;
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
                        let request_id = message.get("requestId").cloned();
                        let result = hub.handle(&session,message).await;
                        if let Some(request_id) = request_id { if result.as_ref().is_ok_and(|r| r.is_none()) { continue; }
                            let reply = match result { Ok(Some(mut value)) => { value["type"] = json!("result"); value["requestId"] = request_id; value["ok"] = json!(true); value }, Err(e) => json!({"type":"result","requestId":request_id,"ok":false,"error":e.to_string()}), _ => continue };
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
