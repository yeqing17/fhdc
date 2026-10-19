//! 应用骨架:顶栏(设备选择)、侧边导航、面板调度、连接管理与设置窗口。

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use eframe::egui;
use eframe::egui::CentralPanel;
use eframe::egui::Context;
use eframe::egui::SidePanel;
use eframe::egui::ThemePreference;
use eframe::egui::TopBottomPanel;
use eframe::egui::Ui;
use eframe::egui::ViewportBuilder;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BridgeError;
use fhdc_domain::ConnectKey;
use fhdc_domain::DeviceSnapshot;
use fhdc_domain::DeviceTarget;
use fhdc_domain::Endpoint;

use crate::panels::PanelCtx;
use crate::panels::applications::AppsState;
use crate::panels::files::FilesState;
use crate::panels::hilog::HilogState;
use crate::panels::overview::OverviewState;
use crate::panels::processes::ProcessesState;
use crate::panels::screenshot::ScreenshotState;
use crate::panels::shell::ShellState;
use crate::platform;
use crate::runtime::RuntimeBridge;
use crate::theme;

const NAVIGATION_WIDTH: f32 = 118.0;
const NAVIGATION_WIDTH_COLLAPSED: f32 = 44.0;
const TOAST_DURATION: Duration = Duration::from_secs(3);
const REMEMBERED_ENDPOINTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Overview,
    Files,
    Applications,
    Processes,
    Shell,
    Hilog,
    Screenshot,
}

impl Panel {
    const ALL: [Panel; 7] = [
        Panel::Overview,
        Panel::Files,
        Panel::Applications,
        Panel::Processes,
        Panel::Shell,
        Panel::Hilog,
        Panel::Screenshot,
    ];

    fn label(self) -> &'static str {
        match self {
            Panel::Overview => "概览",
            Panel::Files => "文件",
            Panel::Applications => "应用",
            Panel::Processes => "进程",
            Panel::Shell => "终端",
            Panel::Hilog => "日志",
            Panel::Screenshot => "截图",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Panel::Overview => "📊",
            Panel::Files => "📁",
            Panel::Applications => "📦",
            Panel::Processes => "📋",
            Panel::Shell => "💻",
            Panel::Hilog => "📜",
            Panel::Screenshot => "📷",
        }
    }

    fn storage_key(self) -> &'static str {
        match self {
            Panel::Overview => "overview",
            Panel::Files => "files",
            Panel::Applications => "applications",
            Panel::Processes => "processes",
            Panel::Shell => "shell",
            Panel::Hilog => "hilog",
            Panel::Screenshot => "screenshot",
        }
    }
}

struct Toast {
    message: String,
    until: Instant,
}

/// 设置页「更新」区与自升级流程的 UI 状态。
#[derive(Default)]
struct UpdateState {
    checking: bool,
    /// 最近一次检查是否由用户主动发起(决定失败是否展示)。
    user_check: bool,
    auto_checked: bool,
    outcome: Option<Result<fhdc_domain::UpdateCheckOutcome, BridgeError>>,
    downloading: bool,
    progress: Option<(u64, Option<u64>)>,
    /// 新版已下载并暂存,等用户点「重启并安装」。
    staged_ready: bool,
    error: Option<String>,
}

#[derive(Default)]
struct WindowFlags {
    manager: bool,
    settings: bool,
}

pub struct FhdcApp {
    runtime: RuntimeBridge,
    snapshot: DeviceSnapshot,
    overview: OverviewState,
    shell: ShellState,
    files: FilesState,
    apps: AppsState,
    processes: ProcessesState,
    hilog: HilogState,
    screenshot: ScreenshotState,
    active_panel: Panel,
    nav_collapsed: bool,
    dark: bool,
    terminal_auto_connect: bool,
    /// true = 使用 Windows 原生标题栏;false = 自绘标题栏(默认)。
    native_titlebar: bool,
    /// 本次指针按下是否已发出过 BeginResize(防止重复进入系统缩放循环)。
    resize_sent: bool,
    windows: WindowFlags,
    toast: Option<Toast>,
    last_error: Option<BridgeError>,
    /// 首帧检查持久化的窗口尺寸/位置,degenerate 时纠正一次。
    viewport_checked: bool,
    hdc_ready: Option<(String, String)>,
    hdc_unavailable: Option<String>,
    hdc_override: String,
    endpoints: Vec<Endpoint>,
    endpoint_input: String,
    listen_port: String,
    pending_select: Option<(ConnectKey, Instant)>,
    update: UpdateState,
}

impl FhdcApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>, native_titlebar: bool) -> Self {
        theme::configure(&creation_context.egui_ctx);
        let storage = creation_context.storage;
        let active_panel = storage
            .and_then(|storage| storage.get_string("fhdc.panel"))
            .and_then(|value| {
                Panel::ALL
                    .iter()
                    .copied()
                    .find(|panel| panel.storage_key() == value)
            })
            .unwrap_or(Panel::Overview);
        let dark = storage
            .and_then(|storage| storage.get_string("fhdc.dark"))
            .is_none_or(|value| value != "0");
        let nav_collapsed = storage
            .and_then(|storage| storage.get_string("fhdc.nav_collapsed"))
            .is_some_and(|value| value == "1");
        let terminal_auto_connect = storage
            .and_then(|storage| storage.get_string("fhdc.terminal_auto_connect"))
            .is_none_or(|value| value != "0");
        let hdc_override = storage
            .and_then(|storage| storage.get_string("fhdc.hdc_override"))
            .unwrap_or_default();
        let endpoints = storage
            .and_then(|storage| storage.get_string("fhdc.endpoints"))
            .and_then(|value| serde_json::from_str::<Vec<Endpoint>>(&value).ok())
            .unwrap_or_default();

