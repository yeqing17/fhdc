//! 交互式终端:滚动回显 + 输入行 + 快捷指令栏。

use std::time::Instant;

use eframe::egui;
use eframe::egui::Key;
use eframe::egui::TextEdit;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BridgeError;
use fhdc_domain::DeviceTarget;
use fhdc_domain::SessionId;

use crate::quick_commands::QuickCommandStore;
use crate::theme;

use super::Commands;
use super::PanelCtx;

/// 回显行数上限。
const MAX_TAIL_LINES: usize = 5000;
/// 回显字节上限(超限从最旧的行开始丢弃)。
const MAX_TAIL_BYTES: usize = 256 * 1024;
/// 回显行高(等宽字体,单行不换行)。
const TAIL_ROW_HEIGHT: f32 = 16.0;
/// 输入历史上限。
const MAX_HISTORY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Disconnected,
    Connecting,
    Connected,
    Exited,
    Failed,
}

pub struct ShellState {
    target: Option<DeviceTarget>,
    session: Option<SessionId>,
    status: Status,
    error: Option<BridgeError>,
    /// 已完成行的回显(只保留尾部,行级虚拟化渲染)。
    tail: Vec<String>,
    /// 末尾尚未收到换行的片段:实时回显输入字符。
    pending_line: String,
    /// tail 中所有行的字节数,用于超限裁剪。
    tail_bytes: usize,
    /// 跨块续接的 UTF-8 解码残留。
    utf8: super::Utf8Accumulator,
    input: String,
    history: Vec<String>,
    history_cursor: Option<usize>,
    quick: QuickCommandStore,
    last_connect_attempt: Option<Instant>,
    auto_connect: bool,
    focus_input: bool,
}

