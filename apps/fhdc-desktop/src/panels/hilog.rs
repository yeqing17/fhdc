//! hilog 实时日志:级别着色、关键字过滤、暂停与保存。

use std::time::Instant;

use eframe::egui;
use eframe::egui::Color32;
use eframe::egui::Key;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BridgeError;
use fhdc_domain::DeviceTarget;
use fhdc_domain::SessionId;

use crate::theme;

use super::Commands;
use super::PanelCtx;

/// 行缓冲上限。
const MAX_LINES: usize = 10_000;
const ROW_HEIGHT: f32 = 17.0;

const LEVELS: [char; 6] = ['*', 'D', 'I', 'W', 'E', 'F'];

#[derive(Debug, Clone)]
#[expect(dead_code, reason = "解析字段供测试与后续列化显示使用")]
struct LogLine {
    timestamp: String,
    pid: String,
    level: char,
    tag: String,
    message: String,
    raw: String,
}

impl LogLine {
    fn parse(line: &str) -> Option<Self> {
        // hilog 默认格式:MM-DD HH:MM:SS.mmm PID TID L Domain/Tag: message
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            return None;
        }
        let level = fields[4].chars().next()?;
        if !matches!(level, 'D' | 'I' | 'W' | 'E' | 'F') {
            return None;
        }
        let tag_field = fields[5];
        let tag = tag_field.strip_suffix(':').unwrap_or(tag_field).to_owned();
        let message = if fields.len() > 6 {
            fields[6..].join(" ")
        } else {
            String::new()
        };
        Some(Self {
            timestamp: format!("{} {}", fields[0], fields[1]),
            pid: format!("{} {}", fields[2], fields[3]),
            level,
            tag,
            message,
            raw: line.to_owned(),
        })
    }
}

fn level_color(level: char, dark: bool) -> Color32 {
    match level {
        'D' => Color32::from_rgb(96, 165, 250),
        'I' => {
            if dark {
                Color32::from_rgb(134, 239, 172)
            } else {
                Color32::from_rgb(22, 163, 74)
            }
        }
        'W' => {
            if dark {
                Color32::from_rgb(250, 204, 21)
            } else {
                Color32::from_rgb(217, 119, 6)
            }
        }
        'E' => {
            if dark {
                Color32::from_rgb(248, 113, 113)
            } else {
                Color32::from_rgb(220, 38, 38)
            }
        }
        'F' => Color32::from_rgb(244, 63, 94),
        _ => {
            if dark {
                Color32::from_gray(140)
            } else {
                Color32::from_gray(110)
            }
        }
    }
}

#[derive(Default)]
pub struct HilogState {
    target: Option<DeviceTarget>,
    session: Option<SessionId>,
    starting: bool,
    running: bool,
    user_stopped: bool,
    paused: bool,
    auto_scroll: bool,
    level_index: usize,
    query: String,
    lines: Vec<LogLine>,
    pending: String,
    /// 跨块续接的 UTF-8 解码残留。
    utf8: super::Utf8Accumulator,
    error: Option<BridgeError>,
    last_start_attempt: Option<Instant>,
}

impl HilogState {
    pub fn reset_for(&mut self, target: Option<DeviceTarget>) -> Option<BackendCommand> {
        if self.target.as_ref() == target.as_ref() {
            return None;
        }
        let close = self
            .session
            .take()
            .map(|session| BackendCommand::StopHilog { session });
        self.target = target;
        self.running = false;
        self.starting = false;
        self.user_stopped = false;
        self.paused = false;
        self.lines.clear();
        self.pending.clear();
        self.utf8 = super::Utf8Accumulator::new();
        self.error = None;
        self.last_start_attempt = None;
        close
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn maybe_auto_start(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if self.user_stopped || self.running || self.starting || !ctx.online() {
            return None;
        }
        let now = Instant::now();
        if let Some(last) = self.last_start_attempt
            && now.duration_since(last).as_secs() < 3
        {
            return None;
        }
        self.last_start_attempt = Some(now);
        ctx.target.clone().map(|target| BackendCommand::StartHilog {
            target,
            session: SessionId::new(),
        })
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::HilogStarted { target, session } => {
                if self.target.as_ref() == Some(target) {
                    self.session = Some(*session);
                    self.running = true;
                    self.starting = false;
                    self.error = None;
                }
            }
            BackendEvent::HilogOutput { session, bytes } => {
                if self.session.is_some_and(|current| current == *session) {
                    self.ingest(bytes);
                }
            }
            BackendEvent::HilogClosed { session } => {
                if self.session.is_some_and(|current| current == *session) {
                    self.session = None;
                    self.running = false;
                }
            }
            BackendEvent::HilogFailed { session, error } => {
                if self.session.is_none() || self.session.is_some_and(|current| current == *session)
                {
                    self.session = None;
                    self.starting = false;
                    self.running = false;
                    self.error = Some(error.clone());
                }
            }
            _ => {}
        }
    }

