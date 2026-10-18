//! 截图:拍照(snapshot_display)、适应/原始显示、保存与复制。

use eframe::egui;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::DeviceTarget;

use super::Commands;
use super::PanelCtx;

pub struct ScreenshotState {
    target: Option<DeviceTarget>,
    loading: bool,
    error: Option<String>,
    jpeg: Option<Vec<u8>>,
    texture: Option<(egui::TextureHandle, egui::Vec2)>,
    fit: bool,
    texture_stamp: u64,
}

impl Default for ScreenshotState {
    fn default() -> Self {
        Self {
            target: None,
            loading: false,
            error: None,
            jpeg: None,
            texture: None,
            fit: true,
            texture_stamp: 0,
        }
    }
}

impl ScreenshotState {
    pub fn reset_for(&mut self, target: Option<DeviceTarget>) {
        if self.target.as_ref() == target.as_ref() {
            return;
        }
        self.target = target;
        self.jpeg = None;
        self.texture = None;
        self.error = None;
        self.loading = false;
    }

    /// 供上层在命令发送失败时回收乐观置位的加载状态。
    pub fn reset_loading(&mut self) {
        self.loading = false;
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::ScreenshotLoading(target) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = true;
                }
            }
            BackendEvent::ScreenshotCaptured(target, jpeg) => {
                if self.target.as_ref() == Some(target) {
                    self.loading = false;
                    self.error = None;
                    self.jpeg = Some(jpeg.clone());
                    self.texture = None;
                }
            }
            BackendEvent::ScreenshotFailed(target, error) => {
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
        ui.heading("截图");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ctx.online() && !self.loading, egui::Button::new("拍照"))
                .on_disabled_hover_text("请先选择设备")
                .clicked()
                && let Some(target) = ctx.target.clone()
            {
                self.loading = true;
                self.error = None;
                commands.push(BackendCommand::CaptureScreenshot(target));
            }
            if ui
                .add_enabled(self.jpeg.is_some(), egui::Button::new("保存…"))
                .clicked()
                && let Some(jpeg) = &self.jpeg
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name("fhdc-screenshot.jpeg")
                    .add_filter("JPEG", &["jpeg", "jpg"])
                    .save_file()
                && let Err(error) = std::fs::write(&path, jpeg)
            {
                self.error = Some(format!("保存失败: {error}"));
            }
            if ui
                .add_enabled(self.jpeg.is_some(), egui::Button::new("复制图像"))
                .clicked()
            {
                self.copy_to_clipboard();
            }
            if self.jpeg.is_some() {
                let fit_label = if self.fit {
                    "适应窗口"
                } else {
                    "原始大小"
                };
                ui.toggle_value(&mut self.fit, fit_label);
            }
            if self.loading {
                ui.spinner();
                ui.label(egui::RichText::new("截取中…").weak());
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(
                crate::theme::palette(ui.visuals().dark_mode).error,
                error.clone(),
            );
        }
        ui.add_space(6.0);

        if self.jpeg.is_none() {
            ui.label(egui::RichText::new("点击「拍照」截取当前屏幕。").weak());
            return commands;
        }

        // 解码并缓存纹理:只在拿到新图后做一次,此处的 clone 每张图至多一次,
        // 之后各帧只借用纹理,不再整张拷贝 JPEG。
        if self.texture.is_none() && self.jpeg.is_some() {
            let jpeg = self.jpeg.clone().unwrap_or_default();
            match image::load_from_memory(&jpeg) {
                Ok(dynamic) => {
                    let rgba = dynamic.to_rgba8();
                    let size = [rgba.width() as f32, rgba.height() as f32];
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        &rgba,
                    );
                    let texture = ui.ctx().load_texture(
                        format!("fhdc-screenshot-{}", self.texture_stamp),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.texture_stamp += 1;
                    self.texture = Some((texture, egui::Vec2::new(size[0], size[1])));
                }
                Err(error) => {
                    self.error = Some(format!("解码图像失败: {error}"));
                    self.jpeg = None;
                    return commands;
                }
            }
        }

        if let Some((texture, size)) = &self.texture {
            let available = ui.available_size();
            let display = if self.fit {
                let scale = (available.x / size.x).min(available.y / size.y).min(1.0);
                egui::Vec2::new(size.x * scale, size.y * scale)
            } else {
                *size
            };
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.image((texture.id(), display));
                });
            ui.label(
                egui::RichText::new(format!("{} × {}", size.x as u32, size.y as u32))
                    .small()
                    .weak(),
            );
        }
        commands
    }

    fn copy_to_clipboard(&mut self) {
        let Some(jpeg) = self.jpeg.clone() else {
            return;
        };
        match image::load_from_memory(&jpeg) {
            Ok(dynamic) => {
                let rgba = dynamic.to_rgba8();
                let image = arboard::ImageData {
                    width: rgba.width() as usize,
                    height: rgba.height() as usize,
                    bytes: rgba.into_raw().into(),
                };
                match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_image(image))
                {
                    Ok(()) => self.error = None,
                    Err(error) => self.error = Some(format!("复制失败: {error}")),
                }
            }
            Err(error) => self.error = Some(format!("解码图像失败: {error}")),
        }
    }
}
