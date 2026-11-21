use crate::{
    bridge::{BridgeEvent, Hub},
    commands,
    herdr::{Core, Launch},
    media,
    model::{PendingLaunch, ResumeTarget, Session, Workspace, WorkspaceId, clean},
    outbox::{Delivery, Outbox, ReplyContext},
    qq, sessions,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

const HELP: &str = "Pi QQ\n\n/workspaces · Workspaces 列表\n/sessions [all] [页码] · Sessions 列表，含可恢复的记录\n/use sN · 选择在线 Session\n/resume sN · 恢复最近关闭的 Session\n/new [sN] · 新建 Pi（没有在线 Pi 也可创建）\n/name [sN] 名称 · 重命名 Session\n/status · 当前 Session 状态\n/peek [sN] · 查看当前输出\n/stop · 停止任务\n/close [sN] · 确认后关闭 Pi\n\n/commands · 模型与命令\n/model、/thinking、/compact、/reload\n/pi 命令 参数 · 已安装 Pi 命令\n/to sN 内容 · 指定 Session\n/steer 内容 · 反馈当前任务\n\n/answer qID 1 · 回答问卷\n/answer qID 1,2;=文字 · 多选/多题\n/cancel qID · 取消问卷\n/permissions · 等待中的权限\n/approve pID、/deny pID [原因]\n\n普通消息会发给选中的 Pi。引用通知回复可以继续原 Session；支持图片、视频、文件。/stop 保留 Pi，/close 才关闭终端。";

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
    questions: HashMap<String, Interaction>,
    permissions: HashMap<String, Interaction>,
    closes: HashMap<String, Close>,
    reply_source: Option<String>,
    reply_context: Option<ReplyContext>,
}
impl Router {
    pub fn new(hub: Hub, outbox: Outbox, core: Arc<RwLock<Option<Core>>>) -> Self {
        Self {
            expected_scope: None,
            hub,
            outbox,
            core,
            questions: HashMap::new(),
            permissions: HashMap::new(),
            closes: HashMap::new(),
            reply_source: None,
            reply_context: None,
        }
    }
    fn delivery(&self) -> Delivery<'_> {
        Delivery::Interactive {
            scope: self.expected_scope.as_deref(),
            reply: self.reply_context.as_ref(),
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
                self.delivery(),
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
        let label = clean(&s.label, 100);
        let label = if buttons.is_empty() {
            label
        } else {
            markdown_text(&label)
        };
        self.send(
            &format!("[{} · {label}]\n{text}", s.slot),
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
        self.reply_source = None;
        self.reply_context = Some(ReplyContext::new(id));
        let result = self.route(http, &message).await;
        if let Err(error) = result {
            self.send(&format!("{}", error), self.reply_source.as_deref(), &[])
                .await?;
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
        let (quoted, selected, workspace) = {
            let routes = self.hub.routes.lock().await;
            let quoted = quote
                .map(|reference| {
                    routes
                        .sources
                        .get(reference)
                        .cloned()
                        .context("这条引用没有可识别的 Pi 来源，请使用 /sessions 重新选择")
                })
                .transpose()?;
            (quoted, routes.selected.clone(), routes.workspace.clone())
        };
        if let Some(slot) = explicit {
            let session = sessions
                .iter()
                .find(|s| s.slot == slot)
                .context("指定 Pi 已离线，请刷新 /sessions")?;
            if quoted.as_ref().is_some_and(|id| id != &session.id) {
                bail!("指定 Session 与引用来源不同，请明确选择后重发");
            }
            return Ok(session.clone());
        }
        if let Some(id) = quoted {
            return sessions
                .into_iter()
                .find(|s| s.id == id)
                .context("引用来源 Pi 已离线，消息没有发给其他 Pi");
        }
        let members = if let Some(key) = workspace {
            let core = self.core().await?;
            let catalog = core.catalog.read().await;
            let workspace = catalog
                .workspaces
                .iter()
                .find(|w| w.key == key)
                .context("选中的 Workspace 已不可用，请重新发送 /workspaces 选择")?;
            Some(workspace.terminals.clone())
        } else {
            None
        };
        let in_workspace = |s: &Session| {
            members.as_ref().is_none_or(|terminals| {
                s.terminal
                    .as_ref()
                    .is_some_and(|terminal| terminals.contains(terminal))
            })
        };
        if let Some(id) = selected {
            let session = sessions
                .iter()
                .find(|s| s.id == id)
                .context("原 Session 已离线。请发送 /sessions 重新选择；消息没有发给其他 Pi")?;
            if !in_workspace(session) {
                bail!("选中的 Session 不在当前 Workspace，请发送 /sessions 重新选择，消息未发送。");
            }
            return Ok(session.clone());
        }
        let mut eligible = sessions.into_iter().filter(in_workspace);
        if let Some(session) = eligible.next() {
            if eligible.next().is_none() {
                return Ok(session);
            }
        }
        bail!("请先发送 /sessions 选择 Session；没有在线 Pi 时可发送 /new 创建")
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
    async fn list_sessions(&self, args: &str) -> Result<()> {
        let all = args.split_whitespace().any(|arg| arg == "all");
        let page_arg = args
            .split_whitespace()
            .find(|arg| *arg != "all")
            .unwrap_or("");
        let mut online = self.hub.sessions().await;
        let (selected, workspace, ranks, mut recent) = {
            let routes = self.hub.routes.lock().await;
            let mut ranks = HashMap::new();
            let mut recent = Vec::new();
            for (index, session) in routes.recent_sessions.iter().enumerate() {
                ranks.insert(session.session_id.clone(), index);
                if !online.iter().any(|s| s.session_id == session.session_id) {
                    if let Some(slot) = routes.slots.get(&session.peer_id) {
                        recent.push((slot.clone(), session.clone()));
                    }
                }
            }
            (
                routes.selected.clone(),
                routes.workspace.clone(),
                ranks,
                recent,
            )
        };
        let core = self.core.read().await.clone();
        let mut workspace_name = None;
        if let (false, Some(key), Some(core)) = (all, workspace, &core) {
            let catalog = core.catalog.read().await;
            if let Some(workspace) = catalog.workspaces.iter().find(|w| w.key == key) {
                workspace_name = Some(list_label(&workspace.name));
                online.retain(|s| {
                    s.terminal
                        .as_ref()
                        .is_some_and(|t| workspace.terminals.contains(t))
                });
                let unique_cwd = !workspace.cwd.is_empty()
                    && catalog
                        .workspaces
                        .iter()
                        .filter(|w| w.cwd == workspace.cwd)
                        .count()
                        == 1;
                recent.retain(|(_, s)| {
                    s.workspace.as_ref() == Some(&key)
                        || (s.workspace.is_none() && unique_cwd && s.cwd == workspace.cwd)
                });
            }
        }
        let offline = match core {
            Some(core) if !recent.is_empty() => {
                tokio::task::spawn_blocking(move || {
                    recent.retain(|(_, s)| {
                        sessions::saved_file(s, &core.environment.agent)
                            .is_ok_and(|path| path.is_some())
                    });
                    recent
                })
                .await?
            }
            _ => Vec::new(),
        };
        let current = online
            .iter()
            .position(|s| selected.as_ref() == Some(&s.id))
            .map(|index| online.remove(index));
        online.sort_by_key(|s| ranks.get(&s.session_id).copied().unwrap_or(usize::MAX));
        let count = online.len() + offline.len();
        let (page, pages, start) = page(page_arg, count, 8)?;
        let mut heading = "Sessions".to_string();
        if let Some(name) = &workspace_name {
            heading.push_str(&format!(" · {name}"));
        }
        if pages > 1 {
            heading.push_str(&format!(" · {page}/{pages}"));
        }
        let mut sections = vec![heading];
        if let Some(s) = &current {
            let name = list_label(&s.label);
            let state = session_list_state(&s.state);
            sections.push(format!("当前 Session\n**{name}**{state}"));
        }
        let online_heading = if current.is_some() {
            "其他在线"
        } else {
            "在线"
        };
        let live_rows = online.iter().map(|s| {
            (
                online_heading,
                &s.label,
                format!("/use {}", s.slot),
                session_list_state(&s.state),
            )
        });
        let saved_rows = offline.iter().map(|(slot, s)| {
            (
                "最近可恢复",
                &s.label,
                format!("/resume {slot}"),
                String::new(),
            )
        });
        let mut last_heading = "";
        for (heading, name, command, state) in live_rows.chain(saved_rows).skip(start).take(8) {
            if heading != last_heading {
                sections.push(format!("**{heading}**"));
                last_heading = heading;
            }
            let section = sections.last_mut().unwrap();
            section.push_str(&format!(
                "\n{} · {}{state}",
                list_label(name),
                command_link(&command, "")
            ));
        }
        if current.is_none() && count == 0 {
            sections.push("暂无在线或可恢复的 Session。".into());
        }
        let mut buttons = Vec::new();
        if let Some(s) = &current {
            buttons.push(("重命名当前", format!("/name {} ", s.slot)));
        }
        buttons.push(("新建 Pi", "/new".into()));
        if workspace_name.is_some() {
            buttons.push(("全部", "/sessions all".into()));
        }
        buttons.push(("Workspaces", "/workspaces".into()));
        buttons.extend(pages_buttons(
            if all { "/sessions all" } else { "/sessions" },
            page,
            pages,
        ));
        self.send(&sections.join("\n\n"), None, &buttons).await
    }

    async fn session_workspace(&self, session: &Session) -> Option<WorkspaceId> {
        let terminal = session.terminal.as_ref()?;
        let core = self.core.read().await.clone()?;
        let catalog = core.catalog.read().await;
        catalog
            .workspaces
            .iter()
            .find(|workspace| workspace.terminals.contains(terminal))
            .map(|workspace| workspace.key.clone())
    }

    async fn activate_session(&mut self, session: &Session) -> Result<()> {
        self.reply_source = Some(session.id.clone());
        let workspace = self.session_workspace(session).await;
        {
            let mut routes = self.hub.routes.lock().await;
            routes.remember_session(session, true);
            if workspace.is_some() {
                routes.set_session_workspace(session, workspace.clone());
            }
            routes.selected = Some(session.id.clone());
            routes.workspace = workspace;
        }
        self.hub.save_routes().await?;
        self.session_send(session, "已选择，可以直接发送消息。", &navigation(session))
            .await
    }

    async fn resume_session(&mut self, args: &str) -> Result<()> {
        let slot = args
            .split_whitespace()
            .next()
            .filter(|slot| is_slot(slot, 's'))
            .context("用法：/resume sN")?;
        let recent = {
            let routes = self.hub.routes.lock().await;
            let session_id = routes
                .slots
                .iter()
                .find(|(_, saved)| *saved == slot)
                .and_then(|(peer_id, _)| peer_id.rsplit_once(':'))
                .map(|(id, _)| id);
            routes
                .recent_sessions
                .iter()
                .find(|s| Some(s.session_id.as_str()) == session_id)
                .cloned()
        }
        .context("找不到这个 Session 的最近记录，请刷新 /sessions")?;
        let online = self.hub.sessions().await;
        let mut copies = online.iter().filter(|s| s.session_id == recent.session_id);
        if let Some(session) = copies.next() {
            if copies.next().is_some() || !sessions::same_directory(&session.cwd, &recent.cwd) {
                bail!("这个 ID 对应多个 Session 或目录已变化，请在 /sessions 中选择在线的 Session");
            }
            return self.activate_session(session).await;
        }
        let core = self.core().await?;
        let workspace = {
            let catalog = core.catalog.read().await;
            if !catalog.ready {
                bail!("Herdr 正在连接，请稍后重试");
            }
            if let Some(key) = recent.workspace.as_ref() {
                catalog
                    .workspaces
                    .iter()
                    .find(|w| &w.key == key)
                    .map(|w| w.key.clone())
            } else {
                let mut matching = catalog.workspaces.iter().filter(|w| w.cwd == recent.cwd);
                let first = matching.next();
                first
                    .filter(|_| matching.next().is_none())
                    .map(|w| w.key.clone())
            }
        };
        let agent = core.environment.agent.clone();
        let current = recent.clone();
        let switched = online.iter().any(|s| s.pid == recent.pid);
        let file = tokio::task::spawn_blocking(move || {
            if !switched && sessions::process_is_live(&current) {
                bail!("原 Pi 进程仍在运行，可能正在重连。请稍后刷新 /sessions");
            }
            sessions::saved_file(&current, &agent)?
                .context("找不到原 Session 的历史文件，或项目目录已不可用")
        })
        .await??;
        self.launch_session(
            &core,
            &recent.cwd,
            workspace,
            Launch::Resume {
                session_id: &recent.session_id,
                file: &file,
            },
        )
        .await
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
            bail!("这条引用无法识别来源，请先 /use 选择 Session");
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
        let input = text.strip_prefix('/').or_else(|| {
            let name = text.split_whitespace().next().unwrap_or_default();
            name.eq_ignore_ascii_case("answer").then_some(text.as_str())
        });
        let (command, args) = if let Some(rest) = input {
            let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            (command.to_ascii_lowercase(), args.trim().to_string())
        } else {
            (String::new(), text.clone())
        };
        if media::has_attachments(message) && !matches!(command.as_str(), "" | "steer") {
            bail!("请将命令和附件分开发送。附件可配普通文字、/to sN 内容或 /steer 内容。");
        }
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
                            ("Workspaces", "/workspaces".into()),
                            ("Sessions", "/sessions all".into()),
                            ("新建 Pi", "/new".into()),
                            ("命令用法", "/help commands".into()),
                        ],
                    )
                    .await;
            }
            "workspaces" => {
                let all = self.workspaces().await?;
                let (page, pages, start) = page(&args, all.len(), 8)?;
                let selected = self.hub.routes.lock().await.workspace.clone();
                let sessions = self.hub.sessions().await;
                let mut lines = vec![list_heading("Workspaces", all.len(), page, pages)];
                for (slot, w) in all.iter().skip(start).take(8) {
                    let count = sessions
                        .iter()
                        .filter(|s| {
                            s.terminal
                                .as_ref()
                                .is_some_and(|terminal| w.terminals.contains(terminal))
                        })
                        .count();
                    lines.push(selection_row(
                        &w.name,
                        &format!("/workspace {slot}"),
                        selected.as_ref() == Some(&w.key),
                        &format!("{count} 个 Session"),
                    ));
                }
                if all.is_empty() {
                    lines.push("暂无 Herdr Workspace。可在 Herdr 中创建，或直接 /new，在用户主目录启动第一个 Pi。".into());
                }
                let mut buttons = pages_buttons("/workspaces", page, pages);
                buttons.push(("全部 Sessions", "/sessions all".into()));
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
                    .context("Workspace 不存在，请刷新 /workspaces")?;
                let members: Vec<_> = self
                    .hub
                    .sessions()
                    .await
                    .into_iter()
                    .filter(|s| {
                        s.terminal
                            .as_ref()
                            .is_some_and(|terminal| w.terminals.contains(terminal))
                    })
                    .collect();
                {
                    let mut routes = self.hub.routes.lock().await;
                    routes.workspace = Some(w.key.clone());
                    routes.selected = (members.len() == 1).then(|| members[0].id.clone());
                    if let [session] = members.as_slice() {
                        routes.remember_session(session, true);
                        routes.set_session_workspace(session, Some(w.key));
                    }
                }
                self.hub.save_routes().await?;
                return self
                    .send(
                        &format!(
                            "已选择 Workspace：{}\n新建 Pi 将使用此 Workspace。",
                            clean(&w.name, 100)
                        ),
                        None,
                        &[("Sessions", "/sessions".into()), ("新建 Pi", "/new".into())],
                    )
                    .await;
            }
            "sessions" => return self.list_sessions(&args).await,
            "use" => {
                let slot = args.split_whitespace().next().context("用法：/use sN")?;
                let session = self.target(Some(slot), None).await?;
                return self.activate_session(&session).await;
            }
            "resume" => return self.resume_session(&args).await,
            "new" => {
                let core = self.core().await?;
                let source = if args.is_empty() {
                    None
                } else {
                    Some(
                        self.target(Some(args.split_whitespace().next().unwrap()), None)
                            .await?,
                    )
                };
                self.reply_source = source.as_ref().map(|s| s.id.clone());
                let selected = self.hub.routes.lock().await.workspace.clone();
                let workspaces = self.workspaces().await?;
                let workspace = if let Some(key) = selected {
                    Some(
                        workspaces
                            .iter()
                            .find(|(_, w)| w.key == key)
                            .context("Workspace 已不可用，请通过 /workspaces 重新选择")?
                            .1
                            .clone(),
                    )
                } else if let Some(source) = source.as_ref() {
                    workspaces
                        .iter()
                        .find(|(_, w)| {
                            source
                                .terminal
                                .as_ref()
                                .is_some_and(|terminal| w.terminals.contains(terminal))
                        })
                        .map(|(_, w)| w.clone())
                } else if workspaces.len() == 1 {
                    Some(workspaces[0].1.clone())
                } else if workspaces.len() > 1 {
                    let mut lines = vec!["请选择新 Pi 的 Workspace，选好后点击“新建 Pi”：".into()];
                    for (slot, workspace) in workspaces.iter().take(8) {
                        lines.push(selection_row(
                            &workspace.name,
                            &format!("/workspace {slot}"),
                            false,
                            "",
                        ));
                    }
                    return self
                        .send(
                            &lines.join("\n\n"),
                            None,
                            &[("Workspaces", "/workspaces".into())],
                        )
                        .await;
                } else {
                    None
                };
                let key = workspace.as_ref().map(|w| w.key.clone());
                let cwd = workspace
                    .as_ref()
                    .map(|w| w.cwd.clone())
                    .or_else(|| source.as_ref().map(|s| s.cwd.clone()))
                    .unwrap_or_else(|| core.environment.home.to_string_lossy().into_owned());
                if let Some(key) = &key {
                    self.hub.routes.lock().await.workspace = Some(key.clone());
                    self.hub.save_routes().await?;
                }
                return self
                    .launch_session(&core, &cwd, key, Launch::New(source.as_ref()))
                    .await;
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
        self.reply_source = Some(session.id.clone());
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
                        "当前输出（Pi 屏幕文字）\n{}",
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
                    "已请求停止当前任务，Session 保留。",
                    &navigation(&session),
                )
                .await
            }
            "name" => {
                if scoped.is_empty()
                    || scoped.chars().count() > 70
                    || scoped.chars().any(char::is_control)
                {
                    bail!("用法：/name [sN] 名称（单行，1–70 字）");
                }
                let result = self
                    .hub
                    .action(&session, json!({"action":"name","name":scoped}))
                    .await?;
                if result["status"] != "renamed" {
                    bail!("Pi 未确认重命名，请查看终端后再重试。");
                }
                let renamed = self.hub.session(&session.id).await.unwrap_or(Session {
                    label: scoped.into(),
                    ..session
                });
                self.hub.save_routes().await?;
                self.session_send(&renamed, "名称已更新。", &navigation(&renamed))
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
                if result["status"] != "completed" || !result["requests"].is_array() {
                    bail!("Pi 未确认权限列表，请查看终端后再重试。");
                }
                let active = result["requests"].as_array().cloned().unwrap();
                self.permissions.retain(|_, p| {
                    p.session != session.id
                        || active
                            .iter()
                            .any(|a| a["requestId"] == p.payload["requestId"])
                });
                for payload in active {
                    self.open_permission(&session, payload, true).await?;
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
                        self.delivery(),
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
    async fn launch_session(
        &mut self,
        core: &Core,
        cwd: &str,
        workspace: Option<WorkspaceId>,
        launch: Launch<'_>,
    ) -> Result<()> {
        let scope = self
            .expected_scope
            .clone()
            .context("QQ 消息缺少绑定来源，操作未执行")?;
        let resume = match &launch {
            Launch::Resume { session_id, .. } => Some(ResumeTarget {
                session_id: (*session_id).into(),
                cwd: cwd.into(),
            }),
            Launch::New(_) => None,
        };
        let restoring = resume.is_some();
        let (terminal, pid) = core.launch(cwd, workspace.as_ref(), launch).await?;
        let core_id = core.catalog.read().await.core_id.clone();
        {
            let mut routes = self.hub.routes.lock().await;
            routes
                .pending_launches
                .retain(|_, launch| launch.expires > now());
            routes.pending_launches.insert(
                terminal.clone(),
                PendingLaunch {
                    scope,
                    core_id,
                    terminal: terminal.clone(),
                    pid,
                    expires: now() + 5 * 60,
                    workspace,
                    resume,
                },
            );
        }
        self.hub.save_routes().await?;
        let note = if restoring {
            "正在打开原 Session，等 Pi 连接后即可继续。"
        } else {
            "Pi 终端已创建，正在等待扩展连接……"
        };
        self.send(note, None, &[]).await?;
        // The Core acknowledgement does not prove Pi has registered.
        for _ in 0..60 {
            if let Some(session) = self
                .hub
                .sessions()
                .await
                .into_iter()
                .find(|s| s.terminal.as_deref() == Some(&terminal))
            {
                if self.complete_launch(&session, true).await? {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let note = if restoring {
            "原 Session 尚未确认连接。连接成功后会补发通知，可先在 Herdr 查看进度。"
        } else {
            "终端已创建，但 Pi 扩展尚未确认连接。连接成功后会补发通知；可查看 /sessions 和 Herdr，请不要重复 /new。"
        };
        self.send(note, None, &[("Sessions", "/sessions all".into())])
            .await
    }

    async fn complete_launch(&mut self, session: &Session, select: bool) -> Result<bool> {
        let Some(terminal) = session.terminal.as_ref() else {
            return Ok(false);
        };
        let pending = self
            .hub
            .routes
            .lock()
            .await
            .pending_launches
            .get(terminal)
            .cloned();
        let Some(pending) = pending else {
            return Ok(false);
        };
        if pending.expires <= now() {
            self.hub
                .routes
                .lock()
                .await
                .pending_launches
                .remove(terminal);
            self.hub.save_routes().await?;
            return Ok(false);
        }
        if pending.resume.as_ref().is_some_and(|target| {
            session.session_id != target.session_id
                || !sessions::same_directory(&session.cwd, &target.cwd)
        }) {
            return Ok(false);
        }
        let core = self.core().await?;
        if core.catalog.read().await.core_id != pending.core_id {
            return Ok(false);
        }
        let record = core.validate(session).await?;
        if record["pid"].as_u64() != Some(pending.pid as u64) {
            return Ok(false);
        }
        let Some(session) = self
            .hub
            .session(&session.id)
            .await
            .filter(|s| s.generation == session.generation)
        else {
            return Ok(false);
        };
        let text = match (pending.resume.is_some(), select) {
            (true, true) => "已恢复原 Session，可以继续发送消息。",
            (true, false) => "原 Session 已恢复，点击「选择 Session」继续。",
            (false, true) => "新 Pi 已连接并选中。",
            (false, false) => "新 Pi 已连接，点击「选择 Session」继续。",
        };
        self.outbox
            .enqueue(
                Delivery::Interactive {
                    scope: Some(&pending.scope),
                    reply: self.reply_context.as_ref(),
                },
                &format!(
                    "[{} · {}]\n{text}",
                    session.slot,
                    markdown_text(&clean(&session.label, 100))
                ),
                Some(&session.id),
                keyboard(&navigation(&session)),
                true,
                None,
            )
            .await?;
        let workspace = self.session_workspace(&session).await.or(pending.workspace);
        {
            let mut routes = self.hub.routes.lock().await;
            routes.pending_launches.remove(terminal);
            routes.remember_session(&session, select);
            routes.set_session_workspace(&session, workspace.clone());
            if select {
                routes.selected = Some(session.id.clone());
                routes.workspace = workspace;
            }
        }
        self.hub.save_routes().await?;
        Ok(true)
    }
    async fn confirm_close(&mut self, args: &str) -> Result<()> {
        let ticket = self
            .closes
            .remove(args)
            .context("关闭确认已失效，请重新 /close")?;
        self.reply_source = Some(ticket.session.id.clone());
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
        let workspace = self.session_workspace(&session).await;
        {
            let mut routes = self.hub.routes.lock().await;
            routes.remember_session(&session, true);
            if workspace.is_some() {
                routes.set_session_workspace(&session, workspace);
            }
        }
        self.hub.save_routes().await?;
        core.request("terminal.kill", json!({"terminal":session.terminal}))
            .await?;
        self.session_send(
            &session,
            "Herdr 已接受关闭请求。",
            &[("Sessions", "/sessions".into())],
        )
        .await
    }
    pub async fn event(&mut self, event: BridgeEvent) -> Result<()> {
        self.reply_context = None;
        match event {
            BridgeEvent::Registered(session) => {
                if !self.complete_launch(&session, false).await? {
                    if let Some(workspace) = self.session_workspace(&session).await {
                        self.hub
                            .routes
                            .lock()
                            .await
                            .set_session_workspace(&session, Some(workspace));
                        self.hub.save_routes().await?;
                    }
                }
            }
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
                    "permission_open" => self.open_permission(&s, message, false).await?,
                    "permission_close" => self.permissions.retain(|_, p| {
                        p.session != session || p.payload["requestId"] != message["permissionId"]
                    }),
                    _ => {}
                }
            }
        }
        Ok(())
    }
    async fn open_permission(
        &mut self,
        s: &Session,
        payload: Value,
        redisplay: bool,
    ) -> Result<()> {
        let text = payload["text"]
            .as_str()
            .filter(|s| s.len() <= 12000)
            .context("权限内容超出手机可显示范围，请在终端处理")?;
        if !payload["requestId"].is_string() {
            bail!("权限请求无效");
        }
        let existing = self
            .permissions
            .iter()
            .find(|(_, p)| {
                p.session == s.id
                    && p.generation == s.generation
                    && p.payload["requestId"] == payload["requestId"]
            })
            .map(|(reference, _)| reference.clone());
        if existing.is_some() && !redisplay {
            return Ok(());
        }
        let reference = existing
            .unwrap_or_else(|| format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
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
        self.reply_source = Some(p.session.clone());
        let s = self
            .hub
            .session(&p.session)
            .await
            .filter(|s| s.generation == p.generation)
            .context("原 Pi 已离线，没有发送权限回答")?;
        self.reply_source = Some(s.id.clone());
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
        let selector = first.to_ascii_lowercase();
        let question_id = selector.starts_with('q')
            && selector.len() > 1
            && selector[1..].chars().all(|c| c.is_ascii_hexdigit());
        let slot = is_slot(&selector, 's').then_some(selector.as_str());
        if slot.is_some() && explicit.is_some() && slot != explicit {
            bail!("指定了两个不同的 Pi，请明确选择后重发。");
        }
        let body = if question_id || slot.is_some() {
            rest
        } else {
            args
        };
        let reference = if question_id {
            selector.clone()
        } else {
            let selected = self.hub.routes.lock().await.selected.is_some();
            let target = if slot.is_some() || explicit.is_some() || quote.is_some() || selected {
                let s = self.target(slot.or(explicit), quote).await?;
                self.reply_source = Some(s.id.clone());
                Some(s.id)
            } else {
                None
            };
            let sessions = self.hub.sessions().await;
            let workspace = self.hub.routes.lock().await.workspace.clone();
            let terminals = if target.is_none() {
                if let Some(key) = workspace {
                    let core = self.core().await?;
                    let catalog = core.catalog.read().await;
                    Some(
                        catalog
                            .workspaces
                            .iter()
                            .find(|w| w.key == key)
                            .context("Workspace 已不可用，请重新选择 /workspaces")?
                            .terminals
                            .clone(),
                    )
                } else {
                    None
                }
            } else {
                None
            };
            let candidates: Vec<_> = self
                .questions
                .iter()
                .filter(|(_, q)| {
                    sessions.iter().any(|s| {
                        s.id == q.session
                            && s.generation == q.generation
                            && target.as_ref().is_none_or(|id| id == &s.id)
                            && terminals.as_ref().is_none_or(|terminals| {
                                s.terminal.as_ref().is_some_and(|t| terminals.contains(t))
                            })
                    })
                })
                .collect();
            if candidates.len() != 1 {
                bail!("没有唯一的待回答问卷，请使用问卷消息中的 /answer qID 回答。");
            }
            candidates[0].0.clone()
        };
        let q = self
            .questions
            .get(&reference)
            .context("问卷已关闭或编号无效")?;
        self.reply_source = Some(q.session.clone());
        let s = self
            .hub
            .session(&q.session)
            .await
            .filter(|s| s.generation == q.generation)
            .context("问卷来源 Pi 已离线")?;
        self.reply_source = Some(s.id.clone());
        if question_id && (explicit.is_some() || quote.is_some()) {
            let target = self.target(explicit, quote).await?;
            if target.id != s.id {
                bail!("问卷编号与指定 Pi 不同，回答未发送。");
            }
        }
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
                body
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
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
        ("选择 Session", format!("/use {}", s.slot)),
        ("重命名", format!("/name {} ", s.slot)),
        ("模型与命令", format!("/to {} /commands", s.slot)),
        ("查看当前输出", format!("/peek {}", s.slot)),
        ("Sessions", "/sessions".into()),
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
fn list_heading(title: &str, count: usize, page: usize, pages: usize) -> String {
    if pages > 1 {
        format!("{title} · {count} 个 · {page}/{pages}")
    } else {
        format!("{title} · {count} 个")
    }
}
fn list_label(name: &str) -> String {
    markdown_text(
        &clean(name, 120)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}
fn selection_row(name: &str, command: &str, selected: bool, detail: &str) -> String {
    format!(
        "{}**{}**\n{}{}",
        if selected { "✓ " } else { "" },
        list_label(name),
        command_link(command, ""),
        if detail.is_empty() {
            String::new()
        } else {
            format!(" · {}", markdown_text(detail))
        }
    )
}
fn command_link(command: &str, label: &str) -> String {
    let text = if label.is_empty() {
        command.to_string()
    } else {
        format!("{command} · {}", clean(label, 65))
    };
    format!(
        "<qqbot-cmd-enter text=\"{}\" />",
        url::form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>()
    )
}
fn session_list_state(state: &str) -> String {
    if state == "idle" {
        String::new()
    } else {
        format!(" · {}", state_name(state))
    }
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
