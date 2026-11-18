//! Independent implementation of QQ's public bot HTTPS/WebSocket protocols.
use crate::model::Credentials;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

pub fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent("pi-qq-herdr-client/0.1")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub struct BindTask {
    pub url: String,
    id: String,
    key: zeroize::Zeroizing<[u8; 32]>,
}
pub enum BindResult {
    Waiting,
    Confirming,
    Expired,
    Complete(Credentials),
}
impl BindTask {
    pub async fn create(http: &Client) -> Result<Self> {
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        rand::thread_rng().fill_bytes(key.as_mut());
        let result = bind_request(
            http,
            "create_bind_task",
            json!({"key":STANDARD.encode(key.as_ref())}),
        )
        .await?;
        let id = result["task_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("QQ 未返回授权任务，请重新扫码")?
            .to_string();
        let mut url = url::Url::parse("https://q.qq.com/qqbot/openclaw/connect.html")?;
        url.query_pairs_mut()
            .append_pair("task_id", &id)
            .append_pair("source", "third-party")
            .append_pair("_wv", "2");
        Ok(Self {
            url: url.into(),
            id,
            key,
        })
    }
    pub async fn poll(&self, http: &Client) -> Result<BindResult> {
        let value = bind_request(http, "poll_bind_result", json!({"task_id":self.id})).await?;
        match value["status"].as_u64() {
            Some(0) => Ok(BindResult::Waiting),
            Some(1) => Ok(BindResult::Confirming),
            Some(3) => Ok(BindResult::Expired),
            Some(2) => {
                let app_id = value["bot_appid"]
                    .as_str()
                    .filter(|s| s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty())
                    .context("QQ 返回的机器人标识无效")?;
                let ciphertext = STANDARD
                    .decode(
                        value["bot_encrypt_secret"]
                            .as_str()
                            .context("QQ 未返回授权密钥")?,
                    )
                    .map_err(|_| anyhow!("QQ 授权密钥格式无效"))?;
                if ciphertext.len() < 28 {
                    bail!("QQ 授权密钥不完整");
                }
                let cipher = Aes256Gcm::new_from_slice(self.key.as_ref())
                    .map_err(|_| anyhow!("无法解锁 QQ 授权"))?;
                let decrypted = zeroize::Zeroizing::new(
                    cipher
                        .decrypt(Nonce::from_slice(&ciphertext[..12]), &ciphertext[12..])
                        .map_err(|_| anyhow!("QQ 授权校验失败，请重新扫码"))?,
                );
                let secret = std::str::from_utf8(&decrypted).context("QQ 授权密钥无效")?;
                if secret.is_empty() {
                    bail!("QQ 授权密钥为空");
                }
                Ok(BindResult::Complete(Credentials {
                    app_id: app_id.into(),
                    app_secret: secret.into(),
                    owner: value["user_openid"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                }))
            }
            _ => bail!("QQ 授权状态未知，请刷新二维码"),
        }
    }
}
async fn bind_request(http: &Client, action: &str, body: Value) -> Result<Value> {
    let response = http
        .post(format!("https://q.qq.com/lite/{action}"))
        .timeout(Duration::from_secs(20))
        .json(&body)
        .send()
        .await
        .map_err(|_| anyhow!("无法连接 QQ 授权服务，请稍后重试"))?;
    if !response.status().is_success() {
        bail!("QQ 授权服务暂时不可用（{}）", response.status().as_u16());
    }
    let value: Value = response
        .json()
        .await
        .map_err(|_| anyhow!("QQ 授权响应无效"))?;
    if value["code"].as_i64().is_some_and(|c| c != 0)
        || value["retcode"].as_i64().is_some_and(|c| c != 0)
    {
        bail!("QQ 授权请求失败，请重新扫码");
    }
    Ok(value.get("data").cloned().unwrap_or(value))
}

struct Token {
    value: zeroize::Zeroizing<String>,
    until: Instant,
}
#[derive(Clone)]
pub struct Api {
    pub http: Client,
    credentials: Arc<Credentials>,
    token: Arc<Mutex<Option<Token>>>,
}
impl Api {
    pub fn new(http: Client, credentials: Credentials) -> Self {
        Self {
            http,
            credentials: Arc::new(credentials),
            token: Arc::new(Mutex::new(None)),
        }
    }
    async fn access_token(&self) -> Result<String> {
        let mut stored = self.token.lock().await;
        if let Some(token) = stored.as_ref().filter(|t| t.until > Instant::now()) {
            return Ok(token.value.to_string());
        }
        let response = self.http.post("https://bots.qq.com/app/getAppAccessToken").timeout(Duration::from_secs(20)).json(&json!({"appId":self.credentials.app_id,"clientSecret":self.credentials.app_secret})).send().await.map_err(|_| anyhow!("QQ 鉴权网络连接失败"))?;
        if !response.status().is_success() {
            bail!("QQ 鉴权失败，请重新绑定机器人");
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| anyhow!("QQ 鉴权响应无效"))?;
        let token = value["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("QQ 未接受机器人授权，请重新绑定")?
            .to_string();
        let expires = value["expires_in"]
            .as_u64()
            .or_else(|| value["expires_in"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(7200);
        *stored = Some(Token {
            value: zeroize::Zeroizing::new(token.clone()),
            until: Instant::now() + Duration::from_secs(expires.saturating_sub(60).max(1)),
        });
        Ok(token)
    }
    pub async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..2 {
            let token = zeroize::Zeroizing::new(self.access_token().await?);
            let mut request = self
                .http
                .request(method.clone(), format!("https://api.sgroup.qq.com{path}"))
                .header("Authorization", format!("QQBot {}", token.as_str()))
                .header("X-Union-Appid", &self.credentials.app_id);
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .map_err(|_| anyhow!("QQ 网络请求未确认，请检查发送状态"))?;
            if response.status().as_u16() == 401 && attempt == 0 {
                *self.token.lock().await = None;
                continue;
            }
            let status = response.status().as_u16();
            let value: Value = response
                .json()
                .await
                .map_err(|_| anyhow!("QQ 返回了无效响应（HTTP {status}）"))?;
            if status >= 400 || value["code"].as_i64().is_some_and(|c| c != 0) {
                bail!("QQ_API_{}_{}", status, value["code"].as_i64().unwrap_or(0));
            }
            return Ok(value);
        }
        bail!("QQ 授权已失效，请重新绑定")
    }
    pub async fn send(&self, owner: &str, body: Value) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(20),
            self.request(
                Method::POST,
                &format!("/v2/users/{}/messages", path_segment(owner)?),
                Some(body),
            ),
        )
        .await
        .context("QQ 消息发送未确认，请检查连接和发送状态")?
    }
    pub async fn upload(&self, owner: &str, body: Value) -> Result<Value> {
        self.request(
            Method::POST,
            &format!("/v2/users/{}/files", path_segment(owner)?),
            Some(body),
        )
        .await
    }
    pub async fn gateway(&self, events: mpsc::Sender<GatewayEvent>) {
        let mut session = String::new();
        let mut sequence: Option<u64> = None;
        let mut backoff = 2u64;
        loop {
            let result: Result<()> = async {
                let token = zeroize::Zeroizing::new(self.access_token().await?);
                let gateway = self.request(Method::GET, "/gateway", None).await?;
                let url = gateway["url"].as_str().filter(|u| u.starts_with("wss://")).context("QQ 网关地址不可用")?;
                let (mut socket, _) = tokio_tungstenite::connect_async(url).await.map_err(|_| anyhow!("QQ 网关连接失败"))?;
                let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
                heartbeat.tick().await;
                let mut waiting_ack = false;
                loop {
                    tokio::select! {
                        _ = heartbeat.tick() => {
                            if waiting_ack { bail!("QQ 网关心跳未确认"); }
                            socket.send(Message::Text(json!({"op":1,"d":sequence}).to_string().into())).await?;
                            waiting_ack = true;
                        },
                        message = socket.next() => {
                            let Some(message) = message else { bail!("QQ 已断开"); };
                            match message? {
                                Message::Text(text) => {
                                    let message: Value = serde_json::from_str(text.as_str())?;
                                    if let Some(seq) = message["s"].as_u64() { sequence = Some(seq); }
                                    match message["op"].as_u64() {
                                        Some(10) => {
                                            let millis = message["d"]["heartbeat_interval"].as_u64().unwrap_or(30000).max(1000);
                                            heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_millis(millis), Duration::from_millis(millis));
                                            let payload = if session.is_empty() { json!({"op":2,"d":{"token":format!("QQBot {}",token.as_str()),"intents":1u64<<25,"shard":[0,1]}}) }
                                            else { json!({"op":6,"d":{"token":format!("QQBot {}",token.as_str()),"session_id":session,"seq":sequence}}) };
                                            socket.send(Message::Text(payload.to_string().into())).await?;
                                        },
                                        Some(11) => waiting_ack = false,
                                        Some(1) => { socket.send(Message::Text(json!({"op":1,"d":sequence}).to_string().into())).await?; },
                                        Some(7) => bail!("QQ 要求重连"),
                                        Some(9) => { session.clear(); sequence = None; bail!("QQ 会话需要重新连接"); },
                                        Some(0) => match message["t"].as_str() {
                                            Some("READY") => { session = message["d"]["session_id"].as_str().unwrap_or_default().into(); backoff = 2; let _ = events.send(GatewayEvent::Connected).await; },
                                            Some("RESUMED") => { backoff = 2; let _ = events.send(GatewayEvent::Connected).await; },
                                            Some("C2C_MESSAGE_CREATE") => { if events.send(GatewayEvent::Message(message["d"].clone())).await.is_err() { return Ok(()); } },
                                            _ => {},
                                        }, _ => {},
                                    }
                                },
                                Message::Ping(p) => { socket.send(Message::Pong(p)).await?; },
                                Message::Close(close) => {
                                    if let Some(frame) = close {
                                        let code: u16 = frame.code.into();
                                        if [4006, 4007, 4009].contains(&code) { session.clear(); sequence = None; }
                                        if [4004, 4014].contains(&code) { *self.token.lock().await = None; session.clear(); sequence = None; }
                                    }
                                    bail!("QQ 网关已断开");
                                }, _ => {},
                            }
                        }
                    }
                }
            }.await;
            if events.is_closed() {
                return;
            }
            let _ = result;
            let _ = events.send(GatewayEvent::Disconnected).await;
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    }
}
pub enum GatewayEvent {
    Connected,
    Disconnected,
    Message(Value),
}
pub fn path_segment(value: &str) -> Result<&str> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("QQ 账号标识无效");
    }
    Ok(value)
}

pub fn reference_keys(reply: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    for key in ["id", "msg_idx", "msgIdx", "message_id"] {
        if let Some(value) = reply[key].as_str() {
            keys.push(value.into());
        }
    }
    if let Some(ext) = reply["ext_info"]["ref_idx"].as_str() {
        keys.push(ext.into());
    }
    keys
}
pub fn quoted_key(message: &Value) -> Option<String> {
    if let Some(ext) = message["message_scene"]["ext"].as_array() {
        for item in ext {
            if let Some(value) = item.as_str().and_then(|s| s.strip_prefix("ref_msg_idx=")) {
                return Some(value.trim().into());
            }
        }
    }
    if message["message_type"] == 103 {
        if let Some(els) = message["msg_elements"].as_array() {
            for el in els {
                if let Some(idx) = el["msg_idx"].as_str() {
                    return Some(idx.into());
                }
            }
        }
    }
    message["message_reference"]["message_id"]
        .as_str()
        .map(String::from)
}
