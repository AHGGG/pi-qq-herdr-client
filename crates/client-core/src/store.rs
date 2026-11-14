use crate::model::{Config, Credentials, Durable};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    root: Arc<PathBuf>,
}

pub struct InstanceLock {
    _file: File,
}

impl Store {
    pub fn open() -> Result<Self> {
        let root = std::env::var_os("PI_QQ_CLIENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                directories::BaseDirs::new()
                    .expect("user data directory")
                    .data_local_dir()
                    .join("pi-qq-herdr-client")
            });
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            root: Arc::new(root),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn lock(&self) -> Result<Option<InstanceLock>> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("instance.lock"))?;
        match f.try_lock_exclusive() {
            Ok(()) => Ok(Some(InstanceLock { _file: f })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn request_show(&self) -> Result<()> {
        self.write("show-window", b"show")
    }
    pub fn take_show_request(&self) -> bool {
        fs::remove_file(self.root.join("show-window")).is_ok()
    }
    pub fn load<T: DeserializeOwned + Default>(&self, name: &str) -> Result<T> {
        match fs::read(self.root.join(name)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("本地 {name} 文件无法读取，请保留文件并检查后重试")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        self.write(name, &serde_json::to_vec_pretty(value)?)
    }
    pub fn config(&self) -> Result<Config> {
        self.load("config.json")
    }
    pub fn durable(&self) -> Result<Durable> {
        self.load("routing.json")
    }
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        atomic_write(&self.root.join(name), bytes)
    }
    pub fn credentials(&self) -> Result<Option<Credentials>> {
        let path = self.root.join("credentials.protected");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        let plain = zeroize::Zeroizing::new(unprotect(&bytes)?);
        Ok(Some(
            serde_json::from_slice(&plain).context("保存的 QQ 授权无法读取，请重新绑定")?,
        ))
    }
    pub fn save_credentials(&self, credentials: &Credentials) -> Result<()> {
        let plain = zeroize::Zeroizing::new(serde_json::to_vec(credentials)?);
        self.write("credentials.protected", &protect(&plain)?)
    }
    pub fn forget_credentials(&self) -> Result<()> {
        match fs::remove_file(self.root.join("credentials.protected")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(windows)]
fn protect(bytes: &[u8]) -> Result<Vec<u8>> {
    crypt(bytes, true)
}
#[cfg(windows)]
fn unprotect(bytes: &[u8]) -> Result<Vec<u8>> {
    crypt(bytes, false)
}
#[cfg(windows)]
fn crypt(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // DPAPI binds this data to the current OS user. Never fall back to plaintext.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            bail!("Windows 无法保护或解锁 QQ 授权，请使用原 Windows 账号重新绑定");
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as *mut _);
        Ok(result)
    }
}
#[cfg(not(windows))]
fn protect(_: &[u8]) -> Result<Vec<u8>> {
    bail!("当前版本的凭据保护仅支持 Windows")
}
#[cfg(not(windows))]
fn unprotect(_: &[u8]) -> Result<Vec<u8>> {
    bail!("当前版本的凭据保护仅支持 Windows")
}
