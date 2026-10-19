//! UI 与后端之间的消息桥:专用线程 + 双向通道,后端事件驱动重绘。
//!
//! UI 侧永不 await:每帧 `drain()` 事件、`try_send()` 命令。WriteShell
//! 走会话专用输入通道,绕过命令队列,保证按键零延迟。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;

use eframe::egui::Context;
use fhdc_device::DeviceRegistry;
use fhdc_domain::{
    AppAction, BackendCommand, BackendEvent, BridgeError, BundleName, BundlePackage, DeviceTarget,
    ErrorCode, OperationId, OverwritePolicy, RemotePath, SessionId,
};
use fhdc_hdc::HdcTransport;
use fhdc_hdc::ShellSessionHandle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 事件通道容量:足够吸收一帧内的日志突发。
const EVENT_CHANNEL_CAPACITY: usize = 512;
/// 会话输出轮询周期:兼顾关闭延迟与 CPU 占用。
const POLL_INTERVAL: Duration = Duration::from_millis(120);
/// 设备发现轮询周期:插拔设备后最多这么多时间就能看到变化。
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 输出合并窗口:多个小块拼成一个事件再唤醒 UI。
const COALESCE_WINDOW: Duration = Duration::from_millis(8);
const COALESCE_MAX_CHUNKS: usize = 8;
const COALESCE_MAX_BYTES: usize = 64 * 1024;

pub struct RuntimeBridge {
    command_tx: mpsc::Sender<BackendCommand>,
    shell_inputs: Arc<RwLock<HashMap<SessionId, mpsc::Sender<Vec<u8>>>>>,
    event_rx: mpsc::Receiver<BackendEvent>,
}

impl RuntimeBridge {
    pub fn spawn(context: Context, hdc_override: Option<PathBuf>) -> Self {
        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let shell_inputs: Arc<RwLock<HashMap<SessionId, mpsc::Sender<Vec<u8>>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let inputs = Arc::clone(&shell_inputs);
        let thread_context = context.clone();
        std::thread::Builder::new()
            .name("fhdc-backend".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("构建 tokio 运行时");
                runtime.block_on(run_backend(
                    command_rx,
                    event_tx,
                    inputs,
                    thread_context,
                    hdc_override,
                ));
            })
            .expect("启动后端线程");
        Self {
            command_tx,
            shell_inputs,
            event_rx,
        }
    }

    /// 发送命令;WriteShell 直接路由到会话输入通道。
    /// 返回是否送达,调用方可据此回收乐观置位的状态。
    pub fn try_send(&self, command: BackendCommand) -> bool {
        if let BackendCommand::WriteShell { session, input } = &command {
            let guard = self.shell_inputs.read().unwrap();
            if let Some(sender) = guard.get(session) {
                let _ = sender.try_send(input.clone());
                return true;
            }
            tracing::debug!(?session, "WriteShell 被丢弃:会话已关闭或尚未注册");
            return false;
        }
        match self.command_tx.try_send(command) {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!("后端命令队列已满,命令被丢弃");
                false
            }
        }
    }

    pub fn drain(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }
}

async fn send_event(tx: &mpsc::Sender<BackendEvent>, context: &Context, event: BackendEvent) {
    if tx.send(event).await.is_ok() {
        context.request_repaint();
    }
}

/// 后台任务完成回执:传输收尾、设备发现结果、或"设备集合已变请立刻重发现"。
enum Completion {
    Transfer(OperationId),
    /// 发现任务完成;Err 时保留上一次快照,不清空设备列表。
    Discovered(Result<Vec<fhdc_domain::DeviceDescriptor>, BridgeError>),
    /// 连接/断开/监听切换等改变了设备集合,主循环应立刻再发现一次,
    /// 不必等下一个轮询周期(最坏约 4 秒)。
    Rediscover,
}