        let mut shell = ShellState::new(storage);
        shell.set_auto_connect(terminal_auto_connect);
        let override_path =
            (!hdc_override.trim().is_empty()).then(|| PathBuf::from(hdc_override.trim()));
        let app = Self {
            runtime: RuntimeBridge::spawn(creation_context.egui_ctx.clone(), override_path),
            snapshot: DeviceSnapshot::default(),
            overview: OverviewState::default(),
            shell,
            files: FilesState::default(),
            apps: AppsState::default(),
            processes: ProcessesState::default(),
            hilog: HilogState::default(),
            screenshot: ScreenshotState::default(),
            active_panel,
            nav_collapsed,
            dark,
            terminal_auto_connect,
            native_titlebar,
            resize_sent: false,
            windows: WindowFlags::default(),
            toast: None,
            last_error: None,
            viewport_checked: false,
            hdc_ready: None,
            hdc_unavailable: None,
            hdc_override,
            endpoints,
            endpoint_input: String::new(),
            listen_port: "5555".to_owned(),
            pending_select: None,
            update: UpdateState::default(),
        };
        app.apply_theme(&creation_context.egui_ctx);
        app
    }

    fn apply_theme(&self, context: &Context) {
        context.set_theme(if self.dark {
            ThemePreference::Dark
        } else {
            ThemePreference::Light
        });
    }

    fn current_target(&self) -> Option<DeviceTarget> {
        self.snapshot
            .devices
            .iter()
            .find(|record| Some(&record.descriptor.key) == self.snapshot.selected.as_ref())
            .filter(|record| record.descriptor.state.is_online())
            .map(|record| record.target())
    }

    fn panel_ctx(&self) -> PanelCtx {
        PanelCtx {
            target: self.current_target(),
        }
    }

    fn send(&mut self, command: BackendCommand) {
        if !self.runtime.try_send(command.clone()) {
            // 命令队列满被丢弃:三个"发送前置位 loading"的面板按钮此刻
            // 是禁用的,不回滚会永久卡在加载态。此处集中回收。
            match &command {
                BackendCommand::LoadOverview(_) => self.overview.reset_loading(),
                BackendCommand::LoadBundles(_) => self.apps.reset_loading(),
                BackendCommand::CaptureScreenshot(_) => self.screenshot.reset_loading(),
                _ => {}
            }
            self.show_toast("操作繁忙,命令未被发送,请稍后重试");
        }
    }

    // ------------------------------------------------------------------
    // 事件处理
    // ------------------------------------------------------------------

    fn process_events(&mut self, context: &Context) {
        let events = self.runtime.drain();
        if events.is_empty() {
            return;
        }
        for event in events {
            self.shell.handle_event(&event);
            self.hilog.handle_event(&event);
            self.files.handle_event(&event);
            self.apps.handle_event(&event);
            self.processes.handle_event(&event);
            self.screenshot.handle_event(&event);
            self.overview.handle_event(&event);
            self.handle_app_event(event);
        }
        let _ = context;
    }

    fn handle_app_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::HdcReady { path, version } => {
                self.hdc_ready = Some((path, version));
            }
            BackendEvent::HdcUnavailable { reason } => {
                self.hdc_unavailable = Some(reason);
            }
            BackendEvent::DevicesChanged(snapshot) => {
                self.apply_snapshot(snapshot);
            }
            BackendEvent::Toast(message) => self.show_toast(message),
            BackendEvent::UpdateChecked(outcome) => {
                self.update.checking = false;
                if let Ok(fhdc_domain::UpdateCheckOutcome::Available(ref info)) = outcome {
                    // 无论手动还是自动检查,发现新版本都值得提示一次。
                    self.show_toast(format!("发现新版本 {},到 设置 → 关于 下载", info.version));
                }
                self.update.outcome = Some(outcome);
            }
            BackendEvent::UpdateDownloadProgress { received, total } => {
                self.update.progress = Some((received, total));
            }
            BackendEvent::UpdateDownloadFinished(result) => {
                self.update.downloading = false;
                self.update.progress = None;
                match result {
                    Ok(()) => {
                        self.update.staged_ready = true;
                        self.update.error = None;
                        self.show_toast("新版本已就绪,到 设置 → 关于 重启安装");
                    }
                    Err(error) => {
                        self.update.staged_ready = false;
                        self.update.error = Some(error.detail.clone());
                    }
                }
            }
            BackendEvent::EndpointConnectFinished { endpoint, message } => {
                self.show_toast(format!("连接成功: {message}"));
                self.remember_endpoint(endpoint.clone());
                if let Ok(key) = ConnectKey::new(endpoint.target_string()) {
                    // 连接成功会立刻触发一次重发现,DevicesChanged 可能
                    // 先于本事件到达(快照已含新设备且不再变化、不会再广播)。
                    // 所以快照里已经有了就直接选,不等下一次广播。
                    if self
                        .snapshot
                        .devices
                        .iter()
                        .any(|record| record.descriptor.key == key)
                    {
                        self.send(BackendCommand::SelectDevice { key: Some(key) });
                    } else {
                        self.pending_select = Some((key, Instant::now()));
                    }
                }
            }
            BackendEvent::EndpointConnectFailed { endpoint, error } => {
                self.last_error = Some(BridgeError::hdc(format!(
                    "连接 {} 失败: {error}",
                    endpoint.target_string()
                )));
            }
            BackendEvent::EndpointDisconnected { endpoint, message } => {
                self.show_toast(format!("已断开 {}: {message}", endpoint.target_string()));
            }
            BackendEvent::TcpListenEnabled { message } => {
                self.show_toast(format!(
                    "设备开始监听: {message};现在可用 tconn 连接其 IP:端口"
                ));
            }
            BackendEvent::TcpListenFailed { error } => {
                self.last_error = Some(BridgeError::hdc(format!("开启监听失败: {error}")));
            }
            BackendEvent::ServerKilled { message } => {
                self.show_toast(format!("hdc 服务已重启: {message}"));
            }
            BackendEvent::OverviewFailed(_, ref error)
            | BackendEvent::ProcessesFailed(_, ref error)
            | BackendEvent::BundlesFailed(_, ref error)
            | BackendEvent::OperationFailed(ref error) => {
                self.last_error = Some(error.clone());
            }
            _ => {}
        }
    }

    fn apply_snapshot(&mut self, snapshot: DeviceSnapshot) {
        let selection_changed = snapshot.selected != self.snapshot.selected;
        self.snapshot = snapshot;
        if let Some((key, since)) = &self.pending_select {
            if since.elapsed().as_secs() > 15 {
                self.pending_select = None;
            } else if self
                .snapshot
                .devices
                .iter()
                .any(|record| &record.descriptor.key == key)
            {
                let key = key.clone();
                self.pending_select = None;
                self.send(BackendCommand::SelectDevice { key: Some(key) });
                return;
            }
        }
        if selection_changed {
            let target = self.current_target();
            if let Some(command) = self.shell.reset_for(target.clone()) {
                self.send(command);
            }
            if let Some(command) = self.hilog.reset_for(target.clone()) {
                self.send(command);
            }
            self.files.reset_for(target.clone());
            self.apps.reset_for(target.clone());
            self.processes.reset_for(target.clone());
            self.screenshot.reset_for(target.clone());
            self.overview.reset_for(target);
        }
    }

    fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            until: Instant::now() + TOAST_DURATION,
        });
    }

    fn remember_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoints.retain(|existing| *existing != endpoint);
        self.endpoints.insert(0, endpoint);
        self.endpoints.truncate(REMEMBERED_ENDPOINTS);
    }

    // ------------------------------------------------------------------
    // 每帧自动行为
    // ------------------------------------------------------------------

    fn auto_behaviors(&mut self, context: &Context) {
        // 后端就绪后静默检查一次更新;失败不打扰(手动检查才展示错误)。
        if !self.update.auto_checked && self.hdc_ready.is_some() {
            self.update.auto_checked = true;
            self.update.user_check = false;
            self.update.checking = true;
            self.send(BackendCommand::CheckForUpdates);
        }
        let ctx = self.panel_ctx();
        // 选中变化后的概览加载
        if let Some(command) = (self.overview.wants_load(&ctx))
            .then(|| self.overview.load_command(&ctx))
            .flatten()
        {
            self.send(command);
        }
        match self.active_panel {
            Panel::Overview => {}
            Panel::Files => {
                if let Some(command) = self.files.maybe_auto_load(&ctx) {
                    self.send(command);
                }
            }
            Panel::Applications => {
                if let Some(command) = self.apps.maybe_auto_load(&ctx) {
                    self.send(command);
                }
            }
            Panel::Processes => {
                if let Some(command) = self.processes.tick(&ctx) {
                    self.send(command);
                }
                context.request_repaint_after(Duration::from_millis(250));
            }
            Panel::Shell => {
                if let Some(command) = self.shell.maybe_auto_connect(&ctx) {
                    self.send(command);
                }
                if self.shell.is_connected() {
                    context.request_repaint_after(Duration::from_millis(250));
                }
            }
            Panel::Hilog => {
                if let Some(command) = self.hilog.maybe_auto_start(&ctx) {
                    self.send(command);
                }
                if self.hilog.is_running() {
                    context.request_repaint_after(Duration::from_millis(250));
                }
            }
            Panel::Screenshot => {}
        }
        self.handle_dropped_files(context);
    }

    /// 拖拽:.hap/.app 走安装确认;文件面板激活时其它文件进上传队列。
    fn handle_dropped_files(&mut self, context: &Context) {
        let dropped: Vec<PathBuf> = context.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let mut installs: Vec<PathBuf> = Vec::new();
        let mut uploads: Vec<PathBuf> = Vec::new();
        for path in dropped {
            let is_app = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(extension.to_lowercase().as_str(), "hap" | "app")
                });
            if is_app {
                installs.push(path);
            } else {
                uploads.push(path);
            }
        }
        for path in installs {
            self.apps.pending_install = Some(path);
        }
        if !uploads.is_empty() {
            match self.active_panel {
                Panel::Files => self.files.queue_dropped_files(uploads),
                _ => self.show_toast("切到「文件」面板后再拖入文件即可上传"),
            }
        }
    }

    // ------------------------------------------------------------------
    // 布局
    // ------------------------------------------------------------------

    /// 顶栏同时充当窗口标题栏。
    ///
    /// 自绘标题栏模式下(默认),窗口是无边框的:拖动顶栏空白处移动
    /// 窗口,双击切换最大化,窗口按钮(— □ ×)在右端;使用原生标题栏
    /// 时,这些交互与按钮都交给系统。
    fn top_bar(&mut self, context: &Context) {
        TopBottomPanel::top("top-bar").show(context, |ui| {
            // 先注册拖拽区,后添加的交互控件才能赢得命中测试、保持可点。
            let title_bar = ui.interact(
                ui.max_rect(),
                ui.id().with("title-bar-drag"),
                egui::Sense::drag(),
            );
            // 边缘拖拽区优先于标题栏拖拽:拖上边缘或上角应当缩放而非移动。
            let custom_chrome = !self.native_titlebar;
            if custom_chrome && title_bar.drag_started() && seam_direction(context).is_none() {
                context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if custom_chrome && title_bar.double_clicked() {
                let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
                context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }

            ui.add_space(3.0);
            ui.horizontal(|ui| {
                self.brand(ui);
                ui.separator();
                self.device_selector(ui);
                if ui.button("⟲ 刷新设备").clicked() {
                    self.send(BackendCommand::RefreshDevices);
                }
                if ui
                    .selectable_label(self.windows.manager, "连接管理")
                    .clicked()
                {
                    self.windows.manager = !self.windows.manager;
                }
                ui.separator();
                let theme_label = if self.dark { "🌙" } else { "☀" };
                ui.toggle_value(&mut self.dark, theme_label)
                    .on_hover_text("切换浅色/深色主题");
                if ui.selectable_label(self.windows.settings, "⚙").clicked() {
                    self.windows.settings = !self.windows.settings;
                }
                if custom_chrome {
                    self.window_controls(ui, context);
                }
            });
            ui.add_space(3.0);
            if let Some(reason) = &self.hdc_unavailable {
                ui.colored_label(
                    theme::palette(ui.visuals().dark_mode).warn,
                    format!("⚠ {reason}(当前使用假后端,可在设置中指定 hdc 路径)"),
                );
            }
        });
    }

    /// 左上角的 logo 与名称(与任务栏、exe 图标同一枚)。
    fn brand(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            logo(ui, 22.0);
            ui.label(egui::RichText::new("fhdc").strong().color(theme::ACCENT));
        });
    }

    /// 窗口按钮(— □ ×),靠右端排列;关闭按钮悬停转 Windows 红。
    fn window_controls(&mut self, ui: &mut Ui, context: &Context) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let bar_height = ui.available_height().max(18.0);
            if close_button(ui, "关闭", bar_height).clicked() {
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
            let (label, tip) = if maximized {
                ("▣", "还原")
            } else {
                ("□", "最大化")
            };
            if caption_button(ui, label, tip, bar_height).clicked() {
                context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if caption_button(ui, "—", "最小化", bar_height).clicked() {
                context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            ui.add_space(6.0);
        });
    }

    fn device_selector(&mut self, ui: &mut Ui) {
        let selected_text = self
            .snapshot
            .selected
            .as_ref()
            .map(|key| key.as_str().to_owned())
            .unwrap_or_else(|| "选择设备…".to_owned());
        egui::ComboBox::from_id_salt("device-selector")
            .selected_text(selected_text)
            .width(300.0)
            .show_ui(ui, |ui| {
                if ui
                    .add(egui::Button::selectable(
                        self.snapshot.selected.is_none(),
                        "未选择",
                    ))
                    .clicked()
                {
                    self.send(BackendCommand::SelectDevice { key: None });
                }
                let mut pick: Option<Option<ConnectKey>> = None;
                for record in &self.snapshot.devices {
                    let key = record.descriptor.key.clone();
                    let state = if record.descriptor.state.is_online() {
                        "已连接"
                    } else {
                        "离线"
                    };
                    let kind = if key.is_network() { "网络" } else { "USB" };
                    let label = format!("{kind} · {state} · {}", key.as_str());
                    let selected = self.snapshot.selected.as_ref() == Some(&key);
                    if ui.add(egui::Button::selectable(selected, label)).clicked() {
                        pick = Some(Some(key));
                    }
                }
                if let Some(choice) = pick {
                    self.send(BackendCommand::SelectDevice { key: choice });
                }
            });
    }

    fn side_bar(&mut self, context: &Context) {
        let width = if self.nav_collapsed {
            NAVIGATION_WIDTH_COLLAPSED
        } else {
            NAVIGATION_WIDTH
        };
        SidePanel::left("navigation")
            .exact_width(width)
            .show_separator_line(false)
            .show(context, |ui| {
                ui.add_space(6.0);
                let mut switch: Option<Panel> = None;
                for panel in Panel::ALL {
                    let active = self.active_panel == panel;
                    if self.nav_collapsed {
                        let button = ui.add(
                            egui::Button::new(egui::RichText::new(panel.icon()).size(18.0))
                                .frame(false)
                                .min_size([width, 34.0].into()),
                        );
                        let response = button.on_hover_text(panel.label());
                        if response.clicked() {
                            switch = Some(panel);
                        }
                    } else {
                        let label = format!("{} {}", panel.icon(), panel.label());
                        if ui
                            .add_sized(
                                [width - 8.0, 30.0],
                                egui::Button::new(label).selected(active),
                            )
                            .clicked()
                        {
                            switch = Some(panel);
                        }
                    }
                }
                if let Some(panel) = switch {
                    self.active_panel = panel;
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    if ui
                        .small_button(if self.nav_collapsed { "»" } else { "«" })
                        .clicked()
                    {
                        self.nav_collapsed = !self.nav_collapsed;
                    }
                    if !self.nav_collapsed {
                        ui.label(egui::RichText::new("收起导航").small().weak());
                    }
                });
            });
    }

    fn central_panel(&mut self, context: &Context) {
        let palette = theme::palette(context.style().visuals.dark_mode);
        CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette.central_fill)
                    .inner_margin(12.0),
            )
            .show(context, |ui| {
                if let Some(error) = self.last_error.clone() {
                    let frame = egui::Frame::new()
                        .fill(palette.danger_fill)
                        .stroke(egui::Stroke::new(1.0, palette.danger_stroke))
                        .corner_radius(6.0)
                        .inner_margin(8.0);
                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(palette.error, format!("⚠ {}", error.detail));
                            if ui.small_button("关闭").clicked() {
                                self.last_error = None;
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
                let ctx = self.panel_ctx();
                let mut commands = match self.active_panel {
                    Panel::Overview => self.overview.show(ui, &ctx),
                    Panel::Files => self.files.show(ui, &ctx),
                    Panel::Applications => self.apps.show(ui, &ctx),
                    Panel::Processes => self.processes.show(ui, &ctx),
                    Panel::Shell => self.shell.show(ui, &ctx),
                    Panel::Hilog => self.hilog.show(ui, &ctx),
                    Panel::Screenshot => self.screenshot.show(ui, &ctx),
                };
                for command in commands.drain(..) {
                    self.send(command);
                }
            });
    }

    fn device_manager(&mut self, context: &Context) {
        if !self.windows.manager {
            return;
        }
        let mut close = false;
        egui::Window::new("连接管理")
            .collapsible(false)
            .default_width(460.0)
            .show(context, |ui| {
                ui.heading("当前设备");
                if self.snapshot.devices.is_empty() {
                    ui.label(egui::RichText::new("(未发现设备,请插好 USB 或连接网络设备)").weak());
                }
                let mut disconnect: Option<Endpoint> = None;
                for record in &self.snapshot.devices {
                    ui.horizontal(|ui| {
                        let state = if record.descriptor.state.is_online() {
                            "●"
                        } else {
                            "○"
                        };
                        ui.label(egui::RichText::new(state).color(
                            if record.descriptor.state.is_online() {
                                theme::palette(ui.visuals().dark_mode).ok
                            } else {
                                ui.visuals().weak_text_color()
                            },
                        ));
                        ui.label(record.descriptor.key.as_str());
                        if record.descriptor.key.is_network()
                            && ui.small_button("断开").clicked()
                            && let Ok(endpoint) = Endpoint::parse(record.descriptor.key.as_str())
                        {
                            disconnect = Some(endpoint);
                        }
                    });
                }
                if let Some(endpoint) = disconnect {
                    self.send(BackendCommand::DisconnectEndpoint(endpoint));
                }
                ui.separator();

                ui.heading("连接网络设备");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.endpoint_input)
                            .hint_text("192.168.1.10:5555")
                            .desired_width(180.0),
                    );
                    let valid = Endpoint::parse(&self.endpoint_input).is_ok();
                    if ui.add_enabled(valid, egui::Button::new("连接")).clicked()
                        && let Ok(endpoint) = Endpoint::parse(&self.endpoint_input)
                    {
                        self.remember_endpoint(endpoint.clone());
                        self.send(BackendCommand::ConnectEndpoint(endpoint));
                    }
                });
                if !self.endpoints.is_empty() {
                    ui.label(egui::RichText::new("历史端点").small().weak());
                    let mut actions: Vec<(Endpoint, bool)> = Vec::new();
                    let mut remove: Option<Endpoint> = None;
                    for endpoint in &self.endpoints {
                        ui.horizontal(|ui| {
                            ui.label(endpoint.target_string());
                            if ui.small_button("连接").clicked() {
                                actions.push((endpoint.clone(), true));
                            }
                            if ui.small_button("断开").clicked() {
                                actions.push((endpoint.clone(), false));
                            }
                            if ui.small_button("×").clicked() {
                                remove = Some(endpoint.clone());
                            }
                        });
                    }
                    for (endpoint, connect) in actions {
                        if connect {
                            self.remember_endpoint(endpoint.clone());
                            self.send(BackendCommand::ConnectEndpoint(endpoint));
                        } else {
                            self.send(BackendCommand::DisconnectEndpoint(endpoint));
                        }
                    }
                    if let Some(endpoint) = remove {
                        self.endpoints.retain(|existing| *existing != endpoint);
                    }
                }
                ui.separator();

                ui.heading("无线调试(设备监听)");
                ui.label(
                    egui::RichText::new(
                        "让选中的 USB 设备开始监听 TCP 端口,之后可用 IP:端口 连接。",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.listen_port)
                            .hint_text("5555")
                            .desired_width(70.0),
                    );
                    let port: Option<u16> = self.listen_port.trim().parse().ok();
                    let target = self.current_target();
                    if ui
                        .add_enabled(
                            port.is_some() && target.is_some(),
                            egui::Button::new("开始监听"),
                        )
                        .clicked()
                        && let (Some(target), Some(port)) = (target, port)
                    {
                        self.send(BackendCommand::EnableTcpListen { target, port });
                    }
                });
                ui.separator();

                ui.heading("hdc 服务");
                ui.label(
                    egui::RichText::new("设备列表异常时重启 hdc 服务(下一次命令会自动拉起)。")
                        .small()
                        .weak(),
                );
                if ui.button("重启 hdc 服务").clicked() {
                    self.send(BackendCommand::KillServer);
                }
                ui.separator();
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
        if close {
            self.windows.manager = false;
        }
    }

    fn settings_window(&mut self, context: &Context) {
        if !self.windows.settings {
            return;
        }
        let mut close = false;
        let mut reconnect_requested: Option<Option<PathBuf>> = None;
        egui::Window::new("设置")
            .collapsible(false)
            .default_width(440.0)
            .show(context, |ui| {
                egui::Grid::new("settings-grid")
                    .num_columns(2)
                    .min_col_width(96.0)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("外观").strong());
                        ui.label("");
                        ui.end_row();
                        ui.label("主题");
                        ui.horizontal(|ui| {
                            if ui.selectable_label(!self.dark, "浅色").clicked() && self.dark {
                                self.dark = false;
                            }
                            if ui.selectable_label(self.dark, "深色").clicked() && !self.dark {
                                self.dark = true;
                            }
                        });
                        ui.end_row();
                        ui.label("标题栏");
                        ui.vertical(|ui| {
                            if ui
                                .selectable_label(!self.native_titlebar, "应用自绘")
                                .clicked()
                                && self.native_titlebar
                            {
                                self.restart_with_title_bar(false);
                            }
                            if ui
                                .selectable_label(self.native_titlebar, "Windows 原生")
                                .clicked()
                                && !self.native_titlebar
                            {
                                self.restart_with_title_bar(true);
                            }
                            ui.label(
                                egui::RichText::new(
                                    "「应用自绘」隐藏系统标题栏,由顶部工具栏兼任标题栏:\
                                     拖动移动、双击最大化、右端为窗口按钮。切换会重启应用。",
                                )
                                .small()
                                .weak(),
                            );
                        });
                        ui.end_row();
                        ui.label(egui::RichText::new("hdc").strong());
                        ui.label("");
                        ui.end_row();
                        ui.label("当前状态");
                        ui.vertical(|ui| match &self.hdc_ready {
                            Some((path, version)) => {
                                ui.label(version.to_string());
                                ui.label(
                                    egui::RichText::new(path.clone()).small().monospace().weak(),
                                );
                            }
                            None => {
                                ui.label("初始化中…");
                            }
                        });
                        ui.end_row();
                        ui.label("可执行文件路径");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.hdc_override)
                                .hint_text("留空则自动搜索(FHDC_HDC 环境变量优先)")
                                .desired_width(260.0),
                        );
                        ui.end_row();
                        ui.label("");
                        if ui.button("应用并重连后端").clicked() {
                            reconnect_requested = Some(
                                (!self.hdc_override.trim().is_empty())
                                    .then(|| PathBuf::from(self.hdc_override.trim())),
                            );
                        }
                        ui.end_row();
                        ui.label(egui::RichText::new("终端").strong());
                        ui.label("");
                        ui.end_row();
                        ui.label("自动连接");
                        if ui
                            .checkbox(&mut self.terminal_auto_connect, "选中设备后自动打开终端")
                            .changed()
                        {
                            self.shell.set_auto_connect(self.terminal_auto_connect);
                        }
                        ui.end_row();
                        ui.label(egui::RichText::new("关于").strong());
                        ui.label("");
                        ui.end_row();
                        ui.label("版本");
                        ui.label(env!("CARGO_PKG_VERSION"));
                        ui.end_row();
                    });
                ui.separator();
                self.update_section(ui);
                ui.separator();
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
        if let Some(override_path) = reconnect_requested {
            self.hdc_ready = None;
            self.hdc_unavailable = None;
            self.snapshot = DeviceSnapshot::default();
            self.overview = OverviewState::default();
            self.files = FilesState::default();
            self.apps = AppsState::default();
            self.processes = ProcessesState::default();
            self.screenshot = ScreenshotState::default();
            if let Some(command) = self.hilog.reset_for(None) {
                self.send(command);
            }
            self.runtime = RuntimeBridge::spawn(context.clone(), override_path);
            self.send(BackendCommand::RefreshDevices);
        }
        if close {
            self.windows.settings = false;
        }
    }

    /// 设置页的「更新」区:检查 → 下载(进度/取消)→ 重启安装。
    fn update_section(&mut self, ui: &mut Ui) {
        ui.label(egui::RichText::new("更新").strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let check_label = if self.update.checking {
                "检查中…"
            } else {
                "检查更新"
            };
            if ui
                .add_enabled(
                    !self.update.checking && !self.update.downloading,
                    egui::Button::new(check_label),
                )
                .clicked()
            {
                self.update.user_check = true;
                self.update.checking = true;
                self.update.error = None;
                self.send(BackendCommand::CheckForUpdates);
            }
            if self.update.checking {
                ui.spinner();
            }
        });
        match self.update.outcome.clone() {
            Some(Ok(fhdc_domain::UpdateCheckOutcome::UpToDate)) => {
                ui.label(egui::RichText::new("已是最新版本。").weak());
            }
            Some(Ok(fhdc_domain::UpdateCheckOutcome::Available(info))) => {
                ui.label(format!("新版本 {} 可用。", info.version));
                if info.asset_url.is_some() {
                    if !self.update.downloading
                        && !self.update.staged_ready
                        && ui.button("下载并安装").clicked()
                    {
                        self.update.downloading = true;
                        self.update.error = None;
                        self.send(BackendCommand::DownloadUpdate);
                    }
                    if self.update.downloading {
                        ui.horizontal(|ui| {
                            let (received, total) =
                                self.update.progress.unwrap_or((0, info.asset_size));
                            let fraction = total
                                .filter(|total| *total > 0)
                                .map(|total| received as f32 / total as f32)
                                .unwrap_or(0.0);
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .show_percentage()
                                    .desired_width(220.0),
                            );
                            if ui.button("取消").clicked() {
                                self.send(BackendCommand::CancelUpdateDownload);
                            }
                        });
                        ui.ctx().request_repaint_after(Duration::from_millis(120));
                    }
                    if self.update.staged_ready && ui.button("重启并安装").clicked() {
                        // 成功的换名不会返回:新进程已接管。
                        if let Err(error) = apply_pending_update() {
                            self.update.error = Some(error);
                        }
                    }
                } else if !info.url.is_empty() {
                    ui.hyperlink_to("打开发布页手动下载", &info.url);
                }
            }
            Some(Err(error)) => {
                // 自动检查的失败保持安静,只在用户主动检查时展示。
                if self.update.user_check {
                    ui.colored_label(
                        theme::palette(ui.visuals().dark_mode).error,
                        error.detail.clone(),
                    );
                }
            }
            None => {
                ui.label(egui::RichText::new("尚未检查更新。").weak());
            }
        }
        if let Some(error) = &self.update.error {
            ui.colored_label(theme::palette(ui.visuals().dark_mode).error, error.clone());
        }
    }

    fn show_toast_overlay(&mut self, context: &Context) {
        let Some(toast) = &self.toast else { return };
        if Instant::now() > toast.until {
            self.toast = None;
            return;
        }
        let frame = egui::Frame::new()
            .fill(theme::palette(context.style().visuals.dark_mode).chip_fill)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 8));
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0])
            .show(context, |ui| {
                frame.show(ui, |ui| {
                    ui.label(egui::RichText::new(&toast.message).small());
                });
            });
        context.request_repaint_after(Duration::from_millis(120));
    }
}

