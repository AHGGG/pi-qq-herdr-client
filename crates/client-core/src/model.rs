use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub const PROTOCOL: u32 = 1;
pub(crate) const RECENT_SESSION_LIMIT: usize = 24;
pub const HERDR_URL: &str = "https://herdr.dev/";
pub const PI_URL: &str = "https://pi.dev/";
pub const CREATE_BOT_URL: &str = "https://bot.q.qq.com/wiki/agent-qqbot/#快速创建一个-qq-机器人";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub pi_path: Option<PathBuf>,
    pub core_path: Option<PathBuf>,
    pub start_at_login: bool,
}

// Never derive Debug for credentials or messages containing account identifiers.
#[derive(Clone, Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
pub struct Credentials {
    pub app_id: String,
    pub app_secret: String,
    pub owner: Option<String>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub config: Config,
    pub core_ready: bool,
    pub core_error: Option<String>,
    pub qq_ready: bool,
    pub paused: bool,
    pub owner_bound: bool,
    pub dependencies_ready: bool,
    pub missing: Option<Dependency>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub menu_error: Option<String>,
    pub qr_url: Option<String>,
    pub qr_status: String,
    pub pair_code: Option<String>,
    pub connected_sessions: usize,
    pub guide_status: GuideStatus,
    pub failed_deliveries: usize,
    pub show_window: u64,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub enum GuideStatus {
    #[default]
    NotSent,
    Sending,
    Sent,
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Dependency {
    Pi,
    Herdr,
}

pub enum Command {
    Detect,
    StartQr,
    CancelQr,
    Rebind,
    Pair,
    Save(Config),
    RetryDeliveries,
    SendGuide,
    ConfigureMenu,
    SetPaused(bool),
    Shutdown,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub label: String,
    pub cwd: String,
    pub state: String,
    pub terminal: Option<String>,
    pub terminal_socket: Option<String>,
    pub pid: u32,
    pub runtime: serde_json::Value,
    pub capabilities: Vec<String>,
    pub reload_token: Option<String>,
    #[serde(skip)]
    pub generation: String,
    #[serde(skip)]
    pub slot: String,
    #[serde(skip)]
    pub(crate) remote: Option<RemoteTerminal>,
}

#[derive(Clone)]
pub(crate) struct RemoteTerminal {
    pub core_id: String,
    pub machine_id: String,
    pub terminal: String,
    pub pid: u32,
    pub relay: String,
    pub scope: String,
    pub instance: String,
}

impl RemoteTerminal {
    pub fn address(&self) -> String {
        format!("{}/{}", self.machine_id, self.terminal)
    }
}

impl Session {
    pub(crate) fn machine_id(&self) -> Option<&str> {
        self.remote
            .as_ref()
            .map(|remote| remote.machine_id.as_str())
    }

    pub(crate) fn model_key(&self) -> Option<String> {
        let model = &self.runtime["model"];
        let provider = model["provider"].as_str().filter(|s| !s.is_empty())?;
        let id = model["id"].as_str().filter(|s| !s.is_empty())?;
        Some(format!("{provider}/{id}"))
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Durable {
    pub slots: BTreeMap<String, String>,
    pub next_slot: u64,
    pub workspace_slots: BTreeMap<String, String>,
    pub next_workspace_slot: u64,
    pub selected: Option<String>,
    pub workspace: Option<WorkspaceId>,
    pub names: BTreeMap<String, String>,
    pub sources: BTreeMap<String, String>,
    pub source_order: Vec<String>,
    pub pending_launches: BTreeMap<String, PendingLaunch>,
    pub(crate) remote_relays: BTreeMap<String, crate::relay::RemoteRelay>,
    pub recent_sessions: Vec<RecentSession>,
    pub recent_sessions_initialized: bool,
    pub model_usage: BTreeMap<String, ModelUsage>,
    pub seen: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub count: u64,
    pub last_used: u64,
}

impl Durable {
    pub(crate) fn record_model(&mut self, model: String) {
        let usage = self.model_usage.entry(model).or_default();
        usage.count = usage.count.saturating_add(1);
        usage.last_used = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
    }

    pub(crate) fn remember_session(&mut self, session: &Session, activate: bool) {
        let existing = self.recent_sessions.iter().position(|recent| {
            recent.session_id == session.session_id
                && recent.machine_id.as_deref() == session.machine_id()
        });
        let previous = existing.map(|index| &self.recent_sessions[index]);
        let same_directory = previous.filter(|recent| recent.cwd == session.cwd);
        let recent = RecentSession {
            peer_id: session.id.clone(),
            session_id: session.session_id.clone(),
            session_file: session
                .session_file
                .clone()
                .or_else(|| same_directory.and_then(|r| r.session_file.clone())),
            cwd: session.cwd.clone(),
            label: session.label.clone(),
            workspace: same_directory.and_then(|r| r.workspace.clone()),
            machine_id: session.machine_id().map(String::from),
            pid: session.pid,
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        if let Some(index) = existing {
            if !activate && self.recent_sessions[index].peer_id == session.id {
                self.recent_sessions[index] = recent;
                return;
            }
            self.recent_sessions.remove(index);
        }
        self.recent_sessions.insert(0, recent);
        self.recent_sessions.truncate(RECENT_SESSION_LIMIT);
    }

    pub(crate) fn set_session_workspace(
        &mut self,
        session: &Session,
        workspace: Option<WorkspaceId>,
    ) {
        if let Some(recent) = self
            .recent_sessions
            .iter_mut()
            .find(|recent| recent.peer_id == session.id)
        {
            recent.workspace = workspace;
        }
    }

    pub(crate) fn remember_source(&mut self, reference: String, source: String) {
        // Older state has no order field. Keep unknown-age entries ahead of
        // references whose acceptance order is known (including receipt recovery).
        if self.source_order.len() != self.sources.len() {
            self.source_order
                .retain(|key| self.sources.contains_key(key));
            let mut missing: Vec<_> = self
                .sources
                .keys()
                .filter(|key| !self.source_order.contains(key))
                .cloned()
                .collect();
            missing.append(&mut self.source_order);
            self.source_order = missing;
        }
        self.source_order.retain(|key| key != &reference);
        self.source_order.push(reference.clone());
        self.sources.insert(reference, source);
        while self.source_order.len() > 500 {
            self.sources.remove(&self.source_order.remove(0));
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PendingLaunch {
    pub scope: String,
    pub core_id: String,
    pub terminal: String,
    pub pid: u32,
    pub expires: u64,
    #[serde(default)]
    pub workspace: Option<WorkspaceId>,
    #[serde(default)]
    pub resume: Option<ResumeTarget>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RecentSession {
    pub peer_id: String,
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub cwd: String,
    pub label: String,
    pub workspace: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    pub pid: u32,
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ResumeTarget {
    pub session_id: String,
    pub cwd: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceId {
    pub core_id: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

impl WorkspaceId {
    pub(crate) fn slot_key(&self) -> String {
        match &self.machine_id {
            Some(machine) => format!("{}:{machine}/{}", self.core_id, self.id),
            None => format!("{}:{}", self.core_id, self.id),
        }
    }
}

#[derive(Clone)]
pub struct Workspace {
    pub key: WorkspaceId,
    pub name: String,
    pub machine: Option<String>,
    pub cwd: String,
    pub terminals: Vec<String>,
}

pub fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(limit)
        .collect()
}
