use crate::{
    install::{Environment, sanitized_command},
    model::{Session, Workspace, WorkspaceId},
    relay::RemoteRelay,
};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;
use serde_json::{Value, json};
use std::{collections::HashMap, fmt, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{RwLock, mpsc, oneshot},
};

struct Request {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
}

#[derive(Debug)]
pub(crate) struct RpcFailure(pub String);

impl fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Herdr 拒绝操作：{}", self.0)
    }
}

impl std::error::Error for RpcFailure {}
#[derive(Clone, Default)]
pub struct Catalog {
    pub core_id: String,
    pub terminals: Vec<Value>,
    pub workspaces: Vec<Workspace>,
    remote_machines: Vec<Machine>,
    remote_layouts: Vec<(String, Value)>,
    pub ready: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
struct Machine {
    id: String,
    name: String,
}

impl Catalog {
    fn remote_workspaces(&self) -> Vec<Workspace> {
        self.remote_machines
            .iter()
            .filter_map(|machine| {
                let (_, doc) = self
                    .remote_layouts
                    .iter()
                    .find(|(id, _)| id == &machine.id)?;
                Some(project_workspaces(
                    doc,
                    &self.terminals,
                    &self.core_id,
                    Some(machine),
                ))
            })
            .flatten()
            .collect()
    }

    pub(crate) fn workspace(&self, key: &WorkspaceId) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|workspace| &workspace.key == key)
    }
}

#[derive(Clone)]
pub struct Core {
    requests: mpsc::Sender<Request>,
    pub catalog: Arc<RwLock<Catalog>>,
    pub environment: Environment,
}

pub(crate) enum Launch<'a> {
    New(Option<&'a Session>),
    Resume { session_id: &'a str, file: &'a Path },
}

