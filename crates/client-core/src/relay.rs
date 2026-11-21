use crate::{
    bridge::Hub,
    herdr::Core,
    install::{COMPANION, RELAY},
    media::{self, Attachment},
    model::{RecentSession, RemoteTerminal, Session, WorkspaceId},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{RwLock, mpsc},
    task::JoinHandle,
};
use uuid::Uuid;
use zeroize::Zeroizing;

type Signature = Hmac<Sha256>;
const CHUNK: usize = 1024 * 1024;

// A short non-interactive call over Herdr's existing machine connection.
// Secrets and payloads go on stdin, outside the terminal's argv and labels.
async fn node(
    core: &Core,
    machine: &str,
    script: &str,
    args: &[&str],
    input: Value,
) -> Result<Value> {
    let argv: Vec<_> = ["node", "--input-type=module", "-e", script]
        .into_iter()
        .chain(args.iter().copied())
        .collect();
    let reply = tokio::time::timeout(
        Duration::from_secs(9),
        core.request(
            "machine.exec",
            json!({"on":machine,"argv":argv,"timeout":6,"input":STANDARD.encode(serde_json::to_vec(&input)?)}),
        ),
    )
    .await
    .context("Herdr 远程通信超时")??;
    if reply["code"] != 0 || reply["truncated"] == true {
        bail!("远程 Pi 扩展通信失败，请在 Herdr 中检查目标机器的 Pi 和 Node");
    }
    let output = STANDARD.decode(reply["stdout"].as_str().unwrap_or_default())?;
    serde_json::from_slice(&output).context("远程 Pi 扩展返回了无效数据")
}

const EXCHANGE: &str = r#"
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { pathToFileURL } from 'node:url';
const [operation, bundle, id] = process.argv.slice(1);
if (!/^[a-f0-9]{32}$/.test(bundle) || !['exchange', 'relayFile'].includes(operation)) throw Error('Invalid relay');
const cache = process.env.XDG_CACHE_HOME;
const helper = pathToFileURL(join(cache && isAbsolute(cache) ? cache : join(homedir(), '.cache'), 'pi-qq-herdr-client', 'bundles', bundle, 'relay.mjs'));
const api = await import(helper.href);
console.log(JSON.stringify(await api[operation](id, JSON.parse(readFileSync(0, 'utf8')))));
"#;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RemoteRelay {
    pub workspace: WorkspaceId,
    pub relay: String,
    pub scope: String,
    pub bundle: String,
    extension: String,
    home: String,
    #[serde(default)]
    pub terminal: Option<String>,
}

impl RemoteRelay {
    pub fn machine(&self) -> &str {
        self.workspace
            .machine_id
            .as_deref()
            .expect("remote workspace")
    }
    pub fn extension(&self) -> &str {
        &self.extension
    }
    pub fn home(&self) -> &str {
        &self.home
    }

    pub async fn prepare(
        core: &Core,
        workspace: WorkspaceId,
        scope: String,
        master: &[u8; 32],
    ) -> Result<Self> {
        let relay = Uuid::new_v4().simple().to_string();
        let key = Zeroizing::new(key_for(master, &relay));
        let script = format!(
            "{RELAY}\nconsole.log(JSON.stringify(await provision(JSON.parse((await import('node:fs')).readFileSync(0, 'utf8')))));"
        );
        let files: BTreeMap<_, _> = COMPANION.into_iter().collect();
        let machine = workspace.machine_id.as_deref().context("缺少远程机器")?;
        let prepared = node(
            core,
            machine,
            &script,
            &[],
            json!({"files":files,"relay":relay,"key":STANDARD.encode(key.as_ref())}),
        )
        .await
        .context("无法在目标机器配置 Pi 扩展（需要 macOS 或 Linux，以及可用的 Node）")?;
        if prepared["error"] == "extension_conflict" {
            bail!("目标机器的 Pi 扩展目录已有同名文件，请保留并移走该文件后再试");
        }
        let field = |name: &str| {
            prepared[name]
                .as_str()
                .map(String::from)
                .context("远程 Pi 扩展安装回执不完整")
        };
        Ok(Self {
            workspace,
            relay,
            scope,
            bundle: field("bundle")?,
            extension: field("extension")?,
            home: field("home")?,
            terminal: None,
        })
    }

