use crate::{APP_NAME, platform};
use gpui_kit::{
    base::Disableable,
    component::{
        ActiveTheme, Icon, IconName, Sizable, StyledExt,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        input::{Input, InputState},
        spinner::Spinner,
    },
    prelude::FluentBuilder as _,
    *,
};
use pi_qq_client_core::{
    Client, Command, Config, Snapshot,
    model::{CREATE_BOT_URL, Dependency, GuideStatus, HERDR_URL, PI_URL},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

pub struct ClientWindow {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    snapshot: Snapshot,
    settings: bool,
    confirm_rebind: bool,
    start_at_login: bool,
    error: Option<String>,
    pi_path: Entity<InputState>,
    core_path: Entity<InputState>,
    qr_url: Option<String>,
    qr: Option<Arc<Image>>,
    tray: Option<TrayIcon>,
    connection_item: MenuItem,
    open_id: MenuId,
    settings_id: MenuId,
    quit_id: MenuId,
    _task: Task<()>,
}
impl ClientWindow {
    pub fn new(
        client: Client,
        background: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pi_path = cx.new(|cx| InputState::new(window, cx).placeholder("自动检测"));
        let core_path = cx.new(|cx| InputState::new(window, cx).placeholder("自动检测"));
        let menu = Menu::new();
        let open = MenuItem::new("打开主页面", true, None);
        let connection = MenuItem::new("暂停连接", true, None);
        let settings = MenuItem::new("设置", true, None);
        let quit = MenuItem::new("退出", true, None);
        let _ = menu.append_items(&[&open, &connection, &settings, &quit]);
        platform::set_window_icon(window);
        let image = image::load_from_memory(include_bytes!("../assets/pi-qq.png"))
            .expect("embedded application icon")
            .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
            .into_rgba8();
        let icon = tray_icon::Icon::from_rgba(image.into_raw(), 32, 32).expect("tray icon");
        let tray = TrayIconBuilder::new()
            .with_tooltip(APP_NAME)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_icon(icon)
            .build()
            .ok();
        let can_hide = tray.is_some();
        window.on_window_should_close(cx, move |window, cx| {
            if can_hide {
                platform::hide(window, cx);
                false
            } else {
                true
            }
        });
        let events = client.snapshots;
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| {
                        let mut latest = None;
                        while let Ok(event) = events.try_recv() {
                            latest = Some(event);
                        }
                        if let Some(event) = latest {
                            this.receive(event, window, cx);
                        }
                        while let Ok(event) = MenuEvent::receiver().try_recv() {
                            if event.id == this.quit_id {
                                let _ = this.commands.send(Command::Shutdown);
                                cx.quit();
                            } else if &event.id == this.connection_item.id() {
                                this.send(Command::SetPaused(!this.snapshot.paused), cx);
                            } else if event.id == this.settings_id {
                                this.open_settings(window, cx);
                                platform::show(window, cx);
                            } else if event.id == this.open_id {
                                this.settings = false;
                                this.confirm_rebind = false;
                                platform::show(window, cx);
                            }
                        }
                        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                            if matches!(
                                event,
                                TrayIconEvent::Click {
                                    button: MouseButton::Left,
                                    button_state: MouseButtonState::Up,
                                    ..
                                } | TrayIconEvent::DoubleClick {
                                    button: MouseButton::Left,
                                    ..
                                }
                            ) {
                                this.settings = false;
                                this.confirm_rebind = false;
                                platform::show(window, cx);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        if background && can_hide {
            platform::hide(window, cx);
        }
        Self {
            commands: client.commands,
            snapshot: Snapshot::default(),
            settings: false,
            confirm_rebind: false,
            start_at_login: false,
            error: None,
            pi_path,
            core_path,
            qr_url: None,
            qr: None,
            tray,
            connection_item: connection,
            open_id: open.id().clone(),
            settings_id: settings.id().clone(),
            quit_id: quit.id().clone(),
            _task: task,
        }
    }
    fn receive(&mut self, snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        if snapshot.show_window != self.snapshot.show_window {
            platform::show(window, cx);
        }
        if self.qr_url != snapshot.qr_url {
            self.qr_url = snapshot.qr_url.clone();
            self.qr = snapshot
                .qr_url
                .as_deref()
                .and_then(|url| qr_image(url).ok());
            if snapshot.qr_url.is_some() && self.qr.is_none() {
                self.error = Some("二维码无法绘制，请刷新重试。".into());
            }
        }
        self.connection_item.set_text(if snapshot.paused {
            "启动连接"
        } else {
            "暂停连接"
        });
        self.snapshot = snapshot;
        cx.notify();
    }
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = &self.snapshot.config;
        self.pi_path.update(cx, |s, cx| {
            s.set_value(path_text(&config.pi_path), window, cx)
        });
        self.core_path.update(cx, |s, cx| {
            s.set_value(path_text(&config.core_path), window, cx)
        });
        self.start_at_login = config.start_at_login;
        self.settings = true;
        self.confirm_rebind = false;
        self.error = None;
        cx.notify();
    }
    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.commands.send(command).is_err() {
            self.error = Some("后台服务已停止，请重新打开客户端。".into());
        }
        cx.notify();
    }
    fn save(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let optional = |s: String| {
            let s = s.trim();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        let config = Config {
            pi_path: optional(self.pi_path.read(cx).value().to_string()),
            core_path: optional(self.core_path.read(cx).value().to_string()),
            start_at_login: self.start_at_login,
        };
        if config.start_at_login != self.snapshot.config.start_at_login {
            if let Err(e) = platform::set_start_at_login(config.start_at_login) {
                self.error = Some(format!("登录启动设置失败：{e}"));
                cx.notify();
                return;
            }
        }
        self.send(Command::Save(config), cx);
        self.settings = false;
    }
    fn body(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.settings {
            return div()
                .flex()
                .flex_col()
                .gap_5()
                .child(div().text_lg().font_semibold().child("设置"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child("Pi 路径（可选）")
                        .child(Input::new(&self.pi_path)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child("Herdr Core 路径（可选）")
                        .child(Input::new(&self.core_path)),
                )
                .child(
                    Checkbox::new("login")
                        .label("登录后启动")
                        .checked(self.start_at_login)
                        .on_click(cx.listener(|this, value, _, cx| {
                            this.start_at_login = *value;
                            cx.notify();
                        })),
                )
                .when(
                    self.snapshot.qq_ready
                        && self.snapshot.owner_bound
                        && self.snapshot.menu_error.is_some(),
                    |el| {
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(div().text_sm().child(format!(
                                    "手机快捷菜单配置失败：{}",
                                    self.snapshot.menu_error.as_deref().unwrap_or_default()
                                )))
                                .child(Button::new("retry-menu").label("重试手机菜单").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.send(Command::ConfigureMenu, cx)
                                    }),
                                )),
                        )
                    },
                )
                .when(self.snapshot.failed_deliveries > 0, |el| {
                    el.child(
                        Button::new("retry-outbox")
                            .label("重试发送失败的消息")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send(Command::RetryDeliveries, cx)
                            })),
                    )
                })
                .child(if self.confirm_rebind {
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .child("重新绑定会断开当前 QQ。旧消息不会发给新账号。"),
                        )
                        .child(
                            Button::new("confirm-rebind")
                                .label("确认重新绑定")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.send(Command::Rebind, cx);
                                    this.settings = false;
                                    this.confirm_rebind = false;
                                })),
                        )
                        .into_any_element()
                } else {
                    Button::new("rebind")
                        .label("QQ 重新绑定")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_rebind = true;
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-settings")
                                .label("返回")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings = false;
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("save")
                                .primary()
                                .label("保存")
                                .on_click(cx.listener(Self::save)),
                        ),
                )
                .into_any_element();
        }
        if self.snapshot.paused {
            return div()
                .flex()
                .flex_col()
                .gap_4()
                .py_8()
                .child(div().text_lg().font_semibold().child("连接已暂停"))
                .child(
                    div()
                        .text_sm()
                        .child("QQ 收发已暂停，Pi 与 Herdr 继续运行。"),
                )
                .child(
                    Button::new("resume-connection")
                        .primary()
                        .label("启动连接")
                        .on_click(
                            cx.listener(|this, _, _, cx| this.send(Command::SetPaused(false), cx)),
                        ),
                )
                .into_any_element();
        }
        if let Some(missing) = &self.snapshot.missing {
            let (text, url) = if *missing == Dependency::Herdr {
                (
                    "未检测到 Herdr Core，请前往 Herdr 官网安装，完成后点击「重新检测」。",
                    HERDR_URL,
                )
            } else {
                (
                    "未检测到 Pi，请前往 Pi 官网安装，完成后点击「重新检测」。",
                    PI_URL,
                )
            };
            return div()
                .flex()
                .flex_col()
                .gap_6()
                .py_12()
                .child(div().text_base().child(text))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(Button::new("install").primary().label("前往安装").on_click(
                            move |_, _, _| {
                                let _ = open::that_detached(url);
                            },
                        ))
                        .child(Button::new("detect").label("重新检测").on_click(
                            cx.listener(|this, _, _, cx| this.send(Command::Detect, cx)),
                        )),
                )
                .into_any_element();
        }
        if !self.snapshot.dependencies_ready {
            return div()
                .flex()
                .flex_col()
                .gap_5()
                .py_8()
                .child(Spinner::new().small())
                .child("正在检查运行环境；如有问题，处理提示后重新检测。")
                .child(
                    Button::new("retry-dependencies")
                        .label("重新检测")
                        .on_click(cx.listener(|this, _, _, cx| this.send(Command::Detect, cx))),
                )
                .into_any_element();
        }
        if let Some(code) = &self.snapshot.pair_code {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .gap_5()
                .py_8()
                .child(div().text_lg().child("在机器人私聊中发送"))
                .child(
                    div()
                        .text_xl()
                        .font_semibold()
                        .child(format!("/pair {code}")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("这一步将你的 QQ 与此电脑配对。配对码 10 分钟内有效。"),
                )
                .child(
                    Button::new("new-pair")
                        .label("生成新配对码")
                        .on_click(cx.listener(|this, _, _, cx| this.send(Command::Pair, cx))),
                )
                .into_any_element();
        }
        if self.snapshot.qq_ready && !self.snapshot.owner_bound {
            return div()
                .flex()
                .flex_col()
                .gap_5()
                .py_8()
                .child("QQ 已连接，请完成用户配对。")
                .child(
                    Button::new("pair")
                        .primary()
                        .label("生成配对码")
                        .on_click(cx.listener(|this, _, _, cx| this.send(Command::Pair, cx))),
                )
                .into_any_element();
        }
        if self.snapshot.qq_ready || self.snapshot.owner_bound {
            if !self.snapshot.qq_ready || !self.snapshot.core_ready {
                return div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .py_8()
                    .child(Spinner::new().small())
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(if self.snapshot.qq_ready {
                                "QQ 已连接，正在连接 Herdr……"
                            } else {
                                "QQ 正在重连……"
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child("连接恢复后，可继续在手机 QQ 中使用 Pi，无需重新扫码。"),
                    )
                    .into_any_element();
            }
            return div()
                .flex()
                .flex_col()
                .gap_4()
                .py_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "QQ、Herdr 已就绪 · 已连接 {} 个 Pi 会话",
                            self.snapshot.connected_sessions
                        )),
                )
                .child(
                    div()
                        .text_sm()
                        .child("打开手机 QQ 中刚才授权的机器人私聊："),
                )
                .child(guide_step(
                    "已有 Pi",
                    "/sessions",
                    "选择一个会话，再直接发送任务。",
                    cx,
                ))
                .child(guide_step(
                    "新建 Pi",
                    "/new",
                    "等创建成功后，再直接发送任务。",
                    cx,
                ))
                .child(div().text_sm().child("发送 /help 查看全部用法。"))
                .child(
                    Button::new("send-guide")
                        .primary()
                        .label(match self.snapshot.guide_status {
                            GuideStatus::NotSent => "发送入门指南到 QQ",
                            GuideStatus::Sending => "正在发送入门指南……",
                            GuideStatus::Sent => "重新发送入门指南到 QQ",
                            GuideStatus::Failed => "发送未成功，点击重试",
                        })
                        .disabled(self.snapshot.guide_status == GuideStatus::Sending)
                        .on_click(cx.listener(|this, _, _, cx| this.send(Command::SendGuide, cx))),
                )
                .into_any_element();
        }
        let mut body = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_4()
            .py_4()
            .child(div().text_lg().font_semibold().child("用手机 QQ 扫码连接"));
        if let Some(image) = &self.qr {
            body = body.child(img(image.clone()).w(px(248.)).h(px(248.)));
        }
        body = body.child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(if self.snapshot.qr_status.is_empty() {
                    "正在检查环境……".to_string()
                } else {
                    self.snapshot.qr_status.clone()
                }),
        );
        if self.qr.is_some() {
            body = body.child(
                Button::new("cancel-qr")
                    .small()
                    .label("取消")
                    .on_click(cx.listener(|this, _, _, cx| this.send(Command::CancelQr, cx))),
            );
        } else {
            body = body.child(
                Button::new("start-qr")
                    .primary()
                    .label("扫码连接")
                    .on_click(cx.listener(|this, _, _, cx| this.send(Command::StartQr, cx))),
            );
        }
        body.child(
            Button::new("create-bot")
                .small()
                .label("还没有机器人？")
                .on_click(|_, _, _| {
                    let _ = open::that_detached(CREATE_BOT_URL);
                }),
        )
        .into_any_element()
    }
}
impl Render for ClientWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = !self.snapshot.paused
            && self.snapshot.dependencies_ready
            && self.snapshot.qq_ready
            && self.snapshot.owner_bound
            && self.snapshot.core_ready;
        let phase: u64 = if self.settings {
            0
        } else if self.snapshot.paused {
            7
        } else if self.snapshot.missing.is_some() {
            1
        } else if !self.snapshot.dependencies_ready {
            2
        } else if self.snapshot.pair_code.is_some() {
            3
        } else if connected {
            4
        } else if self.snapshot.qq_ready || self.snapshot.owner_bound {
            5
        } else {
            6
        };
        let body = self.body(window, cx);
        let body = div().child(body).with_animation(
            ("page-entry", phase),
            Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
            |element, progress| element.opacity(progress),
        );
        div()
            .id("client-window")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_family("Microsoft YaHei UI")
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_xl().font_semibold().child(APP_NAME))
                            .when(connected, |el| {
                                el.child(
                                    Icon::new(IconName::CircleCheck)
                                        .small()
                                        .text_color(cx.theme().success),
                                )
                            }),
                    )
                    .when(!self.settings, |el| {
                        el.child(Button::new("settings").small().label("设置").on_click(
                            cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                        ))
                    }),
            )
            .child(body)
            .when_some(
                self.error
                    .clone()
                    .or_else(|| self.snapshot.error.clone())
                    .or_else(|| self.snapshot.core_error.clone()),
                |el, error| el.child(div().text_sm().text_color(cx.theme().danger).child(error)),
            )
            .when_some(self.snapshot.notice.clone(), |el, text| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(text),
                )
            })
            .when(self.tray.is_none(), |el| {
                el.child(div().text_xs().child("托盘不可用，关闭窗口将退出客户端。"))
            })
    }
}
fn guide_step(title: &str, command: &str, detail: &str, cx: &App) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_lg()
        .bg(cx.theme().muted)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_sm().child(title.to_string()))
                .child(
                    div()
                        .text_base()
                        .font_semibold()
                        .text_color(cx.theme().primary)
                        .child(command.to_string()),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(detail.to_string()),
        )
        .into_any_element()
}

fn path_text(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}
fn qr_image(url: &str) -> anyhow::Result<Arc<Image>> {
    let code = qrcode::QrCode::new(url.as_bytes())?;
    let width = code.width();
    let scale = 5usize;
    let size = (width + 8) * scale;
    let mut bitmap =
        image::RgbaImage::from_pixel(size as u32, size as u32, image::Rgba([255, 255, 255, 255]));
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        bitmap.put_pixel(
                            ((x + 4) * scale + dx) as u32,
                            ((y + 4) * scale + dy) as u32,
                            image::Rgba([20, 35, 31, 255]),
                        );
                    }
                }
            }
        }
    }
    let mut bytes = std::io::Cursor::new(Vec::new());
    bitmap.write_to(&mut bytes, image::ImageFormat::Png)?;
    Ok(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        bytes.into_inner(),
    )))
}
