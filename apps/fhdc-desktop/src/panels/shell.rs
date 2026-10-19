//! 交互式终端:VT 网格渲染 + 逐键输入 + 快捷指令栏。
//!
//! 与 fadb 同构:会话挂在伪控制台上(见 fhdc-hdc),GUI 不再自管输入行,
//! 按键字节直达设备 shell,由 VT 解析器呈现屏幕与光标。

use std::time::Instant;

use eframe::egui;
use eframe::egui::Key;
use eframe::egui::Ui;

use fhdc_domain::BackendCommand;
use fhdc_domain::BackendEvent;
use fhdc_domain::BridgeError;
use fhdc_domain::DeviceTarget;
use fhdc_domain::MAX_SHELL_INPUT_BYTES;
use fhdc_domain::SessionId;
use fhdc_domain::ShellSize;

use crate::quick_commands::QuickCommandStore;
use crate::theme;

use super::Commands;
use super::PanelCtx;

/// 终端固定 80 列(与会话建立时传给设备的 stty 尺寸一致)。
const TERMINAL_COLUMNS: u16 = 80;
const INITIAL_TERMINAL_ROWS: u16 = 24;
const SCROLLBACK_ROWS: usize = 10_000;
const TERMINAL_FONT_SIZE: f32 = 14.0;
const TERMINAL_PADDING: f32 = 10.0;
const MAX_VIEWPORT_ROWS: u16 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Disconnected,
    Connecting,
    Connected,
    Exited,
    Failed,
}

pub struct ShellState {
    parser: vt100::Parser,
    viewport_rows: u16,
    target: Option<DeviceTarget>,
    session: Option<SessionId>,
    status: Status,
    error: Option<BridgeError>,
    focus_terminal: bool,
    /// 惰性创建的系统剪贴板句柄;egui 只在 Ctrl+V 事件里暴露剪贴板。
    clipboard: Option<arboard::Clipboard>,
    /// 可见区的格点选区:`(anchor, head)`,均为 `(row, column)`。
    selection: Option<(TerminalCell, TerminalCell)>,
    quick: QuickCommandStore,
    last_connect_attempt: Option<Instant>,
    auto_connect: bool,
    /// 用户显式点过「断开」:自动连接让位,直到换设备/重新连接/重新开启自动连接。
    user_stopped: bool,
}

/// 终端网格里的 `(row, column)` 位置。
type TerminalCell = (u16, u16);