async fn run_backend(
    mut command_rx: mpsc::Receiver<BackendCommand>,
    event_tx: mpsc::Sender<BackendEvent>,
    shell_inputs: Arc<RwLock<HashMap<SessionId, mpsc::Sender<Vec<u8>>>>>,
    context: Context,
    hdc_override: Option<PathBuf>,
) {
    let Some(transport) = initialize_transport(hdc_override, &event_tx, &context).await else {
        return;
    };
    let mut registry = DeviceRegistry::new();
    // 共享会话表:转发任务结束时能自行清理,避免自然结束的会话泄漏。
    let sessions: Arc<Mutex<HashMap<SessionId, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut transfers: HashMap<OperationId, CancellationToken> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::channel::<Completion>(64);
    // interval 首个 tick 立即就绪;显式推迟一个周期,避免启动时背靠背两次发现。
    let mut discovery_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + DEVICE_POLL_INTERVAL,
        DEVICE_POLL_INTERVAL,
    );
    let mut discovery_healthy = true;
    let mut last_broadcast: Option<fhdc_domain::DeviceSnapshot> = None;
    // 发现进行中收到的刷新请求:结束后补跑一次,不静默丢弃。
    let mut rediscover_requested = false;
    // 自升级:进行中的下载令牌 + 最近一次检查到的更新信息。
    let update_download: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let last_update_info: Arc<Mutex<Option<fhdc_domain::UpdateInfo>>> = Arc::new(Mutex::new(None));

    // 启动时的第一次发现。
    spawn_discovery(&transport, &done_tx);
    let mut discovery_in_flight = true;

    loop {
        let command = tokio::select! {
            // 周期性设备发现:插拔设备无需手动刷新。发现本身在后台任务里
            // 执行,hdc 卡住时也不会堵住命令队列(最多同时挂一个)。
            _ = discovery_tick.tick() => {
                if !discovery_in_flight {
                    discovery_in_flight = true;
                    spawn_discovery(&transport, &done_tx);
                }
                continue;
            }
            done = done_rx.recv() => {
                let Some(completion) = done else { break };
                match completion {
                    Completion::Transfer(operation) => {
                        transfers.remove(&operation);
                    }
                    Completion::Rediscover => {
                        if !discovery_in_flight {
                            discovery_in_flight = true;
                            spawn_discovery(&transport, &done_tx);
                        } else {
                            rediscover_requested = true;
                        }
                    }
                    Completion::Discovered(result) => {
                        discovery_in_flight = false;
                        match result {
                            Ok(descriptors) => {
                                discovery_healthy = true;
                                let snapshot = registry.reconcile(descriptors);
                                broadcast_if_changed(
                                    &event_tx,
                                    &context,
                                    &mut last_broadcast,
                                    snapshot,
                                )
                                .await;
                            }
                            Err(error) => {
                                // 边沿触发:只在"由好变坏"时报一次错,
                                // 否则轮询会让错误条每 2 秒复活一次。
                                if discovery_healthy {
                                    discovery_healthy = false;
                                    send_event(
                                        &event_tx,
                                        &context,
                                        BackendEvent::OperationFailed(error),
                                    )
                                    .await;
                                } else {
                                    tracing::warn!("设备发现失败(持续): {}", error.detail);
                                }
                            }
                        }
                        // 补上发现进行中错过的刷新请求。
                        if rediscover_requested && !discovery_in_flight {
                            rediscover_requested = false;
                            discovery_in_flight = true;
                            spawn_discovery(&transport, &done_tx);
                        }
                    }
                }
                continue;
            }
            command = command_rx.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };

        match command {
            BackendCommand::RefreshDevices => {
                if !discovery_in_flight {
                    discovery_in_flight = true;
                    spawn_discovery(&transport, &done_tx);
                } else {
                    // 正在发现中:记下来,当前这轮结束后立刻补跑。
                    rediscover_requested = true;
                }
            }
            BackendCommand::SelectDevice { key } => match registry.select(key) {
                Ok(snapshot) => {
                    broadcast_if_changed(&event_tx, &context, &mut last_broadcast, snapshot).await
                }
                Err(error) => {
                    send_event(&event_tx, &context, BackendEvent::OperationFailed(error)).await
                }
            },
            BackendCommand::ConnectEndpoint(endpoint) => {
                // 网络连接可能阻塞数秒(不可达地址等),异步执行避免卡住
                // 命令队列;成功后立刻触发一次发现,不必等轮询周期。
                let transport = Arc::clone(&transport);
                let event_tx = event_tx.clone();
                let context = context.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let outcome = transport.connect_endpoint(&endpoint).await;
                    if outcome.is_ok() {
                        let _ = done_tx.send(Completion::Rediscover).await;
                    }
                    match outcome {
                        Ok(message) => {
                            send_event(
                                &event_tx,
                                &context,
                                BackendEvent::EndpointConnectFinished { endpoint, message },
                            )
                            .await;
                        }
                        Err(error) => {
                            send_event(
                                &event_tx,
                                &context,
                                BackendEvent::EndpointConnectFailed {
                                    endpoint,
                                    error: error.detail,
                                },
                            )
                            .await;
                        }
                    }
                });
            }
            BackendCommand::DisconnectEndpoint(endpoint) => {
                let transport = Arc::clone(&transport);
                let event_tx = event_tx.clone();
                let context = context.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let outcome = transport.disconnect_endpoint(&endpoint).await;
                    if outcome.is_ok() {
                        let _ = done_tx.send(Completion::Rediscover).await;
                    }
                    match outcome {
                        Ok(message) => {
                            send_event(
                                &event_tx,
                                &context,
                                BackendEvent::EndpointDisconnected { endpoint, message },
                            )
                            .await;
                        }
                        Err(error) => {
                            send_event(&event_tx, &context, BackendEvent::OperationFailed(error))
                                .await;
                        }
                    }
                });
            }
            BackendCommand::EnableTcpListen { target, port } => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    let event_tx = event_tx.clone();
                    let context = context.clone();
                    let done_tx = done_tx.clone();
                    let key = target.key;
                    tokio::spawn(async move {
                        let outcome = transport.enable_tcp_listen(&key, port).await;
                        if outcome.is_ok() {
                            // tmode 通常会让设备从 USB 侧掉线转为监听,立刻重发现。
                            let _ = done_tx.send(Completion::Rediscover).await;
                        }
                        match outcome {
                            Ok(message) => {
                                send_event(
                                    &event_tx,
                                    &context,
                                    BackendEvent::TcpListenEnabled { message },
                                )
                                .await;
                            }
                            Err(error) => {
                                send_event(
                                    &event_tx,
                                    &context,
                                    BackendEvent::TcpListenFailed {
                                        error: error.detail,
                                    },
                                )
                                .await;
                            }
                        }
                    });
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::TcpListenFailed {
                            error: BridgeError::device_unavailable().detail,
                        },
                    )
                    .await;
                }
            }
            BackendCommand::KillServer => {
                let transport = Arc::clone(&transport);
                let event_tx = event_tx.clone();
                let context = context.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    if let Ok(message) = transport.kill_server().await {
                        send_event(&event_tx, &context, BackendEvent::ServerKilled { message })
                            .await;
                        // 服务重启后设备视图可能整体变化,立刻重新发现。
                        let _ = done_tx.send(Completion::Rediscover).await;
                    }
                });
            }
            BackendCommand::LoadOverview(target) => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::OverviewLoading(target.clone()),
                        async move {
                            match transport.device_overview(&target.key).await {
                                Ok(overview) => BackendEvent::OverviewLoaded(target, overview),
                                Err(error) => BackendEvent::OverviewFailed(target, error),
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::OverviewFailed(target, BridgeError::device_unavailable()),
                    )
                    .await;
                }
            }
            BackendCommand::LoadProcesses(target) => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::ProcessesLoading(target.clone()),
                        async move {
                            match transport.list_processes(&target.key).await {
                                Ok(processes) => BackendEvent::ProcessesLoaded(target, processes),
                                Err(error) => BackendEvent::ProcessesFailed(target, error),
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::ProcessesFailed(target, BridgeError::device_unavailable()),
                    )
                    .await;
                }
            }
            BackendCommand::LoadBundles(target) => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::BundlesLoading(target.clone()),
                        async move {
                            match transport.list_bundles(&target.key).await {
                                Ok(bundles) => BackendEvent::BundlesLoaded(target, bundles),
                                Err(error) => BackendEvent::BundlesFailed(target, error),
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::BundlesFailed(target, BridgeError::device_unavailable()),
                    )
                    .await;
                }
            }
            BackendCommand::LoadBundleIcons(target) => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    let event_tx = event_tx.clone();
                    let context = context.clone();
                    tokio::spawn(async move {
                        let packages = match transport.list_bundle_packages(&target.key).await {
                            Ok(packages) => packages,
                            Err(error) => {
                                send_event(
                                    &event_tx,
                                    &context,
                                    BackendEvent::BundlePackagesFailed(target, error),
                                )
                                .await;
                                return;
                            }
                        };
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::BundlePackagesLoaded(target.clone(), packages.clone()),
                        )
                        .await;
                        // 小包优先:图标逐个出现,常见应用不必等大包。
                        let mut order: Vec<usize> = (0..packages.len()).collect();
                        order.sort_by_key(|&index| {
                            packages[index]
                                .haps
                                .iter()
                                .map(|hap| hap.size_bytes)
                                .min()
                                .unwrap_or(u64::MAX)
                        });
                        for index in order {
                            let package = &packages[index];
                            let event = match fetch_bundle_icon(
                                &transport,
                                &target,
                                package,
                                IconFetchMode::Bulk,
                            )
                            .await
                            {
                                IconOutcome::Icon(bytes) => BackendEvent::BundleIconLoaded(
                                    target.clone(),
                                    package.bundle.clone(),
                                    Some(bytes),
                                ),
                                IconOutcome::Missing => BackendEvent::BundleIconLoaded(
                                    target.clone(),
                                    package.bundle.clone(),
                                    None,
                                ),
                                IconOutcome::Skipped => BackendEvent::BundleIconSkipped(
                                    target.clone(),
                                    package.bundle.clone(),
                                ),
                            };
                            send_event(&event_tx, &context, event).await;
                        }
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::BundleIconsFinished(target),
                        )
                        .await;
                    });
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::BundlePackagesFailed(
                            target,
                            BridgeError::device_unavailable(),
                        ),
                    )
                    .await;
                }
            }
            BackendCommand::LoadBundleIcon { target, package } => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    let event_tx = event_tx.clone();
                    let context = context.clone();
                    tokio::spawn(async move {
                        let event = match fetch_bundle_icon(
                            &transport,
                            &target,
                            &package,
                            IconFetchMode::OnDemand,
                        )
                        .await
                        {
                            IconOutcome::Icon(bytes) => BackendEvent::BundleIconLoaded(
                                target.clone(),
                                package.bundle.clone(),
                                Some(bytes),
                            ),
                            _ => {
                                BackendEvent::BundleIconLoaded(target, package.bundle.clone(), None)
                            }
                        };
                        send_event(&event_tx, &context, event).await;
                    });
                }
            }
            BackendCommand::LoadBundleDetails(target, bundle) => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::BundleDetailsLoading(target.clone(), bundle.clone()),
                        async move {
                            match transport.bundle_details(&target.key, &bundle).await {
                                Ok(details) => BackendEvent::BundleDetailsLoaded(target, details),
                                Err(error) => {
                                    BackendEvent::BundleDetailsFailed(target, bundle, error)
                                }
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::BundleDetailsFailed(
                            target,
                            bundle,
                            BridgeError::device_unavailable(),
                        ),
                    )
                    .await;
                }
            }
            BackendCommand::RunAppAction {
                target,
                bundle,
                action,
            } => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::AppActionStarted(target.clone(), bundle.clone(), action),
                        async move {
                            match transport.run_app_action(&target.key, &bundle, action).await {
                                Ok(message) => BackendEvent::AppActionCompleted(
                                    target, bundle, action, message,
                                ),
                                Err(error) => {
                                    BackendEvent::AppActionFailed(target, bundle, action, error)
                                }
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::AppActionFailed(
                            target,
                            bundle,
                            action,
                            BridgeError::device_unavailable(),
                        ),
                    )
                    .await;
                }
            }
            BackendCommand::InstallHap(target, path) => {
                if ensure_target(&registry, &target) {
                    let label = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("hap")
                        .to_owned();
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::HapInstallStarted(target.clone(), label),
                        async move {
                            match transport.install_hap(&target.key, &path).await {
                                Ok(message) => BackendEvent::HapInstallFinished(target, message),
                                Err(error) => BackendEvent::HapInstallFailed(target, error),
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::HapInstallFailed(target, BridgeError::device_unavailable()),
                    )
                    .await;
                }
            }
            BackendCommand::OpenShell {
                target,
                session,
                size,
            } => {
                if !ensure_target(&registry, &target) {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::ShellFailed {
                            session,
                            error: BridgeError::device_unavailable(),
                        },
                    )
                    .await;
                    continue;
                }
                match transport.start_shell(&target.key, size).await {
                    Ok(handle) => {
                        let input = handle.input();
                        shell_inputs.write().unwrap().insert(session, input);
                        let cancel = CancellationToken::new();
                        sessions.lock().unwrap().insert(session, cancel.clone());
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::ShellOpened { target, session },
                        )
                        .await;
                        spawn_forwarder(
                            session,
                            handle,
                            cancel,
                            false,
                            event_tx.clone(),
                            context.clone(),
                            shell_inputs.clone(),
                            sessions.clone(),
                        );
                    }
                    Err(error) => {
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::ShellFailed { session, error },
                        )
                        .await
                    }
                }
            }
            BackendCommand::WriteShell { session, .. } => {
                // 由 RuntimeBridge::try_send 直接路由;落到这里说明会话已不存在。
                tracing::debug!(?session, "WriteShell 被丢弃:会话已关闭或尚未注册");
            }
            BackendCommand::CloseShell { session } => {
                if let Some(cancel) = sessions.lock().unwrap().remove(&session) {
                    cancel.cancel();
                }
            }
            BackendCommand::StartHilog { target, session } => {
                if !ensure_target(&registry, &target) {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::HilogFailed {
                            session,
                            error: BridgeError::device_unavailable(),
                        },
                    )
                    .await;
                    continue;
                }
                match transport.start_hilog(&target.key).await {
                    Ok(handle) => {
                        let cancel = CancellationToken::new();
                        sessions.lock().unwrap().insert(session, cancel.clone());
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::HilogStarted { target, session },
                        )
                        .await;
                        spawn_forwarder(
                            session,
                            handle,
                            cancel,
                            true,
                            event_tx.clone(),
                            context.clone(),
                            shell_inputs.clone(),
                            sessions.clone(),
                        );
                    }
                    Err(error) => {
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::HilogFailed { session, error },
                        )
                        .await
                    }
                }
            }
            BackendCommand::StopHilog { session } => {
                if let Some(cancel) = sessions.lock().unwrap().remove(&session) {
                    cancel.cancel();
                }
            }
            BackendCommand::ClearDeviceHilog(target) => {
                if ensure_target(&registry, &target) {
                    match transport.clear_hilog(&target.key).await {
                        Ok(()) => {
                            send_event(
                                &event_tx,
                                &context,
                                BackendEvent::Toast("已清除设备日志缓冲".to_owned()),
                            )
                            .await
                        }
                        Err(error) => {
                            send_event(&event_tx, &context, BackendEvent::OperationFailed(error))
                                .await
                        }
                    }
                }
            }
            BackendCommand::RunShellCommand {
                target,
                command,
                purpose,
            } => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::Toast(format!("{purpose}…")),
                        async move {
                            match transport.run_shell_once(&target.key, &command).await {
                                Ok(output) => {
                                    BackendEvent::Toast(format!("{purpose}: {}", output.trim()))
                                }
                                Err(error) => BackendEvent::OperationFailed(error),
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::OperationFailed(BridgeError::device_unavailable()),
                    )
                    .await;
                }
            }
            BackendCommand::CaptureScreenshot(target) => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::ScreenshotLoading(target.clone()),
                        async move {
                            match transport.capture_screenshot(&target.key).await {
                                Ok(jpeg) => BackendEvent::ScreenshotCaptured(target, jpeg),
                                Err(error) => BackendEvent::ScreenshotFailed(target, error),
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::ScreenshotFailed(target, BridgeError::device_unavailable()),
                    )
                    .await;
                }
            }
            BackendCommand::ListDirectory { target, path } => {
                if ensure_target(&registry, &target) {
                    let transport = Arc::clone(&transport);
                    spawn_simple(
                        &event_tx,
                        &context,
                        BackendEvent::DirectoryLoading {
                            target: target.clone(),
                            path: path.clone(),
                        },
                        async move {
                            match transport.list_directory(&target.key, &path).await {
                                Ok(entries) => BackendEvent::DirectoryLoaded {
                                    target,
                                    path,
                                    entries,
                                },
                                Err(error) => BackendEvent::DirectoryFailed {
                                    target,
                                    path,
                                    error,
                                },
                            }
                        },
                    );
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::DirectoryFailed {
                            target,
                            path,
                            error: BridgeError::device_unavailable(),
                        },
                    )
                    .await;
                }
            }
            BackendCommand::UploadFile {
                target,
                local,
                remote,
                overwrite,
                operation,
            } => {
                if ensure_target(&registry, &target) {
                    let token = CancellationToken::new();
                    transfers.insert(operation, token.clone());
                    let transport = Arc::clone(&transport);
                    let event_tx = event_tx.clone();
                    let context = context.clone();
                    let done_tx = done_tx.clone();
                    tokio::spawn(async move {
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::FileTransferStarted {
                                operation,
                                direction: fhdc_domain::FileTransferDirection::Upload,
                                remote: remote.clone(),
                                local: local.clone(),
                            },
                        )
                        .await;
                        let result = transport
                            .push_file(&target.key, &local, &remote, overwrite, &token)
                            .await;
                        send_event(&event_tx, &context, transfer_event(operation, result)).await;
                        let _ = done_tx.send(Completion::Transfer(operation)).await;
                    });
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::FileTransferFailed {
                            operation,
                            error: BridgeError::device_unavailable(),
                        },
                    )
                    .await;
                }
            }
            BackendCommand::DownloadFile {
                target,
                remote,
                local,
                overwrite,
                operation,
            } => {
                if ensure_target(&registry, &target) {
                    let token = CancellationToken::new();
                    transfers.insert(operation, token.clone());
                    let transport = Arc::clone(&transport);
                    let event_tx = event_tx.clone();
                    let context = context.clone();
                    let done_tx = done_tx.clone();
                    tokio::spawn(async move {
                        send_event(
                            &event_tx,
                            &context,
                            BackendEvent::FileTransferStarted {
                                operation,
                                direction: fhdc_domain::FileTransferDirection::Download,
                                remote: remote.clone(),
                                local: local.clone(),
                            },
                        )
                        .await;
                        let result = transport
                            .pull_file(&target.key, &remote, &local, overwrite, &token)
                            .await;
                        send_event(&event_tx, &context, transfer_event(operation, result)).await;
                        let _ = done_tx.send(Completion::Transfer(operation)).await;
                    });
                } else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::FileTransferFailed {
                            operation,
                            error: BridgeError::device_unavailable(),
                        },
                    )
                    .await;
                }
            }
            BackendCommand::CancelFileOperation(operation) => {
                if let Some(token) = transfers.remove(&operation) {
                    token.cancel();
                }
            }
            BackendCommand::CreateDirectory { target, path } => {
                spawn_mutation(
                    &transport,
                    &event_tx,
                    &context,
                    &registry,
                    target,
                    fhdc_domain::FileMutationKind::CreateDirectory,
                    path,
                    None,
                )
                .await;
            }
            BackendCommand::RenameRemoteEntry { target, from, to } => {
                spawn_mutation(
                    &transport,
                    &event_tx,
                    &context,
                    &registry,
                    target,
                    fhdc_domain::FileMutationKind::Rename,
                    from,
                    Some(to),
                )
                .await;
            }
            BackendCommand::DeleteRemoteEntry { target, path } => {
                spawn_mutation(
                    &transport,
                    &event_tx,
                    &context,
                    &registry,
                    target,
                    fhdc_domain::FileMutationKind::Delete,
                    path,
                    None,
                )
                .await;
            }
            BackendCommand::CheckForUpdates => {
                let event_tx = event_tx.clone();
                let context = context.clone();
                let last_info = Arc::clone(&last_update_info);
                tokio::spawn(async move {
                    let outcome = check_for_update().await;
                    if let Ok(fhdc_domain::UpdateCheckOutcome::Available(ref info)) = outcome {
                        *last_info.lock().unwrap() = Some(info.clone());
                    }
                    send_event(&event_tx, &context, BackendEvent::UpdateChecked(outcome)).await;
                });
            }
            BackendCommand::DownloadUpdate => {
                // 同一时间只允许一个下载任务;信息来自最近一次成功检查。
                if update_download
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|token| !token.is_cancelled())
                {
                    continue;
                }
                let Some(info) = last_update_info.lock().unwrap().clone() else {
                    send_event(
                        &event_tx,
                        &context,
                        BackendEvent::UpdateDownloadFinished(Err(BridgeError::new(
                            ErrorCode::Internal,
                            "没有可用的更新信息,请先检查更新",
                        ))),
                    )
                    .await;
                    continue;
                };
                let cancel = CancellationToken::new();
                *update_download.lock().unwrap() = Some(cancel.clone());
                let event_tx = event_tx.clone();
                let context = context.clone();
                let slot = Arc::clone(&update_download);
                tokio::spawn(async move {
                    download_update(info, event_tx, context, cancel).await;
                    slot.lock().unwrap().take();
                });
            }
            BackendCommand::CancelUpdateDownload => {
                if let Some(token) = update_download.lock().unwrap().take() {
                    token.cancel();
                }
            }
        }
    }
}

