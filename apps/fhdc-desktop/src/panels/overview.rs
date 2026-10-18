//! 设备概览:一屏速览型号、版本、CPU、内存、存储,点击值即复制。

use eframe::egui;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BridgeError;
use fhdc_domain::DeviceOverview;
use fhdc_domain::DeviceTarget;

use super::Commands;
use super::PanelCtx;

#[derive(Default)]
pub struct OverviewState {
    target: Option<DeviceTarget>,
    loading: bool,
    loaded: Option<DeviceOverview>,
    error: Option<BridgeError>,
}

impl OverviewState {
    pub fn reset_for(&mut self, target: Option<DeviceTarget>) {
        if self.target.as_ref() == target.as_ref() {
            return;
        }
        self.target = target;
        self.loaded = None;
        self.error = None;
        // 进行中的加载事件带着旧 target 回来不会再匹配,必须就地复位,
        // 否则面板会永久卡在加载态。
        self.loading = false;
    }

    /// 目标在线但数据为空时请求加载(带节流由调用方控制)。
    pub fn wants_load(&self, ctx: &PanelCtx) -> bool {
        ctx.online() && self.loaded.is_none() && self.error.is_none() && !self.loading
    }

    /// 取加载命令;发送前就置位 loading,避免事件往返期间每帧重复请求。
    pub fn load_command(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        let command = ctx.target.clone().map(BackendCommand::LoadOverview);
        if command.is_some() {
            self.loading = true;
        }
        command
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::OverviewLoading(target) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = true;
                }
            }
            BackendEvent::OverviewLoaded(target, overview) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = None;
                    self.loaded = Some(overview.clone());
                }
            }
            BackendEvent::OverviewFailed(target, error) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = Some(error.clone());
                }
            }
            _ => {}
        }
    }

    pub fn show(&mut self, ui: &mut Ui, ctx: &PanelCtx) -> Commands {
        let mut commands = Vec::new();
        ui.heading("设备概览");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let refresh_enabled = ctx.online() && !self.loading;
            if ui
                .add_enabled(refresh_enabled, egui::Button::new("刷新"))
                .on_disabled_hover_text("请先选择设备")
                .clicked()
                && let Some(command) = self.load_command(ctx)
            {
                self.loaded = None;
                self.error = None;
                commands.push(command);
            }
            if self.loading {
                ui.spinner();
                ui.label(egui::RichText::new("读取中…").weak());
            }
        });
        ui.add_space(6.0);

        if !ctx.online() {
            empty_hint(ui);
            return commands;
        }
        if let Some(error) = &self.error {
            ui.colored_label(
                crate::theme::palette(ui.visuals().dark_mode).error,
                format!("读取失败: {}", error.detail),
            );
        }
        let Some(overview) = &self.loaded else {
            if self.error.is_none() {
                ui.label(egui::RichText::new("尚未读取设备信息。").weak());
            }
            return commands;
        };

        let mut rows: Vec<(&str, String)> = Vec::new();
        if let Some(value) = &overview.model {
            rows.push(("型号", value.clone()));
        }
        if let Some(value) = &overview.brand {
            rows.push(("品牌", value.clone()));
        }
        if let Some(value) = &overview.device_type {
            rows.push(("设备类型", value.clone()));
        }
        if let Some(value) = &overview.software_version {
            rows.push(("系统版本", value.clone()));
        }
        if let Some(value) = &overview.api_version {
            rows.push(("API 版本", value.clone()));
        }
        if let Some(value) = &overview.kernel_version {
            rows.push(("内核版本", value.clone()));
        }
        if let Some(cores) = overview.cpu_cores {
            rows.push(("CPU 核心", cores.to_string()));
        }
        if let Some(total) = overview.memory_total_kib {
            rows.push(("内存总量", format_kib(total)));
        }
        if let Some(available) = overview.memory_available_kib {
            rows.push(("可用内存", format_kib(available)));
        }
        if let (Some(total), Some(used)) = (overview.storage_total_kib, overview.storage_used_kib) {
            let percent = if total > 0 { used * 100 / total } else { 0 };
            rows.push((
                "/data 分区",
                format!("{} / {}({percent}%)", format_kib(used), format_kib(total)),
            ));
        }

        egui::Grid::new("overview-grid")
            .num_columns(2)
            .min_col_width(96.0)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                for (label, value) in rows {
                    ui.label(egui::RichText::new(label).weak());
                    let copied = ui
                        .add(
                            egui::Button::new(egui::RichText::new(&value).monospace())
                                .small()
                                .frame(false),
                        )
                        .on_hover_text("点击复制");
                    if copied.clicked() {
                        ui.ctx().copy_text(value.clone());
                    }
                    ui.end_row();
                }
            });
        commands
    }
}

fn empty_hint(ui: &mut Ui) {
    ui.add_space(16.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new("没有选中的设备").strong());
        ui.label(egui::RichText::new("在顶部选择设备,或点击「连接管理」添加网络设备。").weak());
    });
}

pub fn format_kib(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1} GiB", kib as f64 / 1024.0 / 1024.0)
    } else if kib >= 1024 {
        format!("{:.1} MiB", kib as f64 / 1024.0)
    } else {
        format!("{kib} KiB")
    }
}