impl Core {
    pub fn start(env: Environment) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let core = Self {
            requests: tx,
            catalog: Arc::new(RwLock::new(Catalog::default())),
            environment: env,
        };
        tokio::spawn(supervise(core.clone(), rx));
        tokio::spawn(observe(core.clone()));
        tokio::spawn(observe_workspaces(core.clone()));
        core
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request {
                method: method.into(),
                params,
                reply,
            })
            .await
            .map_err(|_| anyhow!("Herdr 连接已关闭"))?;
        tokio::time::timeout(Duration::from_secs(50), response)
            .await
            .context("Herdr 未确认操作，请检查后再重试")?
            .context("Herdr 连接中断，操作结果未确认")?
    }
    pub(crate) async fn refresh_remote_workspaces(&self) {
        let (core_id, machines) = {
            let catalog = self.catalog.read().await;
            (catalog.core_id.clone(), catalog.remote_machines.clone())
        };
        let documents = join_all(machines.into_iter().map(|machine| async move {
            let read = self.request("doc.get", json!({"name":"herdr-app", "on":machine.id}));
            let doc = tokio::time::timeout(Duration::from_secs(3), read)
                .await
                .ok()?
                .ok()?;
            Some((machine.id, doc))
        }))
        .await;
        let mut catalog = self.catalog.write().await;
        if catalog.ready && catalog.core_id == core_id {
            for (id, doc) in documents.into_iter().flatten() {
                catalog.remote_layouts.retain(|(old, _)| old != &id);
                catalog.remote_layouts.push((id, doc));
            }
            let remote = catalog.remote_workspaces();
            catalog.workspaces.retain(|w| w.key.machine_id.is_none());
            catalog.workspaces.extend(remote);
        }
    }
    pub(crate) async fn machine_connected(&self, key: &WorkspaceId) -> bool {
        let catalog = self.catalog.read().await;
        catalog.ready
            && catalog.core_id == key.core_id
            && catalog
                .remote_machines
                .iter()
                .any(|machine| Some(machine.id.as_str()) == key.machine_id.as_deref())
    }
    pub(crate) async fn machine_name(&self, id: &str) -> Option<String> {
        self.catalog
            .read()
            .await
            .remote_machines
            .iter()
            .find(|machine| machine.id == id)
            .map(|machine| machine.name.clone())
    }
    pub(crate) async fn remote_workspace(&self, key: &WorkspaceId) -> Result<Workspace> {
        let machine_id = key.machine_id.as_deref().context("缺少远程机器")?;
        if !self.machine_connected(key).await {
            bail!("这个 Workspace 的机器未连接，请在 Herdr 中检查连接");
        }
        let doc = self
            .request("doc.get", json!({"name":"herdr-app","on":machine_id}))
            .await?;
        let catalog = self.catalog.read().await;
        let machine = catalog
            .remote_machines
            .iter()
            .find(|machine| machine.id == machine_id)
            .filter(|_| catalog.ready && catalog.core_id == key.core_id)
            .context("远程机器已变化，请刷新 /workspaces")?;
        project_workspaces(&doc, &catalog.terminals, &catalog.core_id, Some(machine))
            .into_iter()
            .find(|w| &w.key == key)
            .context("远程 Workspace 已变化，请刷新 /workspaces")
    }
    pub(crate) async fn relay_terminal(
        &self,
        workspace: &WorkspaceId,
        relay: &str,
    ) -> Option<String> {
        let catalog = self.catalog.read().await;
        let machine = workspace.machine_id.as_deref()?;
        if catalog.core_id != workspace.core_id {
            return None;
        }
        catalog
            .terminals
            .iter()
            .filter(|record| record["state"] == "live" && record["labels"]["pi.qq-relay"] == relay)
            .find_map(|record| {
                record["address"]
                    .as_str()?
                    .strip_prefix(machine)?
                    .strip_prefix('/')
            })
            .map(String::from)
    }
    async fn terminal_record(&self, terminal: &str) -> Result<Value> {
        self.request("terminal.get", json!({"terminal":terminal}))
            .await
    }
    pub(crate) async fn relay_ended(&self, terminal: &str, relay: &str) -> bool {
        match self.terminal_record(terminal).await {
            Ok(record) => {
                record["state"] != "live"
                    || record["labels"]["pi.qq-relay"] != relay
                    || (!record["reason"].is_null() && record["reason"] != "spawn_pending")
            }
            Err(error) => error
                .downcast_ref::<RpcFailure>()
                .is_some_and(|e| e.0 == "not_found"),
        }
    }
    pub(crate) async fn confirm_relay(
        &self,
        terminal: &str,
        relay: &str,
        meta: &Value,
    ) -> Result<u32> {
        let record = self.terminal_record(terminal).await?;
        let pid = record["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
            .context("远程 Pi 终端尚未就绪")?;
        if record["state"] != "live"
            || !record["reason"].is_null()
            || record["labels"]["pi.qq-relay"] != relay
            || meta["terminal"].as_str() != terminal.split_once('/').map(|(_, id)| id)
            || (meta["pid"] != pid && meta["pid"] != record["foreground"]["pid"])
        {
            bail!("远程 Pi 与 Herdr 终端身份不匹配");
        }
        Ok(pid)
    }
    pub async fn validate(&self, session: &Session) -> Result<Value> {
        if let Some(remote) = &session.remote {
            if self.catalog.read().await.core_id != remote.core_id {
                bail!("Herdr Core 已变化，请刷新 Sessions");
            }
            let record = self.terminal_record(&remote.address()).await?;
            if record["state"] != "live"
                || !record["reason"].is_null()
                || record["pid"] != remote.pid
                || record["labels"]["pi.qq-relay"] != remote.relay
                || (record["pid"] != session.pid && record["foreground"]["pid"] != session.pid)
            {
                bail!("远程 Pi 终端身份已变化，请刷新 Sessions");
            }
            return Ok(record);
        }
        let terminal = session
            .terminal
            .as_deref()
            .filter(|s| s.starts_with('t') && s.chars().all(|c| c.is_ascii_alphanumeric()))
            .context("这个 Pi 没有可验证的本机 Herdr 终端")?;
        let list = self.request("terminal.list", json!({})).await?;
        let record = list["terminals"]
            .as_array()
            .and_then(|xs| xs.iter().find(|t| t["address"].as_str() == Some(terminal)))
            .context("原终端已不可用")?;
        if record["state"] != "live"
            || !record["reason"].is_null()
            || record["pid"].as_u64().is_none()
        {
            bail!("原 Pi 已退出或终端已恢复为 shell，请刷新 Sessions");
        }
        let pid = session.pid as u64;
        let direct = record["pid"].as_u64() == Some(pid)
            || record["foreground"]["pid"].as_u64() == Some(pid);
        if !direct && !process_belongs(pid as u32, record["pid"].as_u64().unwrap_or(0) as u32).await
        {
            bail!("Pi 进程与这个终端的身份已变化，操作已取消");
        }
        Ok(record.clone())
    }
    pub(crate) async fn check_resume(
        &self,
        machine: Option<&str>,
        session_id: &str,
        file: &Path,
    ) -> Result<()> {
        // A prior request with a lost response may still be starting Pi.
        let list = self.request("terminal.list", json!({})).await?;
        let prefix = if let Some(id) = machine {
            Some(
                list["machines"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|record| record["id"] == id && record["state"] == "connected")
                    .and_then(|record| record["name"].as_str())
                    .context("远程机器已断开")?,
            )
        } else {
            None
        };
        if list["terminals"]
            .as_array()
            .context("无法核对 Herdr 终端")?
            .iter()
            .any(|record| {
                record["state"] == "live"
                    && record["reason"].is_null()
                    && record["labels"]["pi.qq-resume"] == session_id
                    && record["address"].as_str().is_some_and(|address| {
                        let host = address.split_once('/').map(|(host, _)| host);
                        host == prefix || host == machine
                    })
            })
        {
            bail!("这个 Session 正在恢复或仍在 Herdr 中运行，请稍后刷新 Sessions");
        }
        if machine.is_none() && !file.is_file() {
            bail!("原 Session 历史文件已不可用");
        }
        Ok(())
    }
    pub(crate) async fn launch(
        &self,
        cwd: &str,
        workspace: Option<&WorkspaceId>,
        launch: Launch<'_>,
        remote: Option<&RemoteRelay>,
    ) -> Result<(String, u32)> {
        if remote.is_none() && !Path::new(cwd).is_dir() {
            bail!("工作目录无效，请在 Herdr 中检查项目目录，或通过 /workspaces 重新选择 Workspace");
        }
        let mut labels = json!({"pi.qq-client": "1"});
        if let Some(key) = workspace {
            let catalog = self.catalog.read().await;
            if !catalog.ready
                || catalog.core_id != key.core_id
                || (remote.is_none() && catalog.workspace(key).is_none())
            {
                bail!("Workspace 已变化，请重新发送 /workspaces");
            }
            labels["herdr.space"] = json!(key.id);
            labels["herdr.tab"] = json!(format!("tb{}", uuid::Uuid::new_v4().simple()));
        }
        let mut env = serde_json::Map::new();
        env.insert(
            "PI_QQ_RELAY".into(),
            json!(remote.map_or("", |relay| relay.relay.as_str())),
        );
        // Core merges overrides. Empty inherited Pi identities are intentional; Core
        // generates fresh HC_TERMINAL_ID/HC_SCOPE after merging these overrides.
        for name in [
            "PI_SESSION_ID",
            "PI_SESSION_FILE",
            "PI_PROVIDER",
            "PI_MODEL",
            "PI_REASONING_LEVEL",
            "PI_QQ_LAUNCH_SPEC",
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_PANE_ID",
            "HC_EXEC_ID",
        ] {
            env.insert(name.into(), json!(""));
        }
        #[cfg(not(windows))]
        if let (None, Some(path)) = (remote, crate::install::search_path()) {
            let mut paths: Vec<_> = self
                .environment
                .pi
                .parent()
                .map(std::path::PathBuf::from)
                .into_iter()
                .collect();
            paths.extend(std::env::split_paths(&path));
            if let Ok(path) = std::env::join_paths(paths) {
                env.insert("PATH".into(), json!(path.to_string_lossy()));
            }
        }
        let mut argv = if let Some(remote) = remote {
            labels["pi.qq-relay"] = json!(remote.relay);
            vec!["pi".into(), "--extension".into(), remote.extension().into()]
        } else {
            env.insert("PI_CODING_AGENT_DIR".into(), json!(self.environment.agent));
            if let Some(root) = std::env::var_os("PI_QQ_CLIENT_DIR") {
                env.insert("PI_QQ_CLIENT_DIR".into(), json!(root.to_string_lossy()));
            }
            vec![self.environment.pi.to_string_lossy().into_owned()]
        };
        let keep = match launch {
            Launch::New(source) => {
                if let Some(source) =
                    source.filter(|s| s.machine_id() == remote.map(RemoteRelay::machine))
                {
                    if let (Some(provider), Some(model)) = (
                        source.runtime["model"]["provider"].as_str(),
                        source.runtime["model"]["id"].as_str(),
                    ) {
                        argv.extend([
                            "--provider".into(),
                            provider.into(),
                            "--model".into(),
                            model.into(),
                        ]);
                    }
                    if let Some(level) = source.runtime["thinkingLevel"].as_str() {
                        argv.extend(["--thinking".into(), level.into()]);
                    }
                }
                workspace.is_some()
            }
            Launch::Resume { session_id, file } => {
                let file = file.to_str().context("Session 历史路径无法传给 Pi")?;
                labels["pi.qq-resume"] = json!(session_id);
                argv.extend(["--session".into(), file.into()]);
                false
            }
        };
        let mut params = json!({"argv":argv,"cwd":cwd,"env":env,"labels":labels,"no_parent":true,"wait":false,"keep":keep});
        if let Some(remote) = remote {
            params["on"] = json!(remote.machine());
        }
        let result = self.request("terminal.run", params).await?;
        let address = result["address"]
            .as_str()
            .context("新终端回执不完整，请检查 /sessions 后再重试")?;
        let address = if let Some(remote) = remote {
            let terminal = address
                .split_once('/')
                .map(|(_, id)| id)
                .filter(|s| terminal_id(s))
                .context("远程终端回执不完整，请检查 Herdr")?;
            format!("{}/{terminal}", remote.machine())
        } else {
            if !terminal_id(address) {
                bail!("新终端回执不完整，请检查 /sessions 后再重试");
            }
            address.into()
        };
        let pid = result["pid"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .context("新 Pi 进程未确认")?;
        Ok((address, pid))
    }
}

const MAX_CORE_FRAME: usize = 16 * 1024 * 1024;

async fn read_reply(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    framed: bool,
) -> Result<Value> {
    let bytes = if framed {
        // Herdr's nine-byte header is channel, kind, then payload length.
        let mut header = [0; 9];
        reader.read_exact(&mut header).await?;
        let length = u32::from_le_bytes(header[5..].try_into().unwrap()) as usize;
        anyhow::ensure!(
            header[..5] == [0; 5] && length <= MAX_CORE_FRAME,
            "Herdr 回执帧无效"
        );
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).await?;
        bytes
    } else {
        let mut bytes = Vec::new();
        if reader.read_until(b'\n', &mut bytes).await? == 0 {
            bail!("Herdr 连接中断");
        }
        anyhow::ensure!(bytes.len() <= MAX_CORE_FRAME, "Herdr 回执超出限制");
        bytes
    };
    Ok(serde_json::from_slice(&bytes)?)
}

