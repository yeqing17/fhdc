// 把图标嵌进 exe 资源(资源管理器里的文件图标、快捷方式图标)。
// build 脚本按宿主编译;CARGO_CFG_TARGET_OS 再按目标把关,非 Windows
// 目标保持空实现,与 target 门控的 build-dependencies 一致。
#[cfg(windows)]
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/fhdc.ico")
            .compile()
            .expect("嵌入 Windows 图标失败");
    }
}

#[cfg(not(windows))]
fn main() {}
