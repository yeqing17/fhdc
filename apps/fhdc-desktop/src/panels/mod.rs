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
