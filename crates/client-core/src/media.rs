use crate::{
    qq::{Api, path_segment},
    store::atomic_write,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: String,
    pub file_name: String,
}
fn limit(kind: &str) -> usize {
    if kind == "image" {
        30 * 1024 * 1024
    } else {
        100 * 1024 * 1024
    }
}

pub async fn stage(path: &Path, kind: &str) -> Result<Attachment> {
    if !["image", "video", "file"].contains(&kind) {
        bail!("附件类型无效");
    }
    let bytes = read_file(path, kind).await?;
    validate(&bytes, kind)?;
    save(
        &bytes,
        kind,
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment.bin"),
    )
}
async fn read_file(path: &Path, kind: &str) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await.context("无法读取附件")?;
    if !file.metadata().await?.is_file() {
        bail!("附件必须是文件");
    }
    let mut bytes = Vec::new();
    file.take((limit(kind) + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit(kind) {
        bail!("附件过大；图片限 30 MiB，视频和文件限 100 MiB");
    }
    Ok(bytes)
}
fn save(bytes: &[u8], kind: &str, name: &str) -> Result<Attachment> {
    let name = safe_name(name);
    let folder = std::env::temp_dir()
        .join("pi-qq-herdr-client/attachments")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = folder.join(&name);
    atomic_write(&path, bytes).context("附件无法保存，请检查本机剩余空间")?;
    Ok(Attachment {
        path,
        kind: kind.into(),
        file_name: name,
    })
}
fn safe_name(name: &str) -> String {
    let mut result: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(150)
        .collect();
    result = result.trim().trim_end_matches(['.', ' ']).into();
    let stem = result
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.as_str())
    {
        result.insert(0, '_');
    }
    if result.is_empty() {
        "attachment.bin".into()
    } else {
        result
    }
}
fn validate(b: &[u8], kind: &str) -> Result<()> {
    let valid = match kind {
        "file" => true,
        "image" => {
            b.starts_with(&[255, 216, 255])
                || b.starts_with(b"\x89PNG\r\n\x1a\n")
                || b.starts_with(b"GIF87a")
                || b.starts_with(b"GIF89a")
                || (b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"))
                || (b.starts_with(b"BM") && b.len() >= 26)
        }
        "video" => {
            (b.len() >= 16
                && b.get(4..8) == Some(b"ftyp")
                && ![
                    b"avif", b"avis", b"heic", b"heix", b"hevc", b"hevx", b"mif1", b"msf1",
                    b"miaf", b"M4A ", b"M4B ", b"M4P ",
                ]
                .iter()
                .any(|v| b.get(8..12) == Some(*v)))
                || (b.starts_with(b"RIFF") && b.get(8..12) == Some(b"AVI "))
                || (b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
                    && (b.windows(4).take(4096).any(|w| w == b"webm")
                        || b.windows(8).take(4096).any(|w| w == b"matroska")))
        }
        _ => false,
    };
    if !valid {
        bail!("附件格式不支持：图片支持 JPG/PNG/GIF/WebP/BMP，视频支持 MP4/MOV/WebM/MKV/AVI");
    }
    Ok(())
}
fn download_url(value: &str, kind: &str) -> Result<url::Url> {
    let url = url::Url::parse(&if value.starts_with("//") {
        format!("https:{value}")
    } else {
        value.into()
    })
    .map_err(|_| anyhow!("QQ 附件地址无效，请重新发送"))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let allowed = host == "qpic.cn"
        || host.ends_with(".qpic.cn")
        || host == "multimedia.nt.qq.com.cn"
        || (kind == "file" && host == "grouptalk.c2c.qq.com");
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || !allowed
    {
        bail!("QQ 附件下载地址不受支持，请重新发送");
    }
    Ok(url)
}
fn message_attachments(message: &Value) -> Vec<&Value> {
    // QQ puts the referenced message in msg_elements. The reply's own
    // attachments are on the outer message.
    let Some(attachments) = message["attachments"].as_array() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    attachments
        .iter()
        .filter(|entry| entry["url"].as_str().is_none_or(|url| seen.insert(url)))
        .collect()
}

pub fn has_attachments(message: &Value) -> bool {
    !message_attachments(message).is_empty()
}

fn attachment_kind(entry: &Value) -> &'static str {
    let mime = entry["content_type"]
        .as_str()
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime == "file" {
        return "file";
    }
    if matches!(
        mime.as_str(),
        "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp" | "image/bmp"
    ) {
        return "image";
    }
    if mime.starts_with("video/") {
        return "video";
    }
    if mime.is_empty() || mime == "application/octet-stream" {
        let filename = entry["filename"].as_str().unwrap_or_default();
        let url = entry["url"].as_str().and_then(|raw| {
            url::Url::parse(&if raw.starts_with("//") {
                format!("https:{raw}")
            } else {
                raw.into()
            })
            .ok()
        });
        let names = [filename, url.as_ref().map_or("", |url| url.path())];
        for extensions in [
            ("image", &["jpg", "jpeg", "png", "gif", "webp", "bmp"][..]),
            ("video", &["mp4", "mov", "webm", "mkv", "avi"][..]),
        ] {
            if names.iter().any(|name| {
                name.rsplit_once('.').is_some_and(|(_, ext)| {
                    extensions
                        .1
                        .iter()
                        .any(|candidate| ext.eq_ignore_ascii_case(candidate))
                })
            }) {
                return extensions.0;
            }
        }
    }
    "file"
}