    async fn call(
        &self,
        core: &Core,
        key: &[u8; 32],
        operation: &str,
        mut value: Value,
    ) -> Result<Value> {
        let nonce = Uuid::new_v4().simple().to_string();
        value["nonce"] = json!(nonce);
        let body = STANDARD.encode(serde_json::to_vec(&value)?);
        let direction = if operation == "exchange" {
            "desktop"
        } else {
            "file"
        };
        let packet = json!({"body":body,"mac":format!("{:x}",signature(key, direction, &body).finalize().into_bytes())});
        let reply = node(
            core,
            self.machine(),
            EXCHANGE,
            &[operation, &self.bundle, &self.relay],
            packet,
        )
        .await?;
        let body = reply["body"].as_str().context("远程 Pi 回执不完整")?;
        let tag = reply["mac"].as_str().context("远程 Pi 回执缺少签名")?;
        let mut bytes = [0u8; 32];
        ensure!(
            tag.len() == 64 && tag.bytes().all(|c| c.is_ascii_hexdigit()),
            "远程 Pi 回执签名无效"
        );
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&tag[index * 2..index * 2 + 2], 16)?;
        }
        signature(key, "pi", body)
            .verify_slice(&bytes)
            .context("远程 Pi 回执认证失败")?;
        let result: Value = serde_json::from_slice(&STANDARD.decode(body)?)?;
        ensure!(result["nonce"] == nonce, "远程 Pi 回执已过期");
        Ok(result)
    }
}