impl ShellState {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            target: None,
            session: None,
            status: Status::Disconnected,
            error: None,
            tail: Vec::new(),
            pending_line: String::new(),
            tail_bytes: 0,
            utf8: super::Utf8Accumulator::new(),
            input: String::new(),
            history: Vec::new(),
            history_cursor: None,
            quick: QuickCommandStore::load(storage),
            last_connect_attempt: None,
            auto_connect: true,
            focus_input: false,
        }
    }

    pub fn set_auto_connect(&mut self, enabled: bool) {
        self.auto_connect = enabled;
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        self.quick.save(storage);
    }

    pub fn reset_for(&mut self, target: Option<DeviceTarget>) -> Option<BackendCommand> {
        if self.target.as_ref() == target.as_ref() {
            return None;
        }
        let close = self
            .session
            .take()
            .map(|session| BackendCommand::CloseShell { session });
        self.target = target;
        self.status = Status::Disconnected;
        self.error = None;
        self.clear_display();
        self.utf8 = super::Utf8Accumulator::new();
        self.input.clear();
        self.history_cursor = None;
        self.last_connect_attempt = None;
        close
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status, Status::Connected)
    }

    /// 自动连接:设备在线、状态纯净且距上次尝试超过 3 秒。
    pub fn maybe_auto_connect(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if !self.auto_connect || !ctx.online() || self.status != Status::Disconnected {
            return None;
        }
        let now = Instant::now();
        if let Some(last) = self.last_connect_attempt
            && now.duration_since(last).as_secs() < 3
        {
            return None;
        }
        self.last_connect_attempt = Some(now);
        self.open_command(ctx)
    }

    fn open_command(&self, ctx: &PanelCtx) -> Option<BackendCommand> {
        ctx.target.clone().map(|target| BackendCommand::OpenShell {
            target,
            session: SessionId::new(),
        })
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::ShellOpened { target, session } => {
                if self.target.as_ref() == Some(target) && self.status == Status::Connecting {
                    self.session = Some(*session);
                    self.status = Status::Connected;
                    self.error = None;
                    self.focus_input = true;
                }
            }
            BackendEvent::ShellOutput { session, bytes } => {
                if self.session.is_some_and(|current| current == *session) {
                    self.append_bytes(bytes);
                }
            }
            BackendEvent::ShellClosed { session, .. } => {
                if self.session.is_some_and(|current| current == *session) {
                    self.session = None;
                    self.status = Status::Exited;
                }
            }
            BackendEvent::ShellFailed { session, error } => {
                if self.session.is_none() || self.session.is_some_and(|current| current == *session)
                {
                    self.session = None;
                    self.status = Status::Failed;
                    self.error = Some(error.clone());
                }
            }
            _ => {}
        }
    }

    fn append_bytes(&mut self, bytes: &[u8]) {
        // 先经增量解码器续接跨块的汉字,再做 ANSI 清洗。
        let text = self.utf8.push(bytes);
        let cleaned = strip_ansi(&text);
        // 按行切分:首段并入未换行残留成为完整行,末段成为新的残留。
        let mut split = cleaned.split('\n');
        let remainder = split.next_back().unwrap_or_default().to_owned();
        for line in split {
            self.pending_line.push_str(line);
            let complete = std::mem::take(&mut self.pending_line);
            self.tail_bytes += complete.len() + 1;
            self.tail.push(complete);
        }
        self.pending_line.push_str(&remainder);
        // 超限时从最旧的行开始丢弃。
        while self.tail.len() > MAX_TAIL_LINES
            || (self.tail_bytes > MAX_TAIL_BYTES && self.tail.len() > 1)
        {
            let dropped = self.tail.remove(0);
            self.tail_bytes -= dropped.len() + 1;
        }
    }

    fn clear_display(&mut self) {
        self.tail.clear();
        self.pending_line.clear();
        self.tail_bytes = 0;
    }

    /// 完整回显文本(复制全部用)。
    fn display_text(&self) -> String {
        let mut text = self.tail.join("\n");
        if !self.pending_line.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.pending_line);
        }
        text
    }

    fn send_line(&mut self) -> Option<BackendCommand> {
        let session = self.session?;
        let line = std::mem::take(&mut self.input);
        if line.is_empty() {
            return Some(BackendCommand::WriteShell {
                session,
                input: b"\n".to_vec(),
            });
        }
        self.history.push(line.clone());
        // truncate 保留的是最旧的 N 条,会把刚记录的命令丢掉;
        // 超限时从头移除,永远保留最近的 MAX_HISTORY 条。
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        self.history_cursor = None;
        let mut payload = line.into_bytes();
        payload.push(b'\n');
        Some(BackendCommand::WriteShell {
            session,
            input: payload,
        })
    }

    pub fn show(&mut self, ui: &mut Ui, ctx: &PanelCtx) -> Commands {
        let mut commands = Vec::new();
        ui.heading("终端");
        ui.add_space(4.0);
        let palette = theme::palette(ui.visuals().dark_mode);
        ui.horizontal(|ui| {
            let status_color = match self.status {
                Status::Connected => palette.ok,
                Status::Connecting => palette.warn,
                Status::Failed => palette.error,
                Status::Disconnected | Status::Exited => ui.visuals().weak_text_color(),
            };
            let status_text = match self.status {
                Status::Disconnected => "未连接",
                Status::Connecting => "连接中…",
                Status::Connected => "已连接",
                Status::Exited => "已退出(设备侧结束)",
                Status::Failed => "连接失败",
            };
            ui.colored_label(status_color, format!("● {status_text}"));

            match self.status {
                Status::Disconnected | Status::Exited | Status::Failed => {
                    if ui
                        .add_enabled(ctx.online(), egui::Button::new("连接"))
                        .on_disabled_hover_text("请先选择设备")
                        .clicked()
                        && let Some(command) = self.open_command(ctx)
                    {
                        self.status = Status::Connecting;
                        self.error = None;
                        commands.push(command);
                    }
                }
                Status::Connected | Status::Connecting => {
                    if ui.button("断开").clicked() {
                        if let Some(session) = self.session.take() {
                            self.status = Status::Disconnected;
                            commands.push(BackendCommand::CloseShell { session });
                        } else {
                            self.status = Status::Disconnected;
                        }
                    }
                }
            }
            if ui.button("清屏").clicked() {
                self.clear_display();
            }
            if ui.button("复制全部").clicked() {
                ui.ctx().copy_text(self.display_text());
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(palette.error, format!("{}(可重试连接)", error.detail));
        }

        // 快捷指令栏
        self.quick.show_inline(ui, &mut |chunk| {
            if let Some(session) = self.session {
                commands.push(BackendCommand::WriteShell {
                    session,
                    input: chunk,
                });
            }
        });
        self.quick.show_manage(ui);
        ui.add_space(4.0);

        // 回显区:行级虚拟化,只为可见行生成部件,长日志不再整块重排。
        let palette_terminal = theme::palette(ui.visuals().dark_mode);
        let frame = egui::Frame::new()
            .fill(palette_terminal.terminal_bg)
            .corner_radius(4.0)
            .inner_margin(8.0);
        frame.show(ui, |ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
            let total_rows = self.tail.len() + usize::from(!self.pending_line.is_empty());
            if total_rows == 0 {
                ui.set_min_height(160.0);
                ui.label(
                    egui::RichText::new(if ctx.online() {
                        "等待输出…"
                    } else {
                        "选择设备并点击「连接」"
                    })
                    .weak(),
                );
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .stick_to_bottom(true)
                    .show_rows(ui, TAIL_ROW_HEIGHT, total_rows, |ui, range| {
                        ui.set_min_width(ui.available_width());
                        for index in range {
                            let text = if index < self.tail.len() {
                                self.tail[index].as_str()
                            } else {
                                self.pending_line.as_str()
                            };
                            ui.add(
                                egui::Label::new(egui::RichText::new(text))
                                    .wrap_mode(egui::TextWrapMode::Truncate),
                            );
                        }
                    });
            }
        });

        ui.add_space(4.0);
        // 输入行
        let input_enabled = self.status == Status::Connected;
        ui.horizontal(|ui| {
            ui.label(">");
            let response = ui.add_enabled(
                input_enabled,
                TextEdit::singleline(&mut self.input)
                    .hint_text("输入命令,回车发送;↑↓ 翻历史")
                    .desired_width(ui.available_width() - 64.0),
            );
            if response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                if let Some(command) = self.send_line() {
                    commands.push(command);
                }
                self.focus_input = true;
            }
            if self.focus_input {
                response.request_focus();
                self.focus_input = false;
            }
            if ui
                .add_enabled(input_enabled, egui::Button::new("发送"))
                .clicked()
            {
                if let Some(command) = self.send_line() {
                    commands.push(command);
                }
                self.focus_input = true;
            }
        });
        // 历史翻阅
        if input_enabled
            && ui
                .input(|input| input.key_pressed(Key::ArrowUp) || input.key_pressed(Key::ArrowDown))
        {
            let down = ui.input(|input| input.key_pressed(Key::ArrowDown));
            match self.history_cursor {
                None => {
                    if !down && !self.history.is_empty() {
                        self.history_cursor = Some(self.history.len() - 1);
                        self.input = self.history[self.history.len() - 1].clone();
                    }
                }
                Some(index) => {
                    // 索引大 = 更新;↑ 往旧翻,↓ 往新翻。
                    let next = if down {
                        (index + 1).min(self.history.len() - 1)
                    } else {
                        index.saturating_sub(1)
                    };
                    self.history_cursor = Some(next);
                    self.input = self.history[next].clone();
                }
            }
        }
        commands
    }
}

/// 去除 ANSI 转义序列(CSI/OSC),把 CR 归一为 LF,避免回显错乱。
pub fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                output.push('\n');
            }
            '\x1b' => match chars.next() {
                Some('[') => {
                    // CSI:参数与中间字节直到最终字节(0x40..=0x7E)。
                    for inner in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&inner) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC:直到 BEL 或 ST。
                    for inner in chars.by_ref() {
                        if inner == '\x07' {
                            break;
                        }
                        if inner == '\x1b' {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            other => output.push(other),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_osc_and_cr() {
        let input = "\x1b[32mOK\x1b[0m\r\ndone\r\x1b]0;title\x07tail";
        assert_eq!(strip_ansi(input), "OK\ndone\ntail");
    }

    #[test]
    fn keeps_plain_text() {
        assert_eq!(strip_ansi("普通文本 中文 line\n"), "普通文本 中文 line\n");
    }
}