impl ShellState {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            parser: vt100::Parser::new(INITIAL_TERMINAL_ROWS, TERMINAL_COLUMNS, SCROLLBACK_ROWS),
            viewport_rows: INITIAL_TERMINAL_ROWS,
            target: None,
            session: None,
            status: Status::Disconnected,
            error: None,
            focus_terminal: false,
            clipboard: None,
            selection: None,
            quick: QuickCommandStore::load(storage),
            last_connect_attempt: None,
            auto_connect: true,
            user_stopped: false,
        }
    }

    pub fn set_auto_connect(&mut self, enabled: bool) {
        // 重新打开自动连接视为用户的明确意图,清掉此前的「断开」抑制。
        if enabled && !self.auto_connect {
            self.user_stopped = false;
        }
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
        self.selection = None;
        self.clear_display();
        self.last_connect_attempt = None;
        self.user_stopped = false;
        close
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status, Status::Connected)
    }

    /// 自动连接:设备在线、状态纯净且距上次尝试超过 3 秒。
    /// 用户显式断开后不再自动重连,否则「断开」会被下一帧立刻反悔。
    pub fn maybe_auto_connect(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        if !self.auto_connect
            || self.user_stopped
            || !ctx.online()
            || self.status != Status::Disconnected
        {
            return None;
        }
        let now = Instant::now();
        if let Some(last) = self.last_connect_attempt
            && now.duration_since(last).as_secs() < 3
        {
            return None;
        }
        self.last_connect_attempt = Some(now);
        self.begin_connect(ctx)
    }

    /// 主动连接:按钮与自动连接共用,置为"连接中"并产出 OpenShell。
    /// ShellOpened 只在 Connecting 下被接受,状态与命令必须成对产生。
    fn begin_connect(&mut self, ctx: &PanelCtx) -> Option<BackendCommand> {
        let command = self.open_command(ctx)?;
        self.status = Status::Connecting;
        self.error = None;
        self.user_stopped = false;
        Some(command)
    }

    fn open_command(&self, ctx: &PanelCtx) -> Option<BackendCommand> {
        ctx.target.clone().map(|target| {
            let size = ShellSize::new(TERMINAL_COLUMNS, self.viewport_rows.max(1))
                .expect("terminal size is valid");
            BackendCommand::OpenShell {
                target,
                session: SessionId::new(),
                size,
            }
        })
    }

    pub fn handle_event(&mut self, event: &BackendEvent) {
        match event {
            BackendEvent::ShellOpened { target, session } => {
                if self.target.as_ref() == Some(target) && self.status == Status::Connecting {
                    self.session = Some(*session);
                    self.status = Status::Connected;
                    self.error = None;
                    self.focus_terminal = true;
                }
            }
            BackendEvent::ShellOutput { session, bytes } => {
                if self.session.is_some_and(|current| current == *session) {
                    self.parser.process(bytes);
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

    fn clear_display(&mut self) {
        self.parser = vt100::Parser::new(self.viewport_rows, TERMINAL_COLUMNS, SCROLLBACK_ROWS);
    }

    fn resize_viewport(&mut self, rows: u16) {
        if self.viewport_rows != rows {
            self.parser.screen_mut().set_size(rows, TERMINAL_COLUMNS);
            self.viewport_rows = rows;
        }
    }

    fn connected(&self) -> bool {
        self.status == Status::Connected && self.session.is_some()
    }

    fn write(&mut self, bytes: Vec<u8>) -> Option<BackendCommand> {
        let session = self.session?;
        if bytes.is_empty() || bytes.len() > MAX_SHELL_INPUT_BYTES {
            return None;
        }
        Some(BackendCommand::WriteShell {
            session,
            input: bytes,
        })
    }

    /// 选区按阅读顺序,空选区返回 None。
    fn ordered_selection(&self) -> Option<(TerminalCell, TerminalCell)> {
        let (anchor, head) = self.selection?;
        Some(if (anchor.0, anchor.1) <= (head.0, head.1) {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }

    /// 从可见区抽取选中文本;CJK 宽格只贡献一次字符,行尾空白裁掉。
    fn selection_text(&self) -> Option<String> {
        let (start, end) = self.ordered_selection()?;
        let screen = self.parser.screen();
        let mut lines = Vec::new();
        for row in start.0..=end.0 {
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1
            } else {
                TERMINAL_COLUMNS.saturating_sub(1)
            };
            let mut line = String::new();
            for column in first..=last {
                if let Some(cell) = screen.cell(row, column) {
                    line.push_str(cell.contents());
                }
            }
            lines.push(line.trim_end().to_owned());
        }
        Some(lines.join("\n"))
    }

    /// 粘贴载荷:对端开启 bracketed paste 时补上包裹序列。
    fn paste_payload(&self, clipboard: &str) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut bytes = Vec::new();
        if screen.bracketed_paste() {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(clipboard.as_bytes());
        if screen.bracketed_paste() {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        bytes
    }

    /// 惰性读系统剪贴板;无头环境或所有权丢失都归于 None。
    fn clipboard_text(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()?.get_text().ok()
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
                        && let Some(command) = self.begin_connect(ctx)
                    {
                        commands.push(command);
                    }
                }
                Status::Connected | Status::Connecting => {
                    if ui.button("断开").clicked() {
                        // 显式断开:抑制自动连接,直到用户手动重连或换设备。
                        self.user_stopped = true;
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
            if ui.button("复制屏幕").clicked() {
                ui.ctx().copy_text(self.parser.screen().contents());
            }
            if ui
                .add_enabled(self.session.is_some(), egui::Button::new("粘贴"))
                .clicked()
                && let Some(clipboard) = self.clipboard_text().filter(|text| !text.is_empty())
            {
                let payload = self.paste_payload(&clipboard);
                if let Some(command) = self.write(payload) {
                    commands.push(command);
                }
            }
            if ui.button("聚焦").clicked() {
                self.focus_terminal = true;
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(palette.error, format!("{}(可重试连接)", error.detail));
        }

        // 快捷指令栏(闭包只捕获会话副本,避免与 quick 的可变借用冲突)
        let active_session = self.session;
        let mut queued = Vec::new();
        self.quick.show_inline(ui, &mut |chunk| {
            if let Some(session) = active_session
                && !chunk.is_empty()
                && chunk.len() <= MAX_SHELL_INPUT_BYTES
            {
                queued.push(BackendCommand::WriteShell {
                    session,
                    input: chunk,
                });
            }
        });
        if !queued.is_empty() {
            self.focus_terminal = true;
        }
        commands.append(&mut queued);
        self.quick.show_manage(ui);
        ui.add_space(4.0);

        // 终端区:占满剩余高度,点击聚焦,键盘直达设备 shell。
        if !ctx.online() {
            ui.colored_label(palette.warn, "请先选择设备");
        }
        let desired = ui.available_size().max(egui::vec2(400.0, 260.0));
        let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click_and_drag());
        self.resize_viewport(terminal_viewport_rows(ui, rect));
        if response.clicked() || self.focus_terminal {
            response.request_focus();
            self.focus_terminal = false;
        }
        if response.has_focus() {
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
            if self.connected() {
                for bytes in terminal_input(ui, self.parser.screen()) {
                    if let Some(command) = self.write(bytes) {
                        commands.push(command);
                    }
                }
            }
        }
        handle_terminal_pointer(ui, self, &response, rect, &mut commands);
        paint_terminal(ui, rect, self, response.has_focus(), ctx);
        commands
    }
}

/// 终端表面上的拖拽选区与右键剪贴板菜单。
fn handle_terminal_pointer(
    ui: &egui::Ui,
    state: &mut ShellState,
    response: &egui::Response,
    rect: egui::Rect,
    commands: &mut Vec<BackendCommand>,
) {
    if response.drag_started()
        && let Some(position) = response.interact_pointer_pos()
    {
        let cell = cell_at_pointer(ui, rect, state.viewport_rows, position);
        state.selection = Some((cell, cell));
    }
    if response.dragged()
        && let Some(position) = response.interact_pointer_pos()
        && let Some((_, head)) = state.selection.as_mut()
    {
        *head = cell_at_pointer(ui, rect, state.viewport_rows, position);
    }
    if response.drag_stopped()
        && state
            .ordered_selection()
            .is_some_and(|(start, end)| start != end)
        && let Some(selected) = state.selection_text()
        && selected.chars().any(|character| !character.is_whitespace())
    {
        ui.ctx().copy_text(selected);
    }
    if response.clicked() {
        state.selection = None;
    }
    response.context_menu(|ui| {
        let selected = state.selection_text();
        if ui
            .add_enabled(
                selected
                    .as_ref()
                    .is_some_and(|text| !text.trim().is_empty()),
                egui::Button::new("复制选中"),
            )
            .clicked()
        {
            if let Some(selected) = selected {
                ui.ctx().copy_text(selected);
            }
            ui.close();
        }
        if ui
            .add_enabled(state.session.is_some(), egui::Button::new("粘贴"))
            .clicked()
        {
            if let Some(clipboard) = state.clipboard_text().filter(|text| !text.is_empty()) {
                let payload = state.paste_payload(&clipboard);
                if let Some(command) = state.write(payload) {
                    commands.push(command);
                }
            }
            ui.close();
        }
        if ui.button("复制屏幕").clicked() {
            ui.ctx().copy_text(state.parser.screen().contents());
            ui.close();
        }
    });
}

/// 指针位置映射到终端格点,收拢在可见区内。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn cell_at_pointer(
    ui: &egui::Ui,
    rect: egui::Rect,
    rows: u16,
    position: egui::Pos2,
) -> TerminalCell {
    let font_id = egui::FontId::monospace(TERMINAL_FONT_SIZE);
    let row_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id)).max(1.0);
    let cell_width = ui
        .fonts_mut(|fonts| fonts.glyph_width(&font_id, 'W'))
        .max(1.0);
    let origin = rect.left_top() + egui::vec2(TERMINAL_PADDING, TERMINAL_PADDING);
    let column = ((position.x - origin.x) / cell_width).floor().max(0.0);
    let row = ((position.y - origin.y) / row_height).floor().max(0.0);
    (
        (row as u16).min(rows.saturating_sub(1)),
        (column as u16).min(TERMINAL_COLUMNS - 1),
    )
}

/// 可见区行数 = 终端高度能容纳的行数。
fn terminal_viewport_rows(ui: &egui::Ui, rect: egui::Rect) -> u16 {
    let font_id = egui::FontId::monospace(TERMINAL_FONT_SIZE);
    let row_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id));
    rows_for_viewport(rect.height(), row_height)
}

