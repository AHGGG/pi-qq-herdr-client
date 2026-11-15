use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const PROTOCOL: u32 = 1;
pub const HERDR_URL: &str = "https://herdr.dev/";
pub const PI_URL: &str = "https://pi.dev/";
pub const CREATE_BOT_URL: &str = "https://bot.q.qq.com/wiki/agent-qqbot/#快速创建一个-qq-机器人";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_directory: Option<PathBuf>,
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
    Shutdown,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub session_id: String,
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
    pub seen: Vec<String>,
}

impl Durable {
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
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceId {
    pub core_id: String,
    pub id: String,
}

#[derive(Clone)]
pub struct Workspace {
    pub key: WorkspaceId,
    pub name: String,
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