fn transfer_event(operation: OperationId, result: Result<(), BridgeError>) -> BackendEvent {
    match result {
        Ok(()) => BackendEvent::FileTransferFinished { operation },
        Err(error) if error.code == ErrorCode::Cancelled => {
            BackendEvent::FileTransferCancelled { operation }
        }
        Err(error) => BackendEvent::FileTransferFailed { operation, error },
    }
}

fn spawn_simple<F>(
    event_tx: &mpsc::Sender<BackendEvent>,
    context: &Context,
    loading: BackendEvent,
    work: F,
) where
    F: std::future::Future<Output = BackendEvent> + Send + 'static,
{
    let event_tx = event_tx.clone();
    let context = context.clone();
    tokio::spawn(async move {
        send_event(&event_tx, &context, loading).await;
        let event = work.await;
        send_event(&event_tx, &context, event).await;
    });
}

#[expect(clippy::too_many_arguments, reason = "文件变更任务固定上下文集合")]
async fn spawn_mutation(
    transport: &Arc<dyn HdcTransport>,
    event_tx: &mpsc::Sender<BackendEvent>,
    context: &Context,
    registry: &DeviceRegistry,
    target: DeviceTarget,
    kind: fhdc_domain::FileMutationKind,
    path: fhdc_domain::RemotePath,
    second: Option<fhdc_domain::RemotePath>,
) {
    if !ensure_target(registry, &target) {
        send_event(
            event_tx,
            context,
            BackendEvent::FileMutationFailed {
                target,
                kind,
                error: BridgeError::device_unavailable(),
            },
        )
        .await;
        return;
    }
    let transport = Arc::clone(transport);
    let event_tx = event_tx.clone();
    let context = context.clone();
    tokio::spawn(async move {
        send_event(
            &event_tx,
            &context,
            BackendEvent::FileMutationStarted {
                target: target.clone(),
                kind,
                path: path.clone(),
            },
        )
        .await;
        let result = match kind {
            fhdc_domain::FileMutationKind::CreateDirectory => {
                transport.create_directory(&target.key, &path).await
            }
            fhdc_domain::FileMutationKind::Rename => match second.as_ref() {
                Some(to) => transport.rename_entry(&target.key, &path, to).await,
                None => Err(BridgeError::invalid_input("重命名缺少目标路径")),
            },
            fhdc_domain::FileMutationKind::Delete => {
                transport.delete_entry(&target.key, &path).await
            }
        };
        let event = match result {
            Ok(()) => BackendEvent::FileMutationCompleted { target, kind },
            Err(error) => BackendEvent::FileMutationFailed {
                target,
                kind,
                error,
            },
        };
        send_event(&event_tx, &context, event).await;
    });
}