impl FhdcApp {
    /// 持久化的窗口状态可能来自最小化/异常退出的那一刻(0×0 或
    /// -32000 坐标),恢复后窗口会缩成一条或跑到屏幕外。首帧检测到
    /// 这种尺寸就重置成默认大小并摆回可见区域。
    fn sanitize_viewport_once(&mut self, context: &Context) {
        if self.viewport_checked {
            return;
        }
        let Some(rect) = context.input(|input| input.viewport().inner_rect) else {
            return;
        };
        self.viewport_checked = true;
        if rect.width() < 600.0 || rect.height() < 400.0 {
            context.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1180.0, 760.0)));
            context.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(80.0, 80.0)));
        }
    }

    /// 切换标题栏样式:保存选择,再带参数重启自己。
    ///
    /// eframe 只在创建窗口时决定窗口装饰,运行时的
    /// `ViewportCommand::Decorations` 并不被处理,所以切换只能靠重启;
    /// 命令行参数把新样式带进新进程,免得用户还要手动再选一次。
    fn restart_with_title_bar(&mut self, native: bool) {
        let style = if native {
            platform::TitleBarStyle::Native
        } else {
            platform::TitleBarStyle::Custom
        };
        platform::save_title_bar_style(style);
        let flag = if native {
            "--native-titlebar"
        } else {
            "--custom-titlebar"
        };
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        match std::process::Command::new(exe).arg(flag).spawn() {
            Ok(_) => std::process::exit(0),
            Err(error) => {
                self.show_toast(format!("重启失败: {error}"));
            }
        }
    }
}

