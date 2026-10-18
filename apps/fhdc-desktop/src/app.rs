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
    windows: WindowFlags,
    toast: Option<Toast>,
    last_error: Option<BridgeError>,
    hdc_ready: Option<(String, String)>,
    hdc_unavailable: Option<String>,
    hdc_override: String,
    endpoints: Vec<Endpoint>,
    endpoint_input: String,
    listen_port: String,
    pending_select: Option<(ConnectKey, Instant)>,
}

impl FhdcApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
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
            windows: WindowFlags::default(),
            toast: None,
            last_error: None,
            hdc_ready: None,
            hdc_unavailable: None,
            hdc_override,
            endpoints,
            endpoint_input: String::new(),
            listen_port: "5555".to_owned(),
            pending_select: None,
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
        self.runtime.try_send(command);
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
            BackendEvent::EndpointConnectFinished { endpoint, message } => {
                self.show_toast(format!("连接成功: {message}"));
                self.remember_endpoint(endpoint.clone());
                if let Ok(key) = ConnectKey::new(endpoint.target_string()) {
                    self.pending_select = Some((key, Instant::now()));
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

    fn top_bar(&mut self, context: &Context) {
        TopBottomPanel::top("top-bar").show(context, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("fhdc").strong().color(theme::ACCENT));
                ui.label(egui::RichText::new("鸿蒙调试工具箱").small().weak());
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
            });
            ui.add_space(4.0);
            if let Some(reason) = &self.hdc_unavailable {
                ui.colored_label(
                    theme::palette(ui.visuals().dark_mode).warn,
                    format!("⚠ {reason}(当前使用假后端,可在设置中指定 hdc 路径)"),
                );
            }
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

impl eframe::App for FhdcApp {
    fn update(&mut self, context: &Context, _frame: &mut eframe::Frame) {
        self.process_events(context);
        if self.dark != context.style().visuals.dark_mode {
            self.apply_theme(context);
        }
        self.auto_behaviors(context);
        self.top_bar(context);
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
pub fn viewport_builder() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title("fhdc — 鸿蒙调试工具箱")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([900.0, 600.0])
}