async fn supervise(core: Core, mut rx: mpsc::Receiver<Request>) {
    loop {
        let attempt = async {
            let mut child = sanitized_command(&core.environment.core).arg("connect").spawn()?;
            let mut writer = child.stdin.take().context("Core input unavailable")?;
            let mut reader = BufReader::new(child.stdout.take().context("Core output unavailable")?);
            writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\",\"params\":{\"generation\":1,\"framed\":true}}\n").await?;
            let hello = tokio::time::timeout(Duration::from_secs(15), read_reply(&mut reader, false)).await??;
            let result = &hello["result"];
            let framed = result["framed"] == true;
            if result["generation"] != 1 || !result["methods"].as_array().is_some_and(|ms| ["terminal.run", "doc.get", "terminal.read", "terminal.list"].iter().all(|m| ms.contains(&json!(m)))) { bail!("Herdr Core 版本或能力不支持"); }
            {
                let mut cat = core.catalog.write().await;
                let id = result["core"]["id"].as_str().context("Herdr Core 未报告身份")?;
                if cat.core_id != id {
                    *cat = Catalog::default();
                    cat.core_id = id.into();
                }
                cat.ready = true;
                cat.error = None;
            }
            let mut pending: HashMap<u64, oneshot::Sender<Result<Value>>> = HashMap::new();
            let mut next_id = 0u64;
            let result: Result<()> = async {
                loop {
                    // Keep a partially read frame alive while sending more requests.
                    let response = read_reply(&mut reader, framed);
                    tokio::pin!(response);
                    let reply = loop {
                        tokio::select! {
                            reply = &mut response => break reply?,
                            request = rx.recv() => {
                                let Some(request) = request else { return Ok(()); };
                                if request.reply.is_closed() { continue; }
                                next_id += 1;
                                let mut payload = Vec::new();
                                if framed { payload.resize(9, 0); }
                                serde_json::to_writer(&mut payload, &json!({"jsonrpc":"2.0","id":next_id,"method":request.method,"params":request.params}))?;
                                if framed {
                                    let length = (payload.len() - 9) as u32;
                                    if length as usize > MAX_CORE_FRAME {
                                        let _ = request.reply.send(Err(anyhow!("Herdr 请求超出限制")));
                                        continue;
                                    }
                                    payload[5..9].copy_from_slice(&length.to_le_bytes());
                                } else { payload.push(b'\n'); }
                                pending.insert(next_id, request.reply);
                                writer.write_all(&payload).await?;
                            }
                        }
                    };
                    if let Some(id) = reply["id"].as_u64() && let Some(waiting) = pending.remove(&id) {
                        let response = if reply.get("error").is_some() { Err(RpcFailure(reply["error"]["data"]["kind"].as_str().unwrap_or("request_failed").into()).into()) } else { Ok(reply["result"].clone()) };
                        let _ = waiting.send(response);
                    }
                    pending.retain(|_, p| !p.is_closed());
                }
            }.await;
            for (_, waiting) in pending { let _ = waiting.send(Err(anyhow!("Herdr 连接中断，操作结果未确认；请检查后再重试"))); }
            let _ = child.kill().await; let _ = child.wait().await;
            result
        }.await;
        {
            let mut catalog = core.catalog.write().await;
            catalog.ready = false;
            catalog.error = attempt
                .err()
                .map(|error| format!("Herdr 连接失败：{error}。请检查安装和路径后重试。"));
        }
        if rx.is_closed() {
            return;
        }
        let pause = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(pause);
        loop {
            tokio::select! { _ = &mut pause => break, req = rx.recv() => match req {
                Some(req) => { let _ = req.reply.send(Err(anyhow!("Herdr 正在重连，请稍后再试"))); }, None => return,
            } }
        }
    }
}

