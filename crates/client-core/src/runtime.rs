use crate::{
    bridge::{BridgeEvent, Hub},
    herdr::Core,
    install,
    model::{Command, Config, Credentials, Snapshot},
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
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    if let Err(error) = runtime.block_on(run(store, incoming, snapshots.clone())) {
                        let _ = snapshots.send(Snapshot {
                            error: Some(error.to_string()),
                            ..Default::default()
                        });
                    }
                }
                Err(_) => {
                    let _ = snapshots.send(Snapshot {
                        error: Some("无法启动后台服务，请重新打开客户端".into()),
                        ..Default::default()
                    });
                }
            }
        })?;
    Ok(Client {
        commands,
        snapshots: receiver,
    })
}
async fn run(
    store: Store,
    mut commands: mpsc::UnboundedReceiver<Command>,
    snapshots: std_mpsc::SyncSender<Snapshot>,
) -> Result<()> {
    let config = Arc::new(RwLock::new(store.config()?));
    let core = Arc::new(RwLock::new(None::<Core>));
    let http = qq::http_client()?;
    let (mut binding_tx, binding_rx) = watch::channel(None);
    let (accepted_tx, mut accepted_rx) = mpsc::channel::<Accepted>(128);
    let outbox = Outbox::start(store.clone(), binding_rx, accepted_tx)?;
    let (events, mut bridge_events) = mpsc::channel::<BridgeEvent>(128);
    let hub = Hub::start(store.clone(), outbox.clone(), events).await?;
    for source in outbox.recovered_sources().await {
        let mut routes = hub.routes.lock().await;
        for reference in source.references {
            routes.sources.insert(reference, source.source.clone());
        }
    }
    hub.save_routes().await?;
    let mut router = Router::new(hub.clone(), outbox.clone(), core.clone(), config.clone());
    let mut credentials = store.credentials()?;
    let mut snapshot = Snapshot {
        config: config.read().await.clone(),
        qr_status: "准备连接".into(),
        ..Default::default()
    };
    let mut qr = None::<BindTask>;
    let mut gateway = None::<tokio::task::JoinHandle<()>>;
    let (mut gateway_tx, mut gateway_rx) = mpsc::channel(128);
    let mut pairing = None::<(String, Instant)>;
    detect(&config, &core, &mut snapshot).await;
    if snapshot.dependencies_ready {
        if let Some(creds) = credentials.as_ref() {
            connect(
                &http,
                creds,
                &mut gateway,
                &mut gateway_tx,
                &mut gateway_rx,
                &mut binding_tx,
            )
            .await;
            if creds.owner.is_none() {
                pairing = Some(new_pairing());
                snapshot.pair_code = pairing.as_ref().map(|(code, _)| code.clone());
            }
        } else {
            match BindTask::create(&http).await {
                Ok(task) => {
                    snapshot.qr_url = Some(task.url.clone());
                    snapshot.qr_status = "等待扫码".into();
                    qr = Some(task);
                }
                Err(e) => snapshot.error = Some(e.to_string()),
            }
        }
    }
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            command=commands.recv()=>{
                let Some(command)=command else{break;};
                match command {
                    Command::Shutdown=>break,
                    Command::Detect=>{
                        snapshot.error=None; detect(&config,&core,&mut snapshot).await;
                        if snapshot.dependencies_ready && gateway.is_none() && credentials.is_some() {connect(&http,credentials.as_ref().unwrap(),&mut gateway,&mut gateway_tx,&mut gateway_rx,&mut binding_tx).await;}
                    },
                    Command::StartQr=>{
                        if !snapshot.dependencies_ready{continue;}
                        snapshot.error=None;
                        match BindTask::create(&http).await {Ok(task)=>{snapshot.qr_url=Some(task.url.clone());snapshot.qr_status="等待扫码".into();qr=Some(task);},Err(e)=>snapshot.error=Some(e.to_string())}
                    },
                    Command::CancelQr=>{qr=None;snapshot.qr_url=None;snapshot.qr_status="扫码已取消".into();},
                    Command::Rebind=>{
                        qr=None;snapshot.qr_url=None;snapshot.qq_ready=false;pairing=None;snapshot.pair_code=None;
                        if let Some(task)=gateway.take(){task.abort();}
                        binding_tx.send_replace(None);credentials=None;store.forget_credentials()?;
                        // Old pending deliveries retain their original account scope.
                        {let mut routes=hub.routes.lock().await;routes.selected=None;routes.workspace=None;routes.sources.clear();routes.seen.clear();}
                        hub.save_routes().await?; snapshot.qr_status="点击扫码连接，授权另一台机器人前请先停用它的旧网关".into();
                    },
                    Command::Pair=>{
                        if credentials.is_some(){pairing=Some(new_pairing());snapshot.pair_code=pairing.as_ref().map(|(code,_)|code.clone());}
                    },
                    Command::Save(value)=>{
                        if value.default_directory.as_ref().is_some_and(|p|!p.is_dir()){snapshot.error=Some("默认目录不存在，请重新选择".into());continue;}
                        let paths_changed={let old=config.read().await;old.pi_path!=value.pi_path||old.core_path!=value.core_path};
                        store.save("config.json",&value)?;*config.write().await=value.clone();snapshot.config=value;snapshot.error=None;
                        snapshot.notice=Some(if paths_changed{"设置已保存，Pi/Core 路径将在下次启动时使用。"}else{"设置已保存。"}.into());
                    },
                    Command::ConfigureMenu=>{
                        let binding=binding_tx.borrow().clone().filter(|b|b.ready);
                        if let Some(binding)=binding{match crate::menu::configure(&binding.api,&store).await{Ok(())=>snapshot.notice=Some("手机菜单已配置，原有自定义入口和备份已保留。".into()),Err(e)=>snapshot.error=Some(e.to_string())}}
                        else{snapshot.error=Some("请先完成 QQ 连接和配对。".into());}
                    },
                    Command::RetryDeliveries=>{match outbox.retry().await{Ok(())=>snapshot.notice=Some("已重新排队发送失败的消息。".into()),Err(e)=>snapshot.error=Some(e.to_string())}},
                }
            },
            _=poll.tick(),if qr.is_some()=>{
                match qr.as_ref().unwrap().poll(&http).await{
                    Ok(BindResult::Waiting)=>snapshot.qr_status="等待扫码".into(),
                    Ok(BindResult::Confirming)=>snapshot.qr_status="请在手机 QQ 中确认授权".into(),
                    Ok(BindResult::Expired)=>{
                        snapshot.qr_status="二维码已过期，正在刷新".into();let _=snapshots.try_send(snapshot.clone());
                        match BindTask::create(&http).await{Ok(task)=>{snapshot.qr_url=Some(task.url.clone());qr=Some(task);},Err(e)=>{qr=None;snapshot.qr_url=None;snapshot.error=Some(e.to_string());}}
                    },
                    Ok(BindResult::Complete(creds))=>{
                        qr=None;snapshot.qr_url=None;snapshot.qr_status="正在连接 QQ".into();snapshot.error=None;
                        store.save_credentials(&creds)?;credentials=Some(creds);
                        connect(&http,credentials.as_ref().unwrap(),&mut gateway,&mut gateway_tx,&mut gateway_rx,&mut binding_tx).await;
                        if credentials.as_ref().unwrap().owner.is_none(){pairing=Some(new_pairing());snapshot.pair_code=pairing.as_ref().map(|(code,_)|code.clone());}
                    },
                    Err(e)=>{snapshot.error=Some(e.to_string());snapshot.qr_status="扫码连接失败，请重试".into();qr=None;snapshot.qr_url=None;},
                }
            },
            event=gateway_rx.recv(),if gateway.is_some()=>{
                if let Some(event)=event{match event{
                    GatewayEvent::Connected=>{
                        snapshot.qq_ready=true;snapshot.error=None;snapshot.qr_status="QQ 已连接".into();
                        if let Some(creds)=credentials.as_ref(){if let Some(owner)=&creds.owner{
                            let api=Api::new(http.clone(),creds.clone());binding_tx.send_replace(Some(Binding{api,owner:owner.clone(),app_id:creds.app_id.clone(),ready:true}));
                        }}
                    },
                    GatewayEvent::Disconnected=>{snapshot.qq_ready=false;snapshot.qr_status="QQ 正在重连".into();let binding=binding_tx.borrow().clone();if let Some(mut b)=binding{b.ready=false;binding_tx.send_replace(Some(b));}},
                    GatewayEvent::Message(message)=>{
                        let sender=message["author"]["user_openid"].as_str().unwrap_or_default();
                        if sender.is_empty(){continue;}
                        let text=message["content"].as_str().unwrap_or_default().trim();
                        if let Some((code,created))=&pairing{
                            if created.elapsed()<Duration::from_secs(600)&&text==format!("/pair {code}"){
                                if let Some(creds)=credentials.as_mut(){creds.owner=Some(sender.into());store.save_credentials(creds)?;let api=Api::new(http.clone(),creds.clone());binding_tx.send_replace(Some(Binding{api,owner:sender.into(),app_id:creds.app_id.clone(),ready:snapshot.qq_ready}));}
                                snapshot.pair_code=None;pairing=None;outbox.enqueue("已与这台电脑配对。发送 /help 查看用法；没有 Pi 时可直接 /new。",None,None,false,None).await?;continue;
                            }
                        }
                        if credentials.as_ref().and_then(|c|c.owner.as_deref())!=Some(sender){continue;}
                        if let Err(e)=router.inbound(&http,message).await{snapshot.error=Some(e.to_string());}
                    }
                }}
            },
            event=bridge_events.recv()=>{if let Some(event)=event{if let Err(e)=router.event(event).await{snapshot.error=Some(e.to_string());}}},
            accepted=accepted_rx.recv()=>{if let Some(a)=accepted{let mut routes=hub.routes.lock().await;for reference in a.references{routes.sources.insert(reference,a.source.clone());}while routes.sources.len()>500{let key=routes.sources.keys().next().cloned().unwrap();routes.sources.remove(&key);}drop(routes);if let Err(e)=hub.save_routes().await{snapshot.error=Some(e.to_string());}}},
            _=tick.tick()=>{
                snapshot.core_ready=if let Some(core)=core.read().await.as_ref(){core.catalog.read().await.ready}else{false};
                snapshot.owner_bound=credentials.as_ref().is_some_and(|c|c.owner.is_some());
                snapshot.connected_sessions=hub.sessions().await.len();
                snapshot.failed_deliveries=outbox.summary().await["failed"].as_u64().unwrap_or(0) as usize;
                if store.take_show_request(){snapshot.show_window=snapshot.show_window.wrapping_add(1);}
                if pairing.as_ref().is_some_and(|(_,at)|at.elapsed()>=Duration::from_secs(600)){pairing=None;snapshot.pair_code=None;snapshot.notice=Some("配对码已过期，点击生成新配对码。".into());}
                let _=snapshots.try_send(snapshot.clone());
            }
        }
        let _ = snapshots.try_send(snapshot.clone());
    }
    if let Some(task) = gateway {
        task.abort();
    }
    let _ = std::fs::remove_file(store.root().join("bridge.json"));
    Ok(())
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
                    snapshot.notice = Some(
                        "配套扩展已安装。已有 Pi 请在空闲时 /reload，新 Pi 会自动加载。".into(),
                    );
                }
                if core.read().await.is_none() {
                    *core.write().await = Some(Core::start(env));
                }
            }
            Err(e) => {
                snapshot.dependencies_ready = false;
                snapshot.error = Some(e.to_string());
            }
        },
        Err(missing) => {
            snapshot.dependencies_ready = false;
            snapshot.missing = Some(missing);
        }
    }
}
async fn connect(
    http: &reqwest::Client,
    creds: &Credentials,
    gateway: &mut Option<tokio::task::JoinHandle<()>>,
    gateway_tx: &mut mpsc::Sender<GatewayEvent>,
    gateway_rx: &mut mpsc::Receiver<GatewayEvent>,
    binding: &mut watch::Sender<Option<Binding>>,
) {
    if let Some(task) = gateway.take() {
        task.abort();
    }
    let (tx, rx) = mpsc::channel(128);
    *gateway_tx = tx.clone();
    *gateway_rx = rx;
    let api = Api::new(http.clone(), creds.clone());
    if let Some(owner) = &creds.owner {
        binding.send_replace(Some(Binding {
            api: api.clone(),
            owner: owner.clone(),
            app_id: creds.app_id.clone(),
            ready: false,
        }));
    }
    *gateway = Some(tokio::spawn(async move {
        api.gateway(tx).await;
    }));
}
