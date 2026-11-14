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
    let agent = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"));
    Ok(Environment {
        core,
        pi,
        home,
        agent,
    })
}

fn executable(override_path: Option<&Path>, name: &str) -> Option<PathBuf> {
    match override_path {
        Some(p) => p.is_file().then(|| p.to_path_buf()),
        None => which::which(name).ok(),
    }
}

pub fn install_extension(env: &Environment) -> Result<bool> {
    let dir = env.agent.join("extensions");
    let files = [
        (dir.join("pi-qq-client.ts"), EXTENSION),
        (dir.join("pi-qq-client/connection.ts"), CONNECTION),
        (dir.join("pi-qq-client/commands.ts"), COMMANDS),
        (dir.join("pi-qq-client/permissions.ts"), PERMISSIONS),
    ];
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
        for index in [1, 2, 3, 0] {
            let (path, content) = &files[index];
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
