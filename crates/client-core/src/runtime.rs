use crate::{
    bridge::{BridgeEvent, Hub},
    herdr::Core,
    install,
    model::{Command, Config, Credentials, GuideStatus, Snapshot},
    outbox::{Accepted, Binding, Outbox},
    qq::{self, Api, BindResult, BindTask, GatewayEvent},
    router::Router,
    store::Store,
};
use anyhow::Result;
use rand::RngCore;
use std::{
    sync::{Arc, mpsc as std_mpsc},
    time::{Duration, Instant},
};
use tokio::sync::{RwLock, mpsc, watch};

pub struct Client {
    pub commands: mpsc::UnboundedSender<Command>,
    pub snapshots: std_mpsc::Receiver<Snapshot>,
}

pub fn start(store: Store) -> Result<Client> {
    let (commands, incoming) = mpsc::unbounded_channel();
    let (snapshots, receiver) = std_mpsc::sync_channel(4);
    std::thread::Builder::new()
        .name("pi-qq-client".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| runtime.block_on(run(store, incoming, snapshots.clone())));
            if let Err(error) = result {
                let _ = snapshots.send(Snapshot {
                    error: Some(error.to_string()),
                    ..Default::default()
                });
            }
        })?;
    Ok(Client {
        commands,
        snapshots: receiver,
    })
}

