use crate::{
    model::{Config, Dependency},
    store::atomic_write,
};
use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

const EXTENSION: &str = include_str!("../../../integrations/pi/pi-qq-client.ts");
const CONNECTION: &str = include_str!("../../../integrations/pi/connection.ts");
const COMMANDS: &str = include_str!("../../../integrations/pi/commands.ts");
const PERMISSIONS: &str = include_str!("../../../integrations/pi/permissions.ts");
pub(crate) const RELAY: &str = include_str!("../../../integrations/pi/relay.mjs");
pub(crate) const COMPANION: [(&str, &str); 5] = [
    ("pi-qq-client/connection.ts", CONNECTION),
    ("pi-qq-client/commands.ts", COMMANDS),
    ("pi-qq-client/permissions.ts", PERMISSIONS),
    ("pi-qq-client/relay.mjs", RELAY),
    ("pi-qq-client.ts", EXTENSION),
];
const MANAGED: &str = "// Managed by pi-qq-herdr-client; protocol 1.";

#[derive(Clone)]
pub struct Environment {
    pub core: PathBuf,
    pub pi: PathBuf,
    pub home: PathBuf,
    pub agent: PathBuf,
}

pub fn locate(config: &Config) -> std::result::Result<Environment, Dependency> {
    let home = directories::BaseDirs::new()
        .ok_or(Dependency::Pi)?
        .home_dir()
        .to_path_buf();
    let pi = executable(
        config.pi_path.as_deref(),
        if cfg!(windows) { "pi.cmd" } else { "pi" },
    )
    .or_else(|| executable(config.pi_path.as_deref(), "pi"))
    .ok_or(Dependency::Pi)?;
    let core = executable(config.core_path.as_deref(), "hc").ok_or(Dependency::Herdr)?;
    let agent = agent_directory(&home);
    Ok(Environment {
        core,
        pi,
        home,
        agent,
    })
}

pub(crate) fn agent_directory(home: &Path) -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"))
}

fn executable(override_path: Option<&Path>, name: &str) -> Option<PathBuf> {
    match override_path {
        Some(p) => p.is_file().then(|| p.to_path_buf()),
        None => {
            #[cfg(windows)]
            {
                which::which(name).ok()
            }
            #[cfg(not(windows))]
            {
                which::which_in(name, search_path(), std::env::current_dir().ok()?).ok()
            }
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn search_path() -> Option<std::ffi::OsString> {
    let mut paths: Vec<_> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    // Finder/desktop launches do not inherit a terminal's shell PATH.
    if let Some(dirs) = directories::BaseDirs::new() {
        for relative in [".pi/agent/bin", ".local/bin", ".cargo/bin", ".volta/bin"] {
            let path = dirs.home_dir().join(relative);
            if path.is_dir() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    for path in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        let path = PathBuf::from(path);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    std::env::join_paths(paths).ok()
}

pub fn install_extension(env: &Environment) -> Result<bool> {
    let dir = env.agent.join("extensions");
    let files = COMPANION.map(|(name, source)| (dir.join(name), source));
    let mut changed = false;
    // Validate the whole installation before touching an existing file.
    for (path, source) in &files {
        match std::fs::read_to_string(path) {
            Ok(current) if current == *source => {}
            Ok(current) if current.starts_with(MANAGED) => changed = true,
            Ok(_) => bail!(
                "Pi 扩展文件冲突：{}。请先保留并移走同名用户文件，再重新检测",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => changed = true,
            Err(e) => return Err(e).context("无法检查 Pi 扩展文件"),
        }
    }
    if changed {
        // Dependencies first, entry last. Pi only loads this on its next start/reload.
        for (path, content) in &files {
            atomic_write(path, content.as_bytes())?;
        }
    }
    Ok(changed)
}

pub fn sanitized_command(path: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(path);
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        let key_str = key.to_string_lossy().to_ascii_uppercase();
        if [
            "PATH",
            "HOME",
            "USER",
            "USERNAME",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "APPDATA",
            "LOCALAPPDATA",
            "PROGRAMDATA",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "TEMP",
            "TMP",
            "LANG",
            "SHELL",
            "HC_SESSION",
            "HC_SOCKET",
        ]
        .contains(&key_str.as_str())
            || key_str.starts_with("LC_")
            || key_str.starts_with("XDG_")
        {
            command.env(key, value);
        }
    }
    #[cfg(not(windows))]
    if let Some(path) = search_path() {
        command.env("PATH", path);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    command
}
