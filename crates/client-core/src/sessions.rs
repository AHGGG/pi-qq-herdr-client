use crate::model::{Durable, RECENT_SESSION_LIMIT, RecentSession};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    cmp::Reverse,
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};
use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};

pub(crate) fn saved_file(recent: &RecentSession, agent: &Path) -> Result<Option<PathBuf>> {
    let cwd = Path::new(&recent.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Ok(None);
    }
    if let Some(path) = recent
        .session_file
        .as_ref()
        .filter(|path| matches_header(path, recent))
    {
        return Ok(Some(path.clone()));
    }
    // Pi's default directory and filename let older peers register without a file path.
    let encoded = recent
        .cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(&recent.cwd)
        .replace(['/', '\\', ':'], "-");
    let dir = agent.join("sessions").join(format!("--{encoded}--"));
    let suffix = format!("_{}.jsonl", recent.session_id);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("无法读取 Pi 历史目录"),
    };
    let mut candidate = None;
    for entry in entries {
        let entry = entry.context("无法读取 Pi 历史目录")?;
        if !entry.file_name().to_string_lossy().ends_with(&suffix) {
            continue;
        }
        let path = entry.path();
        if matches_header(&path, recent) && candidate.replace(path).is_some() {
            bail!("找到多个同 ID 的历史文件，请在 Pi 终端选择要恢复的记录");
        }
    }
    Ok(candidate)
}

// Older routes store slots and names. Seed recent history once from sessions
// the client already knew, by reading only their on-disk headers.
pub(crate) fn import_known_sessions(agent: &Path, routes: &mut Durable) -> Result<()> {
    if routes.slots.is_empty() {
        return Ok(());
    }
    let root = match fs::read_dir(agent.join("sessions")) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("无法读取 Pi 历史目录"),
    };
    let mut known = HashMap::new();
    let mut slots: Vec<_> = routes
        .slots
        .iter()
        .filter_map(|(peer, slot)| {
            let (id, pid) = peer.rsplit_once(':')?;
            Some((
                id,
                pid.parse::<u32>().ok()?,
                slot.strip_prefix('s')?.parse::<u64>().ok()?,
                peer,
            ))
        })
        .collect();
    slots.sort_by_key(|(_, _, number, peer)| {
        Reverse((Some(*peer) == routes.selected.as_ref(), *number))
    });
    for (id, pid, _, peer) in &slots {
        known.entry(*id).or_insert((*pid, *peer));
    }
    let mut histories = HashMap::new();
    for dir in root
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
    {
        let Ok(entries) = fs::read_dir(dir.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let filename = entry.file_name();
            let Some((_, id)) = filename
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(|name| name.rsplit_once('_'))
            else {
                continue;
            };
            let Some(&(pid, peer)) = known.get(id) else {
                continue;
            };
            let path = entry.path();
            let Some(header) = read_header(&path).filter(|header| header["id"] == id) else {
                continue;
            };
            let Some(cwd) = header["cwd"]
                .as_str()
                .filter(|cwd| Path::new(cwd).is_absolute())
            else {
                continue;
            };
            let project = cwd
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("Pi");
            let label =
                routes.names.get(peer).cloned().unwrap_or_else(|| {
                    format!("{project} {}", id.chars().take(6).collect::<String>())
                });
            let recent = RecentSession {
                peer_id: peer.clone(),
                session_id: id.into(),
                session_file: Some(path),
                cwd: cwd.into(),
                label,
                workspace: None,
                pid,
                last_seen: 0,
            };
            // Ambiguous IDs need a live registration to identify the right file.
            histories
                .entry(id.to_string())
                .and_modify(|record| *record = None)
                .or_insert(Some(recent));
        }
    }
    routes.recent_sessions = slots
        .into_iter()
        .filter_map(|(id, ..)| histories.remove(id).flatten())
        .take(RECENT_SESSION_LIMIT)
        .collect();
    Ok(())
}

fn matches_header(path: &Path, recent: &RecentSession) -> bool {
    read_header(path).is_some_and(|header| {
        header["id"] == recent.session_id
            && header["cwd"]
                .as_str()
                .is_some_and(|cwd| same_directory(cwd, &recent.cwd))
    })
}

fn read_header(path: &Path) -> Option<Value> {
    if !path.is_absolute() || !path.is_file() || path.extension().is_none_or(|ext| ext != "jsonl") {
        return None;
    }
    let mut header = Vec::new();
    // Read only the first line of the history, never load its messages.
    BufReader::new(File::open(path).ok()?)
        .take(1024 * 1024)
        .read_until(b'\n', &mut header)
        .ok()?;
    let header: Value = serde_json::from_slice(&header).ok()?;
    (header["type"] == "session").then_some(header)
}

pub(crate) fn same_directory(a: &str, b: &str) -> bool {
    let (a, b) = (Path::new(a), Path::new(b));
    a == b
        || fs::canonicalize(a)
            .ok()
            .is_some_and(|path| fs::canonicalize(b).ok().as_ref() == Some(&path))
}

pub(crate) fn process_is_live(recent: &RecentSession) -> bool {
    let pid = Pid::from_u32(recent.pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
    system.process(pid).is_some_and(|process| {
        if matches!(
            process.status(),
            ProcessStatus::Dead | ProcessStatus::Zombie
        ) {
            return false;
        }
        if recent.last_seen != 0 {
            return process.start_time() <= recent.last_seen;
        }
        // Historic routes predate liveness timestamps. Ignore unrelated reused PIDs.
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        matches!(name.trim_end_matches(".exe"), "node" | "pi" | "bun")
    })
}
