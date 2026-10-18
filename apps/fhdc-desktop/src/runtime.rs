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
    AppAction, BackendCommand, BackendEvent, BridgeError, DeviceTarget, ErrorCode, OperationId,
    SessionId,
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
    pub fn try_send(&self, command: BackendCommand) {
        if let BackendCommand::WriteShell { session, input } = &command {
            let guard = self.shell_inputs.read().unwrap();
            if let Some(sender) = guard.get(session) {
                let _ = sender.try_send(input.clone());
                return;
            }
            tracing::debug!(?session, "WriteShell 被丢弃:会话已关闭或尚未注册");
        }
        if self.command_tx.try_send(command).is_err() {
            tracing::warn!("后端命令队列已满,命令被丢弃");
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

/// 后台任务完成回执:传输收尾或设备发现结果。
enum Completion {
    Transfer(OperationId),
    /// 发现任务完成;Err 时保留上一次快照,不清空设备列表。
    Discovered(Result<Vec<fhdc_domain::DeviceDescriptor>, BridgeError>),
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
                // 命令队列;设备列表由发现轮询自然刷新。
                let transport = Arc::clone(&transport);
                let event_tx = event_tx.clone();
                let context = context.clone();
                tokio::spawn(async move {
                    match transport.connect_endpoint(&endpoint).await {
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
                tokio::spawn(async move {
                    match transport.disconnect_endpoint(&endpoint).await {
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
                    let key = target.key;
                    tokio::spawn(async move {
                        match transport.enable_tcp_listen(&key, port).await {
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
                tokio::spawn(async move {
                    if let Ok(message) = transport.kill_server().await {
                        send_event(&event_tx, &context, BackendEvent::ServerKilled { message })
                            .await;
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
            BackendCommand::OpenShell { target, session } => {
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
                match transport.start_shell(&target.key).await {
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
        loop {
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
                        break;
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