impl eframe::App for FhdcApp {
    fn update(&mut self, context: &Context, _frame: &mut eframe::Frame) {
        self.sanitize_viewport_once(context);
        self.process_events(context);
        if self.dark != context.style().visuals.dark_mode {
            self.apply_theme(context);
        }
        self.auto_behaviors(context);
        self.top_bar(context);
        // 无边框窗口没有系统缩放边框,自己补上边缘拖拽区。
        if !self.native_titlebar {
            handle_window_resize(context, &mut self.resize_sent);
        }
        self.side_bar(context);
        self.central_panel(context);
        self.device_manager(context);
        self.settings_window(context);
        self.show_toast_overlay(context);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("fhdc.panel", self.active_panel.storage_key().to_owned());
        storage.set_string("fhdc.dark", if self.dark { "1" } else { "0" }.to_owned());
        storage.set_string(
            "fhdc.nav_collapsed",
            if self.nav_collapsed { "1" } else { "0" }.to_owned(),
        );
        storage.set_string(
            "fhdc.terminal_auto_connect",
            if self.terminal_auto_connect { "1" } else { "0" }.to_owned(),
        );
        storage.set_string("fhdc.hdc_override", self.hdc_override.clone());
        if let Ok(value) = serde_json::to_string(&self.endpoints) {
            storage.set_string("fhdc.endpoints", value);
        }
        self.shell.save(storage);
    }
}