/// 转发会话输出直到流结束或取消,合并小包后发事件。
#[expect(clippy::too_many_arguments, reason = "转发任务固定上下文集合")]
fn spawn_forwarder(
    session: SessionId,
    handle: ShellSessionHandle,
    cancel: CancellationToken,
    hilog: bool,
    event_tx: mpsc::Sender<BackendEvent>,
    context: Context,
    shell_inputs: Arc<RwLock<HashMap<SessionId, mpsc::Sender<Vec<u8>>>>>,
    sessions: Arc<Mutex<HashMap<SessionId, CancellationToken>>>,
) {
    tokio::spawn(async move {
        let mut handle = handle;
        let mut exit_code = None;
        // 通道关闭后必须直接跳出整个循环:外层再 poll 一次 next_chunk
        // 会拿到 None 并第二次 wait(),对已完成的 JoinHandle 再 poll
        // 会 panic("JoinHandle polled after completion"),ShellClosed
        // 事件就永远发不出去。
        'session: loop {
            let chunk = tokio::time::timeout(POLL_INTERVAL, handle.next_chunk()).await;
            let chunk = match chunk {
                // 轮询窗口到期:借机检查取消。
                Err(_) => {
                    if cancel.is_cancelled() {
                        handle.cancel();
                        break;
                    }
                    continue;
                }
                Ok(Some(chunk)) => chunk,
                // 输出通道关闭:会话结束。
                Ok(None) => {
                    exit_code = handle.wait().await.ok().flatten();
                    break;
                }
            };
            let mut bytes = chunk.bytes;
            let mut count = 1;
            let deadline = tokio::time::Instant::now() + COALESCE_WINDOW;
            while count < COALESCE_MAX_CHUNKS && bytes.len() < COALESCE_MAX_BYTES {
                match tokio::time::timeout_at(deadline, handle.next_chunk()).await {
                    Ok(Some(next)) => {
                        bytes.extend_from_slice(&next.bytes);
                        count += 1;
                    }
                    Ok(None) => {
                        exit_code = handle.wait().await.ok().flatten();
                        break 'session;
                    }
                    Err(_) => break,
                }
            }
            let event = if hilog {
                BackendEvent::HilogOutput { session, bytes }
            } else {
                BackendEvent::ShellOutput { session, bytes }
            };
            send_event(&event_tx, &context, event).await;
        }
        shell_inputs.write().unwrap().remove(&session);
        sessions.lock().unwrap().remove(&session);
        let event = if hilog {
            BackendEvent::HilogClosed { session }
        } else {
            BackendEvent::ShellClosed { session, exit_code }
        };
        send_event(&event_tx, &context, event).await;
    });
}

