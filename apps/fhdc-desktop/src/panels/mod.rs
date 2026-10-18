//! 面板:每个面板一个状态结构 + show/handle_event/reset_for 三件套。

pub mod applications;
pub mod files;
pub mod hilog;
pub mod overview;
pub mod processes;
pub mod screenshot;
pub mod shell;

use fhdc_domain::BackendCommand;
use fhdc_domain::DeviceTarget;

/// 面板共享的每帧上下文。
pub struct PanelCtx {
    /// 当前选中且在线的设备目标。
    pub target: Option<DeviceTarget>,
}

impl PanelCtx {
    pub fn online(&self) -> bool {
        self.target.is_some()
    }
}

/// 面板统一返回待发送命令。
pub type Commands = Vec<BackendCommand>;

/// 增量 UTF-8 解码器:底层按块转发,一个多字节汉字可能被切在两块之间,
/// 逐块 `from_utf8_lossy` 会产生不可逆的 U+FFFD。这里把不完整的字符
/// 前缀留到下一块续接。
#[derive(Debug, Default)]
pub struct Utf8Accumulator {
    pending: Vec<u8>,
}

impl Utf8Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一块字节,返回当前可安全解码的文本。
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    self.pending.clear();
                    return text;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&self.pending[..valid]));
                    match error.error_len() {
                        // None = 只是字符前缀不完整,保留等待下一块。
                        None => {
                            self.pending.drain(..valid);
                            return text;
                        }
                        // 真正的非法字节:跳过后继续解码剩余部分。
                        Some(invalid) => {
                            self.pending.drain(..valid + invalid);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_joins_multibyte_characters_across_chunks() {
        let mut accumulator = Utf8Accumulator::new();
        // "中" = E4 B8 AD,故意按字节切开。
        assert_eq!(accumulator.push(&[0xE4]), "");
        assert_eq!(accumulator.push(&[0xB8]), "");
        assert_eq!(accumulator.push(&[0xAD, b'a']), "中a");
        // 非法字节被跳过,后续照常解码。
        assert_eq!(accumulator.push(&[0xFF, b'o']), "o");
    }
}
