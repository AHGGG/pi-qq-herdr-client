//! Phone controls, list pagination and source-bound command links.
use crate::{bridge::Hub, model::Session};
use anyhow::{Context, Result, bail};
use futures_util::future::try_join_all;
use serde_json::{Value, json};
use std::cmp::Reverse;

pub(crate) const HELP: &str = "模型与命令\n\n/commands · 打开 Model、Thinking、Compact、Reload、All cmds、Status、Approvals、Peek\n/commands 1 · 浏览已安装命令，每页最多 12 项\n/model [页码] · 浏览可用模型，常用的排在前面，每页最多 12 项\n/model PROVIDER/MODEL · 选择模型\n/thinking · 选择 Thinking 等级\n/thinking high · 设置等级（以模型实际支持为准）\n/compact [补充要求] · 整理上下文，随后通知结果\n/reload · 重载扩展并确认此 Pi 重新连接\n/pi 命令 参数 · 提交此 Pi 已安装的命令\n/status [sN] · 模型、状态与上下文\n/peek [sN] · 查看当前输出（Pi 屏幕文字）\n\n列表中的命令会先填入输入框，可添加参数后发送；模型和 Thinking 选项可直接选择。上一页、下一页、返回和状态按钮都绑定到原 Pi。\n\n模型与 Thinking 修改、压缩、重载和 /pi 需要 Pi 空闲。部分命令或弹窗仍需在 Pi 终端处理。\n\n/to sN /commands · 打开指定 Pi 的模型与命令，不切换当前 Session。";

const SOURCE_PAGES_PER_VIEW: u64 = 4;
const SOURCE_PAGE_SIZE: usize = 3;
const MODEL_PAGE_BATCH: usize = 16;
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub(crate) struct Reply {
    pub text: String,
    pub keyboard: Value,
}

pub(crate) async fn execute(hub: &Hub, session: &Session, name: &str, args: &str) -> Result<Reply> {
    if !session.capabilities.iter().any(|c| c == "commands") {
        bail!(
            "此 Pi 尚不支持手机控制，请在它空闲时于终端执行 /reload：{}",
            session.slot
        );
    }
    if name == "reload" {
        let token = uuid::Uuid::new_v4().to_string();
        let result = hub
            .action(
                session,
                json!({"action":"command","name":name,"args":args,"token":token}),
            )
            .await?;
        if result["status"] != "reloading" {
            bail!("Pi 未确认重载，请查看终端后再重试。");
        }
        // Registration is handled by the Hub independently of the serial router.
        // Waiting for Router::event here would prevent that event from being read.
        let restored = hub.wait_for_reload(session, &token).await;
        return Ok(Reply {
            text: if restored.is_some() {
                "重载已完成，此 Pi 已重新连接。"
            } else {
                "已请求重载，但尚未确认重新连接。请查看 /status 和 Pi 终端后再重试。"
            }
            .into(),
            keyboard: command_keyboard(restored.as_ref().unwrap_or(session), &Value::Null),
        });
    }
    let listing = (name == "model" && (args.is_empty() || positive_page(args)))
        || (name == "commands" && positive_page(args));
    let result = if listing {
        command_list(hub, session, name, args).await?
    } else {
        read_command(hub, session, name, args).await?
    };
    if !matches!(
        result["status"].as_str(),
        Some("completed" | "started" | "submitted")
    ) || !result["text"].is_string()
    {
        bail!("Pi 未确认命令结果，请查看终端后再重试。");
    }
    let needs_choices = (name == "model" && (args.is_empty() || positive_page(args)))
        || (name == "thinking" && args.is_empty())
        || (name == "commands" && !args.is_empty());
    let mut text = command_text(session, &result);
    if needs_choices && !result["view"].is_string() {
        text.push_str("\n请在 Pi 空闲时重载，以加载此 Session 的选项按钮。");
    }
    Ok(Reply {
        text,
        keyboard: command_keyboard(session, &result),
    })
}

async fn read_command(hub: &Hub, session: &Session, name: &str, args: &str) -> Result<Value> {
    hub.action(session, json!({"action":"command","name":name,"args":args}))
        .await
}

