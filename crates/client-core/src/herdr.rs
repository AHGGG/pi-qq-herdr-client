use crate::{
    install::{Environment, sanitized_command},
    model::{Session, Workspace, WorkspaceId},
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{RwLock, mpsc, oneshot},
};

struct Request {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
}
#[derive(Clone, Default)]
pub struct Catalog {
    pub core_id: String,
    pub terminals: Vec<Value>,
    pub workspaces: Vec<Workspace>,
    pub ready: bool,
    pub error: Option<String>,
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
    pub async fn validate(&self, session: &Session) -> Result<Value> {
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
    pub(crate) async fn launch(
        &self,
        cwd: &str,
        workspace: Option<&WorkspaceId>,
        launch: Launch<'_>,
    ) -> Result<(String, u32)> {
        if !Path::new(cwd).is_dir() {
            bail!("工作目录无效，请在 Herdr 中检查项目目录，或通过 /workspaces 重新选择 Workspace");
        }
        let mut labels = json!({"pi.qq-client": "1"});
        if let Some(key) = workspace {
            let catalog = self.catalog.read().await;
            if !catalog.workspaces.iter().any(|w| &w.key == key) {
                bail!("Workspace 已变化，请重新发送 /workspaces");
            }
            labels["herdr.space"] = json!(key.id);
            labels["herdr.tab"] = json!(format!("tb{}", uuid::Uuid::new_v4().simple()));
        }
        let mut env = serde_json::Map::new();
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
        if let Some(path) = crate::install::search_path() {
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
        env.insert("PI_CODING_AGENT_DIR".into(), json!(self.environment.agent));
        if let Some(root) = std::env::var_os("PI_QQ_CLIENT_DIR") {
            env.insert("PI_QQ_CLIENT_DIR".into(), json!(root.to_string_lossy()));
        }
        let mut argv = vec![self.environment.pi.to_string_lossy().into_owned()];
        let keep = match launch {
            Launch::New(source) => {
                if let Some(source) = source {
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
                // A prior request with a lost response may still be starting Pi.
                let list = self.request("terminal.list", json!({})).await?;
                if list["terminals"]
                    .as_array()
                    .context("无法核对 Herdr 终端")?
                    .iter()
                    .any(|record| {
                        record["state"] == "live" && record["labels"]["pi.qq-resume"] == session_id
                    })
                {
                    bail!("这个 Session 正在恢复或仍在 Herdr 中运行，请稍后刷新 Sessions");
                }
                if !file.is_file() {
                    bail!("原 Session 历史文件已不可用");
                }
                let file = file.to_str().context("Session 历史路径无法传给 Pi")?;
                labels["pi.qq-resume"] = json!(session_id);
                argv.extend(["--session".into(), file.into()]);
                false
            }
        };
        let result = self
            .request(
                "terminal.run",
                json!({"argv":argv, "cwd":cwd, "env":env,
            "labels":labels, "no_parent":true,"wait":false,"keep":keep}),
            )
            .await?;
        let address = result["address"]
            .as_str()
            .filter(|s| s.starts_with('t'))
            .context("新终端回执不完整，请检查 /sessions 后再重试")?;
        let pid = result["pid"]
            .as_u64()
            .filter(|v| *v > 0)
            .context("新 Pi 进程未确认")?;
        Ok((address.into(), pid as u32))
    }
}

async fn supervise(core: Core, mut rx: mpsc::Receiver<Request>) {
    loop {
        let attempt = async {
            let mut child = sanitized_command(&core.environment.core).arg("connect").spawn()?;
            let mut writer = child.stdin.take().context("Core input unavailable")?;
            let mut lines = BufReader::new(child.stdout.take().context("Core output unavailable")?).lines();
            writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\",\"params\":{\"generation\":1}}\n").await?;
            let hello: Value = serde_json::from_str(&tokio::time::timeout(Duration::from_secs(15), lines.next_line()).await??.context("Herdr 已断开")?)?;
            let result = &hello["result"];
            if result["generation"] != 1 || !result["methods"].as_array().is_some_and(|ms| ["terminal.run", "doc.get", "terminal.read", "terminal.list"].iter().all(|m| ms.contains(&json!(m)))) { bail!("Herdr Core 版本或能力不支持"); }
            { let mut cat = core.catalog.write().await; cat.core_id = result["core"]["id"].as_str().unwrap_or_default().into(); cat.ready = true; cat.error = None; }
            let mut pending: HashMap<u64, oneshot::Sender<Result<Value>>> = HashMap::new();
            let mut next_id = 0u64;
            let result: Result<()> = async {
                loop {
                    tokio::select! {
                        request = rx.recv() => {
                            let Some(request) = request else { return Ok(()); };
                            if request.reply.is_closed() { continue; }
                            next_id += 1;
                            let payload = json!({"jsonrpc":"2.0","id":next_id,"method":request.method,"params":request.params});
                            pending.insert(next_id, request.reply);
                            writer.write_all(format!("{payload}\n").as_bytes()).await?;
                        },
                        line = lines.next_line() => {
                            let Some(line) = line? else { bail!("Herdr 连接中断"); };
                            if line.len() > 16 * 1024 * 1024 { bail!("Herdr 回执超出限制"); }
                            let reply: Value = serde_json::from_str(&line)?;
                            if let Some(id) = reply["id"].as_u64() {
                                if let Some(waiting) = pending.remove(&id) {
                                    let response = if reply.get("error").is_some() { Err(anyhow!("Herdr 拒绝操作：{}", reply["error"]["data"]["kind"].as_str().unwrap_or("request_failed"))) } else { Ok(reply["result"].clone()) };
                                    let _ = waiting.send(response);
                                }
                            }
                            pending.retain(|_, p| !p.is_closed());
                        }
                    }
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
async fn refresh(core: &Core) -> Result<()> {
    let list = core.request("terminal.list", json!({})).await?;
    let doc = core.request("doc.get", json!({"name":"herdr-app"})).await?;
    let terminals = list["terminals"]
        .as_array()
        .context("terminal list invalid")?
        .clone();
    let mut cat = core.catalog.write().await;
    let mut workspaces = Vec::new();
    if let Some(spaces) = doc["value"]["spaces"].as_array() {
        for space in spaces {
            let Some(id) = space["id"].as_str() else {
                continue;
            };
            if !space["machine"].is_null() {
                continue;
            }
            let mut addresses = Vec::new();
            if let Some(tabs) = space["tabs"].as_array() {
                for tab in tabs {
                    collect_addresses(&tab["root"], &mut addresses);
                }
            }
            let members: Vec<_> = terminals
                .iter()
                .filter(|t| {
                    t["state"] == "live"
                        && (t["labels"]["herdr.space"] == id
                            || t["address"]
                                .as_str()
                                .is_some_and(|s| addresses.iter().any(|a| a == s)))
                })
                .collect();
            let cwd = space["folder"]
                .as_str()
                .filter(|folder| !folder.trim().is_empty())
                .or_else(|| {
                    members.first().and_then(|t| {
                        t["foreground"]["cwd"]
                            .as_str()
                            .or_else(|| t["cwd"].as_str())
                    })
                })
                .unwrap_or_default();
            let name = ["name", "title"]
                .into_iter()
                .filter_map(|field| space[field].as_str())
                .map(str::trim)
                .find(|name| !name.is_empty() && *name != id)
                .or_else(|| {
                    cwd.trim_end_matches(['/', '\\'])
                        .rsplit(['/', '\\'])
                        .next()
                        .filter(|name| !name.is_empty())
                })
                .unwrap_or("未命名 Workspace");
            workspaces.push(Workspace {
                key: WorkspaceId {
                    core_id: cat.core_id.clone(),
                    id: id.into(),
                },
                name: name.into(),
                cwd: cwd.into(),
                terminals: members
                    .iter()
                    .filter_map(|t| t["address"].as_str().map(String::from))
                    .collect(),
            });
        }
    }
    cat.terminals = terminals;
    cat.workspaces = workspaces;
    Ok(())
}
fn collect_addresses(value: &Value, result: &mut Vec<String>) {
    if let Some(s) = value["content"]["terminal"].as_str() {
        result.push(s.into());
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
