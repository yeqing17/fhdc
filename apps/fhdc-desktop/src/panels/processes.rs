//! 进程表:自动刷新、搜索、结束进程(需确认)。

use std::time::Instant;

use eframe::egui;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::DeviceTarget;
use fhdc_domain::ProcessInfo;

use super::Commands;
use super::PanelCtx;

const REFRESH_INTERVAL_SECS: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SortKey {
    #[default]
    Pid,
    Cpu,
    Name,
}

#[derive(Default)]
pub struct ProcessesState {
    target: Option<DeviceTarget>,
    processes: Vec<ProcessInfo>,
    /// 进程表每次刷新时递增,是显示缓存的失效键。
    processes_generation: u64,
    /// 过滤 + 排序后的显示缓存。
    display_cache: Vec<ProcessInfo>,
    display_cache_key: Option<(u64, SortKey, bool, String)>,
    loading: bool,
    error: Option<String>,
    query: String,
    auto_refresh: bool,
    last_refresh: Option<Instant>,
    sort_key: SortKey,
    sort_descending: bool,
    kill_prompt: Option<ProcessInfo>,
}

impl ProcessesState {
    pub fn reset_for(&mut self, target: Option<DeviceTarget>) {
        if self.target.as_ref() == target.as_ref() {
            return;
        }
        self.target = target;
        self.processes.clear();
        self.error = None;
        self.last_refresh = None;
        self.kill_prompt = None;
        // 就地复位加载态,旧 target 的结果事件不再匹配。
        self.loading = false;
    }

    /// 面板可见时的周期刷新。
    pub fn tick(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if !ctx.online() || !self.auto_refresh {
            return None;
        }
        let due = self
            .last_refresh
            .is_none_or(|last| last.elapsed().as_secs() >= REFRESH_INTERVAL_SECS);
        if due && !self.loading {
            self.last_refresh = Some(Instant::now());
            return ctx.target.clone().map(BackendCommand::LoadProcesses);
        }
        None
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::ProcessesLoading(target) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = true;
                }
            }
            BackendEvent::ProcessesLoaded(target, processes) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = None;
                    self.processes = processes.clone();
                    self.processes_generation += 1;
                }
            }
            BackendEvent::ProcessesFailed(target, error) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = Some(error.detail.clone());
                }
            }
            _ => {}
        }
    }

    pub fn show(&mut self, ui: &mut Ui, ctx: &PanelCtx) -> Commands {
        let mut commands = Vec::new();
        ui.heading("进程");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ctx.online() && !self.loading, egui::Button::new("刷新"))
                .on_disabled_hover_text("请先选择设备")
                .clicked()
                && let Some(command) = ctx.target.clone().map(BackendCommand::LoadProcesses)
            {
                self.last_refresh = Some(Instant::now());
                commands.push(command);
            }
            ui.checkbox(&mut self.auto_refresh, "自动刷新(3 秒)");
            if self.loading {
                ui.spinner();
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("搜索…")
                    .desired_width(180.0),
            );
            ui.label(
                egui::RichText::new(format!("{} 个进程", self.processes.len()))
                    .small()
                    .weak(),
            );
        });
        if let Some(error) = &self.error {
            ui.colored_label(
                crate::theme::palette(ui.visuals().dark_mode).error,
                error.clone(),
            );
        }
        ui.add_space(4.0);
        if !ctx.online() {
            ui.label(egui::RichText::new("请先选择设备。").weak());
            return commands;
        }

        // 过滤 + 排序结果缓存:只在(数据版本, 排序键, 方向, 查询词)变化时重算,
        // 多进程面板不再每帧克隆全表。
        let display_key = (
            self.processes_generation,
            self.sort_key,
            self.sort_descending,
            self.query.clone(),
        );
        if self.display_cache_key.as_ref() != Some(&display_key) {
            let query = self.query.to_lowercase();
            let mut display: Vec<ProcessInfo> = self
                .processes
                .iter()
                .filter(|process| {
                    query.is_empty()
                        || process.command.to_lowercase().contains(&query)
                        || process.pid.to_string().contains(&query)
                })
                .cloned()
                .collect();
            let sort_key = self.sort_key;
            let sort_descending = self.sort_descending;
            display.sort_by(|a, b| {
                let ordering = match sort_key {
                    SortKey::Pid => a.pid.cmp(&b.pid),
                    SortKey::Cpu => b
                        .cpu_percent
                        .unwrap_or(0.0)
                        .partial_cmp(&a.cpu_percent.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal),
                    SortKey::Name => a.command.cmp(&b.command),
                };
                if sort_descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
            self.display_cache = display;
            self.display_cache_key = Some(display_key);
        }
        // 行循环借用缓存;排序状态走局部副本,结束后写回。
        let sorted = &self.display_cache;
        let mut sort_key = self.sort_key;
        let mut sort_descending = self.sort_descending;
        let mut kill_target: Option<ProcessInfo> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("processes-grid")
                    .num_columns(4)
                    .min_col_width(64.0)
                    .striped(true)
                    .spacing([18.0, 4.0])
                    .show(ui, |ui| {
                        let mut header = |ui: &mut Ui, label: &str, key: SortKey| {
                            let marker = if sort_key == key {
                                if sort_descending { " ▾" } else { " ▴" }
                            } else {
                                ""
                            };
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(format!("{label}{marker}"))
                                            .weak()
                                            .small(),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                if sort_key == key {
                                    sort_descending = !sort_descending;
                                } else {
                                    sort_key = key;
                                    sort_descending = false;
                                }
                            }
                        };
                        header(ui, "PID", SortKey::Pid);
                        ui.label(egui::RichText::new("用户").weak().small());
                        header(ui, "CPU%", SortKey::Cpu);
                        header(ui, "命令", SortKey::Name);
                        ui.end_row();

                        for process in sorted {
                            ui.label(egui::RichText::new(process.pid.to_string()).monospace());
                            ui.label(
                                egui::RichText::new(process.user.as_deref().unwrap_or("—")).small(),
                            );
                            let cpu = process
                                .cpu_percent
                                .map(|value| format!("{value:.0}"))
                                .unwrap_or_else(|| "—".to_owned());
                            ui.label(egui::RichText::new(cpu).small().monospace());
                            let name = ui.add(
                                egui::Button::new(egui::RichText::new(&process.command).small())
                                    .frame(false),
                            );
                            name.context_menu(|ui| {
                                if ui.button("结束进程…").clicked() {
                                    kill_target = Some(process.clone());
                                    ui.close();
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        // 写回排序状态;与缓存键不同的话,下一帧会自动重算显示列表。
        self.sort_key = sort_key;
        self.sort_descending = sort_descending;

        if let Some(process) = kill_target {
            self.kill_prompt = Some(process);
        }
        if let Some(process) = self.kill_prompt.clone() {
            let mut close = false;
            egui::Window::new("结束进程确认")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("向 PID {} 发送 kill 信号?", process.pid));
                    ui.label(egui::RichText::new(&process.command).monospace().small());
                    ui.colored_label(
                        crate::theme::palette(ui.visuals().dark_mode).error,
                        "结束系统关键进程可能导致设备重启。",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("结束进程").clicked() {
                            if let Some(target) = ctx.target.clone() {
                                commands.push(BackendCommand::RunShellCommand {
                                    target,
                                    command: format!("kill {}", process.pid),
                                    purpose: format!("结束进程 {}", process.pid),
                                });
                            }
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.kill_prompt = None;
            }
        }
        commands
    }
}
