//! 应用管理:图标网格/列表、详情、启动/停止/清除数据/卸载、HAP 安装。

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
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

/// 应用视图:桌面式图标网格(默认)或紧凑列表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppView {
    #[default]
    Grid,
    List,
}

impl AppView {
    fn key(self) -> &'static str {
        match self {
            AppView::Grid => "grid",
            AppView::List => "list",
        }
    }
}

/// 网格图标尺寸档位。
const GRID_ICON_SIZES: [f32; 4] = [48.0, 64.0, 80.0, 96.0];
/// 默认 64px——桌面网格里最顺眼的一档。
const DEFAULT_ICON_SIZE_IDX: usize = 1;

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
    /// 详情值复制反馈:(字段, 时间);短暂显示「已复制」。
    copied: Option<(&'static str, Instant)>,
    /// 视图偏好:网格或列表,跨会话记忆。
    view: AppView,
    /// 网格图标尺寸在 [`GRID_ICON_SIZES`] 中的档位。
    icon_size_idx: usize,
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
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        let mut state = Self {
            icon_size_idx: DEFAULT_ICON_SIZE_IDX,
            ..Self::default()
        };
        let Some(storage) = storage else {
            return state;
        };
        if let Some(view) = storage.get_string("fhdc.apps_view").as_deref() {
            state.view = if view == "list" {
                AppView::List
            } else {
                AppView::Grid
            };
        }
        if let Some(idx) = storage
            .get_string("fhdc.apps_icon_size")
            .and_then(|value| value.parse::<usize>().ok())
            && idx < GRID_ICON_SIZES.len()
        {
            state.icon_size_idx = idx;
        }
        state
    }

    /// 视图与图标尺寸偏好;app.rs 统一落盘时调用。
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string("fhdc.apps_view", self.view.key().to_owned());
        storage.set_string("fhdc.apps_icon_size", self.icon_size_idx.to_string());
    }

    /// 重建状态时保留视图偏好(hdc 路径切换会整体复位面板)。
    pub fn view_prefs(&self) -> (AppView, usize) {
        (self.view, self.icon_size_idx)
    }

    pub fn set_view_prefs(&mut self, view: AppView, icon_size_idx: usize) {
        self.view = view;
        self.icon_size_idx = icon_size_idx.min(GRID_ICON_SIZES.len() - 1);
    }

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
        self.toolbar(ui, ctx, &mut commands);

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
            ui.label(
                egui::RichText::new(format!("共 {} 个应用", self.visible_bundles().len())).weak(),
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

        // 右:详情与操作(可拖宽);左侧剩余空间全部给应用网格。
        // egui 会把面板宽度持久化到磁盘、且默认无上限:早期版本一旦拖出
        // 全窗宽就会每次启动都恢复巨宽,把网格挤没。这里换全新 id 丢弃
        // 那批污染数据,并设上界,保证网格永远占大头。
        egui::SidePanel::right("apps-detail-v2")
            .resizable(true)
            .default_width(340.0)
            .width_range(260.0..=480.0)
            .frame(egui::Frame::new())
            .show_inside(ui, |ui| {
                self.detail_pane(ui, ctx, &mut commands);
            });

        let visible = self.visible_bundles();
        if self.view == AppView::Grid {
            self.grid_pane(ui, ctx, &mut commands, &visible);
        } else {
            self.list_pane(ui, ctx, &mut commands, &visible);
        }
        self.empty_state(ui, &visible);

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

    /// 顶栏:刷新、安装、搜索;右侧对齐图标缩放与网格/列表切换。
    fn toolbar(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands) {
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
                    .hint_text("搜索应用名或包名…")
                    .desired_width(180.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::selectable(self.view == AppView::List, "列表"))
                    .clicked()
                {
                    self.view = AppView::List;
                }
                if ui
                    .add(egui::Button::selectable(self.view == AppView::Grid, "网格"))
                    .clicked()
                {
                    self.view = AppView::Grid;
                }
                if self.view == AppView::Grid {
                    ui.separator();
                    if ui
                        .add_enabled(
                            self.icon_size_idx + 1 < GRID_ICON_SIZES.len(),
                            egui::Button::new("＋").small(),
                        )
                        .on_disabled_hover_text("已经是最大图标")
                        .clicked()
                    {
                        self.icon_size_idx += 1;
                    }
                    if ui
                        .add_enabled(self.icon_size_idx > 0, egui::Button::new("－").small())
                        .on_disabled_hover_text("已经是最小图标")
                        .clicked()
                    {
                        self.icon_size_idx -= 1;
                    }
                }
            });
        });
    }

    /// 按分类与搜索词过滤后的应用,保持设备返回顺序。
    fn visible_bundles(&self) -> Vec<BundleName> {
        let query = self.query.to_lowercase();
        self.bundles
            .iter()
            .filter(|bundle| self.filter.accepts(bundle))
            .filter(|bundle| {
                // 本地化名称与包名都参与搜索。
                if query.is_empty() {
                    return true;
                }
                let label = self
                    .packages
                    .get(&bundle.name)
                    .and_then(|package| package.label.as_deref());
                bundle.name.as_str().to_lowercase().contains(&query)
                    || label.is_some_and(|label| label.to_lowercase().contains(&query))
            })
            .map(|bundle| bundle.name.clone())
            .collect()
    }

    /// 单击选中并拉取详情;批量阶段跳过大包时,选中即按需拉一次图标。
    fn select_bundle(&mut self, commands: &mut Commands, ctx: &PanelCtx, name: BundleName) {
        if self.selected.as_ref() == Some(&name) {
            return;
        }
        self.selected = Some(name.clone());
        self.details = None;
        let Some(target) = ctx.target.clone() else {
            return;
        };
        commands.push(BackendCommand::LoadBundleDetails(target.clone(), name.clone()));
        if !self.icons_loading
            && !self.icons.contains_key(&name)
            && !self.icon_requests.contains(&name)
            && let Some(package) = self.packages.get(&name).cloned()
        {
            self.icon_requests.insert(name.clone());
            commands.push(BackendCommand::LoadBundleIcon { target, package });
        }
    }

    /// 网格单元与列表行共用的右键菜单。
    fn action_menu(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands, name: &BundleName) {
        ui.label(egui::RichText::new(name.as_str()).small().weak());
        ui.separator();
        if ui.button("复制包名").clicked() {
            ui.ctx().copy_text(name.as_str().to_owned());
            ui.close();
        }
        if ui.button("启动").clicked()
            && let Some(target) = ctx.target.clone()
        {
            commands.push(BackendCommand::RunAppAction {
                target,
                bundle: name.clone(),
                action: AppAction::Launch,
            });
            ui.close();
        }
        if ui.button("强制停止").clicked()
            && let Some(target) = ctx.target.clone()
        {
            commands.push(BackendCommand::RunAppAction {
                target,
                bundle: name.clone(),
                action: AppAction::ForceStop,
            });
            ui.close();
        }
        if ui.button("清除数据").clicked() {
            self.confirm = Some((name.clone(), AppAction::ClearData));
            ui.close();
        }
        if ui.button("卸载").clicked() {
            self.confirm = Some((name.clone(), AppAction::Uninstall));
            ui.close();
        }
    }

    /// AYA 式桌面网格:大图标 + 名称,单击选中、双击启动、右键菜单。
    fn grid_pane(
        &mut self,
        ui: &mut Ui,
        ctx: &PanelCtx,
        commands: &mut Commands,
        visible: &[BundleName],
    ) {
        let icon_size = GRID_ICON_SIZES[self.icon_size_idx];
        let cell_w = icon_size + 32.0;
        let cell_h = icon_size + 46.0;
        let spacing = ui.spacing().item_spacing.x;
        let columns =
            (((ui.available_width() + spacing) / (cell_w + spacing)).floor() as usize).max(1);
        let mut launched: Option<BundleName> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                egui::Grid::new("apps-grid")
                    .num_columns(columns)
                    .min_col_width(cell_w)
                    .max_col_width(cell_w)
                    .spacing([spacing, spacing * 1.4])
                    .show(ui, |ui| {
                        for (idx, name) in visible.iter().enumerate() {
                            if idx > 0 && idx % columns == 0 {
                                ui.end_row();
                            }
                            let icon = self.icons.get(name).and_then(Option::as_ref).cloned();
                            let display_name = self
                                .packages
                                .get(name)
                                .and_then(|package| package.label.clone())
                                .unwrap_or_else(|| name.as_str().to_owned());
                            let is_selected = self.selected.as_ref() == Some(name);
                            let (rect, response) = ui
                                .allocate_exact_size(egui::vec2(cell_w, cell_h), egui::Sense::click());
                            let visuals = ui.visuals().clone();
                            let painter = ui.painter().clone();
                            if is_selected || response.hovered() {
                                let fill = if is_selected {
                                    visuals.selection.bg_fill
                                } else {
                                    visuals.widgets.hovered.bg_fill
                                };
                                painter.rect_filled(rect.expand(2.0), 8.0, fill);
                            }
                            if is_selected {
                                painter.rect_stroke(
                                    rect.expand(2.0),
                                    8.0,
                                    visuals.selection.stroke,
                                    egui::StrokeKind::Inside,
                                );
                            }
                            let icon_rect = egui::Rect::from_center_size(
                                egui::pos2(rect.center().x, rect.top() + 6.0 + icon_size * 0.5),
                                egui::vec2(icon_size, icon_size),
                            );
                            draw_app_icon_at(ui, icon.as_ref(), &display_name, icon_rect, icon_size);
                            let galley = elide_text(
                                &painter,
                                &display_name,
                                egui::FontId::proportional(12.5),
                                visuals.text_color(),
                                cell_w - 8.0,
                            );
                            painter.galley(
                                egui::pos2(
                                    rect.center().x - galley.size().x * 0.5,
                                    rect.top() + icon_size + 12.0,
                                ),
                                galley,
                                visuals.text_color(),
                            );
                            if response.clicked() {
                                self.select_bundle(commands, ctx, name.clone());
                            }
                            if response.double_clicked() {
                                launched = Some(name.clone());
                            }
                            response.context_menu(|menu| {
                                self.action_menu(menu, ctx, commands, name);
                            });
                        }
                    });
            });
        if let Some(name) = launched
            && let Some(target) = ctx.target.clone()
        {
            commands.push(BackendCommand::RunAppAction {
                target,
                bundle: name,
                action: AppAction::Launch,
            });
        }
    }

    /// 紧凑列表(旧布局保留为一览模式)。
    fn list_pane(
        &mut self,
        ui: &mut Ui,
        ctx: &PanelCtx,
        commands: &mut Commands,
        visible: &[BundleName],
    ) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for name in visible {
                    let icon = self.icons.get(name).and_then(Option::as_ref).cloned();
                    let display_name = self
                        .packages
                        .get(name)
                        .and_then(|package| package.label.clone())
                        .unwrap_or_else(|| name.as_str().to_owned());
                    let is_selected = self.selected.as_ref() == Some(name);
                    let response = ui
                        .horizontal(|ui| {
                            draw_app_icon(ui, icon.as_ref(), &display_name, 20.0);
                            ui.add(
                                egui::Button::selectable(is_selected, display_name.clone())
                                    .small(),
                            )
                            .on_hover_text(name.as_str())
                        })
                        .inner;
                    if response.clicked() {
                        self.select_bundle(commands, ctx, name.clone());
                    }
                    response.context_menu(|menu| {
                        self.action_menu(menu, ctx, commands, name);
                    });
                }
            });
    }

    /// 右侧详情:图标、名称、包信息与操作按钮(保留原布局)。
    fn detail_pane(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands) {
        let Some(selected) = self.selected.clone() else {
            ui.label(egui::RichText::new("选择应用查看详情。").weak());
            return;
        };
        let display_name = self
            .packages
            .get(&selected)
            .and_then(|package| package.label.clone());
        // 先取出图标句柄:闭包里还要改 self(记录复制反馈),不能同时借 self.icons。
        let icon = self.icons.get(&selected).and_then(Option::as_ref).cloned();
        ui.horizontal(|ui| {
            draw_app_icon(
                ui,
                icon.as_ref(),
                display_name.as_deref().unwrap_or(selected.as_str()),
                40.0,
            );
            if let Some(label) = &display_name {
                ui.label(egui::RichText::new(label).strong());
            }
        });
        // 包名单独一行、整行可点击复制:它经常是唯一要拿去命令行用的东西。
        // 放在主竖向流里而不是横向布局内——横向行里的按钮点击区会被行高截断。
        let pkg_response = ui
            .add(
                egui::Label::new(egui::RichText::new(selected.as_str()).small().weak())
                    .sense(egui::Sense::click()),
            )
            .on_hover_text("点击复制包名");
        if pkg_response.clicked() {
            ui.ctx().copy_text(selected.as_str().to_owned());
            self.copied = Some(("包名", Instant::now()));
        }
        ui.add_space(4.0);
        if self.details_loading {
            ui.spinner();
        }
        // 先把行数据摘出来,渲染中还要改 self(记录复制反馈)。
        let detail_rows: Vec<(&'static str, String)> = self
            .details
            .as_ref()
            .map(|details| {
                [
                    ("版本", details.version_name.clone()),
                    ("版本号", details.version_code.clone()),
                    ("入口 Ability", details.main_ability.clone()),
                    ("AppID", details.app_id.clone()),
                    ("安装时间", details.install_time.clone()),
                    ("更新时间", details.update_time.clone()),
                ]
                .into_iter()
                .filter_map(|(label, value)| value.map(|value| (label, value)))
                .collect()
            })
            .unwrap_or_default();
        for (label, value) in &detail_rows {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [104.0, 18.0],
                    egui::Label::new(egui::RichText::new(*label).weak()),
                );
                if detail_value(ui, value) {
                    ui.ctx().copy_text(value.clone());
                    self.copied = Some((label, Instant::now()));
                }
            });
            ui.add_space(2.0);
        }
        // 复制反馈单独占一行:长值(AppID)会把整行占满,内联提示会被裁掉。
        if let Some((field, at)) = self.copied
            && at.elapsed().as_millis() < 1200
        {
            ui.label(
                egui::RichText::new(format!("已复制 {field}"))
                    .small()
                    .color(crate::theme::ACCENT),
            );
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        if let Some(system) = self.details.as_ref().and_then(|details| details.system) {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [104.0, 18.0],
                    egui::Label::new(egui::RichText::new("类型").weak()),
                );
                ui.label(if system { "系统应用" } else { "三方应用" });
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
    }

    fn empty_state(&self, ui: &mut Ui, visible: &[BundleName]) {
        if !visible.is_empty() {
            return;
        }
        if self.bundles.is_empty() {
            if !self.loading {
                ui.label(egui::RichText::new("(无应用,点击刷新)").weak());
            }
        } else if self.filter != AppFilter::All
            && !self
                .bundles
                .iter()
                .any(|bundle| self.filter.accepts(bundle))
        {
            // 该分类下确实没有应用,和"还没加载出来"要区分开。
            ui.label(
                egui::RichText::new(format!("(没有{})", self.filter.label())).weak(),
            );
        } else if !self.query.is_empty() {
            ui.label(egui::RichText::new("(没有匹配的应用)").weak());
        }
    }
}

