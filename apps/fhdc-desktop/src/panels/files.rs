//! 文件管理:浏览远端目录、上传/下载、新建/重命名/删除,破坏性操作需确认。

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BridgeError;
use fhdc_domain::DeviceTarget;
use fhdc_domain::FileTransferDirection;
use fhdc_domain::OperationId;
use fhdc_domain::OverwritePolicy;
use fhdc_domain::RemoteFileEntry;
use fhdc_domain::RemoteFileKind;
use fhdc_domain::RemotePath;

use crate::theme;

use super::Commands;
use super::PanelCtx;
use super::overview::format_kib;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
}

struct ActiveTransfer {
    operation: OperationId,
    label: String,
}

#[derive(Default)]
pub struct FilesState {
    target: Option<DeviceTarget>,
    path: Option<RemotePath>,
    entries: Vec<RemoteFileEntry>,
    loading: bool,
    error: Option<BridgeError>,
    sort_key: SortKey,
    sort_descending: bool,
    /// 排序结果缓存:只在(列表版本, 排序键, 方向)变化时重算。
    sorted_cache: Vec<RemoteFileEntry>,
    sort_cache_key: Option<(u64, SortKey, bool)>,
    entries_generation: u64,
    transfers: Vec<ActiveTransfer>,
    /// 进行中的上传,用于"已存在→替换?"重试。
    active_uploads: HashMap<OperationId, (PathBuf, RemotePath)>,
    /// 上传遇到"已存在"后的替换确认。
    overwrite_prompt: Option<(PathBuf, RemotePath)>,
    delete_prompt: Option<RemoteFileEntry>,
    rename_prompt: Option<(RemoteFileEntry, String)>,
    mkdir_prompt: Option<String>,
    /// 拖拽进窗口的上传队列(文件面板激活时)。
    drop_queue: Vec<(PathBuf, RemotePath)>,
}

impl FilesState {
    /// 让列表与排序缓存一起失效。所有改动 `entries` 的路径都必须走这里,
    /// 否则新数据到达前会继续画上一份列表。
    fn invalidate_entries(&mut self) {
        self.entries.clear();
        self.sorted_cache.clear();
        self.sort_cache_key = None;
        self.entries_generation += 1;
    }

    pub fn reset_for(&mut self, target: Option<DeviceTarget>) {
        if self.target.as_ref() == target.as_ref() {
            return;
        }
        self.target = target;
        self.path = None;
        self.invalidate_entries();
        self.error = None;
        self.loading = false;
        self.transfers.clear();
        self.drop_queue.clear();
        self.overwrite_prompt = None;
        self.delete_prompt = None;
        self.rename_prompt = None;
        self.mkdir_prompt = None;
    }

    /// 首次进入或设备就绪后自动加载根下常用目录。
    pub fn maybe_auto_load(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if !ctx.online() || self.path.is_some() || self.loading {
            return None;
        }
        let target = ctx.target.clone()?;
        let path = RemotePath::new("/data/local/tmp").unwrap_or_else(|_| RemotePath::root());
        self.path = Some(path.clone());
        self.loading = true;
        Some(BackendCommand::ListDirectory { target, path })
    }

