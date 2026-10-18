//! 应用管理:包列表、详情、启动/停止/清除数据/卸载、HAP 安装。

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;
use eframe::egui::Ui;

use fhdc_domain::AppAction;
use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BundleDetails;
use fhdc_domain::BundleName;
use fhdc_domain::BundleRecord;
use fhdc_domain::DeviceTarget;

use crate::runtime::app_action_label;

use super::Commands;
use super::PanelCtx;

#[derive(Default)]
pub struct AppsState {
    target: Option<DeviceTarget>,
    bundles: Vec<BundleRecord>,
    loading: bool,
    error: Option<String>,
    query: String,
    selected: Option<BundleName>,
    details: Option<BundleDetails>,
    details_loading: bool,
    confirm: Option<(BundleName, AppAction)>,
    installing: Option<String>,
    install_note: Option<String>,
    last_load_attempt: Option<Instant>,
    /// 供 app.rs 投递的拖拽安装队列。
    pub pending_install: Option<PathBuf>,
}

impl AppsState {
    pub fn reset_for(&mut self, target: Option<DeviceTarget>) {
        if self.target.as_ref() == target.as_ref() {
            return;
        }
        self.target = target;
        self.bundles.clear();
        self.error = None;
        self.selected = None;
        self.details = None;
        self.confirm = None;
        self.install_note = None;
        self.last_load_attempt = None;
        // 就地复位加载态,旧 target 的结果事件不再匹配。
        self.loading = false;
    }

