use crate::{
    media::{self, Attachment},
    model::GuideStatus,
    qq::{Api, reference_keys},
    store::Store,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Notify, mpsc, watch};
use uuid::Uuid;

#[derive(Clone)]
pub struct Binding {
    pub api: Api,
    pub owner: String,
    pub app_id: String,
    pub ready: bool,
}
impl Binding {
    pub fn scope(&self) -> String {
        format!("{}:{}", self.app_id, self.owner)
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    pending: Vec<Record>,
    receipts: Vec<Value>,
    welcome: Option<Welcome>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Welcome {
    scope: String,
    delivery_id: String,
    accepted: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    scope: String,
    owner: String,
    source: Option<String>,
    parts: Vec<String>,
    cursor: usize,
    attachment: Option<Attachment>,
    keyboard: Option<Value>,
    markdown: bool,
    attempts: u32,
    next_attempt: u64,
    failed: bool,
    error: Option<String>,
}
#[derive(Clone)]
pub struct Outbox {
    saved: Arc<Mutex<Saved>>,
    store: Store,
    binding: watch::Receiver<Option<Binding>>,
    notify: Arc<Notify>,
}
pub struct Accepted {
    pub scope: String,
    pub source: String,
    pub references: Vec<String>,
}
impl Outbox {
    pub fn start(
        store: Store,
        binding: watch::Receiver<Option<Binding>>,
        accepted: mpsc::Sender<Accepted>,
    ) -> Result<Self> {
        let outbox = Self {
            saved: Arc::new(Mutex::new(store.load("outbox.json")?)),
            store,
            binding,
            notify: Arc::new(Notify::new()),
        };
        tokio::spawn(outbox.clone().worker(accepted));
        Ok(outbox)
    }
    pub async fn enqueue(
        &self,
        expected_scope: Option<&str>,
        text: &str,
        source: Option<&str>,
        keyboard: Option<Value>,
        markdown: bool,
        attachment: Option<Attachment>,
    ) -> Result<Value> {
        if text.chars().count() > 20000 || (text.trim().is_empty() && attachment.is_none()) {
            bail!("消息为空或超过长度限制");
        }
        let binding = self
            .binding
            .borrow()
            .clone()
            .context("请先在桌面完成 QQ 授权和用户配对")?;
        if expected_scope.is_some_and(|scope| scope != binding.scope()) {
            bail!("QQ 绑定已改变，此操作的回执不会发给新账号");
        }
        let mut state = self.saved.lock().await;
        if state.pending.len() >= 100 {
            bail!("待发队列已满，请在桌面检查发送状态");
        }
        let id = Uuid::new_v4().to_string();
        let parts = text_parts(text);
        state.pending.push(Record {
            id: id.clone(),
            scope: binding.scope(),
            owner: binding.owner,
            source: source.map(String::from),
            parts,
            cursor: 0,
            attachment,
            keyboard,
            markdown,
            attempts: 0,
            next_attempt: 0,
            failed: false,
            error: None,
        });
        if let Err(e) = self.store.save("outbox.json", &*state) {
            state.pending.pop();
            return Err(e);
        }
        self.notify.notify_one();
        Ok(json!({"deliveryId":id,"state":"queued"}))
    }
    pub async fn welcome(&self, text: &str, keyboard: Option<Value>, resend: bool) -> Result<()> {
        let binding = self
            .binding
            .borrow()
            .clone()
            .filter(|b| b.ready)
            .context("请先完成 QQ 连接和用户配对")?;
        let scope = binding.scope();
        let mut state = self.saved.lock().await;
        if let Some(welcome) = state.welcome.as_ref().filter(|w| w.scope == scope) {
            if !resend {
                return Ok(());
            }
            let id = welcome.delivery_id.clone();
            if let Some(record) = state.pending.iter_mut().find(|r| r.id == id) {
                record.failed = false;
                record.attempts = 0;
                record.next_attempt = 0;
                record.error = None;
                self.store.save("outbox.json", &*state)?;
                self.notify.notify_one();
                return Ok(());
            }
        }
        if state.pending.len() >= 100 {
            bail!("待发队列已满，请在设置中检查发送状态");
        }
        let id = Uuid::new_v4().to_string();
        state.pending.push(Record {
            id: id.clone(),
            scope: scope.clone(),
            owner: binding.owner,
            source: None,
            parts: text_parts(text),
            cursor: 0,
            attachment: None,
            keyboard,
            markdown: true,
            attempts: 0,
            next_attempt: 0,
            failed: false,
            error: None,
        });
        let previous = state.welcome.replace(Welcome {
            scope,
            delivery_id: id,
            accepted: false,
        });
        if let Err(error) = self.store.save("outbox.json", &*state) {
            state.pending.pop();
            state.welcome = previous;
            return Err(error);
        }
        self.notify.notify_one();
        Ok(())
    }
    pub async fn guide_status(&self) -> GuideStatus {
        let Some(binding) = self.binding.borrow().clone() else {
            return GuideStatus::NotSent;
        };
        let state = self.saved.lock().await;
        let Some(welcome) = state
            .welcome
            .as_ref()
            .filter(|w| w.scope == binding.scope())
        else {
            return GuideStatus::NotSent;
        };
        if welcome.accepted {
            return GuideStatus::Sent;
        }
        if state
            .pending
            .iter()
            .any(|r| r.id == welcome.delivery_id && r.failed)
        {
            GuideStatus::Failed
        } else {
            GuideStatus::Sending
        }
    }
    pub async fn retry(&self) -> Result<()> {
        let mut state = self.saved.lock().await;
        for record in &mut state.pending {
            record.failed = false;
            record.attempts = 0;
            record.next_attempt = 0;
        }
        self.store.save("outbox.json", &*state)?;
        self.notify.notify_one();
        Ok(())
    }
    pub async fn summary(&self) -> Value {
        let state = self.saved.lock().await;
        json!({"pending":state.pending.iter().filter(|r| !r.failed).count(),"failed":state.pending.iter().filter(|r| r.failed).count()})
    }
    pub async fn recovered_sources(&self, scope: Option<&str>) -> Vec<Accepted> {
        let state = self.saved.lock().await;
        state
            .receipts
            .iter()
            .filter_map(|r| {
                if scope.is_none() || r["scope"].as_str() != scope {
                    return None;
                }
                Some(Accepted {
                    scope: r["scope"].as_str()?.into(),
                    source: r["source"].as_str()?.into(),
                    references: r["references"]
                        .as_array()?
                        .iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect(),
                })
            })
            .collect()
    }
    async fn worker(self, accepted: mpsc::Sender<Accepted>) {
        loop {
            let ready = self.binding.borrow().clone().filter(|b| b.ready);
            let next = {
                let state = self.saved.lock().await;
                state
                    .pending
                    .iter()
                    .find(|r| !r.failed && r.next_attempt <= now())
                    .cloned()
            };
            if let (Some(binding), Some(record)) = (ready, next) {
                let result = if record.scope != binding.scope() {
                    Err(anyhow::anyhow!("QQ 绑定已改变，旧消息不会发送给新账号"))
                } else {
                    send_record(&binding.api, &record).await
                };
                let mut state = self.saved.lock().await;
                if let Some(index) = state
                    .pending
                    .iter()
                    .position(|r| r.id == record.id && r.cursor == record.cursor)
                {
                    match result {
                        Ok(references) => {
                            let current = &mut state.pending[index];
                            current.cursor += 1;
                            current.attempts = 0;
                            current.error = None;
                            if current.cursor
                                >= current.parts.len() + usize::from(current.attachment.is_some())
                            {
                                state.pending.remove(index);
                                if let Some(welcome) = state
                                    .welcome
                                    .as_mut()
                                    .filter(|w| w.delivery_id == record.id)
                                {
                                    welcome.accepted = true;
                                }
                                state.receipts.push(json!({"deliveryId":record.id,"state":"sent","sentAt":now(),"scope":record.scope,"source":record.source,"references":references}));
                                if state.receipts.len() > 100 {
                                    state.receipts.remove(0);
                                }
                            } else {
                                state.receipts.push(json!({"deliveryId":record.id,"state":"part_sent","scope":record.scope,"source":record.source,"references":references}));
                                if state.receipts.len() > 100 {
                                    state.receipts.remove(0);
                                }
                            }
                            // Persist the accepted cursor before routing-reference bookkeeping.
                            // A failed reference save must not resend an already accepted part.
                            let persisted = self.store.save("outbox.json", &*state);
                            if persisted.is_err() {
                                if let Some(r) =
                                    state.pending.iter_mut().find(|r| r.id == record.id)
                                {
                                    r.failed = true;
                                    r.error =
                                        Some("发送已接受，但本地回执保存失败，请检查磁盘".into());
                                }
                            }
                            if let Some(source) = record.source {
                                let _ = accepted
                                    .send(Accepted {
                                        scope: record.scope,
                                        source,
                                        references,
                                    })
                                    .await;
                            }
                        }
                        Err(error) => {
                            let current = &mut state.pending[index];
                            current.attempts += 1;
                            current.error = Some(error.to_string());
                            current.next_attempt =
                                now() + 30 * (1 << current.attempts.saturating_sub(1).min(3));
                            if current.attempts >= 5 || current.scope != binding.scope() {
                                current.failed = true;
                            }
                            let _ = self.store.save("outbox.json", &*state);
                        }
                    }
                }
                drop(state);
                tokio::time::sleep(Duration::from_millis(3100)).await;
            } else {
                tokio::select! { _ = self.notify.notified() => {}, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
            }
        }
    }
}
fn text_parts(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut length = 0;
    for line in text.split_inclusive('\n') {
        let chars: Vec<_> = line.chars().collect();
        for chunk in chars.chunks(1800) {
            if length + chunk.len() > 1800 {
                parts.push(std::mem::take(&mut current));
                length = 0;
            }
            current.extend(chunk.iter());
            length += chunk.len();
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
async fn send_record(api: &Api, record: &Record) -> Result<Vec<String>> {
    let response = if record.cursor < record.parts.len() {
        let text = &record.parts[record.cursor];
        if record.markdown || record.keyboard.is_some() {
            let mut body = json!({"msg_type":2,"markdown":{"content":text},"msg_seq":1});
            if record.cursor + 1 == record.parts.len() {
                if let Some(keyboard) = &record.keyboard {
                    body["keyboard"] = keyboard.clone();
                }
            }
            match api.send(&record.owner, body).await {
                Ok(reply) => reply,
                Err(error) => {
                    let reason = error.to_string();
                    let rejected = reason
                        .strip_prefix("QQ_API_")
                        .and_then(|s| s.split('_').next())
                        .and_then(|s| s.parse::<u16>().ok())
                        .is_some_and(|s| (400..500).contains(&s) && s != 429);
                    if !rejected {
                        return Err(error);
                    }
                    api.send(
                        &record.owner,
                        json!({"msg_type":0,"content":plain_text(text),"msg_seq":1}),
                    )
                    .await?
                }
            }
        } else {
            api.send(
                &record.owner,
                json!({"msg_type":0,"content":text,"msg_seq":1}),
            )
            .await?
        }
    } else {
        let attachment = record.attachment.as_ref().context("附件丢失")?;
        let info = media::upload(api, &record.owner, attachment).await?;
        api.send(
            &record.owner,
            json!({"msg_type":7,"media":{"file_info":info},"msg_seq":1}),
        )
        .await?
    };
    Ok(reference_keys(&response))
}
fn plain_text(text: &str) -> String {
    let mut result = text.to_string();
    for tag in ["qqbot-cmd-enter", "qqbot-cmd-input"] {
        while let Some(start) = result.find(&format!("<{tag} ")) {
            let Some(end) = result[start..].find("/>").map(|n| n + start + 2) else {
                break;
            };
            let segment = &result[start..end];
            let value = segment
                .split("text=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("/help");
            let decoded = url::form_urlencoded::parse(format!("v={value}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_else(|| "/help".into());
            result.replace_range(start..end, &decoded);
        }
    }
    result
}