/// 视口配置(供 main.rs 使用)。
/// 把暂存的更新换到当前可执行文件名上并重启。
///
/// Windows 不允许覆盖运行中的 exe、但允许改名,所以舞步是:
/// 运行中 → `.old`,暂存的 `.new` → 原名,拉起新进程,退出。
/// `main` 在下次启动时清理残留的 `.old`。任一步失败都会恢复原布局,
/// 程序就地继续可用。
fn apply_pending_update() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let staged = crate::runtime::staged_update_path(&exe);
    if !staged.exists() {
        return Err("暂存的更新不存在,请重新下载".to_owned());
    }
    let backup = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&exe, &backup).map_err(|error| format!("无法移开当前程序: {error}"))?;
    if let Err(error) = std::fs::rename(&staged, &exe) {
        let _ = std::fs::rename(&backup, &exe);
        return Err(format!("无法放入新版本: {error}"));
    }
    match std::process::Command::new(&exe).spawn() {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            // 换回旧二进制;暂存的新版保留,可再试一次。
            let _ = std::fs::rename(&exe, &staged);
            let _ = std::fs::rename(&backup, &exe);
            Err(format!("无法启动新版本: {error}"))
        }
    }
}

/// 窗口初始一律无边框;若用户选择原生标题栏,首帧再切回去
/// (eframe 拿不到窗口创建前的持久化设置)。
pub fn viewport_builder(native_titlebar: bool) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title("fhdc — 鸿蒙调试工具箱")
        .with_decorations(native_titlebar)
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([900.0, 600.0])
}