    pub fn maybe_auto_load(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if !ctx.online() || self.loading || !self.bundles.is_empty() {
            return None;
        }
        let now = Instant::now();
        if let Some(last) = self.last_load_attempt
            && now.duration_since(last).as_secs() < 5
        {
            return None;
        }
        self.last_load_attempt = Some(now);
        self.loading = true;
        ctx.target.clone().map(BackendCommand::LoadBundles)
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::BundlesLoading(target) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = true;
                }
            }
            BackendEvent::BundlesLoaded(target, bundles) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = None;
                    self.bundles = bundles.clone();
                    if let Some(selected) = &self.selected
                        && !bundles.iter().any(|bundle| bundle.name == *selected)
                    {
                        self.selected = None;
                        self.details = None;
                    }
                }
            }
            BackendEvent::BundlesFailed(target, error) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = Some(error.detail.clone());
                }
            }
            BackendEvent::BundleDetailsLoading(target, bundle) => {
                // 目标与所选包双重校验:切设备后旧结果不落到新页面。
                if self.target.as_ref() == Some(target) && self.selected.as_ref() == Some(bundle) {
                    self.details_loading = true;
                }
            }
            BackendEvent::BundleDetailsLoaded(target, details) => {
                if self.target.as_ref() == Some(target)
                    && self.selected.as_ref() == Some(&details.name)
                {
                    self.details_loading = false;
                    self.details = Some(details.clone());
                }
            }
            BackendEvent::BundleDetailsFailed(target, bundle, error) => {
                if self.target.as_ref() == Some(target) && self.selected.as_ref() == Some(bundle) {
                    self.details_loading = false;
                    self.error = Some(error.detail.clone());
                }
            }
            BackendEvent::AppActionCompleted(target, bundle, action, message) => {
                if self.target.as_ref() != Some(target) {
                    return;
                }
                self.install_note = Some(format!(
                    "{} {}: {}",
                    app_action_label(*action),
                    bundle.as_str(),
                    message
                ));
                if *action == AppAction::Uninstall {
                    self.bundles.retain(|record| record.name != *bundle);
                }
            }
            BackendEvent::AppActionFailed(target, _, action, error) => {
                if self.target.as_ref() != Some(target) {
                    return;
                }
                self.error = Some(format!(
                    "{}失败: {}",
                    app_action_label(*action),
                    error.detail
                ));
            }
            BackendEvent::HapInstallStarted(target, label) => {
                if self.target.as_ref() != Some(target) {
                    return;
                }
                self.installing = Some(label.clone());
                self.install_note = None;
            }
            BackendEvent::HapInstallFinished(target, message) => {
                if self.target.as_ref() != Some(target) {
                    return;
                }
                self.installing = None;
                self.install_note = Some(format!("安装完成: {message}"));
            }
            BackendEvent::HapInstallFailed(target, error) => {
                if self.target.as_ref() != Some(target) {
                    return;
                }
                self.installing = None;
                self.error = Some(format!("安装失败: {}", error.detail));
            }
            _ => {}
        }
    }

    pub fn show(&mut self, ui: &mut Ui, ctx: &PanelCtx) -> Commands {
        let mut commands = Vec::new();
        ui.heading("应用");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ctx.online() && !self.loading, egui::Button::new("刷新"))
                .on_disabled_hover_text("请先选择设备")
                .clicked()
                && let Some(target) = ctx.target.clone()
            {
                self.loading = true;
                commands.push(BackendCommand::LoadBundles(target));
            }
            if ui.button("安装 HAP…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("鸿蒙应用包", &["hap", "app"])
                    .pick_file()
            {
                self.pending_install = Some(path);
            }
            if self.loading {
                ui.spinner();
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("搜索包名…")
                    .desired_width(180.0),
            );
        });
        if let Some(note) = &self.install_note {
            ui.label(
                egui::RichText::new(note)
                    .small()
                    .color(crate::theme::ACCENT),
            );
        }
        if let Some(error) = &self.error {
            ui.colored_label(
                crate::theme::palette(ui.visuals().dark_mode).error,
                error.clone(),
            );
        }
        if let Some(label) = &self.installing {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("正在安装 {label}…"));
            });
        }
        ui.add_space(4.0);

        if !ctx.online() {
            ui.label(egui::RichText::new("请先选择设备。").weak());
            return commands;
        }

        ui.columns(2, |columns| {
            // 左:包列表
            columns[0].vertical(|ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height(ui.available_height())
                    .show(ui, |ui| {
                        let query = self.query.to_lowercase();
                        let mut clicked: Option<BundleName> = None;
                        for bundle in &self.bundles {
                            if !query.is_empty()
                                && !bundle.name.as_str().to_lowercase().contains(&query)
                            {
                                continue;
                            }
                            let is_selected = self.selected.as_ref() == Some(&bundle.name);
                            let label = if bundle.system {
                                format!("⚙ {}", bundle.name.as_str())
                            } else {
                                bundle.name.as_str().to_owned()
                            };
                            if ui
                                .add(egui::Button::selectable(is_selected, label))
                                .clicked()
                            {
                                clicked = Some(bundle.name.clone());
                            }
                        }
                        if let Some(name) = clicked {
                            self.selected = Some(name.clone());
                            self.details = None;
                            if let Some(target) = ctx.target.clone() {
                                commands.push(BackendCommand::LoadBundleDetails(target, name));
                            }
                        }
                        if self.bundles.is_empty() && !self.loading {
                            ui.label(egui::RichText::new("(无应用,点击刷新)").weak());
                        }
                    });
            });
            // 右:详情与操作
            columns[1].vertical(|ui| {
                let Some(selected) = self.selected.clone() else {
                    ui.label(egui::RichText::new("选择左侧的应用查看详情。").weak());
                    return;
                };
                ui.label(egui::RichText::new(selected.as_str()).strong());
                ui.add_space(4.0);
                if self.details_loading {
                    ui.spinner();
                }
                if let Some(details) = &self.details {
                    egui::Grid::new("bundle-details")
                        .num_columns(2)
                        .min_col_width(84.0)
                        .spacing([18.0, 5.0])
                        .show(ui, |ui| {
                            for (label, value) in [
                                ("版本", details.version_name.as_deref()),
                                ("版本号", details.version_code.as_deref()),
                                ("入口 Ability", details.main_ability.as_deref()),
                                ("AppID", details.app_id.as_deref()),
                                ("安装时间", details.install_time.as_deref()),
                                ("更新时间", details.update_time.as_deref()),
                            ] {
                                if let Some(value) = value {
                                    ui.label(egui::RichText::new(label).weak());
                                    ui.label(egui::RichText::new(value).monospace().small());
                                    ui.end_row();
                                }
                            }
                            if let Some(system) = details.system {
                                ui.label(egui::RichText::new("类型").weak());
                                ui.label(if system {
                                    "系统应用"
                                } else {
                                    "三方应用"
                                });
                                ui.end_row();
                            }
                        });
                }
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("启动").clicked()
                        && let Some(target) = ctx.target.clone()
                    {
                        commands.push(BackendCommand::RunAppAction {
                            target,
                            bundle: selected.clone(),
                            action: AppAction::Launch,
                        });
                    }
                    if ui.button("强制停止").clicked()
                        && let Some(target) = ctx.target.clone()
                    {
                        commands.push(BackendCommand::RunAppAction {
                            target,
                            bundle: selected.clone(),
                            action: AppAction::ForceStop,
                        });
                    }
                    if ui.button("清除数据").clicked() {
                        self.confirm = Some((selected.clone(), AppAction::ClearData));
                    }
                    if ui.button("卸载").clicked() {
                        self.confirm = Some((selected.clone(), AppAction::Uninstall));
                    }
                });
            });
        });

        // 安装确认(按钮选择或拖拽投递)
        if let Some(path) = self.pending_install.clone() {
            let mut close = false;
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("hap")
                .to_owned();
            egui::Window::new("安装确认")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("安装 {} 到设备?", file_name));
                    ui.label(
                        egui::RichText::new(path.display().to_string())
                            .small()
                            .monospace()
                            .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(ctx.online(), egui::Button::new("安装"))
                            .clicked()
                        {
                            if let Some(target) = ctx.target.clone() {
                                commands.push(BackendCommand::InstallHap(target, path.clone()));
                            }
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.pending_install = None;
            }
        }

        // 破坏性操作确认
        if let Some((bundle, action)) = self.confirm.clone() {
            let mut close = false;
            egui::Window::new(format!("{}确认", app_action_label(action)))
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "确定要对 {} 执行「{}」吗?",
                        bundle.as_str(),
                        app_action_label(action)
                    ));
                    if action == AppAction::Uninstall {
                        ui.colored_label(
                            crate::theme::palette(ui.visuals().dark_mode).error,
                            "卸载后应用数据一并删除,系统应用可能失败。",
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui.button(app_action_label(action)).clicked() {
                            if let Some(target) = ctx.target.clone() {
                                commands.push(BackendCommand::RunAppAction {
                                    target,
                                    bundle: bundle.clone(),
                                    action,
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
                self.confirm = None;
            }
        }
        commands
    }
}
