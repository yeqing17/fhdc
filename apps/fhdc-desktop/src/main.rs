//! fhdc 入口:tracing 初始化 + eframe 启动。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod panels;
mod quick_commands;
mod runtime;
mod theme;

use tracing_subscriber::EnvFilter;

fn main() -> eframe::Result<()> {
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