fn rows_for_viewport(viewport_height: f32, row_height: f32) -> u16 {
    let usable_height = (viewport_height - 2.0 * TERMINAL_PADDING).max(row_height);
    let mut lower = 1;
    let mut upper = MAX_VIEWPORT_ROWS;
    while lower < upper {
        let middle = lower + (upper - lower).div_ceil(2);
        if f32::from(middle) * row_height <= usable_height {
            lower = middle;
        } else {
            upper = middle - 1;
        }
    }
    lower
}

/// 收集本帧的按键/文本/剪贴板事件,编码成发往设备 shell 的字节。
fn terminal_input(ui: &egui::Ui, screen: &vt100::Screen) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    let events = ui.input(|input| input.events.clone());
    let text_supplies_enter = events.iter().any(event_has_line_ending);
    for event in events {
        match event {
            egui::Event::Text(text) if !text.is_empty() => {
                append_text_input(&mut output, &text);
            }
            egui::Event::Paste(text) if !text.is_empty() => {
                if screen.bracketed_paste() {
                    append_terminal_input(&mut output, b"\x1b[200~");
                }
                append_terminal_input(&mut output, text.as_bytes());
                if screen.bracketed_paste() {
                    append_terminal_input(&mut output, b"\x1b[201~");
                }
            }
            egui::Event::Copy => append_terminal_input(&mut output, &[3]),
            egui::Event::Cut => append_terminal_input(&mut output, &[24]),
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if key == Key::Enter && text_supplies_enter {
                    continue;
                }
                if let Some(bytes) = key_bytes(key, modifiers, screen.application_cursor()) {
                    append_terminal_input(&mut output, &bytes);
                }
            }
            _ => {}
        }
    }
    output
}