enum QrEvent {
    Display(String),
    Status(&'static str),
    Complete(Credentials),
    Error(String),
}
enum RouteEvent {
    Message(String, serde_json::Value),
    Bridge(BridgeEvent),
}

async fn run(
    store: Store,
    mut commands: mpsc::UnboundedReceiver<Command>,
    snapshots: std_mpsc::SyncSender<Snapshot>,
) -> Result<()> {
    let config = Arc::new(RwLock::new(store.config()?));
    let core = Arc::new(RwLock::new(None::<Core>));
    let http = qq::http_client()?;
    let (binding_tx, binding_rx) = watch::channel(None);
    let (accepted_tx, mut accepted_rx) = mpsc::channel::<Accepted>(128);
    let outbox = Outbox::start(store.clone(), binding_rx, accepted_tx)?;
    let (events, mut bridge_events) = mpsc::channel::<BridgeEvent>(128);
    let hub = Hub::start(store.clone(), outbox.clone(), events).await?;
    let (route_tx, mut route_rx) = mpsc::channel(128);
    let (errors, mut error_rx) = mpsc::unbounded_channel::<String>();
    let (menu_results, mut menu_rx) = mpsc::unbounded_channel::<(String, Option<String>)>();
    let mut router = Router::new(hub.clone(), outbox.clone(), core.clone());
    let routing_http = http.clone();
    // User tasks can take time (new Pi registration, file transfer). They must not
    // hold up QR cancellation, desktop state, account changes or shutdown.
    tokio::spawn(async move {
        while let Some(event) = route_rx.recv().await {
            let result = match event {
                RouteEvent::Message(scope, message) => {
                    router.expected_scope = Some(scope);
                    router.inbound(&routing_http, message).await
                }
                RouteEvent::Bridge(event) => {
                    router.expected_scope = None;
                    router.event(event).await
                }
            };
            if let Err(error) = result {
                let _ = errors.send(error.to_string());
            }
        }
    });
    let mut snapshot = Snapshot {
        config: config.read().await.clone(),
        qr_status: "准备连接".into(),
        ..Default::default()
    };
    let mut credentials = match store.credentials() {
        Ok(value) => value,
        Err(error) => {
            snapshot.error = Some(error.to_string());
            None
        }
    };
    let scope = credentials.as_ref().and_then(|c| {
        c.owner
            .as_ref()
            .map(|owner| format!("{}:{owner}", c.app_id))
    });
    for source in outbox.recovered_sources(scope.as_deref()).await {
        let mut routes = hub.routes.lock().await;
        for reference in source.references {
            routes.remember_source(reference, source.source.clone());
        }
    }
    hub.save_routes().await?;
    let mut qr_job = None::<tokio::task::JoinHandle<()>>;
    let (_, mut qr_rx) = mpsc::channel(8);
    let mut gateway = None::<tokio::task::JoinHandle<()>>;
    let mut menu_job = None::<(String, tokio::task::JoinHandle<()>)>;
    let (_, mut gateway_rx) = mpsc::channel(128);
    let mut pairing = None::<(String, Instant)>;
    detect(&config, &core, &mut snapshot).await;
    if snapshot.dependencies_ready {
        if let Some(creds) = credentials.as_ref() {
            let (job, events) = connect(&http, creds, &binding_tx);
            gateway = Some(job);
            gateway_rx = events;
            if creds.owner.is_none() {
                pairing = Some(new_pairing());
            }
        } else {
            let (job, events) = start_qr(http.clone());
            qr_job = Some(job);
            qr_rx = events;
        }
    }
    snapshot.pair_code = pairing.as_ref().map(|(code, _)| code.clone());
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                match command {
                    Command::Shutdown => break,
                    Command::SetPaused(paused) => {
                        if snapshot.paused == paused { continue; }
                        snapshot.paused = paused;
                        snapshot.error = None;
                        if paused {
                            if let Some(job) = gateway.take() { job.abort(); }
                            if let Some(job) = qr_job.take() { job.abort(); }
                            if menu_job.as_ref().is_some_and(|(_, job)| !job.is_finished()) {
                                if let Some((_, job)) = menu_job.take() { job.abort(); }
                            }
                            snapshot.qq_ready = false;
                            snapshot.qr_url = None;
                            snapshot.qr_status = "连接已暂停".into();
                            if let Err(error) = disconnect_outbox(&binding_tx, &outbox).await {
                                snapshot.error = Some(error.to_string());
                            }
                        } else {
                            snapshot.qr_status = "准备连接".into();
                            if !snapshot.dependencies_ready {
                                detect(&config, &core, &mut snapshot).await;
                            }
                            if snapshot.dependencies_ready {
                                if let Some(creds) = credentials.as_ref() {
                                    let (job, events) = connect(&http, creds, &binding_tx);
                                    gateway = Some(job); gateway_rx = events;
                                    if creds.owner.is_none() { pairing = Some(new_pairing()); }
                                } else {
                                    let (job, events) = start_qr(http.clone());
                                    qr_job = Some(job); qr_rx = events;
                                }
                            }
                        }
                    }
                    Command::Detect => {
                        snapshot.error = None;
                        detect(&config, &core, &mut snapshot).await;
                        if !snapshot.paused && snapshot.dependencies_ready && gateway.is_none() {
                            if let Some(creds) = credentials.as_ref() {
                                let (job, events) = connect(&http, creds, &binding_tx);
                                gateway = Some(job); gateway_rx = events;
                                if creds.owner.is_none() { pairing = Some(new_pairing()); }
                            } else if qr_job.is_none() {
                                let (job, events) = start_qr(http.clone()); qr_job = Some(job); qr_rx = events;
                            }
                        }
                    }
                    Command::StartQr => {
                        if snapshot.paused {
                            snapshot.error = Some("连接已暂停，请先启动连接。".into());
                            continue;
                        }
                        if credentials.is_some() {
                            snapshot.error = Some("已有 QQ 授权，正在自动重连。若要更换机器人，请在设置中选择重新绑定。".into());
                            continue;
                        }
                        if !snapshot.dependencies_ready { continue; }
                        if let Some(job) = qr_job.take() { job.abort(); }
                        snapshot.error = None; snapshot.qr_status = "正在生成二维码……".into();
                        let (job, events) = start_qr(http.clone()); qr_job = Some(job); qr_rx = events;
                    }
                    Command::CancelQr => {
                        if let Some(job) = qr_job.take() { job.abort(); }
                        snapshot.qr_url = None; snapshot.qr_status = "扫码已取消".into();
                    }
                    Command::Rebind => {
                        if let Some(job) = qr_job.take() { job.abort(); }
                        if let Some(job) = gateway.take() { job.abort(); }
                        if let Some((_, job)) = menu_job.take() { job.abort(); }
                        snapshot.menu_error = None;
                        snapshot.qr_url = None; snapshot.qq_ready = false; pairing = None;
                        binding_tx.send_replace(None); credentials = None;
                        if let Err(error) = outbox.discard_pending().await { snapshot.error = Some(error.to_string()); }
                        if let Err(error) = store.forget_credentials() { snapshot.error = Some(error.to_string()); }
                        {
                            let mut routes = hub.routes.lock().await;
                            routes.selected = None; routes.workspace = None; routes.sources.clear(); routes.source_order.clear(); routes.pending_launches.clear(); routes.recent_sessions.clear(); routes.model_usage.clear(); routes.seen.clear();
                        }
                        if let Err(error) = hub.save_routes().await { snapshot.error = Some(error.to_string()); }
                        snapshot.paused = false;
                        if snapshot.dependencies_ready {
                            snapshot.qr_status = "正在生成二维码……".into();
                            let (job, events) = start_qr(http.clone());
                            qr_job = Some(job); qr_rx = events;
                        } else {
                            snapshot.qr_status = "准备连接".into();
                        }
                    }
                    Command::Pair => { if credentials.is_some() { pairing = Some(new_pairing()); } }
                    Command::Save(value) => {
                        if let Err(error) = store.save("config.json", &value) {
                            snapshot.error = Some(error.to_string());
                        } else {
                            let paths_changed = { let old = config.read().await; old.pi_path != value.pi_path || old.core_path != value.core_path };
                            *config.write().await = value.clone(); snapshot.config = value; snapshot.error = None;
                            snapshot.notice = Some(if paths_changed { "设置已保存，Pi/Core 路径将在下次启动时使用。" } else { "设置已保存。" }.into());
                        }
                    }
                    Command::ConfigureMenu => {
                        if binding_tx.borrow().as_ref().is_some_and(|b| b.ready) {
                            if let Some((_, job)) = menu_job.take() { job.abort(); }
                            snapshot.menu_error = None;
                        } else { snapshot.menu_error = Some("请先完成 QQ 连接和配对。".into()); }
                    }
                    Command::SendGuide => {
                        match send_guide(&outbox, true).await {
                            Ok(()) => snapshot.error = None,
                            Err(error) => snapshot.error = Some(error.to_string()),
                        }
                    }
                    Command::RetryDeliveries => {
                        match outbox.retry().await {
                            Ok(()) => snapshot.notice = Some("已重新排队发送失败的消息。".into()),
                            Err(error) => snapshot.error = Some(error.to_string()),
                        }
                    }
                }
            }
            event = qr_rx.recv(), if qr_job.is_some() => {
                match event {
                    Some(QrEvent::Display(url)) => { snapshot.qr_url = Some(url); snapshot.qr_status = "等待扫码".into(); }
                    Some(QrEvent::Status(status)) => snapshot.qr_status = status.into(),
                    Some(QrEvent::Error(error)) => { snapshot.error = Some(error); snapshot.qr_status = "扫码连接失败，请重试".into(); snapshot.qr_url = None; qr_job = None; }
                    Some(QrEvent::Complete(creds)) => {
                        qr_job = None; snapshot.qr_url = None;
                        match store.save_credentials(&creds) {
                            Ok(()) => {
                                if let Some(job) = gateway.take() { job.abort(); }
                                let (job, events) = connect(&http, &creds, &binding_tx); gateway = Some(job); gateway_rx = events;
                                if creds.owner.is_none() { pairing = Some(new_pairing()); }
                                credentials = Some(creds); snapshot.error = None; snapshot.qr_status = "正在连接 QQ".into();
                            }
                            Err(error) => { snapshot.error = Some(error.to_string()); snapshot.qr_status = "授权保存失败，请重新连接".into(); }
                        }
                    }
                    None => qr_job = None,
                }
            }
            event = gateway_rx.recv(), if gateway.is_some() => {
                match event {
                    Some(GatewayEvent::Connected) => {
                        snapshot.qq_ready = true; snapshot.qr_status = "QQ 已连接".into();
                        if let Some(creds) = credentials.as_ref() {
                            if let Some(owner) = &creds.owner {
                                binding_tx.send_replace(Some(Binding { api: Api::new(http.clone(), creds.clone()), owner: owner.clone(), app_id: creds.app_id.clone(), ready: true }));
                            }
                        }
                    }
                    Some(GatewayEvent::Disconnected) => {
                        snapshot.qq_ready = false; snapshot.qr_status = "QQ 正在重连".into();
                        if let Err(error) = disconnect_outbox(&binding_tx, &outbox).await {
                            snapshot.error = Some(error.to_string());
                        }
                    }
                    Some(GatewayEvent::Message(message)) => {
                        let sender = message["author"]["user_openid"].as_str().unwrap_or_default();
                        if sender.is_empty() { continue; }
                        let text = message["content"].as_str().unwrap_or_default().trim();
                        let pair_matches = pairing.as_ref().is_some_and(|(code, created)| created.elapsed() < Duration::from_secs(600) && text == format!("/pair {code}"));
                        if pair_matches {
                            if let Some(creds) = credentials.as_mut() {
                                let previous = creds.owner.clone(); creds.owner = Some(sender.into());
                                if let Err(error) = store.save_credentials(creds) { creds.owner = previous; snapshot.error = Some(error.to_string()); }
                                else {
                                    binding_tx.send_replace(Some(Binding { api: Api::new(http.clone(), creds.clone()), owner: sender.into(), app_id: creds.app_id.clone(), ready: snapshot.qq_ready }));
                                    pairing = None;
                                }
                            }
                        } else if credentials.as_ref().and_then(|c| c.owner.as_deref()) == Some(sender) {
                            if let Some(creds) = credentials.as_ref() {
                                let scope = format!("{}:{sender}", creds.app_id);
                                let _ = route_tx.send(RouteEvent::Message(scope, message)).await;
                            }
                        }
                    }
                    None => {
                        gateway = None; snapshot.qq_ready = false;
                        snapshot.error = Some("QQ 服务已停止，请重新检测。".into());
                        if let Err(error) = disconnect_outbox(&binding_tx, &outbox).await { snapshot.error = Some(error.to_string()); }
                    }
                }
            }
            event = bridge_events.recv() => { if let Some(event) = event { let _ = route_tx.send(RouteEvent::Bridge(event)).await; } }
            accepted = accepted_rx.recv() => {
                if let Some(accepted) = accepted {
                    if binding_tx.borrow().as_ref().is_none_or(|b| b.scope() != accepted.scope) { continue; }
                    let mut routes = hub.routes.lock().await;
                    for reference in accepted.references { routes.remember_source(reference, accepted.source.clone()); }
                    drop(routes);
                    if let Err(error) = hub.save_routes().await { snapshot.error = Some(error.to_string()); }
                }
            }
            Some(error) = error_rx.recv() => snapshot.error = Some(error),
            Some((scope, error)) = menu_rx.recv() => {
                if binding_tx.borrow().as_ref().is_some_and(|b| b.scope() == scope) {
                    snapshot.menu_error = error;
                }
            }
            _ = tick.tick() => {
                if let Some(core) = core.read().await.as_ref() {
                    let catalog = core.catalog.read().await;
                    snapshot.core_ready = catalog.ready; snapshot.core_error = catalog.error.clone();
                } else { snapshot.core_ready = false; snapshot.core_error = None; }
                snapshot.connected_sessions = hub.sessions().await.len();
                snapshot.failed_deliveries = outbox.summary().await["failed"].as_u64().unwrap_or(0) as usize;
                if store.take_show_request() { snapshot.show_window = snapshot.show_window.wrapping_add(1); }
                if pairing.as_ref().is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(600)) { pairing = None; snapshot.notice = Some("配对码已过期，点击生成新配对码。".into()); }
            }
        }
        snapshot.owner_bound = credentials.as_ref().is_some_and(|c| c.owner.is_some());
        snapshot.guide_status = outbox.guide_status().await;
        if snapshot.qq_ready && snapshot.owner_bound {
            let binding = binding_tx.borrow().clone().filter(|b| b.ready);
            if let Some(binding) = binding {
                let scope = binding.scope();
                if menu_job
                    .as_ref()
                    .is_none_or(|(current, _)| current != &scope)
                {
                    if let Some((_, job)) = menu_job.take() {
                        job.abort();
                    }
                    snapshot.menu_error = None;
                    let store = store.clone();
                    let results = menu_results.clone();
                    let result_scope = scope.clone();
                    let job = tokio::spawn(async move {
                        let error = crate::menu::configure(&binding.api, &store)
                            .await
                            .err()
                            .map(|error| error.to_string());
                        let _ = results.send((result_scope, error));
                    });
                    // Retain the completed handle so gateway reconnects do not
                    // reconfigure the menu; a new binding or explicit retry can.
                    menu_job = Some((scope, job));
                }
            }
        }
        if snapshot.qq_ready
            && snapshot.owner_bound
            && snapshot.guide_status == GuideStatus::NotSent
        {
            if let Err(error) = send_guide(&outbox, false).await {
                snapshot.error = Some(error.to_string());
            }
            snapshot.guide_status = outbox.guide_status().await;
        }
        snapshot.pair_code = pairing.as_ref().map(|(code, _)| code.clone());
        let _ = snapshots.try_send(snapshot.clone());
    }
    if let Some(job) = qr_job {
        job.abort();
    }
    if let Some(job) = gateway {
        job.abort();
    }
    if let Some((_, job)) = menu_job {
        job.abort();
    }
    let _ = std::fs::remove_file(store.root().join("bridge.json"));
    Ok(())
}