/// 批量加载时单个 HAP 的硬上限:超过它的包连图标也不取。
/// 拉整个 HAP 只是为了取 resources 里的图标,几百 MB 的包不值得为
/// 一张小图付这个代价——这类包留给用户选中时按需拉取。
const BULK_ICON_HAP_LIMIT: u64 = 256 * 1024 * 1024;

/// 图标获取方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconFetchMode {
    /// 批量:静默预热所有应用图标,只跳过极端超大的包。
    Bulk,
    /// 按需:用户明确要看这个应用的图标,任何大小都照拉。
    OnDemand,
}

/// 图标缓存路径:`%LOCALAPPDATA%hdc\icons\<bundle>-<stamp hash>.png`;
/// 没有图标时写同名 `.none` 标记,避免每次启动重复拉包。
fn icon_cache_path(bundle: &BundleName, stamp: &str) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = base.join("fhdc").join("icons");
    std::fs::create_dir_all(&directory).ok()?;
    let digest = Sha256::digest(stamp.as_bytes());
    let suffix: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(directory.join(format!("{}-{suffix}.png", bundle.as_str())))
}

/// 取一个包的图标:缓存命中直接返回;否则逐个 HAP 拉回本地解包。
/// 批量模式跳过超大 HAP(不写负缓存,选中时还能按需重试)。
enum IconOutcome {
    Icon(Vec<u8>),
    Missing,
    Skipped,
}