async fn command_list(hub: &Hub, session: &Session, name: &str, args: &str) -> Result<Value> {
    let first = read_command(hub, session, name, "1").await?;
    let model = name == "model";
    let view = if model { "models" } else { "commands" };
    let Some(source_pages) = first["pages"].as_u64().filter(|p| *p > 0) else {
        return if args.is_empty() || args == "1" {
            Ok(first)
        } else {
            read_command(hub, session, name, args).await
        };
    };
    if first["status"] != "completed"
        || first["view"] != view
        || !first["text"].is_string()
        || !first["choices"].is_array()
    {
        return if args.is_empty() || args == "1" {
            Ok(first)
        } else {
            read_command(hub, session, name, args).await
        };
    }
    let pages = source_pages.div_ceil(SOURCE_PAGES_PER_VIEW);
    let page = if args.is_empty() {
        1
    } else {
        args.parse::<u64>().unwrap_or(u64::MAX).min(pages)
    };
    let start = (page - 1) * SOURCE_PAGES_PER_VIEW + 1;
    // Rank across all source pages so a favorite can move to the first phone page.
    // The command catalog only needs the pages that will be displayed.
    let (from, through) = if model {
        (1, source_pages)
    } else {
        (
            start,
            start
                .saturating_add(SOURCE_PAGES_PER_VIEW - 1)
                .min(source_pages),
        )
    };
    let mut choices = Vec::new();
    let mut entries = Vec::new();
    for batch in (from..=through).step_by(MODEL_PAGE_BATCH) {
        let end = batch
            .saturating_add(MODEL_PAGE_BATCH as u64 - 1)
            .min(through);
        let replies = try_join_all((batch..=end).map(|number| {
            let first = &first;
            async move {
                if number == 1 {
                    Ok(first.clone())
                } else {
                    read_command(hub, session, name, &number.to_string()).await
                }
            }
        }))
        .await?;
        for (index, reply) in replies.iter().enumerate() {
            let items = reply["choices"]
                .as_array()
                .context("列表在加载过程中发生变化，请重新打开。")?;
            if reply["status"] != "completed"
                || reply["view"] != view
                || reply["page"].as_u64() != Some(batch + index as u64)
                || reply["pages"].as_u64() != Some(source_pages)
                || !reply["text"].is_string()
                || items.len() > SOURCE_PAGE_SIZE
            {
                bail!("列表在加载过程中发生变化，请重新打开。");
            }
            let text = reply["text"].as_str().unwrap();
            for item in items {
                let value = item["value"]
                    .as_str()
                    .context("Pi 返回的列表项无效，请重新打开。")?;
                if !model {
                    let prefix = format!("/pi {value}");
                    entries.push(
                        text.lines()
                            .find(|line| {
                                *line == prefix || line.starts_with(&format!("{prefix} — "))
                            })
                            .unwrap_or(&prefix)
                            .to_string(),
                    );
                }
                choices.push(item.clone());
            }
        }
    }
    if model {
        let usage = &hub.routes.lock().await.model_usage;
        choices.sort_by_cached_key(|choice| {
            let rank = usage
                .get(choice["value"].as_str().unwrap())
                .map(|stat| (stat.count, stat.last_used))
                .unwrap_or_default();
            Reverse((rank, choice["selected"] == true))
        });
        choices = choices
            .into_iter()
            .skip((start - 1) as usize * SOURCE_PAGE_SIZE)
            .take(SOURCE_PAGES_PER_VIEW as usize * SOURCE_PAGE_SIZE)
            .collect();
        entries = choices
            .iter()
            .map(|choice| choice["value"].as_str().unwrap().to_string())
            .collect();
    }
    let mut lines = Vec::new();
    if model {
        lines.push(
            first["text"]
                .as_str()
                .unwrap()
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        );
    }
    lines.push(format!(
        "{} · Page {page} of {pages}",
        if model {
            "Available models"
        } else {
            "Installed commands"
        }
    ));
    lines.push(if entries.is_empty() {
        if model {
            "No authenticated models."
        } else {
            "No installed commands."
        }
        .into()
    } else {
        entries.join("\n")
    });
    lines.push(if model {
        "Select: /model PROVIDER/MODEL"
    } else {
        "Tap a command to fill the composer, add arguments if needed, then send. Some commands need the Pi terminal."
    }.into());
    if page < pages {
        lines.push(format!("Next: /{name} {}", page + 1));
    }
    let mut result = first;
    result["choices"] = json!(choices);
    result["page"] = json!(page);
    result["pages"] = json!(pages);
    result["text"] = json!(lines.join("\n"));
    Ok(result)
}

fn positive_page(value: &str) -> bool {
    value.starts_with(|c: char| c.is_ascii_digit() && c != '0')
        && value.chars().all(|c| c.is_ascii_digit())
}

fn valid_choice(value: &str, model: bool) -> bool {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if model {
        value
            .split_once('/')
            .is_some_and(|(provider, id)| !provider.is_empty() && !id.is_empty())
    } else {
        !value.contains('/')
    }
}

fn markdown_text(text: &str) -> String {
    let mut result = String::new();
    for c in text.chars() {
        if "\\`*_{}[]()#+.!<>|~-".contains(c) {
            result.push('\\');
        }
        result.push(c);
    }
    result
}

fn choice_link(command: &str, label: &str, model: bool) -> String {
    let label: String = label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    let show = if label.chars().count() > 100 {
        format!("{}…", label.chars().take(99).collect::<String>())
    } else {
        label
    };
    let tag = if model {
        let encoded: String = url::form_urlencoded::byte_serialize(command.as_bytes()).collect();
        format!("<qqbot-cmd-enter text=\"{encoded}\" />")
    } else {
        input_link(command, &show)
    };
    if command.chars().count() <= 100 && tag.len() <= 1200 {
        tag
    } else {
        markdown_text(command.trim_end())
    }
}

pub(crate) fn input_link(command: &str, label: &str) -> String {
    let text: String = url::form_urlencoded::byte_serialize(command.as_bytes()).collect();
    let show: String = url::form_urlencoded::byte_serialize(label.as_bytes()).collect();
    format!("<qqbot-cmd-input text=\"{text}\" show=\"{show}\" reference=\"false\" />")
}

