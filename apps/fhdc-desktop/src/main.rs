//! fhdc 入口:tracing 初始化 + eframe 启动。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod panels;
mod quick_commands;
mod runtime;
mod theme;

use tracing_subscriber::EnvFilter;

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
        viewport: app::viewport_builder(),
        ..Default::default()
    };
    eframe::run_native(
        "fhdc",
        options,
        Box::new(|creation_context| Ok(Box::new(app::FhdcApp::new(creation_context)))),
    )
}