async fn disconnect_outbox(
    binding: &watch::Sender<Option<Binding>>,
    outbox: &Outbox,
) -> Result<()> {
    let current = binding.borrow().clone();
    if let Some(mut current) = current {
        current.ready = false;
        binding.send_replace(Some(current));
    }
    outbox.discard_pending().await
}

async fn send_guide(outbox: &Outbox, resend: bool) -> Result<()> {
    outbox.welcome(
        "Pi QQ 已连接\n\n你已与这台电脑连接，接下来就在这个机器人私聊中操作：\n\n已有 Pi：发送 /sessions，选择 Session 后直接发消息。\n新建 Pi：发送 /new，等创建成功后再发送任务。\n全部用法：发送 /help。\n\n电脑上的 Pi QQ 客户端需要保持运行。关闭窗口会驻留托盘，选择退出才会断开连接。",
        crate::router::keyboard(&[
            ("已有 Pi", "/sessions".into()),
            ("新建 Pi", "/new".into()),
            ("查看帮助", "/help".into()),
        ]),
        resend,
    ).await
}

fn new_pairing() -> (String, Instant) {
    let mut bytes = [0u8; 5];
    rand::thread_rng().fill_bytes(&mut bytes);
    (
        bytes.iter().map(|b| format!("{b:02X}")).collect(),
        Instant::now(),
    )
}
async fn detect(
    config: &Arc<RwLock<Config>>,
    core: &Arc<RwLock<Option<Core>>>,
    snapshot: &mut Snapshot,
) {
    let config = config.read().await.clone();
    snapshot.config = config.clone();
    match install::locate(&config) {
        Ok(env) => match install::install_extension(&env) {
            Ok(changed) => {
                snapshot.missing = None;
                snapshot.dependencies_ready = true;
                if changed {
                    snapshot.notice = Some("配套扩展已安装。".into());
                }
                if core.read().await.is_none() {
                    *core.write().await = Some(Core::start(env));
                }
            }
            Err(error) => {
                snapshot.missing = None;
                snapshot.dependencies_ready = false;
                snapshot.error = Some(error.to_string());
            }
        },
        Err(missing) => {
            snapshot.dependencies_ready = false;
            snapshot.missing = Some(missing);
        }
    }
}
fn connect(
    http: &reqwest::Client,
    creds: &Credentials,
    binding: &watch::Sender<Option<Binding>>,
) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<GatewayEvent>) {
    let (tx, rx) = mpsc::channel(128);
    let api = Api::new(http.clone(), creds.clone());
    binding.send_replace(creds.owner.as_ref().map(|owner| Binding {
        api: api.clone(),
        owner: owner.clone(),
        app_id: creds.app_id.clone(),
        ready: false,
    }));
    let job = tokio::spawn(async move {
        api.gateway(tx).await;
    });
    (job, rx)
}
fn start_qr(http: reqwest::Client) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<QrEvent>) {
    let (tx, rx) = mpsc::channel(8);
    let job = tokio::spawn(async move {
        let result: Result<()> = async {
            loop {
                let task = BindTask::create(&http).await?;
                if tx.send(QrEvent::Display(task.url.clone())).await.is_err() {
                    return Ok(());
                }
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let event = match task.poll(&http).await? {
                        BindResult::Waiting => QrEvent::Status("等待扫码"),
                        BindResult::Confirming => QrEvent::Status("请在手机 QQ 中确认授权"),
                        BindResult::Expired => {
                            let _ = tx.send(QrEvent::Status("二维码已过期，正在刷新")).await;
                            break;
                        }
                        BindResult::Complete(creds) => {
                            let _ = tx.send(QrEvent::Complete(creds)).await;
                            return Ok(());
                        }
                    };
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        .await;
        if let Err(error) = result {
            let _ = tx.send(QrEvent::Error(error.to_string())).await;
        }
    });
    (job, rx)
}