fn signature(key: &[u8; 32], direction: &str, body: &str) -> Signature {
    let mut mac = Signature::new_from_slice(key).expect("HMAC takes a 32-byte key");
    mac.update(direction.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    mac
}

pub(crate) fn load_key(store: &Store) -> Result<Zeroizing<[u8; 32]>> {
    let name = "remote-relay.key";
    let key = match std::fs::read(store.root().join(name)) {
        Ok(bytes) => Zeroizing::new(
            bytes
                .as_slice()
                .try_into()
                .context("远程 Pi 本地密钥格式错误")?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = Zeroizing::new([0u8; 32]);
            rand::thread_rng().fill_bytes(key.as_mut());
            store.write(name, key.as_ref())?;
            key
        }
        Err(error) => return Err(error.into()),
    };
    Ok(key)
}

pub(crate) fn key_for(master: &[u8; 32], relay: &str) -> [u8; 32] {
    signature(master, "relay", relay)
        .finalize()
        .into_bytes()
        .into()
}

fn registration(session: &crate::model::Session, name: Option<String>) -> Value {
    json!({"type":"result","requestId":Uuid::new_v4(),"ok":true,"registration":true,
        "slot":session.slot,"name":name,"generation":session.generation,"protocol":1})
}

pub(crate) struct RemoteSessions {
    jobs: HashMap<String, JoinHandle<bool>>,
}

impl RemoteSessions {
    pub fn start(hub: Hub, core: Arc<RwLock<Option<Core>>>) {
        tokio::spawn(async move {
            let mut active = Self {
                jobs: HashMap::new(),
            };
            loop {
                let core = core.read().await.clone();
                if let Some(core) = core {
                    hub.use_core(core.clone()).await;
                    active.sync(&hub, &core).await;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    async fn sync(&mut self, hub: &Hub, core: &Core) {
        let known: Vec<_> = self.jobs.keys().cloned().collect();
        for id in known {
            let saved = hub
                .routes
                .lock()
                .await
                .remote_relays
                .get(&id)
                .is_some_and(|relay| hub.matches_scope(&relay.scope));
            if !saved || self.jobs[&id].is_finished() {
                let job = self.jobs.remove(&id).unwrap();
                if !saved {
                    job.abort();
                }
                let ended = job.await.unwrap_or(false);
                hub.disconnect_relay(&id).await;
                if ended {
                    forget(hub, &id).await;
                }
            }
        }
        let saved = hub.routes.lock().await.remote_relays.clone();
        for (id, mut relay) in saved {
            if self.jobs.contains_key(&id)
                || !hub.matches_scope(&relay.scope)
                || !core.machine_connected(&relay.workspace).await
            {
                continue;
            }
            if relay.terminal.is_none() {
                let Some(terminal) = core.relay_terminal(&relay.workspace, &id).await else {
                    if hub
                        .routes
                        .lock()
                        .await
                        .pending_launches
                        .get(&id)
                        .is_none_or(|launch| launch.expires <= crate::router::now())
                    {
                        forget(hub, &id).await;
                    }
                    continue;
                };
                let mut routes = hub.routes.lock().await;
                let Some(record) = routes.remote_relays.get_mut(&id) else {
                    continue;
                };
                record.terminal = Some(terminal.clone());
                relay.terminal = Some(terminal);
                drop(routes);
                let _ = hub.save_routes().await;
            }
            let (hub, core) = (hub.clone(), core.clone());
            self.jobs.insert(
                id,
                tokio::spawn(async move { poll(hub, core, relay).await }),
            );
        }
    }
}

async fn forget(hub: &Hub, id: &str) {
    let mut routes = hub.routes.lock().await;
    routes.remote_relays.remove(id);
    routes.pending_launches.remove(id);
    drop(routes);
    let _ = hub.save_routes().await;
}

// The extension consumes an action once. An unconfirmed batch is never sent
// again; late responses and repeated events are scoped to their connection.
async fn poll(hub: Hub, core: Core, relay: RemoteRelay) -> bool {
    let Some(terminal) = relay.terminal.as_deref() else {
        return false;
    };
    let Ok(key) = hub.relay_key(&relay.relay).await else {
        return false;
    };
    let address = format!("{}/{terminal}", relay.machine());
    let mut instance = String::new();
    let mut ack = 0;
    let mut round = 0;
    let mut peer: Option<Session> = None;
    let mut ready = false;
    let mut rx = None;
    let mut transfer = tokio::task::JoinSet::new();
    let mut outbound = Vec::new();
    let mut last_check = None;
    let ended = loop {
        if !hub.matches_scope(&relay.scope) || !core.machine_connected(&relay.workspace).await {
            break false;
        }
        if last_check.is_none_or(|at: std::time::Instant| at.elapsed() >= Duration::from_secs(5)) {
            last_check = Some(std::time::Instant::now());
            if core.relay_ended(&address, &relay.relay).await {
                break true;
            }
        }
        round += 1;
        let generation = peer
            .as_ref()
            .map_or("", |session| session.generation.as_str());
        let request = relay.call(
            &core,
            &key,
            "exchange",
            json!({"instance":instance,"generation":generation,"round":round,"ack":ack,"messages":outbound}),
        );
        let Ok(reply) = request.await else {
            break false;
        };
        outbound.clear();
        let Some(current) = reply["instance"]
            .as_str()
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            if peer.is_some() {
                break false;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };
        if instance != current {
            transfer.abort_all();
            ready = false;
            if let Some(previous) = peer.take() {
                hub.disconnect(&previous).await;
            }
            let meta = &reply["meta"];
            let Ok(pid) = core.confirm_relay(&address, &relay.relay, meta).await else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            let identity = RemoteTerminal {
                core_id: relay.workspace.core_id.clone(),
                machine_id: relay.machine().into(),
                terminal: terminal.into(),
                pid,
                relay: relay.relay.clone(),
                scope: relay.scope.clone(),
                instance: current.into(),
            };
            let (tx, inbox) = mpsc::channel(64);
            rx = Some(inbox);
            let Ok(session) = hub.register_remote(meta, tx, identity).await else {
                break false;
            };
            let name = hub.registration_name(&session).await;
            outbound.push(registration(&session, name));
            peer = Some(session);
            instance = current.into();
            ack = 0;
            continue;
        }
        let mut replies = Vec::new();
        if let Some(session) = &peer {
            if reply["generation"] != session.generation {
                if ready {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            if !ready {
                hub.announce_registration(session).await;
                ready = true;
            }
            if hub.update_remote(session, &reply["meta"]).await.is_err() {
                break false;
            }
            for event in reply["events"].as_array().into_iter().flatten() {
                let Some(seq) = event["seq"].as_u64().filter(|seq| *seq > ack) else {
                    continue;
                };
                let message = event["message"].clone();
                if message["type"] == "notify"
                    && ["filePath", "imagePath", "videoPath"]
                        .iter()
                        .any(|key| message[key].is_string())
                {
                    if transfer.is_empty() {
                        let (hub, session) = (hub.clone(), session.clone());
                        transfer.spawn(async move {
                            let reply = hub.handle_reply(&session, message).await;
                            (session.generation, reply)
                        });
                    } else {
                        replies.push(json!({"type":"result","requestId":message["requestId"],"ok":false,"error":"上一个远程附件仍在传输"}));
                    }
                } else if let Some(reply) = hub.handle_reply(session, message).await {
                    replies.push(reply);
                }
                ack = seq;
            }
        }
        outbound = replies;
        while outbound.len() < 16 {
            let Some(result) = transfer.try_join_next() else {
                break;
            };
            if let Ok((generation, Some(reply))) = result
                && peer.as_ref().is_some_and(|s| s.generation == generation)
            {
                outbound.push(reply);
            }
        }
        if let Some(rx) = &mut rx {
            while outbound.len() < 16 {
                let Ok(action) = rx.try_recv() else {
                    break;
                };
                outbound.push(action);
            }
        }
    };
    if let Some(session) = peer {
        hub.disconnect(&session).await;
    }
    ended
}

pub(crate) async fn remote_history(
    core: &Core,
    recent: &RecentSession,
) -> Result<std::path::PathBuf> {
    let machine = recent.machine_id.as_deref().context("缺少远程机器")?;
    let script = r#"
import fs from 'node:fs/promises';
import { openSync, readFileSync, readSync, closeSync } from 'node:fs';
import { isAbsolute, extname } from 'node:path';
const { file, id, cwd, pid } = JSON.parse(readFileSync(0, 'utf8'));
let state = 'missing';
if (pid) {
  try { process.kill(pid, 0); state = 'running'; }
  catch (error) { if (error.code !== 'ESRCH') state = 'running'; }
}
if (state === 'missing' && isAbsolute(file) && extname(file) === '.jsonl' && isAbsolute(cwd)) {
  try {
    if (!(await fs.stat(file)).isFile() || !(await fs.stat(cwd)).isDirectory()) throw Error('Not history');
    const fd = openSync(file, 'r');
    try {
      const head = Buffer.alloc(1024 * 1024);
      const count = readSync(fd, head, 0, head.length, 0);
      const end = head.subarray(0, count).indexOf(10);
      if (end < 0 && count === head.length) throw Error('Header exceeds limit');
      const header = JSON.parse(head.subarray(0, end < 0 ? count : end).toString('utf8'));
      if (header.type === 'session' && header.id === id && await fs.realpath(header.cwd) === await fs.realpath(cwd)) state = 'saved';
    } finally { closeSync(fd); }
  } catch {}
}
console.log(JSON.stringify({ state }));
"#;
    let path = recent
        .session_file
        .as_ref()
        .and_then(|path| path.to_str())
        .filter(|path| path.starts_with('/'))
        .context("没有保存的远程 Session 历史")?;
    let result = node(
        core,
        machine,
        script,
        &[],
        json!({"file":path,"id":recent.session_id,"cwd":recent.cwd,"pid":recent.pid}),
    )
    .await?;
    match result["state"].as_str() {
        Some("saved") => Ok(std::path::PathBuf::from(path)),
        Some("running") => bail!("原远程 Pi 进程仍在运行，请等待重新连接"),
        _ => bail!("找不到目标机器上的原 Session 历史文件"),
    }
}

pub(crate) async fn put_attachment(
    core: &Core,
    session: &Session,
    attachment: &Attachment,
    hub: &Hub,
) -> Result<String> {
    let terminal = session.remote.as_ref().context("缺少远程 Pi")?;
    let relay = hub.relay(terminal).await?;
    let key = hub.relay_key(&relay.relay).await?;
    let id = Uuid::new_v4().simple().to_string();
    let mut file = tokio::fs::File::open(&attachment.path).await?;
    let mut offset = 0;
    let mut buffer = vec![0; CHUNK];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 && offset != 0 {
            break;
        }
        relay
            .call(
                core,
                &key,
                "relayFile",
                json!({"op":"put","instance":terminal.instance,"generation":session.generation,"id":id,"offset":offset,"data":STANDARD.encode(&buffer[..n])}),
            )
            .await?;
        offset += n;
        if n == 0 {
            break;
        }
    }
    let reply = relay
        .call(
            core,
            &key,
            "relayFile",
            json!({"op":"finish","instance":terminal.instance,"generation":session.generation,"id":id,"offset":offset,"name":attachment.file_name}),
        )
        .await?;
    reply["path"]
        .as_str()
        .map(String::from)
        .context("远程附件未确认保存")
}

pub(crate) async fn get_attachment(
    core: &Core,
    session: &Session,
    path: &str,
    kind: &str,
    hub: &Hub,
) -> Result<Attachment> {
    ensure!(
        path.starts_with('/') && !path.contains('\0'),
        "远程附件路径必须是绝对路径"
    );
    let terminal = session.remote.as_ref().context("缺少远程 Pi")?;
    let relay = hub.relay(terminal).await?;
    let key = hub.relay_key(&relay.relay).await?;
    let id = Uuid::new_v4().simple().to_string();
    let name = path.rsplit('/').next().unwrap_or_default();
    let mut bytes = Vec::new();
    let mut size = None;
    loop {
        let reply = relay
            .call(
                core,
                &key,
                "relayFile",
                json!({"op":"get","instance":terminal.instance,"generation":session.generation,"id":id,"offset":bytes.len(),"path":path,"kind":kind}),
            )
            .await?;
        let total = reply["size"]
            .as_u64()
            .filter(|n| *n <= media::limit(kind) as u64)
            .context("远程附件大小无效")?;
        ensure!(
            size.replace(total).is_none_or(|old| old == total),
            "传输期间远程附件已变化"
        );
        let data = STANDARD.decode(reply["data"].as_str().context("远程附件内容不完整")?)?;
        ensure!(
            data.len() <= CHUNK && bytes.len() + data.len() <= total as usize,
            "远程附件内容超出限制"
        );
        let empty = data.is_empty();
        bytes.extend(data);
        if bytes.len() == total as usize {
            break;
        }
        ensure!(!empty, "远程附件内容不完整");
    }
    media::stage_bytes(&bytes, kind, name)
}
