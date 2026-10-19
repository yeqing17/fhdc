//! fhdc 入口:tracing 初始化 + eframe 启动。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod panels;
mod platform;
mod quick_commands;
mod runtime;
mod theme;

use tracing_subscriber::EnvFilter;

/// 窗口/任务栏图标,编译期内嵌自 assets/(资源管理器里的 exe 文件
/// 图标由 build.rs 嵌入的 ico 提供)。
fn app_icon() -> eframe::egui::IconData {
    let png = include_bytes!("../assets/icon-256.png");
    let img = image::load_from_memory(png)
        .expect("内嵌图标必须是有效 PNG")
        .to_rgba8();
    let (width, height) = img.dimensions();
    eframe::egui::IconData {
        width,
        height,
        rgba: img.into_raw(),
    }
}

fn main() -> eframe::Result<()> {
    // 上一次应用内升级换名留下的旧二进制:换名只是为了运行中的 exe 能
    // 被替换,此刻我们已是新进程,旧文件纯属占地方。
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }

    // 窗口装饰必须在窗口创建前定下来:命令行参数优先,其次是上次保存
    // 的选择,默认自绘标题栏。
    let title_bar =
        platform::title_bar_style_from_args().unwrap_or_else(platform::load_title_bar_style);
    let native_titlebar = title_bar == platform::TitleBarStyle::Native;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fhdc=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let options = eframe::NativeOptions {
        viewport: app::viewport_builder(native_titlebar).with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "fhdc",
        options,
        Box::new(|creation_context| {
            // 无边框窗口在 Windows 上是尖角且没有投影,交给 DWM 修正。
            platform::apply_window_chrome(creation_context);
            Ok(Box::new(app::FhdcApp::new(
                creation_context,
                native_titlebar,
            )))
        }),
    )
}