async fn fetch_bundle_icon(
    transport: &Arc<dyn HdcTransport>,
    target: &DeviceTarget,
    package: &BundlePackage,
    mode: IconFetchMode,
) -> IconOutcome {
    let Some(cache) = icon_cache_path(&package.bundle, &package.stamp()) else {
        return IconOutcome::Missing;
    };
    if let Ok(bytes) = std::fs::read(&cache) {
        return IconOutcome::Icon(bytes);
    }
    if cache.with_extension("none").exists() {
        return IconOutcome::Missing;
    }
    let too_large = |hap: &fhdc_domain::BundleHap| {
        mode == IconFetchMode::Bulk && hap.size_bytes > BULK_ICON_HAP_LIMIT
    };
    if package.haps.iter().all(too_large) {
        return IconOutcome::Skipped;
    }

    let mut pulled_any = false;
    let mut icon = None;
    for hap in &package.haps {
        if too_large(hap) {
            continue;
        }
        let Ok(remote) = RemotePath::new(hap.path.as_str()) else {
            continue;
        };
        let local =
            std::env::temp_dir().join(format!("fhdc_icon_{}.hap", uuid::Uuid::new_v4().simple()));
        let token = CancellationToken::new();
        let pulled = transport
            .pull_file(
                &target.key,
                &remote,
                &local,
                OverwritePolicy::ReplaceConfirmed,
                &token,
            )
            .await;
        if pulled.is_err() {
            let _ = tokio::fs::remove_file(&local).await;
            continue;
        }
        pulled_any = true;
        icon = fhdc_hdc::extract_hap_icon(&local);
        let _ = tokio::fs::remove_file(&local).await;
        if icon.is_some() {
            break;
        }
    }

    match (&icon, pulled_any) {
        (Some(bytes), _) => {
            let _ = tokio::fs::write(&cache, bytes).await;
            IconOutcome::Icon(bytes.clone())
        }
        (None, true) => {
            let _ = tokio::fs::write(cache.with_extension("none"), b"").await;
            IconOutcome::Missing
        }
        (None, false) => IconOutcome::Missing,
    }
}

fn ensure_target(registry: &DeviceRegistry, target: &DeviceTarget) -> bool {
    registry.current_online(target).is_some()
}

/// 后台执行一次设备发现,结果经完成通道回到主循环。
fn spawn_discovery(transport: &Arc<dyn HdcTransport>, done_tx: &mpsc::Sender<Completion>) {
    let transport = Arc::clone(transport);
    let done_tx = done_tx.clone();
    tokio::spawn(async move {
        let result = transport.list_devices().await;
        let _ = done_tx.send(Completion::Discovered(result)).await;
    });
}

/// 快照与上次广播相同则跳过:省掉每 2 秒一次的无谓重绘与事件。
async fn broadcast_if_changed(
    event_tx: &mpsc::Sender<BackendEvent>,
    context: &Context,
    last_broadcast: &mut Option<fhdc_domain::DeviceSnapshot>,
    snapshot: fhdc_domain::DeviceSnapshot,
) {
    if last_broadcast.as_ref() == Some(&snapshot) {
        return;
    }
    *last_broadcast = Some(snapshot.clone());
    send_event(event_tx, context, BackendEvent::DevicesChanged(snapshot)).await;
}