fn event_has_line_ending(event: &egui::Event) -> bool {
    matches!(event, egui::Event::Text(text) if text.contains(['\r', '\n']))
}

fn append_text_input(output: &mut Vec<Vec<u8>>, text: &str) {
    let mut printable = String::new();
    let mut previous_was_line_ending = false;
    for character in text.chars() {
        match character {
            '\r' | '\n' => {
                if !previous_was_line_ending {
                    if !printable.is_empty() {
                        append_terminal_input(output, printable.as_bytes());
                        printable.clear();
                    }
                    append_terminal_input(output, b"\r");
                }
                previous_was_line_ending = true;
            }
            _ if character.is_control() => {}
            _ => {
                printable.push(character);
                previous_was_line_ending = false;
            }
        }
    }
    if !printable.is_empty() {
        append_terminal_input(output, printable.as_bytes());
    }
}

/// 输入按域内上限分片,保持顺序。
fn append_terminal_input(output: &mut Vec<Vec<u8>>, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        if output
            .last()
            .is_none_or(|chunk| chunk.len() == MAX_SHELL_INPUT_BYTES)
        {
            output.push(Vec::with_capacity(bytes.len().min(MAX_SHELL_INPUT_BYTES)));
        }
        let chunk = output.last_mut().expect("input chunk exists");
        let count = bytes
            .len()
            .min(MAX_SHELL_INPUT_BYTES.saturating_sub(chunk.len()));
        chunk.extend_from_slice(&bytes[..count]);
        bytes = &bytes[count..];
    }
}