    pub fn queue_dropped_files(&mut self, paths: Vec<PathBuf>) {
        let Some(directory) = self.path.clone() else {
            return;
        };
        for local in paths {
            if let Some(name) = local.file_name().and_then(|n| n.to_str())
                && let Ok(remote) = directory.join_name(name)
            {
                self.drop_queue.push((local, remote));
            }
        }
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::DirectoryLoading { target, path } => {
                if self.target.as_ref() == Some(target) && self.path.as_ref() == Some(path) {
                    self.loading = true;
                }
            }
            BackendEvent::DirectoryLoaded {
                target,
                path,
                entries,
            } => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = None;
                    if self.path.as_ref() == Some(path) {
                        self.entries = entries.clone();
                        self.entries_generation += 1;
                    }
                }
            }
            BackendEvent::DirectoryFailed {
                target,
                path,
                error,
            } => {
                if self.target.as_ref() == Some(target) && self.path.as_ref() == Some(path) {
                    self.loading = false;
                    self.error = Some(error.clone());
                }
            }
            BackendEvent::FileTransferStarted {
                operation,
                direction,
                remote,
                local,
            } => {
                self.transfers
                    .retain(|transfer| transfer.operation != *operation);
                if *direction == FileTransferDirection::Upload {
                    self.active_uploads
                        .insert(*operation, (local.clone(), remote.clone()));
                }
                self.transfers.push(ActiveTransfer {
                    operation: *operation,
                    label: match direction {
                        FileTransferDirection::Upload => {
                            format!("上传 {}", remote.file_name().unwrap_or(""))
                        }
                        FileTransferDirection::Download => format!("下载 → {}", local.display()),
                    },
                });
            }
            BackendEvent::FileTransferFinished { operation }
            | BackendEvent::FileTransferCancelled { operation } => {
                self.transfers
                    .retain(|transfer| transfer.operation != *operation);
                self.active_uploads.remove(operation);
            }
            BackendEvent::FileTransferFailed { operation, error } => {
                self.transfers
                    .retain(|transfer| transfer.operation != *operation);
                if error.code == fhdc_domain::ErrorCode::AlreadyExists {
                    if let Some((local, remote)) = self.active_uploads.remove(operation) {
                        self.overwrite_prompt = Some((local, remote));
                    } else {
                        self.error = Some(error.clone());
                    }
                } else {
                    self.error = Some(error.clone());
                }
            }
            BackendEvent::FileMutationCompleted { target, .. } => {
                if self.target.as_ref() == Some(target) {
                    self.reload(&self.path.clone());
                }
            }
            BackendEvent::FileMutationFailed { target, error, .. } => {
                if self.target.as_ref() == Some(target) {
                    self.error = Some(error.clone());
                }
            }
            _ => {}
        }
    }

    fn reload(&mut self, path: &Option<RemotePath>) -> Option<BackendCommand> {
        let target = self.target.clone()?;
        let path = path.clone()?;
        self.loading = true;
        Some(BackendCommand::ListDirectory { target, path })
    }

    fn navigate(&mut self, path: RemotePath) -> Option<BackendCommand> {
        self.path = Some(path.clone());
        // 切目录立即失效列表缓存,新数据到达前不再显示上一个目录的行。
        self.invalidate_entries();
        self.reload(&Some(path))
    }

    pub fn show(&mut self, ui: &mut Ui, ctx: &PanelCtx) -> Commands {
        let mut commands = Vec::new();
        ui.heading("文件");
        ui.add_space(4.0);
        if !ctx.online() {
            ui.label(egui::RichText::new("请先选择设备。").weak());
            return commands;
        }
        let Some(current) = self.path.clone() else {
            if let Some(command) = self.maybe_auto_load(ctx) {
                commands.push(command);
            }
            return commands;
        };

        // 工具栏 + 面包屑
        ui.horizontal_wrapped(|ui| {
            if ui.button("⟲ 刷新").clicked()
                && let Some(command) = self.reload(&Some(current.clone()))
            {
                commands.push(command);
            }
            if ui.button("上级").clicked()
                && let Some(parent) = current.parent()
                && let Some(command) = self.navigate(parent)
            {
                commands.push(command);
            }
            if ui.button("新建文件夹").clicked() {
                self.mkdir_prompt = Some(String::new());
            }
            if ui.button("上传…").clicked()
                && let Some(local) = rfd::FileDialog::new().pick_file()
                && let Some(name) = local.file_name().and_then(|n| n.to_str())
                && let Ok(remote) = current.join_name(name)
            {
                commands.push(BackendCommand::UploadFile {
                    target: ctx.target.clone().expect("checked online"),
                    local,
                    remote,
                    overwrite: OverwritePolicy::Deny,
                    operation: OperationId::new(),
                });
            }
            ui.separator();
            breadcrumb(ui, &current, &mut |path| {
                if let Some(command) = self.navigate(path) {
                    commands.push(command);
                }
            });
            if self.loading {
                ui.spinner();
            }
        });
        ui.add_space(4.0);
        if let Some(error) = &self.error {
            ui.colored_label(
                theme::palette(ui.visuals().dark_mode).error,
                format!("{} — {}", error_code_text(error.code), error.detail),
            );
            if ui.small_button("关闭").clicked() {
                self.error = None;
            }
        }

        // 传输状态行
        if !self.transfers.is_empty() {
            ui.horizontal(|ui| {
                for transfer in &self.transfers {
                    ui.label(egui::RichText::new(format!("⇅ {}…", transfer.label)).small());
                }
            });
        }

        // 列表:排序结果缓存,大目录不再每帧克隆重排。
        let sort_key_tuple = (self.entries_generation, self.sort_key, self.sort_descending);
        if self.sort_cache_key != Some(sort_key_tuple) {
            let mut sorted = self.entries.clone();
            sort_entries(&mut sorted, self.sort_key, self.sort_descending);
            self.sorted_cache = sorted;
            self.sort_cache_key = Some(sort_key_tuple);
        }
        // 行循环直接借用缓存;排序状态用局部副本驱动表头,
        // 结束后一次性写回,避免闭包交叉借用 self。
        let sorted = &self.sorted_cache;
        let mut sort_key = self.sort_key;
        let mut sort_descending = self.sort_descending;
        let mut enter: Option<RemotePath> = None;
        let mut download: Option<RemoteFileEntry> = None;
        let mut delete: Option<RemoteFileEntry> = None;
        let mut rename: Option<RemoteFileEntry> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("files-grid")
                    .num_columns(4)
                    .min_col_width(72.0)
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
                        header(ui, "名称", SortKey::Name);
                        // 类型列不支持排序,普通弱化文本即可(不该有可点的假象)。
                        ui.label(egui::RichText::new("类型").weak().small());
                        header(ui, "大小", SortKey::Size);
                        header(ui, "修改时间", SortKey::Modified);
                        ui.end_row();

                        for entry in sorted {
                            let icon = match entry.kind {
                                RemoteFileKind::Directory => "📁",
                                RemoteFileKind::File => "📄",
                                RemoteFileKind::Symlink => "↪",
                                RemoteFileKind::Other => "•",
                            };
                            // 图标与名称合入同一单元格,保证列数与表头一致。
                            let name_text = if entry.kind == RemoteFileKind::Directory {
                                egui::RichText::new(format!("{icon} {}", entry.name)).strong()
                            } else {
                                egui::RichText::new(format!("{icon} {}", entry.name))
                            };
                            let name_response = ui
                                .add(egui::Button::new(name_text).frame(false))
                                .on_hover_text("双击进入/下载;右键更多操作");
                            let kind_text = match entry.kind {
                                RemoteFileKind::Directory => "目录",
                                RemoteFileKind::File => "文件",
                                RemoteFileKind::Symlink => "链接",
                                RemoteFileKind::Other => "其它",
                            };
                            ui.label(egui::RichText::new(kind_text).small().weak());
                            ui.label(
                                egui::RichText::new(
                                    entry.size_bytes.map(format_kib).unwrap_or_default(),
                                )
                                .small()
                                .monospace(),
                            );
                            ui.label(
                                egui::RichText::new(
                                    entry
                                        .modified_unix_seconds
                                        .map(format_mtime)
                                        .unwrap_or_default(),
                                )
                                .small()
                                .weak(),
                            );
                            if name_response.double_clicked() {
                                match entry.kind {
                                    RemoteFileKind::Directory => enter = Some(entry.path.clone()),
                                    _ => download = Some(entry.clone()),
                                }
                            }
                            name_response.context_menu(|ui| {
                                if entry.kind == RemoteFileKind::Directory
                                    && ui.button("打开").clicked()
                                {
                                    enter = Some(entry.path.clone());
                                    ui.close();
                                }
                                if ui.button("下载…").clicked() {
                                    download = Some(entry.clone());
                                    ui.close();
                                }
                                if ui.button("重命名…").clicked() {
                                    rename = Some(entry.clone());
                                    ui.close();
                                }
                                if ui.button("删除…").clicked() {
                                    delete = Some(entry.clone());
                                    ui.close();
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        // 写回排序状态;与缓存键不同的话,下一帧会自动重算排序列表。
        // (此后不再借用 sorted,避免与可变写回冲突。)
        self.sort_key = sort_key;
        self.sort_descending = sort_descending;
        if self.sorted_cache.is_empty() && !self.loading {
            ui.label(egui::RichText::new("(空目录)").weak());
        }

        if let Some(path) = enter
            && let Some(command) = self.navigate(path)
        {
            commands.push(command);
        }
        if let Some(entry) = download
            && let Some(name) = entry.path.file_name()
            && let Some(local) = rfd::FileDialog::new().set_file_name(name).save_file()
        {
            commands.push(BackendCommand::DownloadFile {
                target: ctx.target.clone().expect("checked online"),
                remote: entry.path.clone(),
                local,
                overwrite: OverwritePolicy::ReplaceConfirmed,
                operation: OperationId::new(),
            });
        }
        if let Some(entry) = delete {
            self.delete_prompt = Some(entry);
        }
        if let Some(entry) = rename {
            self.rename_prompt = Some((entry, String::new()));
        }

        // 拖拽上传确认
        if !self.drop_queue.is_empty() {
            let names: Vec<String> = self
                .drop_queue
                .iter()
                .map(|(local, _)| {
                    local
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_owned()
                })
                .collect();
            egui::Window::new("上传确认")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "上传 {} 个文件到 {}?",
                        names.len(),
                        current.as_str()
                    ));
                    for name in &names {
                        ui.label(egui::RichText::new(format!("· {name}")).small().monospace());
                    }
                    ui.horizontal(|ui| {
                        if ui.button("上传").clicked() {
                            let queue = std::mem::take(&mut self.drop_queue);
                            for (local, remote) in queue {
                                commands.push(BackendCommand::UploadFile {
                                    target: ctx.target.clone().expect("checked online"),
                                    local,
                                    remote,
                                    overwrite: OverwritePolicy::Deny,
                                    operation: OperationId::new(),
                                });
                            }
                        }
                        if ui.button("取消").clicked() {
                            self.drop_queue.clear();
                        }
                    });
                });
        }

        self.show_delete_prompt(ui, ctx, &mut commands);
        self.show_rename_prompt(ui, ctx, &mut commands);
        self.show_mkdir_prompt(ui, ctx, &mut commands);
        self.show_overwrite_prompt(ui, ctx, &mut commands);
        commands
    }

    fn show_overwrite_prompt(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands) {
        let Some((local, remote)) = self.overwrite_prompt.clone() else {
            return;
        };
        let mut close = false;
        egui::Window::new("覆盖确认")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("远端已存在 {}", remote.as_str()));
                ui.label("要覆盖上传吗?");
                ui.horizontal(|ui| {
                    if ui.button("覆盖上传").clicked() {
                        if let Some(target) = ctx.target.clone() {
                            commands.push(BackendCommand::UploadFile {
                                target,
                                local,
                                remote,
                                overwrite: OverwritePolicy::ReplaceConfirmed,
                                operation: OperationId::new(),
                            });
                        }
                        close = true;
                    }
                    if ui.button("跳过").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.overwrite_prompt = None;
        }
    }

    fn show_delete_prompt(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands) {
        let Some(entry) = self.delete_prompt.clone() else {
            return;
        };
        let mut close = false;
        egui::Window::new("删除确认")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("删除 {} ?", entry.path.as_str()));
                if matches!(entry.kind, RemoteFileKind::Directory) {
                    ui.colored_label(
                        theme::palette(ui.visuals().dark_mode).error,
                        "目录将递归删除,不可恢复。",
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("删除").clicked() {
                        if let Some(target) = ctx.target.clone() {
                            commands.push(BackendCommand::DeleteRemoteEntry {
                                target,
                                path: entry.path.clone(),
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
            self.delete_prompt = None;
        }
    }

    fn show_rename_prompt(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands) {
        let Some((entry, mut name)) = self.rename_prompt.clone() else {
            return;
        };
        let mut close = false;
        egui::Window::new("重命名")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("重命名 {}", entry.name));
                ui.add(egui::TextEdit::singleline(&mut name).desired_width(240.0));
                let valid =
                    !name.trim().is_empty() && !name.contains('/') && name != "." && name != "..";
                ui.horizontal(|ui| {
                    if ui.add_enabled(valid, egui::Button::new("重命名")).clicked() {
                        if let (Some(target), Some(parent)) =
                            (ctx.target.clone(), entry.path.parent())
                            && let Ok(to) = parent.join_name(name.trim())
                        {
                            commands.push(BackendCommand::RenameRemoteEntry {
                                target,
                                from: entry.path.clone(),
                                to,
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
            self.rename_prompt = None;
        } else {
            self.rename_prompt = Some((entry, name));
        }
    }

    fn show_mkdir_prompt(&mut self, ui: &mut Ui, ctx: &PanelCtx, commands: &mut Commands) {
        let Some(mut name) = self.mkdir_prompt.clone() else {
            return;
        };
        let mut close = false;
        let Some(current) = self.path.clone() else {
            return;
        };
        egui::Window::new("新建文件夹")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("在 {} 下创建:", current.as_str()));
                ui.add(egui::TextEdit::singleline(&mut name).desired_width(240.0));
                let valid = !name.trim().is_empty() && !name.trim().contains('/');
                ui.horizontal(|ui| {
                    if ui.add_enabled(valid, egui::Button::new("创建")).clicked() {
                        if let (Some(target), Ok(path)) =
                            (ctx.target.clone(), current.join_name(name.trim()))
                        {
                            commands.push(BackendCommand::CreateDirectory { target, path });
                        }
                        close = true;
                    }
                    if ui.button("取消").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.mkdir_prompt = None;
        } else {
            self.mkdir_prompt = Some(name);
        }
    }
}

fn breadcrumb(ui: &mut Ui, path: &RemotePath, navigate: &mut dyn FnMut(RemotePath)) {
    let text = path.as_str().trim_start_matches('/');
    if text.is_empty() {
        return;
    }
    let mut accumulated = String::new();
    for segment in text.split('/') {
        accumulated.push('/');
        accumulated.push_str(segment);
        if ui.small_button(segment).clicked()
            && let Ok(target) = RemotePath::new(&accumulated)
        {
            navigate(target);
        }
    }
}

fn sort_entries(entries: &mut [RemoteFileEntry], key: SortKey, descending: bool) {
    entries.sort_by(|a, b| {
        let directories = match (a.kind, b.kind) {
            (RemoteFileKind::Directory, RemoteFileKind::Directory) => std::cmp::Ordering::Equal,
            (RemoteFileKind::Directory, _) => return std::cmp::Ordering::Less,
            (_, RemoteFileKind::Directory) => return std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        };
        let ordering = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.size_bytes.unwrap_or(0).cmp(&b.size_bytes.unwrap_or(0)),
            SortKey::Modified => a
                .modified_unix_seconds
                .unwrap_or(0)
                .cmp(&b.modified_unix_seconds.unwrap_or(0)),
        };
        let ordering = if descending {
            ordering.reverse()
        } else {
            ordering
        };
        directories.then(ordering).then(a.name.cmp(&b.name))
    });
}

/// epoch 秒转本地可读时间(无时区库,展示 UTC + 秒数)。
fn format_mtime(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        time_of_day / 3600,
        (time_of_day % 3600) / 60
    )
}

/// Howard Hinnant 的 days→civil 算法,避免引入 chrono。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 错误码转可读文本(供错误条展示)。
fn error_code_text(code: fhdc_domain::ErrorCode) -> &'static str {
    match code {
        fhdc_domain::ErrorCode::HdcNotFound => "未找到 hdc",
        fhdc_domain::ErrorCode::HdcFailed => "hdc 错误",
        fhdc_domain::ErrorCode::DeviceNotFound => "设备不存在",
        fhdc_domain::ErrorCode::DeviceUnavailable => "设备不可用",
        fhdc_domain::ErrorCode::InvalidInput => "输入非法",
        fhdc_domain::ErrorCode::TimedOut => "超时",
        fhdc_domain::ErrorCode::OutputLimit => "输出超限",
        fhdc_domain::ErrorCode::PathNotFound => "路径不存在",
        fhdc_domain::ErrorCode::AlreadyExists => "已存在",
        fhdc_domain::ErrorCode::PermissionDenied => "权限不足",
        fhdc_domain::ErrorCode::Cancelled => "已取消",
        fhdc_domain::ErrorCode::Internal => "内部错误",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtime_formatting() {
        assert_eq!(format_mtime(0), "1970-01-01 00:00 UTC");
        // 1_728_800_000 = 20009 天 + 22400 秒 → 2024-10-13 06:13:20 UTC
        assert_eq!(format_mtime(1_728_800_000), "2024-10-13 06:13 UTC");
    }

    #[test]
    fn civil_algorithm_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