    fn ingest(&mut self, bytes: &[u8]) {
        // 暂停时继续消费并丢弃:恢复后从当下继续。
        if self.paused {
            return;
        }
        // 先经增量解码器续接跨块的汉字,再与半行残留合并切分。
        let decoded = self.utf8.push(bytes);
        let mut text = std::mem::take(&mut self.pending);
        text.push_str(&decoded);
        let mut split = text.split('\n');
        let remainder = split.next_back().unwrap_or_default().to_owned();
        for line in split {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.is_empty() {
                continue;
            }
            self.lines
                .push(LogLine::parse(trimmed).unwrap_or_else(|| LogLine {
                    timestamp: String::new(),
                    pid: String::new(),
                    level: '?',
                    tag: String::new(),
                    message: String::new(),
                    raw: trimmed.to_owned(),
                }));
        }
        self.pending = remainder;
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, ctx: &PanelCtx) -> Commands {
        let mut commands = Vec::new();
        ui.heading("日志(hilog)");
        ui.add_space(4.0);
        let palette = theme::palette(ui.visuals().dark_mode);
        ui.horizontal_wrapped(|ui| {
            if !self.running {
                if ui
                    .add_enabled(ctx.online() && !self.starting, egui::Button::new("开始"))
                    .on_disabled_hover_text("请先选择设备")
                    .clicked()
                    && let Some(target) = ctx.target.clone()
                {
                    self.starting = true;
                    self.user_stopped = false;
                    self.error = None;
                    commands.push(BackendCommand::StartHilog {
                        target,
                        session: SessionId::new(),
                    });
                }
            } else if ui.button("停止").clicked() {
                if let Some(session) = self.session.take() {
                    self.user_stopped = true;
                    commands.push(BackendCommand::StopHilog { session });
                }
                self.running = false;
            }
            let pause_label = if self.paused {
                "恢复(从当下)"
            } else {
                "暂停"
            };
            if ui.button(pause_label).clicked() {
                self.paused = !self.paused;
            }
            if ui.button("清空").clicked() {
                self.lines.clear();
                self.pending.clear();
            }
            if ui
                .add_enabled(ctx.online(), egui::Button::new("清除设备日志"))
                .on_disabled_hover_text("请先选择设备")
                .clicked()
                && let Some(target) = ctx.target.clone()
            {
                commands.push(BackendCommand::ClearDeviceHilog(target));
            }
            if ui.button("保存…").clicked()
                && !self.lines.is_empty()
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name("fhdc-hilog.log")
                    .add_filter("Log", &["log", "txt"])
                    .save_file()
            {
                let content: String = self
                    .lines
                    .iter()
                    .map(|line| line.raw.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Err(error) = std::fs::write(&path, content) {
                    self.error = Some(BridgeError::hdc(format!("保存失败: {error}")));
                }
            }
            ui.separator();
            ui.label("级别");
            egui::ComboBox::from_id_salt("hilog-level")
                .selected_text(LEVELS[self.level_index].to_string())
                .width(64.0)
                .show_ui(ui, |ui| {
                    for (index, level) in LEVELS.iter().enumerate() {
                        ui.selectable_value(&mut self.level_index, index, level.to_string());
                    }
                });
            let search = egui::TextEdit::singleline(&mut self.query)
                .hint_text("过滤…")
                .desired_width(160.0);
            let search = ui.add(search);
            if search.has_focus() && ui.input(|input| input.key_pressed(Key::Escape)) {
                self.query.clear();
            }
            ui.checkbox(&mut self.auto_scroll, "自动滚动");
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(format!("已缓冲 {} 行", self.lines.len()))
                .small()
                .weak(),
        );
        if let Some(error) = &self.error {
            ui.colored_label(palette.error, format!("hilog: {}", error.detail));
        }
        if self.starting {
            ui.spinner();
        }
        ui.add_space(4.0);

        let min_level = LEVELS[self.level_index];
        let query = self.query.to_lowercase();
        let visible: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let level_ok = min_level == '*'
                    || (line.level != '?' && level_rank(line.level) >= level_rank(min_level));
                let query_ok = query.is_empty() || line.raw.to_lowercase().contains(&query);
                level_ok && query_ok
            })
            .map(|(index, _)| index)
            .collect();

        let stick = self.auto_scroll;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .stick_to_bottom(stick)
            .show_rows(ui, ROW_HEIGHT, visible.len(), |ui, range| {
                let frame = egui::Frame::new()
                    .fill(palette.terminal_bg)
                    .corner_radius(4.0)
                    .inner_margin(6.0);
                frame.show(ui, |ui| {
                    ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
                    ui.set_min_width(ui.available_width());
                    for index in range {
                        let Some(buffer_index) = visible.get(index).copied() else {
                            continue;
                        };
                        let Some(line) = self.lines.get(buffer_index) else {
                            continue;
                        };
                        let color = level_color(line.level, ui.visuals().dark_mode);
                        ui.add(
                            egui::Label::new(egui::RichText::new(&line.raw).color(color))
                                .wrap_mode(egui::TextWrapMode::Truncate),
                        );
                    }
                });
            });
        commands
    }
}

fn level_rank(level: char) -> u8 {
    match level {
        'D' => 0,
        'I' => 1,
        'W' => 2,
        'E' => 3,
        'F' => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hilog_default_format() {
        let line = LogLine::parse("10-18 09:15:23.456  1234  1300 I C0170/JSAPP: 页面加载完成")
            .expect("parse");
        assert_eq!(line.level, 'I');
        assert_eq!(line.tag, "C0170/JSAPP");
        assert_eq!(line.message, "页面加载完成");
        assert_eq!(line.pid, "1234 1300");
        let warn = LogLine::parse("10-18 09:15:23.456  1234  1300 W A0d00/DSoftBus: timeout")
            .expect("parse");
        assert_eq!(warn.level, 'W');
        assert_eq!(warn.tag, "A0d00/DSoftBus");
    }

    #[test]
    fn rejects_non_log_lines() {
        assert!(LogLine::parse("some random text").is_none());
        assert!(LogLine::parse("10-18 09:15:23 1234 X nope").is_none());
    }
}
