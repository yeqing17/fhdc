//! 交互式终端:滚动回显 + 输入行 + 快捷指令栏。

use std::collections::VecDeque;
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
/// 未换行残留的上限:超过就强制断行,避免无换行输出撑爆内存。
const MAX_PENDING_BYTES: usize = 64 * 1024;
/// ANSI 转义续接尾巴的上限:超过视为畸形序列,按字面量放行。
const ANSI_PENDING_LIMIT: usize = 4096;
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
    /// 已完成行的回显(只保留尾部,行级虚拟化渲染;队首弹出 O(1))。
    tail: VecDeque<String>,
    /// 末尾尚未收到换行的片段:实时回显输入字符。
    pending_line: String,
    /// tail 中所有行的字节数,用于超限裁剪。
    tail_bytes: usize,
    /// 跨块续接的 UTF-8 解码残留。
    utf8: super::Utf8Accumulator,
    /// 跨块续接的 ANSI 转义尾巴。
    ansi_pending: String,
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
            tail: VecDeque::new(),
            pending_line: String::new(),
            tail_bytes: 0,
            utf8: super::Utf8Accumulator::new(),
            ansi_pending: String::new(),
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
        // 先经增量解码器续接跨块的汉字,再流式清洗 ANSI(转义尾巴
        // 也可能被切块切断,同样跨块续接)。
        let text = self.utf8.push(bytes);
        let cleaned = strip_ansi_stream(&mut self.ansi_pending, &text);
        // 按行切分:首段并入未换行残留成为完整行,末段成为新的残留。
        let mut split = cleaned.split('\n');
        let remainder = split.next_back().unwrap_or_default().to_owned();
        for line in split {
            self.pending_line.push_str(line);
            let complete = std::mem::take(&mut self.pending_line);
            self.tail_bytes += complete.len() + 1;
            self.tail.push_back(complete);
        }
        self.pending_line.push_str(&remainder);
        // 无换行的超长输出(cat 二进制、dd 刷屏)会让 pending_line 无界
        // 增长:超过上限就强制断行推进 tail,交给下面的裁剪逻辑接管。
        if self.pending_line.len() > MAX_PENDING_BYTES {
            let mut cut = MAX_PENDING_BYTES;
            while !self.pending_line.is_char_boundary(cut) {
                cut -= 1;
            }
            let overflow = self.pending_line.split_off(cut);
            let head = std::mem::take(&mut self.pending_line);
            self.tail_bytes += head.len() + 1;
            self.tail.push_back(head);
            self.pending_line = overflow;
        }
        // 超限时从最旧的行开始丢弃(VecDeque 头部弹出是 O(1))。
        while self.tail.len() > MAX_TAIL_LINES
            || (self.tail_bytes > MAX_TAIL_BYTES && self.tail.len() > 1)
        {
            if let Some(dropped) = self.tail.pop_front() {
                self.tail_bytes -= dropped.len() + 1;
            }
        }
    }

    fn clear_display(&mut self) {
        self.tail.clear();
        self.pending_line.clear();
        self.tail_bytes = 0;
        self.ansi_pending.clear();
        // 半个汉字可能正卡在解码残片里,一并重置,避免清屏后首字符乱码。
        self.utf8 = super::Utf8Accumulator::new();
    }

    /// 完整回显文本(复制全部用)。
    fn display_text(&self) -> String {
        let mut text = self
            .tail
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
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
            // 每行一个 Label,全局 8px 的纵向 item_spacing 会把终端撑成
            // 双倍行距;行高用字体实际行高,不再手工估。
            ui.spacing_mut().item_spacing.y = 0.0;
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let row_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id));
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
                    .show_rows(ui, row_height, total_rows, |ui, range| {
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

/// 尝试消费 `index` 处开始的转义序列,返回序列结束位置;
/// None 表示序列在块尾不完整,应留待下一块续接。
fn ansi_sequence_end(chars: &[char], index: usize) -> Option<usize> {
    match chars.get(index + 1)? {
        // CSI:参数与中间字节之后,最终字节落在 0x40..=0x7E。
        '[' => {
            let mut scan = index + 2;
            loop {
                match chars.get(scan) {
                    Some(final_byte) if ('\u{40}'..='\u{7e}').contains(final_byte) => {
                        return Some(scan + 1);
                    }
                    Some(_) => scan += 1,
                    None => return None,
                }
            }
        }
        // OSC:直到 BEL,或 ST(ESC \)。
        ']' => {
            let mut scan = index + 2;
            loop {
                match chars.get(scan) {
                    Some('\u{07}') => return Some(scan + 1),
                    Some('\x1b') if chars.get(scan + 1) == Some(&'\\') => {
                        return Some(scan + 2);
                    }
                    Some(_) => scan += 1,
                    None => return None,
                }
            }
        }
        // 其它两字符转义。
        _ => Some(index + 2),
    }
}

/// 流式清洗 ANSI 转义并把 CR 归一为 LF。转义序列可能被分块切断,
/// 不完整的尾巴存进 `pending`,与下一块拼接后重新处理。
fn strip_ansi_stream(pending: &mut String, input: &str) -> String {
    let mut source = std::mem::take(pending);
    source.push_str(input);
    // 畸形转义保险:待续接的尾巴超过上限(终止符迟迟不来,cat 二进制
    // 常见),放弃续接按字面量吐出,避免 pending 无界增长并吞掉显示。
    let mut output = String::with_capacity(source.len());
    if source.len() > ANSI_PENDING_LIMIT {
        output.push_str(&source);
        *pending = String::new();
        return output;
    }
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\x1b' {
            match ansi_sequence_end(&chars, index) {
                Some(end) => index = end,
                None => break,
            }
        } else if ch == '\r' {
            if chars.get(index + 1) == Some(&'\n') {
                index += 2;
                output.push('\n');
            } else if index + 1 == chars.len() {
                // 块尾的孤立 CR:可能是被切断的 CRLF,留到下一块判断,
                // 否则跨块的 \r\n 会变成两个换行。
                break;
            } else {
                index += 1;
                output.push('\n');
            }
        } else {
            output.push(ch);
            index += 1;
        }
    }
    *pending = chars[index..].iter().collect();
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_osc_and_cr() {
        let mut pending = String::new();
        let input = "\x1b[32mOK\x1b[0m\r\ndone\r\x1b]0;title\x07tail";
        assert_eq!(strip_ansi_stream(&mut pending, input), "OK\ndone\ntail");
        assert!(pending.is_empty());
    }

    #[test]
    fn keeps_plain_text() {
        let mut pending = String::new();
        assert_eq!(
            strip_ansi_stream(&mut pending, "普通文本 中文 line\n"),
            "普通文本 中文 line\n"
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn holds_incomplete_escape_across_chunks() {
        // 切在 ESC 与 '[' 之间
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "A\x1b"), "A");
        assert_eq!(strip_ansi_stream(&mut pending, "[32mB"), "B");
        assert!(pending.is_empty());
        // 切在 CSI 参数中间
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "\x1b[3"), "");
        assert_eq!(strip_ansi_stream(&mut pending, "2mC"), "C");
        // 切在 OSC 内部(BEL 终止符在下一块)
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "\x1b]0;ti"), "");
        assert_eq!(strip_ansi_stream(&mut pending, "tle\x07D"), "D");
        // OSC 以 ST(ESC \) 终止
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "\x1b]8;;"), "");
        assert_eq!(strip_ansi_stream(&mut pending, "url\x1b\\E"), "E");
    }

    #[test]
    fn normalizes_lone_cr() {
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "a\rb\r\n"), "a\nb\n");
        assert!(pending.is_empty());
    }

    #[test]
    fn holds_chunk_final_cr_to_detect_crlf() {
        // \r 与 \n 被切在两块:不能输出两个换行。
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "a\r"), "a");
        assert_eq!(strip_ansi_stream(&mut pending, "\nb"), "\nb");
        assert!(pending.is_empty());
        // 块尾孤立 CR 后面跟的不是 \n:按单个换行处理。
        let mut pending = String::new();
        assert_eq!(strip_ansi_stream(&mut pending, "x\r"), "x");
        assert_eq!(strip_ansi_stream(&mut pending, "y"), "\ny");
        assert!(pending.is_empty());
    }

    #[test]
    fn flushes_degenerate_escape_as_literal() {
        // 永不终止的"转义"超过上限后按字面量放行,不再吞内容。
        // 填充用 ';'(0x3B):它是纯 CSI 参数字节,永远不会终止序列
        // (字母如 'x' 落在 0x40..=0x7E,本身就能当终止符)。
        let mut pending = String::new();
        let mut first = String::from("\x1b[");
        for _ in 0..3000 {
            first.push(';');
        }
        assert_eq!(strip_ansi_stream(&mut pending, &first), "");
        // 再压进一段,总长超过上限:整个尾巴按字面量吐出。
        let mut second = String::new();
        for _ in 0..2000 {
            second.push(';');
        }
        let flushed = strip_ansi_stream(&mut pending, &second);
        assert!(flushed.starts_with('\x1b'));
        assert_eq!(flushed.chars().count(), 2 + 5000);
        assert!(pending.is_empty());
        assert_eq!(strip_ansi_stream(&mut pending, "tail"), "tail");
    }
}