/// 决定使用真实 hdc 还是假后端;环境变量 FHDC_FAKE=1 强制假后端。
async fn initialize_transport(
    hdc_override: Option<PathBuf>,
    event_tx: &mpsc::Sender<BackendEvent>,
    context: &Context,
) -> Option<Arc<dyn HdcTransport>> {
    let fake_requested = std::env::var("FHDC_FAKE")
        .map(|value| matches!(value.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if fake_requested {
        send_event(
            event_tx,
            context,
            BackendEvent::HdcReady {
                path: "fake://hdc".to_owned(),
                version: "假后端".to_owned(),
            },
        )
        .await;
        return Some(Arc::new(fhdc_fake::FakeHdcTransport::new()));
    }

    let locator = fhdc_hdc::HdcLocator {
        override_path: hdc_override,
    };
    match locator.locate() {
        Some(path) => {
            let transport: Arc<dyn HdcTransport> =
                Arc::new(fhdc_hdc::ProcessHdcTransport::new(path.clone()));
            match transport.version().await {
                Ok(version) => {
                    send_event(
                        event_tx,
                        context,
                        BackendEvent::HdcReady {
                            path: path.display().to_string(),
                            version,
                        },
                    )
                    .await;
                    Some(transport)
                }
                Err(error) => {
                    send_event(
                        event_tx,
                        context,
                        BackendEvent::HdcUnavailable {
                            reason: format!("找到 {} 但无法运行: {}", path.display(), error.detail),
                        },
                    )
                    .await;
                    send_event(
                        event_tx,
                        context,
                        BackendEvent::HdcReady {
                            path: "fake://hdc".to_owned(),
                            version: "假后端".to_owned(),
                        },
                    )
                    .await;
                    Some(Arc::new(fhdc_fake::FakeHdcTransport::new()))
                }
            }
        }
        None => {
            let searched: Vec<String> = locator
                .candidates()
                .iter()
                .map(|path| path.display().to_string())
                .take(8)
                .collect();
            send_event(
                event_tx,
                context,
                BackendEvent::HdcUnavailable {
                    reason: format!("未找到 hdc 可执行文件。已尝试: {}", searched.join("; ")),
                },
            )
            .await;
            send_event(
                event_tx,
                context,
                BackendEvent::HdcReady {
                    path: "fake://hdc".to_owned(),
                    version: "假后端".to_owned(),
                },
            )
            .await;
            Some(Arc::new(fhdc_fake::FakeHdcTransport::new()))
        }
    }
}

/// 供 UI 构造带确认的应用操作。
pub fn app_action_label(action: AppAction) -> &'static str {
    match action {
        AppAction::Launch => "启动",
        AppAction::ForceStop => "强制停止",
        AppAction::ClearData => "清除数据",
        AppAction::Uninstall => "卸载",
    }
}

// ---------------------------------------------------------------------------
// 应用自升级(仿照 fadb:GitHub Releases 检查 → 校验下载 → 换名重启)
// ---------------------------------------------------------------------------

/// GitHub Releases API;`/releases/latest` 自动跳过草稿与预发布。
const RELEASES_API_URL: &str = "https://api.github.com/repos/yeqing17/fhdc/releases/latest";
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const UPDATE_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// 发布清单里标识"本平台裸 exe"的资源名后缀(自升级直接下载它)。
#[cfg(windows)]
const UPDATE_BINARY_SUFFIX: Option<&str> = Some("x86_64-pc-windows-msvc.exe");
#[cfg(not(windows))]
const UPDATE_BINARY_SUFFIX: Option<&str> = None;
const UPDATE_CHECKSUM_SUFFIX: &str = "SHA256SUMS.txt";

/// 查询 GitHub 最新发布并与当前构建比较。
/// reqwest 走系统代理设置,用户挂代理时与浏览器同路;任何失败
/// (离线、被墙、限流)都降级为可展示的 BridgeError。
async fn check_for_update() -> Result<fhdc_domain::UpdateCheckOutcome, BridgeError> {
    let failure = |error: reqwest::Error| {
        BridgeError::new(ErrorCode::Internal, format!("检查更新失败: {error}"))
    };
    let client = reqwest::Client::builder()
        .timeout(UPDATE_HTTP_TIMEOUT)
        .build()
        .map_err(failure)?;
    // GitHub API 对无 User-Agent 的请求直接 403。
    let release = client
        .get(RELEASES_API_URL)
        .header(
            reqwest::header::USER_AGENT,
            concat!("fhdc/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(failure)?
        .json::<LatestRelease>()
        .await
        .map_err(failure)?;
    Ok(evaluate_update(&release, env!("CARGO_PKG_VERSION")))
}

/// 更新器消费的 GitHub 发布 JSON 子集。
#[derive(serde::Deserialize, Clone, Debug)]
struct LatestRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq)]
struct ReleaseAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    browser_download_url: String,
}

/// 比较发布与当前版本。解析不了的 tag 一律视为"已最新":
/// 畸形的广告应该保持安静,而不是骚扰用户。
fn evaluate_update(release: &LatestRelease, current: &str) -> fhdc_domain::UpdateCheckOutcome {
    let parse = |text: &str| {
        text.trim()
            .trim_start_matches(['v', 'V'])
            .parse::<semver::Version>()
            .ok()
    };
    let (Some(latest), Some(running)) = (parse(&release.tag_name), parse(current)) else {
        return fhdc_domain::UpdateCheckOutcome::UpToDate;
    };
    if latest <= running {
        return fhdc_domain::UpdateCheckOutcome::UpToDate;
    }
    let (binary, checksum) = select_update_assets(&release.assets, UPDATE_BINARY_SUFFIX);
    fhdc_domain::UpdateCheckOutcome::Available(fhdc_domain::UpdateInfo {
        version: latest.to_string(),
        url: release.html_url.clone(),
        asset_url: binary.map(|asset| asset.browser_download_url.clone()),
        asset_size: binary.map(|asset| asset.size),
        checksum_url: binary
            .and(checksum)
            .map(|asset| asset.browser_download_url.clone()),
    })
}

/// 挑出本构建可自升级的资源:裸 exe + 校验和清单。
/// 没有裸 exe 就没有可校验的东西,两者都返回 None,流程降级为纯链接。
fn select_update_assets<'a>(
    assets: &'a [ReleaseAsset],
    binary_suffix: Option<&str>,
) -> (Option<&'a ReleaseAsset>, Option<&'a ReleaseAsset>) {
    let Some(suffix) = binary_suffix else {
        return (None, None);
    };
    let binary = assets.iter().find(|asset| asset.name.ends_with(suffix));
    let checksum = binary.and_then(|_| {
        assets
            .iter()
            .find(|asset| asset.name.ends_with(UPDATE_CHECKSUM_SUFFIX))
    });
    (binary, checksum)
}

/// 从 sha256sum 风格的清单里取指定资源的摘要,统一小写。
fn parse_checksums(manifest: &str, asset_name: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?;
        (name == asset_name && digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| digest.to_ascii_lowercase())
    })
}

/// `<exe>.download` 落盘路径:校验通过前,半个二进制绝不会被当成更新。
fn partial_download_path(exe: &std::path::Path) -> PathBuf {
    exe.with_extension("exe.download")
}

/// `<exe>.new` 暂存路径:UI 触发的换名会把它挪到运行中的二进制名上。
pub fn staged_update_path(exe: &std::path::Path) -> PathBuf {
    exe.with_extension("exe.new")
}

/// 下载发布二进制到运行目录旁,校验 SHA256 后暂存,等 UI 触发换名。
/// 进度按 UPDATE_PROGRESS_INTERVAL 节流;取消或失败都会删除半成品文件。
async fn download_update(
    info: fhdc_domain::UpdateInfo,
    events: mpsc::Sender<BackendEvent>,
    context: Context,
    cancel: CancellationToken,
) {
    let scratch = std::env::current_exe()
        .ok()
        .map(|exe| (partial_download_path(&exe), staged_update_path(&exe)));
    let outcome = match scratch.as_ref() {
        Some((partial, staging)) => {
            stream_update(&info, partial, staging, &events, &context, &cancel).await
        }
        None => Err(BridgeError::new(
            ErrorCode::Internal,
            "无法定位当前可执行文件,更新已取消",
        )),
    };
    if outcome.is_err()
        && let Some((partial, _)) = &scratch
    {
        let _ = tokio::fs::remove_file(partial).await;
    }
    send_event(
        &events,
        &context,
        BackendEvent::UpdateDownloadFinished(outcome),
    )
    .await;
}

