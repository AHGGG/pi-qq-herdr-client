use crate::{
    bridge::{BridgeEvent, Hub},
    commands,
    herdr::Core,
    media,
    model::{Config, Session, Workspace, clean},
    outbox::Outbox,
    qq,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

const HELP: &str = "Pi QQ\n\n/workspaces · 选择工作区\n/sessions [all] [页码] · 查看 Pi\n/use sN · 选择会话\n/new [sN] · 新建 Pi（没有会话也可创建）\n/name [sN] 名称 · 重命名\n/status · 当前会话状态\n/peek [sN] · 查看当前终端\n/stop · 停止任务\n/close [sN] · 确认后关闭 Pi\n\n/commands · Pi 控制\n/model、/thinking、/compact、/reload\n/pi 命令 参数 · 已安装 Pi 命令\n/to sN 内容 · 指定会话\n/steer 内容 · 反馈当前任务\n\n/answer qID 1 · 回答问卷\n/answer qID 1,2;=文字 · 多选/多题\n/cancel qID · 取消问卷\n/permissions · 等待中的权限\n/approve pID、/deny pID [原因]\n\n普通消息会发给选中的 Pi。引用通知回复可以继续原会话；支持图片、视频、文件。/stop 保留 Pi，/close 才关闭终端。";

struct Interaction {
    session: String,
    generation: String,
    payload: Value,
}
struct Close {
    session: Session,
    core_id: String,
    pid: Value,
    created: Instant,
}
pub struct Router {
    pub expected_scope: Option<String>,
    pub hub: Hub,
    pub outbox: Outbox,
    pub core: Arc<RwLock<Option<Core>>>,
    pub config: Arc<RwLock<Config>>,
    questions: HashMap<String, Interaction>,
    permissions: HashMap<String, Interaction>,
    closes: HashMap<String, Close>,
}
impl Router {
    pub fn new(
        hub: Hub,
        outbox: Outbox,
        core: Arc<RwLock<Option<Core>>>,
        config: Arc<RwLock<Config>>,
    ) -> Self {
        Self {
            expected_scope: None,
            hub,
            outbox,
            core,
            config,
            questions: HashMap::new(),
            permissions: HashMap::new(),
            closes: HashMap::new(),
        }
    }
    async fn send(
        &self,
        text: &str,
        source: Option<&str>,
        buttons: &[(&str, String)],
    ) -> Result<()> {
        self.outbox
            .enqueue(
                self.expected_scope.as_deref(),
                text,
                source,
                keyboard(buttons),
                !buttons.is_empty(),
                None,
            )
            .await?;
        Ok(())
    }
    async fn session_send(
        &self,
        s: &Session,
        text: &str,
        buttons: &[(&str, String)],
    ) -> Result<()> {
        self.send(
            &format!("[{} · {}]\n{}", s.slot, clean(&s.label, 100), text),
            Some(&s.id),
            buttons,
        )
        .await
    }
    pub async fn inbound(&mut self, http: &reqwest::Client, message: Value) -> Result<()> {
        let id = message["id"].as_str().context("QQ 消息缺少标识")?;
        {
            let mut routes = self.hub.routes.lock().await;
            if routes.seen.iter().any(|s| s == id) {
                return Ok(());
            }
            routes.seen.push(id.into());
            if routes.seen.len() > 500 {
                routes.seen.remove(0);
            }
        }
        self.hub.save_routes().await?;
        let result = self.route(http, &message).await;
        if let Err(error) = result {
            self.send(&format!("{}", error), None, &[]).await?;
        }
        Ok(())
    }
    async fn core(&self) -> Result<Core> {
        self.core
            .read()
            .await
            .clone()
            .context("Herdr 尚未就绪，请在桌面重新检测")
    }
    async fn target(&self, explicit: Option<&str>, quote: Option<&str>) -> Result<Session> {
        let sessions = self.hub.sessions().await;
        let routes = self.hub.routes.lock().await;
        let quoted = quote
            .map(|q| {
                routes
                    .sources
                    .get(q)
                    .context("这条引用没有可识别的 Pi 来源，请使用 /sessions 重新选择")
            })
            .transpose()?;
        let selected = if let Some(slot) = explicit {
            let s = sessions
                .iter()
                .find(|s| s.slot == slot)
                .context("指定 Pi 已离线，请刷新 /sessions")?;
            if quoted.is_some_and(|id| id != &s.id) {
                bail!("指定会话与引用来源不同，请明确选择后重发");
            }
            return Ok(s.clone());
        } else if let Some(id) = quoted {
            Some(id)
        } else {
            routes.selected.as_ref()
        };
        if let Some(id) = selected {
            return sessions
                .iter()
                .find(|s| &s.id == id)
                .cloned()
                .context("原会话已离线。请发送 /sessions 重新选择；消息没有发给其他 Pi");
        }
        if sessions.len() == 1 {
            return Ok(sessions[0].clone());
        }
        bail!("请先发送 /sessions 选择 Pi；没有会话时可发送 /new 创建")
    }
    async fn workspaces(&self) -> Result<Vec<(String, Workspace)>> {
        let core = self.core().await?;
        let cat = core.catalog.read().await;
        if !cat.ready {
            bail!("Herdr 正在连接，请稍后重试");
        }
        let mut routes = self.hub.routes.lock().await;
        let mut result = Vec::new();
        for w in &cat.workspaces {
            let identity = format!("{}:{}", w.key.core_id, w.key.id);
            if !routes.workspace_slots.contains_key(&identity) {
                routes.next_workspace_slot += 1;
                let slot = format!("w{}", routes.next_workspace_slot);
                routes.workspace_slots.insert(identity.clone(), slot);
            }
            result.push((routes.workspace_slots[&identity].clone(), w.clone()));
        }
        drop(routes);
        drop(cat);
        self.hub.save_routes().await?;
        Ok(result)
    }
    async fn route(&mut self, http: &reqwest::Client, message: &Value) -> Result<()> {
        let mut text = message["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.len() > 20000 {
            bail!("消息过长，请分开发送");
        }
        let quote = qq::quoted_key(message);
        // A quote marker without a resolvable reference must never redirect silently.
        if message["message_type"] == 103 && quote.is_none() {
            bail!("这条引用无法识别来源，请先 /use 选择会话");
        }
        let mut explicit = None;
        if text.starts_with("/to ") {
            let rest = text[4..].trim();
            let (slot, body) = rest
                .split_once(char::is_whitespace)
                .context("用法：/to sN 内容")?;
            explicit = Some(slot.to_string());
            text = body.trim().to_string();
        }
        let (command, args) = if let Some(rest) = text.strip_prefix('/') {
            let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            (command.to_ascii_lowercase(), args.trim().to_string())
        } else {
            (String::new(), text.clone())
        };
        match command.as_str() {
            "help" => {
                if args.eq_ignore_ascii_case("commands") {
                    return self
                        .send(commands::HELP, None, &[("返回帮助", "/help".into())])
                        .await;
                }
                return self
                    .send(
                        HELP,
                        None,
                        &[
                            ("工作区", "/workspaces".into()),
                            ("Pi 会话", "/sessions all".into()),
                            ("新建 Pi", "/new".into()),
                            ("手机控制用法", "/help commands".into()),
                        ],
                    )
                    .await;
            }
            "workspaces" => {
                let all = self.workspaces().await?;
                let (page, pages, start) = page(&args, all.len(), 8)?;
                let selected = self.hub.routes.lock().await.workspace.clone();
                let mut lines = vec![format!("工作区 · {page}/{pages}")];
                for (slot, w) in all.iter().skip(start).take(8) {
                    lines.push(format!(
                        "{}{}\n{}",
                        if selected.as_ref() == Some(&w.key) {
                            "✓ "
                        } else {
                            ""
                        },
                        command_link(&format!("/workspace {slot}"), &w.name),
                        clean(&w.cwd, 300)
                    ));
                }
                if all.is_empty() {
                    lines.push("暂无 Herdr 工作区。可直接 /new，在默认目录创建 Pi。".into());
                }
                let mut buttons = pages_buttons("/workspaces", page, pages);
                buttons.push(("全部 Pi", "/sessions all".into()));
                return self.send(&lines.join("\n\n"), None, &buttons).await;
            }
            "workspace" => {
                let slot = args
                    .split_whitespace()
                    .next()
                    .context("用法：/workspace wN")?;
                let (_, w) = self
                    .workspaces()
                    .await?
                    .into_iter()
                    .find(|(s, _)| s == slot)
                    .context("工作区不存在，请刷新 /workspaces")?;
                self.hub.routes.lock().await.workspace = Some(w.key);
                self.hub.save_routes().await?;
                return self
                    .send(
                        &format!(
                            "已选择工作区：{}\n新会话将使用此工作区。",
                            clean(&w.name, 100)
                        ),
                        None,
                        &[("Pi 会话", "/sessions".into()), ("新建 Pi", "/new".into())],
                    )
                    .await;
            }
            "sessions" => {
                let all = args.split_whitespace().any(|a| a == "all");
                let page_arg = args.split_whitespace().find(|a| *a != "all").unwrap_or("");
                let mut sessions = self.hub.sessions().await;
                let routes = self.hub.routes.lock().await;
                let selected = routes.selected.clone();
                let workspace = routes.workspace.clone();
                drop(routes);
                if !all {
                    if let Some(key) = workspace {
                        let core = self.core().await?;
                        let cat = core.catalog.read().await;
                        let w = cat.workspaces.iter().find(|w| w.key == key).context(
                            "选中的工作区已不可用，请重新选择 /workspaces 或 /sessions all",
                        )?;
                        sessions.retain(|s| {
                            s.terminal.as_ref().is_some_and(|t| w.terminals.contains(t))
                        });
                    }
                }
                let (page, pages, start) = page(page_arg, sessions.len(), 8)?;
                let mut lines = vec![format!("Pi 会话 · {page}/{pages}")];
                for s in sessions.iter().skip(start).take(8) {
                    lines.push(format!(
                        "{}{}\n{} · {}",
                        if selected.as_ref() == Some(&s.id) {
                            "✓ "
                        } else {
                            ""
                        },
                        command_link(&format!("/use {}", s.slot), &s.label),
                        state_name(&s.state),
                        clean(&s.cwd, 200)
                    ));
                }
                if sessions.is_empty() {
                    lines.push("没有在线 Pi，可以直接 /new 创建。".into());
                }
                let mut buttons =
                    pages_buttons(if all { "/sessions all" } else { "/sessions" }, page, pages);
                buttons.extend([("工作区", "/workspaces".into()), ("新建 Pi", "/new".into())]);
                return self.send(&lines.join("\n\n"), None, &buttons).await;
            }
            "use" => {
                let slot = args.split_whitespace().next().context("用法：/use sN")?;
                let s = self.target(Some(slot), None).await?;
                self.hub.routes.lock().await.selected = Some(s.id.clone());
                self.hub.save_routes().await?;
                return self
                    .session_send(&s, "已选择，可以直接发送消息。", &navigation(&s))
                    .await;
            }
            "new" => {
                let core = self.core().await?;
                let config = self.config.read().await.clone();
                let source = if args.is_empty() {
                    None
                } else {
                    Some(
                        self.target(Some(args.split_whitespace().next().unwrap()), None)
                            .await?,
                    )
                };
                let key = self.hub.routes.lock().await.workspace.clone();
                let cwd = if let Some(key) = &key {
                    core.catalog
                        .read()
                        .await
                        .workspaces
                        .iter()
                        .find(|w| &w.key == key)
                        .context("工作区已不可用，请重新选择")?
                        .cwd
                        .clone()
                } else if let Some(source) = source {
                    source.cwd
                } else {
                    config
                        .default_directory
                        .unwrap_or_else(|| core.environment.home.clone())
                        .to_string_lossy()
                        .to_string()
                };
                let (terminal, _) = core.launch(&cwd, key.as_ref()).await?;
                self.send("Pi 终端已创建，正在等待扩展连接……", None, &[])
                    .await?;
                // The Core acknowledgement does not prove Pi has registered. Never retry spawn.
                for _ in 0..60 {
                    if let Some(s) = self
                        .hub
                        .sessions()
                        .await
                        .into_iter()
                        .find(|s| s.terminal.as_deref() == Some(&terminal))
                    {
                        core.validate(&s).await?;
                        self.hub.routes.lock().await.selected = Some(s.id.clone());
                        self.hub.save_routes().await?;
                        return self
                            .session_send(&s, "新 Pi 已连接并选中。", &navigation(&s))
                            .await;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                return self.send("终端已创建，但 Pi 扩展尚未确认连接。请查看 /sessions 和 Herdr；不要重复 /new。",None,&[("Pi 会话","/sessions all".into())]).await;
            }
            "answer" | "cancel" | "pick" => {
                return self
                    .answer_question(&command, &args, explicit.as_deref(), quote.as_deref())
                    .await;
            }
            "approve" | "deny" | "permission" => {
                return self.answer_permission(&command, &args).await;
            }
            "keep" => {
                self.closes
                    .remove(args.split_whitespace().next().unwrap_or_default());
                return self.send("已取消关闭。", None, &[]).await;
            }
            "close" if args.starts_with('c') => return self.confirm_close(&args).await,
            _ => {}
        }
        let mut scoped = args.as_str();
        if ["status", "peek", "stop", "close", "permissions", "name"].contains(&command.as_str())
            && explicit.is_none()
        {
            let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((&args, ""));
            if is_slot(first, 's') {
                explicit = Some(first.into());
                scoped = rest.trim();
            }
        }
        let session = self.target(explicit.as_deref(), quote.as_deref()).await?;
        match command.as_str() {
            "status" => {
                let runtime = &session.runtime;
                let text = format!(
                    "{}\n目录：{}\n模型：{}/{}\nThinking：{}\n上下文：{} / {}\n自动转发：{}",
                    state_name(&session.state),
                    clean(&session.cwd, 500),
                    runtime["model"]["provider"].as_str().unwrap_or("—"),
                    runtime["model"]["id"].as_str().unwrap_or("—"),
                    runtime["thinkingLevel"].as_str().unwrap_or("—"),
                    runtime["contextUsage"]["tokens"],
                    runtime["contextUsage"]["contextWindow"],
                    if runtime["autoForward"] == false {
                        "关闭"
                    } else {
                        "开启"
                    }
                );
                self.session_send(&session, &text, &navigation(&session))
                    .await
            }
            "peek" => {
                let core = self.core().await?;
                core.validate(&session).await?;
                let result = core
                    .request(
                        "terminal.read",
                        json!({"terminal":session.terminal,"lines":0}),
                    )
                    .await?;
                self.session_send(
                    &session,
                    &format!(
                        "当前终端\n{}",
                        clean(result["text"].as_str().unwrap_or("无法读取当前画面"), 7000)
                    ),
                    &navigation(&session),
                )
                .await
            }
            "stop" => {
                let result = self.hub.action(&session, json!({"action":"stop"})).await?;
                if result["status"] != "stopping" {
                    bail!("停止请求未确认");
                }
                self.session_send(
                    &session,
                    "已请求停止当前任务，Pi 会话保留。",
                    &navigation(&session),
                )
                .await
            }
            "name" => {
                if scoped.trim().is_empty() || scoped.chars().count() > 70 {
                    bail!("用法：/name [sN] 名称（1–70 字）");
                }
                self.hub
                    .action(&session, json!({"action":"name","name":scoped}))
                    .await?;
                self.hub
                    .routes
                    .lock()
                    .await
                    .names
                    .insert(session.id.clone(), scoped.into());
                self.hub.save_routes().await?;
                self.session_send(&session, "名称已更新。", &navigation(&session))
                    .await
            }
            "close" => {
                let core = self.core().await?;
                let record = core.validate(&session).await?;
                let core_id = core.catalog.read().await.core_id.clone();
                let reference = format!("c{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
                self.closes.insert(
                    reference.clone(),
                    Close {
                        session: session.clone(),
                        core_id,
                        pid: record["pid"].clone(),
                        created: Instant::now(),
                    },
                );
                self.session_send(&session,&format!("关闭后会结束这个 Pi 和它的 Herdr 终端，未完成任务也会结束。\n确认：/close {reference}\n取消：/keep {reference}"),&[("关闭 Pi",format!("/close {reference}")),("保留",format!("/keep {reference}"))]).await
            }
            "permissions" => {
                let result = self
                    .hub
                    .action(&session, json!({"action":"permission_list"}))
                    .await?;
                let active = result["requests"].as_array().cloned().unwrap_or_default();
                self.permissions.retain(|_, p| {
                    p.session != session.id
                        || active
                            .iter()
                            .any(|a| a["requestId"] == p.payload["requestId"])
                });
                for payload in active {
                    self.open_permission(&session, payload).await?;
                }
                if !self.permissions.values().any(|p| p.session == session.id) {
                    self.session_send(&session,"没有可从手机回答的权限请求。策略自动允许的操作不会询问；其他弹窗请查看 Pi 终端。",&navigation(&session)).await?;
                }
                Ok(())
            }
            "commands" | "model" | "thinking" | "compact" | "reload" | "pi" => {
                let reply = commands::execute(&self.hub, &session, &command, &args).await?;
                self.outbox
                    .enqueue(
                        self.expected_scope.as_deref(),
                        &format!(
                            "[{} · {}]\n{}",
                            session.slot,
                            markdown_text(&clean(&session.label, 100)),
                            reply.text
                        ),
                        Some(&session.id),
                        Some(reply.keyboard),
                        true,
                        None,
                    )
                    .await?;
                Ok(())
            }
            "" | "steer" => {
                let files = media::receive(http, message).await?;
                let mut prompt = if command == "steer" { args } else { text };
                for f in &files {
                    prompt.push_str(&format!("\n[QQ {}: {}]", f.kind, f.path.display()));
                }
                if prompt.trim().is_empty() {
                    bail!("消息为空");
                }
                // QQ text is feedback to the active Pi turn. Keep ordinary messages,
                // /to, and explicit /steer aligned with the legacy bridge: all use
                // Pi's immediate steering queue. Follow-up delivery remains reserved
                // for client-generated control commands in the Pi extension.
                let result = self
                    .hub
                    .action(
                        &session,
                        json!({"action":"prompt","text":prompt,"mode":"steer"}),
                    )
                    .await?;
                let note = match result["status"].as_str() {
                    Some("compacting") => "已排队，压缩完成后提交给 Pi。",
                    Some("submitted") => {
                        if result["permissionFeedback"] == "cancelled" {
                            "消息已提交，等待中的权限已拒绝，Pi 将先读取你的反馈。"
                        } else if result["permissionFeedback"] == "waiting" {
                            "消息已提交，但仍有权限等待；请用 /permissions 处理，不要重复发送。"
                        } else if session.state == "waiting_for_answer" {
                            "消息已提交；当前追问结束后才会读取。请按 /answer 指引回答问卷。"
                        } else {
                            "消息已提交给 Pi。"
                        }
                    }
                    _ => bail!("Pi 未确认消息，请查看终端后再重发"),
                };
                self.session_send(&session, note, &[]).await
            }
            _ => bail!("未知指令。发送 /help 查看手机命令；Pi 扩展命令请用 /pi 命令 参数。"),
        }
    }
    async fn confirm_close(&mut self, args: &str) -> Result<()> {
        let ticket = self
            .closes
            .remove(args)
            .context("关闭确认已失效，请重新 /close")?;
        if ticket.created.elapsed() > Duration::from_secs(120) {
            bail!("关闭确认已过期，请重新 /close");
        }
        let session = self
            .hub
            .session(&ticket.session.id)
            .await
            .filter(|s| s.generation == ticket.session.generation)
            .context("Pi 连接已变化，关闭已取消")?;
        let core = self.core().await?;
        let record = core.validate(&session).await?;
        if core.catalog.read().await.core_id != ticket.core_id || record["pid"] != ticket.pid {
            bail!("终端身份已变化，关闭已取消");
        }
        core.request("terminal.kill", json!({"terminal":session.terminal}))
            .await?;
        self.session_send(&session, "Herdr 已接受关闭请求。", &[])
            .await
    }
    pub async fn event(&mut self, event: BridgeEvent) -> Result<()> {
        match event {
            BridgeEvent::Registered(_session) => {}
            BridgeEvent::Disconnected(id, generation) => {
                self.questions
                    .retain(|_, p| p.session != id || p.generation != generation);
                self.permissions
                    .retain(|_, p| p.session != id || p.generation != generation);
            }
            BridgeEvent::Interaction {
                session,
                generation,
                message,
            } => {
                let Some(s) = self
                    .hub
                    .session(&session)
                    .await
                    .filter(|s| s.generation == generation)
                else {
                    return Ok(());
                };
                match message["type"].as_str().unwrap_or_default() {
                    "question_open" => {
                        let questions = message["questions"]
                            .as_array()
                            .filter(|qs| !qs.is_empty() && qs.len() <= 4)
                            .context("问卷结构无效，请在终端回答")?;
                        if questions.iter().any(|q| {
                            !q["question"].is_string()
                                || q["options"]
                                    .as_array()
                                    .is_none_or(|os| os.len() < 2 || os.len() > 4)
                        }) {
                            bail!("问卷选项无效，请在终端回答");
                        }
                        if self.questions.values().any(|q| {
                            q.session == session && q.payload["toolCallId"] == message["toolCallId"]
                        }) {
                            return Ok(());
                        }
                        let reference =
                            format!("q{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
                        let mut text = format!("需要回答 · {reference}");
                        for (index, q) in questions.iter().enumerate() {
                            text.push_str(&format!(
                                "\n\n{}. {}{}",
                                index + 1,
                                clean(q["question"].as_str().unwrap_or_default(), 1000),
                                if q["multiSelect"] == true {
                                    "（多选）"
                                } else {
                                    ""
                                }
                            ));
                            for (n, o) in q["options"].as_array().unwrap().iter().enumerate() {
                                let label = o["label"].as_str().unwrap_or("选项");
                                let option = if questions.len() == 1 && q["multiSelect"] != true {
                                    command_link(&format!("/pick {reference} {}", n + 1), label)
                                } else {
                                    format!("{}) {}", n + 1, clean(label, 200))
                                };
                                text.push_str(&format!(
                                    "\n{}\n{}",
                                    option,
                                    clean(o["description"].as_str().unwrap_or_default(), 400)
                                ));
                            }
                        }
                        text.push_str(&format!("\n\n回答：/answer {reference} {}\n多题用分号，多选用逗号，自定义文字以 = 开头。\n取消：/cancel {reference}",questions.iter().map(|q|if q["multiSelect"]==true{"1,2"}else{"1"}).collect::<Vec<_>>().join(";")));
                        let buttons = vec![
                            ("输入回答", format!("/answer {reference} =")),
                            ("取消", format!("/cancel {reference}")),
                        ];
                        self.questions.insert(
                            reference,
                            Interaction {
                                session,
                                generation,
                                payload: message,
                            },
                        );
                        self.session_send(&s, &text, &buttons).await?;
                    }
                    "question_close" => self.questions.retain(|_, p| {
                        p.session != session || p.payload["toolCallId"] != message["toolCallId"]
                    }),
                    "permission_open" => self.open_permission(&s, message).await?,
                    "permission_close" => self.permissions.retain(|_, p| {
                        p.session != session || p.payload["requestId"] != message["permissionId"]
                    }),
                    _ => {}
                }
            }
        }
        Ok(())
    }
    async fn open_permission(&mut self, s: &Session, payload: Value) -> Result<()> {
        let text = payload["text"]
            .as_str()
            .filter(|s| s.len() <= 12000)
            .context("权限内容超出手机可显示范围，请在终端处理")?;
        if !payload["requestId"].is_string() {
            bail!("权限请求无效");
        }
        if self
            .permissions
            .values()
            .any(|p| p.session == s.id && p.payload["requestId"] == payload["requestId"])
        {
            return Ok(());
        }
        let reference = format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let display = markdown_text(text);
        if display.chars().count() > 18000 {
            bail!("权限内容超出手机显示范围，请在终端处理");
        }
        let text = format!(
            "需要权限 · {reference}\n\n{display}\n\n允许一次：/approve {reference}\n拒绝：/deny {reference}"
        );
        self.permissions.insert(
            reference.clone(),
            Interaction {
                session: s.id.clone(),
                generation: s.generation.clone(),
                payload,
            },
        );
        self.session_send(
            s,
            &text,
            &[
                ("允许一次", format!("/approve {reference}")),
                ("拒绝", format!("/deny {reference}")),
            ],
        )
        .await
    }
    async fn answer_permission(&mut self, command: &str, args: &str) -> Result<()> {
        let (reference, reason) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        if command == "approve" && !reason.trim().is_empty() {
            bail!("请用 /approve 加准确的权限编号");
        }
        let p = self
            .permissions
            .get(reference)
            .context("权限请求已关闭，请查看 /permissions")?;
        let s = self
            .hub
            .session(&p.session)
            .await
            .filter(|s| s.generation == p.generation)
            .context("原 Pi 已离线，没有发送权限回答")?;
        if command == "permission" {
            return self
                .session_send(
                    &s,
                    p.payload["text"].as_str().unwrap_or_default(),
                    &[
                        ("允许一次", format!("/approve {reference}")),
                        ("拒绝", format!("/deny {reference}")),
                    ],
                )
                .await;
        }
        let decision = if command == "approve" {
            "allow"
        } else {
            "deny"
        };
        let result=self.hub.action(&s,json!({"action":"permission_answer","permissionId":p.payload["requestId"],"decision":decision,"reason":reason.trim()})).await?;
        if result["status"] != "answered" || result["decision"] != decision {
            bail!("权限回答未确认，请查看 Pi 终端");
        }
        self.permissions.remove(reference);
        self.session_send(
            &s,
            if decision == "allow" {
                "已允许本次操作。这是权限回执，任务仍可能继续。"
            } else {
                "已拒绝本次操作。"
            },
            &[],
        )
        .await
    }
    async fn answer_question(
        &mut self,
        command: &str,
        args: &str,
        explicit: Option<&str>,
        quote: Option<&str>,
    ) -> Result<()> {
        let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let (reference, body) = if first.starts_with('q') {
            (first.to_string(), rest.to_string())
        } else {
            let s = self.target(explicit, quote).await?;
            let matches: Vec<_> = self
                .questions
                .iter()
                .filter(|(_, q)| q.session == s.id)
                .collect();
            if matches.len() != 1 {
                bail!("请使用问卷消息中的 /answer qID 回答");
            }
            (matches[0].0.clone(), args.to_string())
        };
        let q = self
            .questions
            .get(&reference)
            .context("问卷已关闭或编号无效")?;
        let s = self
            .hub
            .session(&q.session)
            .await
            .filter(|s| s.generation == q.generation)
            .context("问卷来源 Pi 已离线")?;
        let questions = q.payload["questions"].as_array().context("问卷无效")?;
        let mut action = json!({"action":"answer","toolCallId":q.payload["toolCallId"]});
        if command == "cancel" {
            action["cancel"] = json!(true);
        } else {
            if command == "pick" && (questions.len() != 1 || questions[0]["multiSelect"] == true) {
                bail!("请按问卷中的多题或多选指引回答");
            }
            let body = if command == "pick" {
                body.split_whitespace().next().unwrap_or_default()
            } else {
                &body
            };
            action["answers"] = parse_answers(questions, body)?;
        }
        let result = self.hub.action(&s, action).await?;
        if result["status"] != "answered" {
            bail!("Pi 尚未确认问卷回答，请查看终端后再重试");
        }
        self.questions.remove(&reference);
        self.session_send(
            &s,
            if command == "cancel" {
                "问卷已取消。"
            } else {
                "回答已被 Pi 接受。"
            },
            &[],
        )
        .await
    }
}
fn parse_answers(questions: &[Value], text: &str) -> Result<Value> {
    let pieces: Vec<_> = if questions.len() == 1 {
        vec![text.trim()]
    } else {
        text.split(';').map(str::trim).collect()
    };
    if pieces.len() != questions.len() || pieces.iter().any(|s| s.is_empty()) {
        bail!("请回答全部 {} 题，题目之间用分号分隔", questions.len());
    }
    let mut answers = Vec::new();
    for (index, (q, part)) in questions.iter().zip(pieces).enumerate() {
        let numeric = part
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == '，' || c.is_whitespace());
        if numeric {
            let indexes: Vec<usize> = part
                .split([',', '，'])
                .map(|s| {
                    s.trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                })
                .collect::<Option<Vec<_>>>()
                .context("选项编号无效")?;
            let options = q["options"].as_array().context("问卷选项无效")?;
            if (q["multiSelect"] != true && indexes.len() != 1)
                || indexes.iter().any(|n| *n >= options.len())
                || indexes
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    != indexes.len()
            {
                bail!("第 {} 题选项无效", index + 1);
            }
            answers.push(json!({"questionIndex":index,"optionIndexes":indexes}));
        } else {
            let text = part.strip_prefix('=').unwrap_or(part);
            if text.trim().is_empty() {
                bail!("请在 = 后填写回答再发送");
            }
            answers.push(json!({"questionIndex":index,"text":text}));
        }
    }
    Ok(json!(answers))
}
fn page(arg: &str, count: usize, size: usize) -> Result<(usize, usize, usize)> {
    let pages = count.div_ceil(size).max(1);
    let page = if arg.is_empty() {
        1
    } else {
        arg.parse::<usize>()
            .ok()
            .filter(|p| *p > 0)
            .context("页码应为正整数")?
    }
    .min(pages);
    Ok((page, pages, (page - 1) * size))
}
fn is_slot(s: &str, prefix: char) -> bool {
    s.starts_with(prefix) && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit())
}
fn pages_buttons(command: &str, page: usize, pages: usize) -> Vec<(&'static str, String)> {
    let mut b = Vec::new();
    if page > 1 {
        b.push(("上一页", format!("{command} {}", page - 1)));
    }
    if page < pages {
        b.push(("下一页", format!("{command} {}", page + 1)));
    }
    b
}
fn navigation(s: &Session) -> Vec<(&'static str, String)> {
    vec![
        ("选择此 Pi", format!("/use {}", s.slot)),
        ("Pi 控制", format!("/to {} /commands", s.slot)),
        ("查看终端", format!("/peek {}", s.slot)),
        ("会话列表", "/sessions".into()),
    ]
}
pub fn keyboard(buttons: &[(&str, String)]) -> Option<Value> {
    if buttons.is_empty() {
        return None;
    }
    let rows:Vec<_>=buttons.chunks(2).take(5).enumerate().map(|(row,buttons)|json!({"buttons":buttons.iter().enumerate().map(|(col,(label,command))|json!({"id":format!("{row}-{col}"),"render_data":{"label":label,"visited_label":label,"style":1},"action":{"type":2,"permission":{"type":2},"data":command,"enter":!command.ends_with([' ','=']),"reply":false,"unsupport_tips":"请发送文字指令"}})).collect::<Vec<_>>()})).collect();
    Some(json!({"content":{"rows":rows}}))
}
fn markdown_text(text: &str) -> String {
    let mut result = String::new();
    for c in text.chars() {
        if "\\`*_{}[]()#+.!<>|~-".contains(c) {
            result.push('\\');
        }
        result.push(c);
    }
    result
}
fn command_link(command: &str, label: &str) -> String {
    let text = format!("{command} · {}", clean(label, 65));
    format!(
        "<qqbot-cmd-enter text=\"{}\" />",
        url::form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>()
    )
}
fn state_name(state: &str) -> &str {
    match state {
        "idle" => "可发送消息",
        "working" => "正在处理任务",
        "waiting_for_answer" => "等待问卷回答",
        "waiting_for_permission" => "等待权限决定",
        "waiting_for_input" => "等待终端确认",
        "compacting" => "正在整理上下文",
        _ => "正在连接",
    }
}