fn command_text(session: &Session, result: &Value) -> String {
    let text = result["text"].as_str().unwrap_or_default();
    let model = result["view"] == "models";
    let Some(choices) = result["choices"]
        .as_array()
        .filter(|_| model || result["view"] == "commands")
    else {
        return text.to_string();
    };
    text.lines()
        .map(|line| {
            if line == "Select: /model PROVIDER/MODEL" {
                return format!("Text: /to {} /model PROVIDER/MODEL", session.slot);
            }
            if let Some(next) = line.strip_prefix("Next: /") {
                if next.split_once(' ').is_some_and(|(name, page)| {
                    matches!(name, "model" | "commands") && positive_page(page)
                }) {
                    return format!("Next: /to {} /{next}", session.slot);
                }
            }
            let choice = choices.iter().find(|choice| {
                let Some(value) = choice["value"]
                    .as_str()
                    .filter(|value| valid_choice(value, model))
                else {
                    return false;
                };
                if model {
                    line.trim() == value
                } else {
                    line == format!("/pi {value}") || line.starts_with(&format!("/pi {value} — "))
                }
            });
            let Some(choice) = choice else {
                return line.to_string();
            };
            let value = choice["value"].as_str().unwrap();
            let command = if model {
                format!("/to {} /model {value}", session.slot)
            } else {
                format!("/to {} /pi {value} ", session.slot)
            };
            let label = choice["label"]
                .as_str()
                .filter(|label| !label.is_empty())
                .unwrap_or(value);
            let link = choice_link(&command, label, model);
            let suffix = if model {
                if choice["selected"] == true {
                    " · **Current**"
                } else {
                    ""
                }
            } else {
                &line[format!("/pi {value}").len()..]
            };
            format!("- {link}{suffix}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Button {
    label: String,
    command: String,
    selected: bool,
}
impl Button {
    fn new(label: &str, command: String) -> Self {
        Self {
            label: label.into(),
            command,
            selected: false,
        }
    }
}

fn command_keyboard(session: &Session, result: &Value) -> Value {
    let run = |text: &str| format!("/to {} {text}", session.slot);
    let mut buttons = Vec::new();
    if matches!(result["view"].as_str(), Some("models" | "commands"))
        && result["choices"].is_array()
    {
        let command = if result["view"] == "models" {
            "model"
        } else {
            "commands"
        };
        if let (Some(page), Some(pages)) = (result["page"].as_u64(), result["pages"].as_u64()) {
            if page > 1 {
                buttons.push(Button::new(
                    "Previous",
                    run(&format!("/{command} {}", page - 1)),
                ));
            }
            if page < pages {
                buttons.push(Button::new(
                    "Next",
                    run(&format!("/{command} {}", page + 1)),
                ));
            }
        }
    } else if result["view"] == "thinking"
        && result["choices"]
            .as_array()
            .is_some_and(|choices| choices.len() <= 7)
    {
        for choice in result["choices"].as_array().unwrap() {
            let Some(level) = choice["value"]
                .as_str()
                .filter(|level| THINKING_LEVELS.contains(level))
            else {
                continue;
            };
            let mut button = Button::new(
                choice["label"].as_str().unwrap_or(level),
                run(&format!("/thinking {level}")),
            );
            button.selected = choice["selected"] == true;
            buttons.push(button);
        }
    } else {
        buttons.extend([
            Button::new("Model", run("/model")),
            Button::new("Thinking", run("/thinking")),
            Button::new("Compact", run("/compact")),
            Button::new("Reload", run("/reload")),
            Button::new("All cmds", run("/commands 1")),
            Button::new("Status", format!("/status {}", session.slot)),
            Button::new("Approvals", format!("/permissions {}", session.slot)),
            Button::new("Peek", format!("/peek {}", session.slot)),
        ]);
        return keyboard(&buttons);
    }
    // Keep Back/Status in their own row, as in the original phone panel.
    let mut rows = keyboard(&buttons)["content"]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let back = [
        Button::new("Back", run("/commands")),
        Button::new("Status", format!("/status {}", session.slot)),
    ];
    let mut row = keyboard(&back)["content"]["rows"][0].clone();
    for (index, button) in row["buttons"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        button["id"] = json!(format!("back-{index}"));
    }
    rows.push(row);
    json!({"content":{"rows":rows}})
}

fn keyboard(buttons: &[Button]) -> Value {
    let rows: Vec<_> = buttons.chunks(2).enumerate().map(|(row, buttons)| {
        json!({"buttons": buttons.iter().enumerate().map(|(column, button)| {
            json!({"id":format!("control-{row}-{column}"),"render_data":{"label":button.label,"visited_label":button.label,"style":if button.selected {0} else {1}},"action":{"type":2,"permission":{"type":2},"data":button.command,"enter":true,"reply":false,"unsupport_tips":"请发送文字指令"}})
        }).collect::<Vec<_>>()})
    }).collect();
    json!({"content":{"rows":rows}})
}