/// 顶栏 logo:与任务栏、exe 图标同一枚,解码一次后缓存为纹理。
fn logo(ui: &mut Ui, size: f32) -> egui::Response {
    let texture = logo_texture(ui.ctx());
    ui.add(
        egui::Image::from_texture(&texture)
            .max_size(egui::vec2(size, size))
            .corner_radius(size * 0.22),
    )
}

fn logo_texture(ctx: &Context) -> egui::TextureHandle {
    let logo_id = egui::Id::new("fhdc-logo");
    if let Some(handle) = ctx.data_mut(|cache| cache.get_temp::<egui::TextureHandle>(logo_id)) {
        return handle;
    }
    let img = image::load_from_memory(include_bytes!("../assets/icon-64.png"))
        .expect("内嵌 logo 是有效 PNG")
        .to_rgba8();
    let [width, height] = [img.width() as usize, img.height() as usize];
    let handle = ctx.load_texture(
        "fhdc-logo",
        egui::ColorImage::from_rgba_unmultiplied([width, height], img.as_raw()),
        egui::TextureOptions::default(),
    );
    ctx.data_mut(|cache| cache.insert_temp(logo_id, handle.clone()));
    handle
}

/// 窗口按钮:字形避开 Dingbats 区段(✕/❐ 在内置字体里是豆腐块),
/// × 与 ▣ 在拉丁与中文回退字体里都有。
fn caption_button(ui: &mut Ui, label: &str, tooltip: &str, bar_height: f32) -> egui::Response {
    ui.add_sized(
        [bar_height * 1.6, bar_height],
        egui::Button::new(egui::RichText::new(label).size(13.0)),
    )
    .on_hover_text(tooltip)
}

