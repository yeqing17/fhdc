//! 应用管理:包列表、详情、启动/停止/清除数据/卸载、HAP 安装。

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui;
use eframe::egui::Ui;

use fhdc_domain::AppAction;
use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BundleDetails;
use fhdc_domain::BundleName;
use fhdc_domain::BundlePackage;
use fhdc_domain::BundleRecord;
use fhdc_domain::DeviceTarget;

use crate::runtime::app_action_label;

use super::Commands;
use super::PanelCtx;

/// 应用分类:三方应用、系统应用或全部。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppFilter {
    /// 默认聚焦三方应用——系统应用动辄上百个,少说无益。
    #[default]
    ThirdParty,
    System,
    All,
}

impl AppFilter {
    const ALL: [AppFilter; 3] = [AppFilter::ThirdParty, AppFilter::System, AppFilter::All];

    fn label(self) -> &'static str {
        match self {
            AppFilter::ThirdParty => "第三方应用",
            AppFilter::System => "系统应用",
            AppFilter::All => "全部",
        }
    }

    fn accepts(self, bundle: &BundleRecord) -> bool {
        match self {
            AppFilter::ThirdParty => !bundle.system,
            AppFilter::System => bundle.system,
            AppFilter::All => true,
        }
    }
}

#[derive(Default)]
pub struct AppsState {
    target: Option<DeviceTarget>,
    bundles: Vec<BundleRecord>,
    /// 列表分类;默认第三方应用。
    filter: AppFilter,
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
    /// 图标加载:包安装信息(本地化名称 + HAP 路径)。
    packages: HashMap<BundleName, BundlePackage>,
    /// 已解码的图标;None 表示该包确认没有图标(不再重试)。
    icons: HashMap<BundleName, Option<egui::TextureHandle>>,
    /// 运行时事件先入队,渲染线程统一建纹理。
    pending_icons: Vec<(BundleName, Option<Vec<u8>>)>,
    icons_requested: bool,
    icons_loading: bool,
    icons_done: usize,
    icons_total: usize,
    /// 已发起按需加载的包,避免反复请求。
    icon_requests: HashSet<BundleName>,
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
        self.packages.clear();
        self.icons.clear();
        self.pending_icons.clear();
        self.icons_requested = false;
        self.icons_loading = false;
        self.icons_done = 0;
        self.icons_total = 0;
        self.icon_requests.clear();
    }

    /// 供上层在命令发送失败时回收乐观置位的加载状态。
    pub fn reset_loading(&mut self) {
        self.loading = false;
    }

    pub fn maybe_auto_load(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if !ctx.online() {
            return None;
        }
        if self.bundles.is_empty() {
            if self.loading {
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
            return ctx.target.clone().map(BackendCommand::LoadBundles);
        }
        // 包列表就绪后自动开始图标加载(磁盘缓存命中时几乎瞬间完成)。
        if !self.icons_requested {
            self.icons_requested = true;
            self.icons_loading = true;
            self.icons_done = 0;
            return ctx.target.clone().map(BackendCommand::LoadBundleIcons);
        }
        None
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
            BackendEvent::BundlePackagesLoaded(target, packages) => {
                if self.target.as_ref() == Some(target) {
                    self.packages = packages
                        .iter()
                        .cloned()
                        .map(|package| (package.bundle.clone(), package))
                        .collect();
                    self.icons_total = packages.len();
                    self.icons_done = 0;
                    self.icons_loading = true;
                }
            }
            BackendEvent::BundlePackagesFailed(target, _) => {
                if self.target.as_ref() == Some(target) {
                    self.icons_loading = false;
                }
            }
            BackendEvent::BundleIconLoaded(target, bundle, bytes) => {
                if self.target.as_ref() == Some(target) {
                    self.icons_done += 1;
                    self.pending_icons.push((bundle.clone(), bytes.clone()));
                    self.icon_requests.remove(bundle);
                }
            }
            BackendEvent::BundleIconSkipped(target, bundle) => {
                if self.target.as_ref() == Some(target) {
                    self.icons_done += 1;
                    // 不写入 icons:选中该包时会按需再拉一次。
                    self.icon_requests.remove(bundle);
                }
            }
            BackendEvent::BundleIconsFinished(target) => {
                if self.target.as_ref() == Some(target) {
                    self.icons_loading = false;
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
        // 后台事件只带字节;纹理必须在渲染线程创建,这里统一转。
        for (bundle, bytes) in std::mem::take(&mut self.pending_icons) {
            let texture = bytes.and_then(|bytes| decode_icon(ui.ctx(), &bundle, &bytes));
            self.icons.insert(bundle, texture);
        }
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
                self.icons_requested = false;
                self.icons.clear();
                self.pending_icons.clear();
                self.icon_requests.clear();
                self.packages.clear();
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
            } else if self.icons_loading {
                // 图标在后台静默补齐,只在悬停时提示进度,不打扰。
                ui.spinner();
                ui.label(
                    egui::RichText::new(format!("图标 {}/{}", self.icons_done, self.icons_total))
                        .small()
                        .weak(),
                )
                .on_hover_text("正在后台加载应用图标");
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("搜索包名…")
                    .desired_width(180.0),
            );
        });
        // 分类:默认三方应用,系统应用与全部按需切换。
        ui.horizontal(|ui| {
            for filter in AppFilter::ALL {
                let count = self
                    .bundles
                    .iter()
                    .filter(|bundle| filter.accepts(bundle))
                    .count();
                let label = format!("{} {}", filter.label(), count);
                if ui
                    .add(egui::Button::selectable(self.filter == filter, label))
                    .clicked()
                {
                    self.filter = filter;
                }
            }
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
                            // 分类过滤:默认只列三方应用。
                            if !self.filter.accepts(bundle) {
                                continue;
                            }
                            let display_name = self
                                .packages
                                .get(&bundle.name)
                                .and_then(|package| package.label.clone());
                            // 本地化名称与包名都参与搜索。
                            if !query.is_empty()
                                && !bundle.name.as_str().to_lowercase().contains(&query)
                                && !display_name
                                    .as_deref()
                                    .is_some_and(|label| label.to_lowercase().contains(&query))
                            {
                                continue;
                            }
                            let is_selected = self.selected.as_ref() == Some(&bundle.name);
                            // 列表只放图标 + 应用名称;包名在右侧详情里看。
                            ui.horizontal(|ui| {
                                draw_app_icon(
                                    ui,
                                    self.icons.get(&bundle.name).and_then(Option::as_ref),
                                    display_name.as_deref().unwrap_or(bundle.name.as_str()),
                                    20.0,
                                );
                                let title = display_name.as_deref().unwrap_or(bundle.name.as_str());
                                if ui
                                    .add(egui::Button::selectable(is_selected, title))
                                    .clicked()
                                {
                                    clicked = Some(bundle.name.clone());
                                }
                            });
                        }
                        if let Some(name) = clicked {
                            self.selected = Some(name.clone());
                            self.details = None;
                            if let Some(target) = ctx.target.clone() {
                                commands.push(BackendCommand::LoadBundleDetails(
                                    target.clone(),
                                    name.clone(),
                                ));
                                // 批量阶段跳过大包时,选中即按需拉一次。
                                if !self.icons_loading
                                    && !self.icons.contains_key(&name)
                                    && !self.icon_requests.contains(&name)
                                    && let Some(package) = self.packages.get(&name).cloned()
                                {
                                    self.icon_requests.insert(name.clone());
                                    commands
                                        .push(BackendCommand::LoadBundleIcon { target, package });
                                }
                            }
                        }
                        if self.bundles.is_empty() && !self.loading {
                            ui.label(egui::RichText::new("(无应用,点击刷新)").weak());
                        } else if self.filter != AppFilter::All
                            && !self
                                .bundles
                                .iter()
                                .any(|bundle| self.filter.accepts(bundle))
                        {
                            // 该分类下确实没有应用,和"还没加载出来"要区分开。
                            ui.label(
                                egui::RichText::new(format!("(没有{})", self.filter.label()))
                                    .weak(),
                            );
                        }
                    });
            });
            // 右:详情与操作
            columns[1].vertical(|ui| {
                let Some(selected) = self.selected.clone() else {
                    ui.label(egui::RichText::new("选择左侧的应用查看详情。").weak());
                    return;
                };
                let display_name = self
                    .packages
                    .get(&selected)
                    .and_then(|package| package.label.clone());
                ui.horizontal(|ui| {
                    draw_app_icon(
                        ui,
                        self.icons.get(&selected).and_then(Option::as_ref),
                        display_name.as_deref().unwrap_or(selected.as_str()),
                        40.0,
                    );
                    ui.vertical(|ui| {
                        if let Some(label) = &display_name {
                            ui.label(egui::RichText::new(label).strong());
                            ui.label(egui::RichText::new(selected.as_str()).small().weak());
                        } else {
                            ui.label(egui::RichText::new(selected.as_str()).strong());
                        }
                    });
                });
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

/// 列表/详情里的应用图标;没有图标时画一个首字母圆形占位,保持行对齐。
fn draw_app_icon(
    ui: &mut Ui,
    texture: Option<&egui::TextureHandle>,
    fallback_text: &str,
    size: f32,
) {
    if let Some(texture) = texture {
        ui.add(
            egui::Image::new(texture)
                .fit_to_exact_size(egui::vec2(size, size))
                .corner_radius(3.0),
        );
        return;
    }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let visuals = ui.visuals();
    ui.painter()
        .circle_filled(rect.center(), size * 0.48, visuals.widgets.inactive.bg_fill);
    let initial = fallback_text
        .chars()
        .next()
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(size * 0.52),
        visuals.weak_text_color(),
    );
}

/// 把图标字节解码成纹理;超过 128px 的缩到 96px,省显存也够清晰。
fn decode_icon(
    context: &egui::Context,
    bundle: &BundleName,
    bytes: &[u8],
) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > 128 || image.height() > 128 {
        image.thumbnail(96, 96)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(context.load_texture(
        format!("app-icon-{}", bundle.as_str()),
        color,
        egui::TextureOptions::LINEAR,
    ))
}

#[cfg(test)]
mod tests {
    use super::{AppFilter, AppsState};
    use fhdc_domain::BundleName;
    use fhdc_domain::BundleRecord;

    fn record(name: &str, system: bool) -> BundleRecord {
        BundleRecord {
            name: BundleName::new(name).expect("valid bundle name"),
            system,
        }
    }

    #[test]
    fn default_filter_is_third_party() {
        assert_eq!(AppFilter::default(), AppFilter::ThirdParty);
        let state = AppsState::default();
        assert_eq!(state.filter, AppFilter::ThirdParty);
    }

    #[test]
    fn filter_selects_expected_subsets() {
        let third = record("com.example.demo", false);
        let system = record("com.ohos.launcher", true);
        assert!(AppFilter::ThirdParty.accepts(&third));
        assert!(!AppFilter::ThirdParty.accepts(&system));
        assert!(AppFilter::System.accepts(&system));
        assert!(!AppFilter::System.accepts(&third));
        assert!(AppFilter::All.accepts(&third));
        assert!(AppFilter::All.accepts(&system));
    }
}
