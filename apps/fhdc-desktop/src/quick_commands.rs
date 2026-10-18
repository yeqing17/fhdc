//! 终端快捷指令:本地持久化,支持 JSON 导入/导出。

use eframe::egui;
use eframe::egui::Ui;
use serde::Deserialize;
use serde::Serialize;

use fhdc_domain::MAX_SHELL_INPUT_BYTES;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommand {
    pub label: String,
    pub command: String,
    #[serde(default = "default_run")]
    pub run: bool,
}

fn default_run() -> bool {
    true
}

impl QuickCommand {
    pub fn display_label(&self) -> String {
        let source = if self.label.trim().is_empty() {
            &self.command
        } else {
            &self.label
        };
        if source.chars().count() > 24 {
            let prefix: String = source.chars().take(24).collect();
            format!("{prefix}…")
        } else {
            source.clone()
        }
    }

    pub fn is_sendable(&self) -> bool {
        !self.command.trim().is_empty()
    }

    /// 命令字节 + 可选回车,按输入上限分片。
    pub fn payload_chunks(&self) -> Vec<Vec<u8>> {
        let mut bytes = self.command.clone().into_bytes();
        if self.run {
            bytes.push(b'\n');
        }
        bytes
            .chunks(MAX_SHELL_INPUT_BYTES)
            .filter(|chunk| !chunk.is_empty())
            .map(<[u8]>::to_vec)
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct QuickCommandStore {
    pub commands: Vec<QuickCommand>,
    pub manage_open: bool,
    pub status: Option<String>,
}

fn defaults() -> Vec<QuickCommand> {
    vec![
        QuickCommand {
            label: "设备型号".to_owned(),
            command: "param get const.product.model".to_owned(),
            run: true,
        },
        QuickCommand {
            label: "系统版本".to_owned(),
            command: "param get const.product.software.version".to_owned(),
            run: true,
        },
        QuickCommand {
            label: "内存".to_owned(),
            command: "cat /proc/meminfo".to_owned(),
            run: true,
        },
    ]
}

impl QuickCommandStore {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let commands = storage
            .and_then(|storage| storage.get_string("fhdc.shell_quick_commands"))
            .and_then(|value| serde_json::from_str::<Vec<QuickCommand>>(&value).ok())
            .unwrap_or_else(defaults);
        Self {
            commands,
            ..Self::default()
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        let sendable: Vec<&QuickCommand> = self
            .commands
            .iter()
            .filter(|command| command.is_sendable())
            .collect();
        if let Ok(value) = serde_json::to_string(&sendable) {
            storage.set_string("fhdc.shell_quick_commands", value);
        }
    }

    /// 工具栏内联条:点击即发送。
    pub fn show_inline(&mut self, ui: &mut Ui, send: &mut dyn FnMut(Vec<u8>)) {
        let mut overflow: Vec<&QuickCommand> = Vec::new();
        ui.horizontal_wrapped(|ui| {
            let mut shown = 0;
            for command in &self.commands {
                if !command.is_sendable() {
                    continue;
                }
                if shown >= 8 {
                    overflow.push(command);
                    continue;
                }
                shown += 1;
                if ui.small_button(command.display_label()).clicked() {
                    for chunk in command.payload_chunks() {
                        send(chunk);
                    }
                }
            }
            if !overflow.is_empty() {
                ui.menu_button("…", |ui| {
                    for command in &overflow {
                        if ui.button(command.display_label()).clicked() {
                            for chunk in command.payload_chunks() {
                                send(chunk);
                            }
                            ui.close();
                        }
                    }
                });
            }
            if ui.small_button("管理").clicked() {
                self.manage_open = !self.manage_open;
            }
        });
    }

    pub fn show_manage(&mut self, ui: &mut Ui) {
        if !self.manage_open {
            return;
        }
        egui::Window::new("快捷指令管理")
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        let mut remove: Option<usize> = None;
                        let mut swap: Option<(usize, usize)> = None;
                        let command_count = self.commands.len();
                        for (index, command) in self.commands.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(true, |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut command.label)
                                            .desired_width(110.0)
                                            .hint_text("名称"),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut command.command)
                                            .desired_width(230.0)
                                            .hint_text("命令"),
                                    );
                                    ui.checkbox(&mut command.run, "回车");
                                });
                                if ui.small_button("↑").clicked() && index > 0 {
                                    swap = Some((index - 1, index));
                                }
                                if ui.small_button("↓").clicked() && index + 1 < command_count {
                                    swap = Some((index, index + 1));
                                }
                                if ui.small_button("×").clicked() {
                                    remove = Some(index);
                                }
                            });
                        }
                        if let Some(index) = remove {
                            self.commands.remove(index);
                        }
                        if let Some((a, b)) = swap {
                            self.commands.swap(a, b);
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("新增").clicked() {
                        self.commands.push(QuickCommand {
                            label: String::new(),
                            command: String::new(),
                            run: true,
                        });
                    }
                    if ui.button("导入 JSON…").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("JSON", &["json"])
                            .pick_file()
                    {
                        match import_json(&path) {
                            Ok(imported) => {
                                // 快照 + 循环内同步更新,同一文件内的重复条目也去重。
                                let mut existing: Vec<(String, String)> = self
                                    .commands
                                    .iter()
                                    .map(|command| (command.label.clone(), command.command.clone()))
                                    .collect();
                                let mut added = 0;
                                for command in imported {
                                    if command.is_sendable() {
                                        let key = (command.label.clone(), command.command.clone());
                                        if !existing.contains(&key) {
                                            existing.push(key);
                                            self.commands.push(command);
                                            added += 1;
                                        }
                                    }
                                }
                                self.status = Some(format!("已导入 {added} 条"));
                            }
                            Err(error) => self.status = Some(format!("导入失败: {error}")),
                        }
                    }
                    if ui.button("导出 JSON…").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .set_file_name("fhdc-quick-commands.json")
                            .add_filter("JSON", &["json"])
                            .save_file()
                    {
                        match export_json(self, &path) {
                            Ok(()) => self.status = Some("已导出".to_owned()),
                            Err(error) => self.status = Some(format!("导出失败: {error}")),
                        }
                    }
                    if ui.button("关闭").clicked() {
                        self.manage_open = false;
                    }
                });
                if let Some(status) = &self.status {
                    ui.label(egui::RichText::new(status).small().weak());
                }
            });
    }
}

#[derive(Serialize, Deserialize)]
struct QuickCommandFile {
    version: u32,
    commands: Vec<QuickCommand>,
}

fn import_json(path: &std::path::Path) -> Result<Vec<QuickCommand>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let commands = match &parsed {
        serde_json::Value::Array(_) => serde_json::from_value::<Vec<QuickCommand>>(parsed)
            .map_err(|error| error.to_string())?,
        value => {
            let file = serde_json::from_value::<QuickCommandFile>(value.clone())
                .map_err(|error| error.to_string())?;
            file.commands
        }
    };
    Ok(commands)
}

fn export_json(store: &QuickCommandStore, path: &std::path::Path) -> Result<(), String> {
    let file = QuickCommandFile {
        version: 1,
        commands: store
            .commands
            .iter()
            .filter(|command| command.is_sendable())
            .cloned()
            .collect(),
    };
    let text = serde_json::to_string_pretty(&file).map_err(|error| error.to_string())?;
    std::fs::write(path, text).map_err(|error| error.to_string())
}