/// 关闭按钮:悬停转 Windows 风格的红底白字。
fn close_button(ui: &mut Ui, tooltip: &str, bar_height: f32) -> egui::Response {
    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(196, 44, 52);
        widgets.hovered.bg_stroke = egui::Stroke::NONE;
        widgets.hovered.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        widgets.active.weak_bg_fill = egui::Color32::from_rgb(224, 54, 62);
        widgets.active.bg_stroke = egui::Stroke::NONE;
        ui.add_sized(
            [bar_height * 1.6, bar_height],
            egui::Button::new(egui::RichText::new("×").size(13.0)),
        )
        .on_hover_text(tooltip)
    })
    .inner
}

/// 无边框窗口没有系统的缩放边框,用窗口边缘的隐形拖拽区补上。
///
/// 一次按下只发一次 BeginResize:系统缩放循环退出时,winit 会补发一个
/// 按钮抬起事件,egui 可能晚一帧才看到;无条件重发会带着已松开的按钮
/// 立刻再次进入缩放循环(窗口卡在缩放状态)。
fn handle_window_resize(context: &Context, resize_sent: &mut bool) {
    let Some(direction) = seam_direction(context) else {
        return;
    };
    context.set_cursor_icon(seam_cursor(direction));
    let (dragging, primary_down) = context.input(|input| {
        (
            input.pointer.is_decidedly_dragging(),
            input.pointer.primary_down(),
        )
    });
    // 只在手势确实在拖动时闩住;一旦不在拖动(按钮抬起,或按下但未移动)
    // 就清掉闩锁,被吞掉的发送不会污染后续手势。
    if !primary_down || !dragging {
        *resize_sent = false;
        return;
    }
    if !*resize_sent {
        *resize_sent = true;
        context.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
    }
}