async fn stream_update(
    info: &fhdc_domain::UpdateInfo,
    partial: &std::path::Path,
    staging: &std::path::Path,
    events: &mpsc::Sender<BackendEvent>,
    context: &Context,
    cancel: &CancellationToken,
) -> Result<(), BridgeError> {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    use tokio::io::AsyncWriteExt as _;

    fn fail(detail: impl std::fmt::Display) -> BridgeError {
        BridgeError::new(ErrorCode::Internal, detail.to_string())
    }
    let (Some(asset_url), Some(checksum_url)) = (&info.asset_url, &info.checksum_url) else {
        return Err(fail("该发布没有可校验的二进制资源,请手动下载"));
    };
    let user_agent = concat!("fhdc/", env!("CARGO_PKG_VERSION"));
    let client = reqwest::Client::builder()
        // 不设总超时:慢链路上的大文件是合法的;连接与停滞读仍有界,
        // 用户随时可取消。
        .connect_timeout(UPDATE_HTTP_TIMEOUT)
        .read_timeout(UPDATE_HTTP_TIMEOUT)
        .build()
        .map_err(|error| fail(format!("下载更新失败: {error}")))?;
    let mut download = client
        .get(asset_url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| fail(format!("下载更新失败: {error}")))?;
    let total = download.content_length().or(info.asset_size);
    let mut file = tokio::fs::File::create(partial)
        .await
        .map_err(|error| fail(format!("写入更新文件失败: {error}")))?;
    let mut hasher = sha2::Sha256::new();
    let mut received: u64 = 0;
    let mut last_progress = std::time::Instant::now();
    loop {
        if cancel.is_cancelled() {
            return Err(BridgeError::new(ErrorCode::Cancelled, "已取消下载"));
        }
        let Some(chunk) = download
            .chunk()
            .await
            .map_err(|error| fail(format!("下载更新失败: {error}")))?
        else {
            break;
        };
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| fail(format!("写入更新文件失败: {error}")))?;
        received += chunk.len() as u64;
        if last_progress.elapsed() >= UPDATE_PROGRESS_INTERVAL {
            last_progress = std::time::Instant::now();
            send_event(
                events,
                context,
                BackendEvent::UpdateDownloadProgress { received, total },
            )
            .await;
        }
    }
    file.flush()
        .await
        .map_err(|error| fail(format!("写入更新文件失败: {error}")))?;
    drop(file);

    // 清单在正文下载完成后才取,长下载不会占着第二条连接。
    let manifest = client
        .get(checksum_url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| fail(format!("获取校验和失败: {error}")))?
        .text()
        .await
        .map_err(|error| fail(format!("获取校验和失败: {error}")))?;
    let expected = parse_checksums(&manifest, asset_url.rsplit('/').next().unwrap_or_default())
        .ok_or_else(|| fail("校验和清单里没有该资源的条目"))?;
    let actual = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(fail("下载的文件与发布的校验和不匹配"));
    }
    tokio::fs::rename(partial, staging)
        .await
        .map_err(|error| fail(format!("暂存更新失败: {error}")))
}

#[cfg(test)]
mod update_check_tests {
    use super::{
        LatestRelease, ReleaseAsset, evaluate_update, parse_checksums, select_update_assets,
    };
    use fhdc_domain::UpdateCheckOutcome;

    fn release(tag: &str, assets: Vec<ReleaseAsset>) -> LatestRelease {
        LatestRelease {
            tag_name: tag.to_owned(),
            html_url: format!("https://github.com/yeqing17/fhdc/releases/tag/{tag}"),
            assets,
        }
    }

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_owned(),
            size: 1024,
            browser_download_url: format!(
                "https://github.com/yeqing17/fhdc/releases/download/x/{name}"
            ),
        }
    }

    #[test]
    fn newer_release_with_windows_assets_is_available() {
        let assets = vec![
            asset("fhdc-v0.2.0-x86_64-pc-windows-msvc"),
            asset("fhdc-v0.2.0-x86_64-pc-windows-msvc.exe"),
            asset("fhdc-v0.2.0-SHA256SUMS.txt"),
        ];
        let outcome = evaluate_update(&release("v0.2.0", assets), "0.1.0");
        let UpdateCheckOutcome::Available(info) = outcome else {
            panic!("应当有更新");
        };
        assert_eq!(info.version, "0.2.0");
        assert!(
            info.asset_url
                .as_deref()
                .expect("裸 exe 资源")
                .ends_with(".exe")
        );
        assert!(
            info.checksum_url
                .as_deref()
                .expect("校验和资源")
                .ends_with("SHA256SUMS.txt")
        );
    }

    #[test]
    fn same_or_older_or_malformed_tags_stay_quiet() {
        let assets = vec![asset("fhdc-v0.0.1-x86_64-pc-windows-msvc.exe")];
        assert_eq!(
            evaluate_update(&release("v0.0.1", assets.clone()), "0.1.0"),
            UpdateCheckOutcome::UpToDate
        );
        assert_eq!(
            evaluate_update(&release("v0.1.0", assets), "0.1.0"),
            UpdateCheckOutcome::UpToDate
        );
        assert_eq!(
            evaluate_update(&release("not-a-version", Vec::new()), "0.1.0"),
            UpdateCheckOutcome::UpToDate
        );
    }

    #[test]
    fn checksum_manifest_parsing() {
        // sha256sum 风格:合法行必须恰好 64 个十六进制字符。
        let digest = "a1".repeat(32);
        let manifest = format!("{digest}  one.zip\n00112233ff  two.exe\n{digest} three.zip\n");
        assert_eq!(parse_checksums(&manifest, "one.zip"), Some(digest.clone()));
        // 摘要长度不是 64 的行视为非法。
        assert_eq!(parse_checksums(&manifest, "two.exe"), None);
        // 字段间必须至少一个空格;名字不匹配的行跳过。
        assert_eq!(parse_checksums(&manifest, "three.zip"), Some(digest));
        assert_eq!(parse_checksums(&manifest, "missing"), None);
    }

    #[test]
    fn asset_selection_requires_checksum_for_self_update() {
        let assets = vec![asset("x86_64-pc-windows-msvc.exe")];
        let (binary, checksum) = select_update_assets(&assets, Some("x86_64-pc-windows-msvc.exe"));
        assert!(binary.is_some());
        assert!(checksum.is_none());
        assert_eq!(select_update_assets(&assets, None), (None, None));
    }
}
