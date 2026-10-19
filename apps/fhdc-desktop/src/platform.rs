//! egui 没有暴露的原生窗口装饰调整。
//!
//! 应用自绘标题栏时,窗口是无边框的,Windows 会画成尖角且没有阴影。
//! 两次只读的 DWM 调用能修正:Windows 11 的圆角偏好,以及向下扩展
//! 1px 边框让 DWM 画出投影(只留底部边距,可以避开 winit 自带的
//! 无边框阴影辅助函数在顶部留下的 1px 亮线)。两者都是尽力而为——
//! Windows 10 不支持圆角属性,调用会静默失败。其它平台此模块为空实现。

// 工作区禁用 `unsafe_code`;本模块是唯一经过评审的例外
// (两次针对自有窗口的 DWM 合成调用)。
#![allow(unsafe_code)]

/// 标题栏样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitleBarStyle {
    /// 应用自绘:窗口无边框,顶部工具栏兼任标题栏(默认)。
    #[default]
    Custom,
    /// 使用 Windows 原生标题栏。
    Native,
}

impl TitleBarStyle {
    /// 持久化用的短标记。
    fn token(self) -> &'static str {
        match self {
            TitleBarStyle::Custom => "custom",
            TitleBarStyle::Native => "native",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token.trim() {
            "custom" => Some(TitleBarStyle::Custom),
            "native" => Some(TitleBarStyle::Native),
            _ => None,
        }
    }
}

/// 标题栏选择的存放位置。
///
/// 不放在 eframe 的持久化里:eframe 的 storage 到 `CreationContext`
/// 才可用,而窗口装饰必须在窗口创建之前定下来——那时读不到。
/// 自己存一份小文件,顺带让命令行参数能覆盖它。
fn style_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(std::path::PathBuf::from)?;
    Some(base.join("fhdc").join("data").join("titlebar"))
}

/// 读取标题栏样式;没有记录时用自绘。
pub fn load_title_bar_style() -> TitleBarStyle {
    let Some(path) = style_path() else {
        return TitleBarStyle::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .as_deref()
        .and_then(TitleBarStyle::from_token)
        .unwrap_or_default()
}

/// 保存标题栏样式(下次启动生效)。
pub fn save_title_bar_style(style: TitleBarStyle) {
    let Some(path) = style_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, style.token());
}

/// 命令行参数覆盖:`--native-titlebar` / `--custom-titlebar`。
///
/// 供"切换后立即重启"使用:重启时带上参数,不必等用户下次手动启动。
pub fn title_bar_style_from_args() -> Option<TitleBarStyle> {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--native-titlebar" => return Some(TitleBarStyle::Native),
            "--custom-titlebar" => return Some(TitleBarStyle::Custom),
            _ => {}
        }
    }
    None
}

/// 为应用主窗口申请圆角与投影。
pub fn apply_window_chrome(creation_context: &eframe::CreationContext<'_>) {
    #[cfg(windows)]
    {
        use raw_window_handle::HasWindowHandle;
        use raw_window_handle::RawWindowHandle;
        if let Ok(handle) = creation_context.window_handle()
            && let RawWindowHandle::Win32(win) = handle.as_raw()
        {
            rounded_corners_and_shadow(win.hwnd.get() as *mut std::ffi::c_void);
        }
    }
    #[cfg(not(windows))]
    let _ = creation_context;
}

#[cfg(windows)]
fn rounded_corners_and_shadow(hwnd: *mut std::ffi::c_void) {
    use std::mem::size_of_val;
    use windows_sys::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE;
    use windows_sys::Win32::Graphics::Dwm::DWMWCP_ROUND;
    use windows_sys::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::UI::Controls::MARGINS;

    // SAFETY:两次调用只描述 DWM 应如何合成我们自己的窗口(圆角形状与
    // 边框边距),既不读也不写进程数据。失败(如 Windows 10)时保持方角。
    unsafe {
        let preference = DWMWCP_ROUND;
        let result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            std::ptr::from_ref(&preference).cast(),
            u32::try_from(size_of_val(&preference)).unwrap_or_default(),
        );
        if result != 0 {
            tracing::debug!("DwmSetWindowAttribute(圆角) 失败: {result}");
        }
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 0,
            cyBottomHeight: 1,
        };
        let result = DwmExtendFrameIntoClientArea(hwnd, &raw const margins);
        if result != 0 {
            tracing::debug!("DwmExtendFrameIntoClientArea(阴影) 失败: {result}");
        }
    }
}