/// 指针当前落在哪条缩放边上。
fn seam_direction(context: &Context) -> Option<egui::ResizeDirection> {
    if context.input(|input| input.viewport().maximized.unwrap_or(false)) {
        return None;
    }
    context
        .pointer_hover_pos()
        .and_then(|pos| resize_direction(pos, context.content_rect()))
}

/// 缩放边的隐形厚度(点)。
const RESIZE_SEAM_THICKNESS: f32 = 8.0;

fn resize_direction(pointer: egui::Pos2, content: egui::Rect) -> Option<egui::ResizeDirection> {
    let rect = content.expand2(egui::vec2(RESIZE_SEAM_THICKNESS, RESIZE_SEAM_THICKNESS));
    if !rect.contains(pointer) {
        return None;
    }
    let left = pointer.x - content.left() < RESIZE_SEAM_THICKNESS;
    let right = content.right() - pointer.x < RESIZE_SEAM_THICKNESS;
    let top = pointer.y - content.top() < RESIZE_SEAM_THICKNESS;
    let bottom = content.bottom() - pointer.y < RESIZE_SEAM_THICKNESS;
    match (left, right, top, bottom) {
        (true, false, true, false) => Some(egui::ResizeDirection::NorthWest),
        (false, true, true, false) => Some(egui::ResizeDirection::NorthEast),
        (true, false, false, true) => Some(egui::ResizeDirection::SouthWest),
        (false, true, false, true) => Some(egui::ResizeDirection::SouthEast),
        (true, false, false, false) => Some(egui::ResizeDirection::West),
        (false, true, false, false) => Some(egui::ResizeDirection::East),
        (false, false, true, false) => Some(egui::ResizeDirection::North),
        (false, false, false, true) => Some(egui::ResizeDirection::South),
        _ => None,
    }
}

fn seam_cursor(direction: egui::ResizeDirection) -> egui::CursorIcon {
    match direction {
        egui::ResizeDirection::North => egui::CursorIcon::ResizeNorth,
        egui::ResizeDirection::South => egui::CursorIcon::ResizeSouth,
        egui::ResizeDirection::East => egui::CursorIcon::ResizeEast,
        egui::ResizeDirection::West => egui::CursorIcon::ResizeWest,
        egui::ResizeDirection::NorthEast => egui::CursorIcon::ResizeNorthEast,
        egui::ResizeDirection::NorthWest => egui::CursorIcon::ResizeNorthWest,
        egui::ResizeDirection::SouthEast => egui::CursorIcon::ResizeSouthEast,
        egui::ResizeDirection::SouthWest => egui::CursorIcon::ResizeSouthWest,
    }
}
