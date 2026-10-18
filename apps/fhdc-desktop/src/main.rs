//! fhdc 入口:tracing 初始化 + eframe 启动。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod panels;
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
        width: width.try_into().expect("图标宽度在 u32 范围内"),
        height: height.try_into().expect("图标高度在 u32 范围内"),
        rgba: img.into_raw(),
    }
}

fn main() -> eframe::Result<()> {
    // 上一次应用内升级换名留下的旧二进制:换名只是为了运行中的 exe 能
    // 被替换,此刻我们已是新进程,旧文件纯属占地方。
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fhdc=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let options = eframe::NativeOptions {
        viewport: app::viewport_builder().with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "fhdc",
        options,
        Box::new(|creation_context| Ok(Box::new(app::FhdcApp::new(creation_context)))),
    )
}
