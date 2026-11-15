use crate::{qq::Api, store::Store};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub async fn configure(api: &Api, store: &Store) -> Result<()> {
    let current = api.request(reqwest::Method::GET, "/v2/menu", None).await?;
    let mut existing = current["menu"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if current["menu"].is_object()
        && !current["menu"]["items"].is_null()
        && !current["menu"]["items"].is_array()
    {
        bail!("QQ 菜单格式变化，未修改原菜单");
    }
    let shortcuts = vec![
        ("Workspaces", "/workspaces"),
        ("Sessions", "/sessions"),
        ("模型与命令", "/commands"),
        ("新建 Pi", "/new"),
        ("状态", "/status"),
        ("停止任务", "/stop"),
        ("关闭 Pi", "/close"),
        ("帮助", "/help"),
    ];
    let renamed = refresh_labels(&mut existing);
    let mut commands = Vec::new();
    collect_commands(&existing, &mut commands);
    let additions: Vec<Value> = shortcuts
        .into_iter()
        .filter(|(_, cmd)| !commands.iter().any(|c| c == cmd))
        .map(|(name, cmd)| json!({"type":"send_message","name":name,"send_message":cmd}))
        .collect();
    if additions.is_empty() && !renamed {
        return Ok(());
    }
    let mut items = existing;
    if items.is_empty() {
        items.extend(additions.iter().take(3).cloned());
        items.push(json!({"type":"menu","name":"更多","sub_menu_items":additions.iter().skip(3).cloned().collect::<Vec<_>>()}));
    } else {
        if items.len() + additions.len().div_ceil(5) > 10 {
            bail!("QQ 菜单已满，原菜单未修改。可直接在私聊发送 /help");
        }
        for (index, group) in additions.chunks(5).enumerate() {
            let name = ["Pi", "Pi 工具", "Pi 菜单"]
                .into_iter()
                .find(|name| !items.iter().any(|i| i["name"] == *name))
                .with_context(|| format!("QQ 菜单名称冲突（{}），未修改原菜单", index + 1))?;
            items.push(json!({"type":"menu","name":name,"sub_menu_items":group}));
        }
    }
    let mut menu = current["menu"].as_object().cloned().unwrap_or_default();
    menu.insert("items".into(), json!(items));
    store.save(
        &format!("menu-backup-{}.json", uuid::Uuid::new_v4()),
        &current,
    )?;
    api.request(reqwest::Method::PUT, "/v2/menu", Some(json!({"menu":menu})))
        .await?;
    let verified = api.request(reqwest::Method::GET, "/v2/menu", None).await?;
    if signature(&verified["menu"]["items"]) != signature(&json!(items)) {
        bail!("QQ 菜单提交后无法确认，请在手机查看；原菜单备份已保留");
    }
    Ok(())
}
// Only replace this client's old default labels; keep custom names and actions.
fn refresh_labels(items: &mut [Value]) -> bool {
    let mut changed = false;
    for item in items {
        if item["type"] == "send_message" {
            let label = match (item["send_message"].as_str(), item["name"].as_str()) {
                (Some("/workspaces"), Some("工作区")) => Some("Workspaces"),
                (Some("/sessions"), Some("Pi 会话")) => Some("Sessions"),
                (Some("/commands"), Some("Pi 控制")) => Some("模型与命令"),
                _ => None,
            };
            if let Some(label) = label {
                item["name"] = json!(label);
                changed = true;
            }
        }
        if let Some(children) = item["sub_menu_items"].as_array_mut() {
            changed |= refresh_labels(children);
        }
    }
    changed
}
fn collect_commands(items: &[Value], result: &mut Vec<String>) {
    for item in items {
        if let Some(cmd) = item["send_message"].as_str() {
            result.push(cmd.into());
        }
        if let Some(children) = item["sub_menu_items"].as_array() {
            collect_commands(children, result);
        }
    }
}
fn signature(items: &Value) -> Value {
    json!(items.as_array().unwrap_or(&Vec::new()).iter().map(|item|json!({"name":item["name"],"type":item["type"],"send_message":item["send_message"],"sub_menu_items":if item["sub_menu_items"].is_array(){signature(&item["sub_menu_items"])}else{Value::Null}})).collect::<Vec<_>>())
}