async fn observe(core: Core) {
    loop {
        if !core.catalog.read().await.ready {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let attempt: Result<()> = async {
            let mut child = sanitized_command(&core.environment.core).arg("connect").spawn()?;
            let mut input = child.stdin.take().context("Core event input")?;
            let mut lines = BufReader::new(child.stdout.take().context("Core event output")?).lines();
            input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"events.subscribe\",\"params\":{}}\n").await?;
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = interval.tick() => { refresh(&core).await?; },
                    line = lines.next_line() => {
                        let Some(line) = line? else { bail!("event disconnected"); };
                        if line.len() > 16*1024*1024 { bail!("event too large"); }
                        let msg: Value = serde_json::from_str(&line)?;
                        if msg.get("error").is_some() { bail!("events unavailable"); }
                        if msg["method"] != "heartbeat" { refresh(&core).await?; }
                    }
                }
            }
        }.await;
        let _ = attempt;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
async fn observe_workspaces(core: Core) {
    loop {
        if core.catalog.read().await.ready {
            core.refresh_remote_workspaces().await;
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

async fn refresh(core: &Core) -> Result<()> {
    let list = core.request("terminal.list", json!({})).await?;
    let doc = core.request("doc.get", json!({"name":"herdr-app"})).await?;
    let terminals = list["terminals"]
        .as_array()
        .context("terminal list invalid")?
        .clone();
    let remote_machines = list["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|machine| machine["temporary"] != true)
        .filter_map(|machine| {
            Some((
                Machine {
                    id: nonempty(&machine["id"])?.into(),
                    name: nonempty(&machine["name"])?.into(),
                },
                machine["state"] == "connected",
            ))
        })
        .collect::<Vec<_>>();
    let mut cat = core.catalog.write().await;
    cat.terminals = terminals
        .into_iter()
        .filter_map(|mut record| {
            if let Some((name, terminal)) = record["address"].as_str()?.split_once('/') {
                let machine = remote_machines
                    .iter()
                    .find(|(machine, connected)| *connected && machine.name == name)?;
                record["address"] = json!(format!("{}/{terminal}", machine.0.id));
            }
            Some(record)
        })
        .collect();
    cat.remote_layouts
        .retain(|(id, _)| remote_machines.iter().any(|(machine, _)| &machine.id == id));
    cat.remote_machines = remote_machines
        .into_iter()
        .filter_map(|(machine, connected)| connected.then_some(machine))
        .collect();
    cat.workspaces = project_workspaces(&doc, &cat.terminals, &cat.core_id, None);
    let remote = cat.remote_workspaces();
    cat.workspaces.extend(remote);
    Ok(())
}

fn project_workspaces(
    doc: &Value,
    terminals: &[Value],
    core_id: &str,
    machine: Option<&Machine>,
) -> Vec<Workspace> {
    document_spaces(doc)
        .map(|(id, space)| {
            let members = workspace_members(space, terminals, machine.map(|m| m.id.as_str()));
            let cwd = nonempty(&space["folder"])
                .or_else(|| members.iter().find_map(|t| terminal_directory(t)))
                .unwrap_or_default();
            Workspace {
                key: WorkspaceId {
                    core_id: core_id.into(),
                    id: id.into(),
                    machine_id: machine.map(|m| m.id.clone()),
                },
                name: workspace_name(space, &members).into(),
                machine: machine.map(|m| m.name.clone()),
                cwd: cwd.into(),
                terminals: members
                    .iter()
                    .filter_map(|t| t["address"].as_str().map(String::from))
                    .collect(),
            }
        })
        .collect()
}

fn terminal_id(value: &str) -> bool {
    value.len() > 1 && value.starts_with('t') && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn document_spaces(doc: &Value) -> impl Iterator<Item = (&str, &Value)> {
    doc["value"]["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|space| space["machine"].is_null())
        .filter_map(|space| nonempty(&space["id"]).map(|id| (id, space)))
}

fn workspace_members<'a>(
    space: &Value,
    terminals: &'a [Value],
    machine: Option<&str>,
) -> Vec<&'a Value> {
    let mut addresses = Vec::new();
    for tab in space["tabs"].as_array().into_iter().flatten() {
        collect_addresses(&tab["root"], &mut addresses);
    }
    let mut members: Vec<_> = terminals
        .iter()
        .filter(|t| t["state"] == "live")
        .filter_map(|t| {
            // Layouts store bare IDs. Core qualifies addresses in cross-machine listings.
            let address = t["address"].as_str()?;
            let (host, id) = address
                .split_once('/')
                .map_or((None, address), |(host, id)| (Some(host), id));
            if host != machine {
                return None;
            }
            let position = addresses.iter().position(|&address| address == id);
            // A new terminal can carry its Workspace ID before App places it in the layout.
            (position.is_some() || t["labels"]["herdr.space"] == space["id"])
                .then_some((position.unwrap_or(usize::MAX), t))
        })
        .collect();
    members.sort_by_key(|(position, _)| *position);
    members.into_iter().map(|(_, t)| t).collect()
}

fn workspace_name<'a>(space: &'a Value, members: &[&'a Value]) -> &'a str {
    ["name", "title"]
        .into_iter()
        .filter_map(|field| nonempty(&space[field]).map(str::trim))
        .find(|&name| space["id"] != name)
        .or_else(|| {
            members.iter().find_map(|t| {
                let (name, _) = t["labels"]["where"].as_str()?.split_once(" / ")?;
                let name = name.trim();
                (!name.is_empty()).then_some(name)
            })
        })
        .or_else(|| {
            let folder = members
                .iter()
                .find_map(|t| terminal_directory(t))
                .or_else(|| nonempty(&space["folder"]))?;
            folder
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|name| !name.is_empty())
        })
        .unwrap_or("未命名 Workspace")
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.trim().is_empty())
}

fn terminal_directory(terminal: &Value) -> Option<&str> {
    nonempty(&terminal["foreground"]["cwd"]).or_else(|| nonempty(&terminal["cwd"]))
}

fn collect_addresses<'a>(value: &'a Value, result: &mut Vec<&'a str>) {
    if let Some(s) = value["content"]["terminal"].as_str() {
        result.push(s);
    }
    if let Some(children) = value["children"].as_array() {
        for child in children {
            collect_addresses(child, result);
        }
    }
}

async fn process_belongs(pid: u32, ancestor: u32) -> bool {
    if pid == 0 || ancestor == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let script = format!(
            "$p={pid}; for($i=0;$i -lt 32 -and $p -gt 0;$i++){{if($p -eq {ancestor}){{Write-Output 'yes';exit}};$r=Get-CimInstance Win32_Process -Filter \"ProcessId=$p\";if(!$r){{exit}};$p=$r.ParentProcessId}}"
        );
        let output = tokio::time::timeout(
            Duration::from_secs(4),
            sanitized_command(std::path::Path::new("powershell.exe"))
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output(),
        )
        .await;
        return output.ok().and_then(Result::ok).is_some_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "yes"
        });
    }
    #[cfg(not(windows))]
    {
        tokio::time::timeout(Duration::from_secs(4), async {
            let mut current = pid;
            for _ in 0..32 {
                if current == ancestor {
                    return true;
                }
                if current <= 1 {
                    return false;
                }
                let output = match sanitized_command(std::path::Path::new("/bin/ps"))
                    .args(["-o", "ppid=", "-p", &current.to_string()])
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => output,
                    _ => return false,
                };
                let parent = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(0);
                if parent == current {
                    return false;
                }
                current = parent;
            }
            false
        })
        .await
        .unwrap_or(false)
    }
}