fn key_bytes(key: Key, modifiers: egui::Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    let fixed: Option<&[u8]> = match key {
        Key::Enter => Some(b"\r"),
        Key::Tab => Some(b"\t"),
        Key::Backspace => Some(b"\x7f"),
        Key::Escape => Some(b"\x1b"),
        Key::ArrowUp => Some(if application_cursor {
            b"\x1bOA"
        } else {
            b"\x1b[A"
        }),
        Key::ArrowDown => Some(if application_cursor {
            b"\x1bOB"
        } else {
            b"\x1b[B"
        }),
        Key::ArrowRight => Some(if application_cursor {
            b"\x1bOC"
        } else {
            b"\x1b[C"
        }),
        Key::ArrowLeft => Some(if application_cursor {
            b"\x1bOD"
        } else {
            b"\x1b[D"
        }),
        Key::Home => Some(b"\x1b[H"),
        Key::End => Some(b"\x1b[F"),
        Key::Insert => Some(b"\x1b[2~"),
        Key::Delete => Some(b"\x1b[3~"),
        Key::PageUp => Some(b"\x1b[5~"),
        Key::PageDown => Some(b"\x1b[6~"),
        _ => None,
    };
    if let Some(bytes) = fixed {
        return Some(bytes.to_vec());
    }
    if modifiers.ctrl
        && !modifiers.shift
        && let Some(index) = control_letter(key)
    {
        return Some(vec![index]);
    }
    None
}

fn control_letter(key: Key) -> Option<u8> {
    let keys = [
        Key::A,
        Key::B,
        Key::C,
        Key::D,
        Key::E,
        Key::F,
        Key::G,
        Key::H,
        Key::I,
        Key::J,
        Key::K,
        Key::L,
        Key::M,
        Key::N,
        Key::O,
        Key::P,
        Key::Q,
        Key::R,
        Key::S,
        Key::T,
        Key::U,
        Key::V,
        Key::W,
        Key::X,
        Key::Y,
        Key::Z,
    ];
    keys.iter()
        .position(|candidate| *candidate == key)
        .and_then(|index| u8::try_from(index + 1).ok())
}