pub async fn receive(http: &reqwest::Client, message: &Value) -> Result<Vec<Attachment>> {
    let entries = message_attachments(message);
    if entries.len() > 10 {
        bail!("每条消息最多发送 10 个附件");
    }
    let mut result = Vec::new();
    for entry in entries {
        let kind = attachment_kind(entry);
        let mut url = download_url(entry["url"].as_str().context("QQ 附件缺少下载地址")?, kind)?;
        let mut saved = false;
        for redirects in 0..=3 {
            let mut response = http
                .get(url.clone())
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .map_err(|_| anyhow!("QQ 附件下载失败，Pi 尚未收到消息"))?;
            if response.status().is_redirection() {
                if redirects == 3 {
                    bail!("QQ 附件重定向过多");
                }
                let target = url.join(
                    response
                        .headers()
                        .get("location")
                        .and_then(|s| s.to_str().ok())
                        .context("附件跳转无效")?,
                )?;
                url = download_url(target.as_str(), kind)?;
                continue;
            }
            if !response.status().is_success()
                || response
                    .content_length()
                    .is_some_and(|len| len > limit(kind) as u64)
            {
                bail!("QQ 附件下载失败或文件过大");
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow!("QQ 附件下载中断"))?
            {
                if bytes.len() + chunk.len() > limit(kind) {
                    bail!("QQ 附件超出大小限制");
                }
                bytes.extend_from_slice(&chunk);
            }
            validate(&bytes, kind)?;
            let file = save(
                &bytes,
                kind,
                entry["filename"].as_str().unwrap_or(if kind == "image" {
                    "image"
                } else if kind == "video" {
                    "video"
                } else {
                    "attachment.bin"
                }),
            )?;
            result.push(file);
            saved = true;
            break;
        }
        if !saved {
            bail!("QQ 附件下载失败");
        }
    }
    Ok(result)
}

pub async fn upload(api: &Api, owner: &str, attachment: &Attachment) -> Result<String> {
    let bytes = read_file(&attachment.path, &attachment.kind).await?;
    let file_type = match attachment.kind.as_str() {
        "image" => 1,
        "video" => 2,
        _ => 4,
    };
    let result = if bytes.len() <= 10 * 1024 * 1024 {
        api.upload(owner, json!({"file_type":file_type,"file_name":attachment.file_name,"file_data":STANDARD.encode(&bytes),"srv_send_msg":false})).await?
    } else {
        let path = format!("/v2/users/{}", path_segment(owner)?);
        let prepared = api.request(reqwest::Method::POST, &format!("{path}/upload_prepare"), Some(json!({"file_type":file_type,"file_name":attachment.file_name,"file_size":bytes.len(),"md5":format!("{:x}",Md5::digest(&bytes)),"sha1":format!("{:x}",Sha1::digest(&bytes)),"md5_10m":format!("{:x}",Md5::digest(&bytes[..bytes.len().min(10_002_432)]))}))).await?;
        let id = prepared["upload_id"]
            .as_str()
            .context("QQ 附件上传任务无效")?;
        let block = prepared["block_size"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= 100 * 1024 * 1024)
            .context("QQ 上传分块无效")? as usize;
        let parts = prepared["parts"].as_array().context("QQ 附件分块缺失")?;
        for part in parts {
            let index = part["index"]
                .as_u64()
                .filter(|n| *n > 0)
                .context("QQ 附件分块编号无效")? as usize;
            let start = (index - 1).checked_mul(block).context("上传偏移无效")?;
            let slice = bytes
                .get(start..bytes.len().min(start.saturating_add(block)))
                .context("QQ 附件分块越界")?;
            let target =
                url::Url::parse(part["presigned_url"].as_str().context("QQ 上传地址缺失")?)?;
            if target.scheme() != "https"
                || target.host_str().is_none()
                || !target.username().is_empty()
                || target.password().is_some()
            {
                bail!("QQ 上传地址无效");
            }
            let response = api
                .http
                .put(target)
                .body(slice.to_vec())
                .send()
                .await
                .map_err(|_| anyhow!("QQ 附件分块上传失败"))?;
            if !response.status().is_success() {
                bail!("QQ 附件分块上传失败");
            }
            api.request(reqwest::Method::POST, &format!("{path}/upload_part_finish"), Some(json!({"upload_id":id,"part_index":index,"block_size":slice.len(),"md5":format!("{:x}",Md5::digest(slice))}))).await?;
        }
        api.request(
            reqwest::Method::POST,
            &format!("{path}/complete_upload"),
            Some(json!({"upload_id":id})),
        )
        .await?
    };
    Ok(result["file_info"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("QQ 未确认附件上传")?
        .into())
}