/// 内联应用图标(详情栏/列表行);缺图标时画一个首字母圆形占位,保持行对齐。
fn draw_app_icon(ui: &mut Ui, texture: Option<&egui::TextureHandle>, fallback_text: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    draw_app_icon_at(ui, texture, fallback_text, rect, size);
}

/// 在指定矩形绘制应用图标(网格单元 / 列表行共用);缺图标时退化为首字母圆形。
fn draw_app_icon_at(
    ui: &Ui,
    texture: Option<&egui::TextureHandle>,
    fallback_text: &str,
    rect: egui::Rect,
    size: f32,
) {
    if let Some(texture) = texture {
        egui::Image::new(texture)
            .fit_to_exact_size(egui::vec2(size, size))
            .corner_radius(size * 0.18)
            .paint_at(ui, rect);
        return;
    }
    let painter = ui.painter();
    let visuals = ui.visuals();
    painter.circle_filled(rect.center(), size * 0.48, visuals.widgets.inactive.bg_fill);
    let initial = fallback_text
        .chars()
        .next()
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned());
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(size * 0.52),
        visuals.weak_text_color(),
    );
}

/// 超宽文本按像素宽度截断为「…」结尾的单行 galley。
fn elide_text(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    max_width: f32,
) -> Arc<egui::Galley> {
    let galley = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if galley.size().x <= max_width {
        return galley;
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let candidate: String = chars[..keep].iter().collect::<String>() + "…";
        let galley = painter.layout_no_wrap(candidate, font.clone(), color);
        if galley.size().x <= max_width {
            return galley;
        }
    }
    galley
}

/// 详情栏的值:占满剩余宽度,超长(AppID 这类)截断显示;
/// 点击即复制(与概览面板一致),返回是否刚被点击。
fn detail_value(ui: &mut Ui, value: &str) -> bool {
    let font = egui::FontId::monospace(12.5);
    let color = ui.visuals().text_color();
    let max_width = ui.available_width();
    let galley = elide_text(ui.painter(), value, font, color, max_width);
    let height = galley.size().y.max(18.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(max_width, height), egui::Sense::click());
    ui.painter().galley(rect.min, galley, color);
    response.clicked()
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
    use super::{AppFilter, AppView, AppsState, GRID_ICON_SIZES};
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
        let state = AppsState::new(None);
        assert_eq!(state.view, AppView::Grid);
        assert_eq!(state.icon_size_idx, super::DEFAULT_ICON_SIZE_IDX);
        assert_eq!(GRID_ICON_SIZES[state.icon_size_idx], 64.0);
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