fn paint_terminal(ui: &Ui, rect: egui::Rect, state: &ShellState, focused: bool, ctx: &PanelCtx) {
    let dark_mode = ui.visuals().dark_mode;
    let palette = theme::palette(dark_mode);
    let foreground = if dark_mode {
        egui::Color32::from_rgb(220, 226, 235)
    } else {
        egui::Color32::from_rgb(24, 31, 42)
    };
    // 终端输出可能远大于固定 VT 视口,文字与光标都要裁剪在区域内。
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 4.0, palette.terminal_bg);
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(
            if focused { 2.0 } else { 1.0 },
            if focused {
                palette.chip_fill
            } else {
                ui.visuals().widgets.inactive.bg_stroke.color
            },
        ),
        egui::StrokeKind::Inside,
    );
    let screen = state.parser.screen();
    let contents = screen.contents();
    let text = if contents.is_empty() && state.session.is_none() && ctx.target.is_none() {
        "选择设备并点击「连接」".to_owned()
    } else {
        contents
    };
    let font_id = egui::FontId::monospace(TERMINAL_FONT_SIZE);
    let content_origin = rect.left_top() + egui::vec2(TERMINAL_PADDING, TERMINAL_PADDING);

    // 选区高亮
    if let Some((start, end)) = state.ordered_selection() {
        let row_height = ui.fonts_mut(|fonts| fonts.row_height(&font_id));
        let cell_width = ui.fonts_mut(|fonts| fonts.glyph_width(&font_id, 'W'));
        let highlight = if dark_mode {
            egui::Color32::from_rgba_premultiplied(60, 110, 180, 110)
        } else {
            egui::Color32::from_rgba_premultiplied(120, 170, 240, 110)
        };
        for row in start.0..=end.0 {
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1
            } else {
                TERMINAL_COLUMNS - 1
            };
            let position = content_origin
                + egui::vec2(f32::from(first) * cell_width, f32::from(row) * row_height);
            let size = egui::vec2(f32::from(last - first + 1) * cell_width, row_height);
            painter.rect_filled(egui::Rect::from_min_size(position, size), 2.0, highlight);
        }
    }

    let galley = painter.layout_no_wrap(text, font_id.clone(), foreground);
    if !galley.is_empty() {
        painter.galley(content_origin, galley.clone(), foreground);
    }
    if !screen.hide_cursor() && focused {
        let (row, column) = screen.cursor_position();
        // `Screen::contents` 会略过空白行而 VT 光标保持整网行号;不一致时
        // 最后一行就是活动的提示符/命令行,作为光标锚点最可靠。
        let placed_row = galley
            .rows
            .get(usize::from(row))
            .or_else(|| galley.rows.last());
        if let Some(placed_row) = placed_row {
            let cell_width = ui.fonts_mut(|fonts| fonts.glyph_width(&font_id, 'W'));
            let cursor_position = content_origin
                + placed_row.pos.to_vec2()
                + egui::vec2(placed_row.x_offset(usize::from(column)), 0.0);
            let cursor_size = egui::vec2(cell_width, placed_row.rect().height());
            let cursor_color = if dark_mode {
                egui::Color32::from_white_alpha(80)
            } else {
                egui::Color32::from_black_alpha(64)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(cursor_position, cursor_size),
                0.0,
                cursor_color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_output(bytes: &[u8]) -> ShellState {
        let mut state = ShellState::new(None);
        state.parser.process(bytes);
        state
    }

    #[test]
    fn selection_text_extracts_grid_range() {
        let mut state = state_with_output(b"hello world\r\nsecond line");
        state.selection = Some(((0, 0), (0, 4)));
        assert_eq!(state.selection_text(), Some("hello".to_owned()));
        state.selection = Some(((0, 6), (1, 5)));
        assert_eq!(state.selection_text(), Some("world\nsecond".to_owned()));
        state.selection = Some(((1, 0), (1, 79)));
        assert_eq!(state.selection_text(), Some("second line".to_owned()));
    }

    #[test]
    fn ordered_selection_normalizes_drag_direction() {
        let mut state = ShellState::new(None);
        state.selection = Some(((3, 10), (1, 2)));
        assert_eq!(state.ordered_selection(), Some(((1, 2), (3, 10))));
        state.selection = None;
        assert_eq!(state.ordered_selection(), None);
    }

    #[test]
    fn paste_payload_respects_bracketed_paste_mode() {
        let state = ShellState::new(None);
        assert_eq!(state.paste_payload("ls"), b"ls".to_vec());
        let state = state_with_output(b"\x1b[?2004h");
        assert_eq!(state.paste_payload("ls"), b"\x1b[200~ls\x1b[201~".to_vec());
    }

    #[test]
    fn control_keys_are_encoded() {
        assert_eq!(
            key_bytes(Key::C, egui::Modifiers::CTRL, false),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, egui::Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(Key::Enter, egui::Modifiers::NONE, false),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn terminal_input_batches_and_preserves_order() {
        let mut output = Vec::new();
        append_terminal_input(&mut output, b"hello");
        append_terminal_input(&mut output, b"\r");
        assert_eq!(output, [b"hello\r".to_vec()]);
    }

    #[test]
    fn text_input_normalizes_line_endings_to_cr() {
        let mut output = Vec::new();
        append_text_input(&mut output, "echo ready\r\n");
        assert_eq!(output, [b"echo ready\r".to_vec()]);
    }

    #[test]
    fn only_text_events_with_line_endings_suppress_enter_key_events() {
        assert!(event_has_line_ending(&egui::Event::Text("\r".to_owned())));
        assert!(!event_has_line_ending(&egui::Event::Text(
            "echo ready".to_owned()
        )));
    }

    #[test]
    fn terminal_input_splits_at_domain_limit() {
        let input = vec![b'x'; MAX_SHELL_INPUT_BYTES + 7];
        let mut output = Vec::new();
        append_terminal_input(&mut output, &input);
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].len(), MAX_SHELL_INPUT_BYTES);
        assert_eq!(output[1], vec![b'x'; 7]);
    }

    #[test]
    fn viewport_rows_fill_available_terminal_height() {
        assert_eq!(rows_for_viewport(660.0, 16.0), 40);
        assert_eq!(rows_for_viewport(20.0, 16.0), 1);
        assert_eq!(rows_for_viewport(20_000.0, 16.0), MAX_VIEWPORT_ROWS);
    }

    #[test]
    fn vt100_parser_handles_fragmented_ansi() {
        let mut state = ShellState::new(None);
        state.parser.process(b"\x1b[3");
        state.parser.process(b"1mred\x1b[0m");
        assert!(state.parser.screen().contents().contains("red"));
    }

    #[test]
    fn shell_size_is_sent_with_open_command() {
        // 视口行数会随窗口高度变化,但列数与合法性约束不变。
        assert!(ShellSize::new(80, 0).is_err());
        assert!(ShellSize::new(0, 24).is_err());
        let size = ShellSize::new(80, 24).expect("valid");
        assert_eq!(size.columns(), 80);
        assert_eq!(size.rows(), 24);
    }

    #[test]
    fn manual_reconnect_after_exit_produces_open_shell() {
        let target = DeviceTarget {
            key: fhdc_domain::ConnectKey::new("192.168.100.8:8710").expect("valid key"),
            generation: 1,
        };
        let mut state = ShellState::new(None);
        state.target = Some(target.clone());
        state.status = Status::Exited;
        let ctx = PanelCtx {
            target: Some(target),
        };
        let command = state.begin_connect(&ctx).expect("reconnect command");
        assert!(matches!(command, BackendCommand::OpenShell { .. }));
        assert_eq!(state.status, Status::Connecting);
        // 没有选中设备时不产生命令,状态也不变。
        let mut state = ShellState::new(None);
        state.status = Status::Exited;
        let empty = PanelCtx { target: None };
        assert!(state.begin_connect(&empty).is_none());
        assert_eq!(state.status, Status::Exited);
    }

    #[test]
    fn explicit_disconnect_is_not_fought_by_auto_connect() {
        let target = DeviceTarget {
            key: fhdc_domain::ConnectKey::new("192.168.100.8:8710").expect("valid key"),
            generation: 1,
        };
        let ctx = PanelCtx {
            target: Some(target.clone()),
        };
        let mut state = ShellState::new(None);
        state.target = Some(target);
        state.status = Status::Disconnected;
        // 模拟「断开」:会话清空、未连接、标记用户主动停止。
        state.user_stopped = true;
        assert!(state.maybe_auto_connect(&ctx).is_none());
        // 用户手动「连接」后抑制解除,自动连接也可再次生效。
        assert!(state.begin_connect(&ctx).is_some());
        assert!(!state.user_stopped);
        state.status = Status::Disconnected;
        assert!(state.maybe_auto_connect(&ctx).is_some());
    }
}
