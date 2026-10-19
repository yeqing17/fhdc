//! 假后端:完整实现 [`HdcTransport`],供无设备开发与演示。
//!
//! 与 fadb 的 fake 策略一致——同一 trait、内存态、随环境变量接入生产运行时。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use fhdc_domain::{
    AppAction, BridgeError, BundleDetails, BundleName, BundlePackage, BundleRecord, ConnectKey,
    DeviceDescriptor, DeviceOverview, DeviceState, Endpoint, ErrorCode, OverwritePolicy,
    ProcessInfo, RemoteFileEntry, RemoteFileKind, RemotePath, ShellOutputChunk, ShellSize,
    ShellStream,
};
use fhdc_hdc::HdcTransport;
use fhdc_hdc::ShellSessionHandle;

#[derive(Debug, Clone)]
struct FakeEntry {
    name: String,
    directory: bool,
    size: u64,
}

#[derive(Default)]
struct FakeState {
    devices: Vec<(String, DeviceState)>,
    directories: Vec<(String, Vec<FakeEntry>)>,
    bundles: Vec<BundleRecord>,
}

impl FakeState {
    fn seed() -> Self {
        let mut directories = Vec::new();
        let mut populate = |dir: &str, entries: &[(&str, bool, u64)]| {
            directories.push((
                dir.to_owned(),
                entries
                    .iter()
                    .map(|(name, directory, size)| FakeEntry {
                        name: (*name).to_owned(),
                        directory: *directory,
                        size: *size,
                    })
                    .collect(),
            ));
        };
        populate(
            "/",
            &[
                ("data", true, 0),
                ("system", true, 0),
                ("vendor", true, 0),
                ("proc", true, 0),
            ],
        );
        populate(
            "/data",
            &[("local", true, 0), ("service", true, 0), ("app", true, 0)],
        );
        populate(
            "/data/local/tmp",
            &[
                ("demo.hap", false, 1_234_567),
                ("日志.txt", false, 2048),
                ("dump", true, 0),
            ],
        );
        Self {
            devices: vec![
                (
                    "USER_default__f4a6e1b2c3d4059876a5b4c3d2e1f0a9".to_owned(),
                    DeviceState::Connected,
                ),
                ("192.168.100.100:5555".to_owned(), DeviceState::Connected),
            ],
            directories,
            bundles: vec![
                BundleRecord {
                    system: true,
                    name: BundleName::new("com.ohos.launcher").unwrap(),
                },
                BundleRecord {
                    system: true,
                    name: BundleName::new("com.ohos.settings").unwrap(),
                },
                BundleRecord {
                    system: false,
                    name: BundleName::new("com.example.demo").unwrap(),
                },
                BundleRecord {
                    system: false,
                    name: BundleName::new("com.example.helloworld").unwrap(),
                },
            ],
        }
    }
}

#[derive(Default, Clone)]
pub struct FakeHdcTransport {
    state: Arc<Mutex<FakeState>>,
}

impl FakeHdcTransport {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState::seed())),
        }
    }

    async fn require_key(&self, key: &ConnectKey) -> Result<(), BridgeError> {
        let state = self.state.lock().await;
        state
            .devices
            .iter()
            .any(|(candidate, state)| candidate == key.as_str() && state.is_online())
            .then_some(())
            .ok_or_else(|| {
                BridgeError::new(
                    ErrorCode::DeviceNotFound,
                    format!("假后端中没有设备 {}", key.as_str()),
                )
            })
    }

    async fn fake_shell_response(&self, command: &str) -> String {
        let command = command.trim();
        let state = self.state.lock().await;
        match command {
            "param get const.product.model" => "rk3568 (fake)".to_owned(),
            "param get const.product.brand" => "HiSilicon (fake)".to_owned(),
            "param get const.product.devicetype" => "default".to_owned(),
            "param get const.product.software.version" => "OpenHarmony 5.0.2 (fake)".to_owned(),
            "param get const.ohos.apiversion" => "12".to_owned(),
            "uname -r" => "5.10.79 (fake)".to_owned(),
            "grep -c '^processor' /proc/cpuinfo" => "4".to_owned(),
            "cat /proc/meminfo" => {
                "MemTotal:        3992948 kB\nMemFree:          812345 kB\nMemAvailable:   2100000 kB\n".to_owned()
            }
            "df -k /data" => "/dev/block/sda10  25394624 12345676  11775148  52% /data".to_owned(),
            "ps -ef" => fake_processes().join("\n"),
            "bm dump -a" => state
                .bundles
                .iter()
                .map(|bundle| bundle.name.as_str().to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
            "hilog -r" => String::new(),
            other if other.starts_with("bm dump -n ") => format!(
                "[\n  {{\n    \"appId\": \"{name}___fake\",\n    \"versionCode\": 1000000,\n    \"versionName\": \"1.0.0\",\n    \"mainAbility\": \"EntryAbility\",\n    \"installTime\": 1719000000000,\n    \"isSystemApp\": {system}\n  }}\n]",
                name = other.trim_start_matches("bm dump -n "),
                system = state.bundles.iter().any(|b| b.name.as_str() == other.trim_start_matches("bm dump -n ") && b.system),
            ),
            other => format!("{other}: 未找到命令(假后端)"),
        }
    }
}

fn fake_processes() -> Vec<String> {
    vec![
        "UID            PID  PPID C STIME TTY    TIME     CMD".to_owned(),
        "root             1     0 0 08:00 ?      00:00:02 init".to_owned(),
        "root             2     0 0 08:00 ?      00:00:00 [kthreadd]".to_owned(),
        "system         250     1 3 08:00 ?      00:00:41 foundation".to_owned(),
        "system         410     1 1 08:00 ?      00:00:12 com.ohos.launcher".to_owned(),
        "system         666   250 7 08:01 ?      00:01:09 com.example.demo".to_owned(),
    ]
}

#[async_trait]
impl HdcTransport for FakeHdcTransport {
    async fn version(&self) -> Result<String, BridgeError> {
        Ok("Ver: 1.2.0a (fake)".to_owned())
    }

    async fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, BridgeError> {
        let state = self.state.lock().await;
        Ok(state
            .devices
            .iter()
            .filter_map(|(key, state)| {
                Some(DeviceDescriptor {
                    key: ConnectKey::new(key).ok()?,
                    state: *state,
                })
            })
            .collect())
    }

    async fn connect_endpoint(&self, endpoint: &Endpoint) -> Result<String, BridgeError> {
        let target = endpoint.target_string();
        let mut state = self.state.lock().await;
        if state.devices.iter().any(|(key, _)| *key == target) {
            return Ok("Connect OK (已连接)".to_owned());
        }
        state.devices.push((target, DeviceState::Connected));
        Ok("Connect OK (fake)".to_owned())
    }

    async fn disconnect_endpoint(&self, endpoint: &Endpoint) -> Result<String, BridgeError> {
        let target = endpoint.target_string();
        let mut state = self.state.lock().await;
        state.devices.retain(|(key, _)| *key != target);
        Ok(format!("Disconnect OK (fake): {target}"))
    }

    async fn enable_tcp_listen(&self, _key: &ConnectKey, port: u16) -> Result<String, BridgeError> {
        Ok(format!(
            "设备开始监听 TCP {port} 端口(fake);随后用 hdc tconn <设备IP>:{port} 连接"
        ))
    }

    async fn kill_server(&self) -> Result<String, BridgeError> {
        Ok("hdc 服务已停止(fake),下一次命令会自动拉起".to_owned())
    }

    async fn device_overview(&self, key: &ConnectKey) -> Result<DeviceOverview, BridgeError> {
        self.require_key(key).await?;
        Ok(DeviceOverview {
            model: Some("rk3568 (fake)".to_owned()),
            brand: Some("Rockchip (fake)".to_owned()),
            device_type: Some("default".to_owned()),
            software_version: Some("OpenHarmony 5.0.2".to_owned()),
            api_version: Some("12".to_owned()),
            kernel_version: Some("5.10.79".to_owned()),
            cpu_cores: Some(4),
            memory_total_kib: Some(3_992_948),
            memory_available_kib: Some(2_100_000),
            storage_total_kib: Some(25_394_624),
            storage_used_kib: Some(12_345_676),
        })
    }

    async fn list_processes(&self, key: &ConnectKey) -> Result<Vec<ProcessInfo>, BridgeError> {
        self.require_key(key).await?;
        let output = fake_processes().join("\n");
        Ok(fhdc_fake_parse_processes(&output))
    }

    async fn list_bundles(&self, key: &ConnectKey) -> Result<Vec<BundleRecord>, BridgeError> {
        self.require_key(key).await?;
        Ok(self.state.lock().await.bundles.clone())
    }

    async fn list_bundle_packages(
        &self,
        key: &ConnectKey,
    ) -> Result<Vec<BundlePackage>, BridgeError> {
        self.require_key(key).await?;
        // 假后端没有安装物,返回空清单;图标加载会立即结束。
        Ok(Vec::new())
    }

    async fn bundle_details(
        &self,
        key: &ConnectKey,
        name: &BundleName,
    ) -> Result<BundleDetails, BridgeError> {
        self.require_key(key).await?;
        let system = self
            .state
            .lock()
            .await
            .bundles
            .iter()
            .find(|b| b.name == *name)
            .map(|b| b.system);
        Ok(BundleDetails {
            name: name.clone(),
            version_name: Some("1.0.0".to_owned()),
            version_code: Some("1000000".to_owned()),
            main_ability: Some("EntryAbility".to_owned()),
            app_id: Some(format!("{}___fake", name.as_str())),
            install_time: Some("1719000000000".to_owned()),
            update_time: Some("1719000000000".to_owned()),
            system,
        })
    }

    async fn run_app_action(
        &self,
        key: &ConnectKey,
        name: &BundleName,
        action: AppAction,
    ) -> Result<String, BridgeError> {
        self.require_key(key).await?;
        match action {
            AppAction::Launch => Ok(format!(
                "start ability successfully. (fake {})",
                name.as_str()
            )),
            AppAction::ForceStop => Ok("force stop successfully. (fake)".to_owned()),
            AppAction::ClearData => Ok("clean bundle data successfully. (fake)".to_owned()),
            AppAction::Uninstall => {
                let mut state = self.state.lock().await;
                state.bundles.retain(|bundle| bundle.name != *name);
                Ok("uninstall bundle successfully. (fake)".to_owned())
            }
        }
    }

    async fn install_hap(&self, key: &ConnectKey, path: &Path) -> Result<String, BridgeError> {
        self.require_key(key).await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("com.example.installed");
        let name =
            BundleName::new(stem).map_err(|_| BridgeError::invalid_input("文件名不是合法包名"))?;
        let mut state = self.state.lock().await;
        if !state.bundles.iter().any(|bundle| bundle.name == name) {
            state.bundles.push(BundleRecord {
                system: false,
                name,
            });
        }
        Ok("install bundle successfully. (fake)".to_owned())
    }

    async fn start_shell(
        &self,
        key: &ConnectKey,
        _size: ShellSize,
    ) -> Result<ShellSessionHandle, BridgeError> {
        self.require_key(key).await?;
        // 克隆共享同一份内存状态,否则 default() 的空状态会让假终端
        // 查不到包列表等数据。
        let transport = self.clone();
        Ok(ShellSessionHandle::from_handler(
            |mut input, output, cancellation| async move {
                let _ = output
                    .send(ShellOutputChunk {
                        stream: ShellStream::Stdout,
                        bytes: b"OpenHarmony fake shell \r\n# ".to_vec(),
                    })
                    .await;
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok(Some(0)),
                        chunk = input.recv() => {
                            let Some(chunk) = chunk else { return Ok(Some(0)) };
                            let text = String::from_utf8_lossy(&chunk).trim_end().to_owned();
                            let response = transport.fake_shell_response(&text).await;
                            let payload = format!("{response}\r\n# ");
                            if output
                                .send(ShellOutputChunk { stream: ShellStream::Stdout, bytes: payload.into_bytes() })
                                .await
                                .is_err()
                            {
                                return Ok(Some(0));
                            }
                        }
                    }
                }
            },
        ))
    }

    async fn start_hilog(&self, key: &ConnectKey) -> Result<ShellSessionHandle, BridgeError> {
        self.require_key(key).await?;
        Ok(ShellSessionHandle::from_handler(
            |input, output, cancellation| async move {
                drop(input);
                let mut counter: u32 = 0;
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok(None),
                        _ = tokio::time::sleep(Duration::from_millis(260)) => {
                            counter += 1;
                            let line = format!(
                                "10-18 09:{:02}:{:02}.{:03}  1234  1234 {} C01800/hilog_fake: 演示日志行 #{} — OpenHarmony 设备模拟输出\n",
                                (counter / 600) % 60, (counter / 10) % 60, (counter % 10) * 111,
                                match counter % 11 { 0 => 'W', 1..=3 => 'I', 4 => 'E', 5 => 'D', _ => 'I' },
                                counter,
                            );
                            if output
                                .send(ShellOutputChunk { stream: ShellStream::Stdout, bytes: line.into_bytes() })
                                .await
                                .is_err()
                            {
                                return Ok(None);
                            }
                        }
                    }
                }
            },
        ))
    }

    async fn clear_hilog(&self, key: &ConnectKey) -> Result<(), BridgeError> {
        self.require_key(key).await
    }

    async fn run_shell_once(&self, key: &ConnectKey, command: &str) -> Result<String, BridgeError> {
        self.require_key(key).await?;
        Ok(self.fake_shell_response(command).await)
    }

    async fn list_directory(
        &self,
        key: &ConnectKey,
        path: &RemotePath,
    ) -> Result<Vec<RemoteFileEntry>, BridgeError> {
        self.require_key(key).await?;
        let state = self.state.lock().await;
        let entries = state
            .directories
            .iter()
            .find(|(dir, _)| dir == path.as_str())
            .map(|(_, entries)| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let full = path.join_name(&entry.name).ok()?;
                        Some(RemoteFileEntry {
                            path: full,
                            name: entry.name.clone(),
                            kind: if entry.directory {
                                RemoteFileKind::Directory
                            } else {
                                RemoteFileKind::File
                            },
                            size_bytes: (!entry.directory).then_some(entry.size),
                            modified_unix_seconds: Some(1_728_800_000),
                            permissions: Some(if entry.directory {
                                "drwxr-xr-x".to_owned()
                            } else {
                                "-rw-r--r--".to_owned()
                            }),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(entries)
    }

    async fn push_file(
        &self,
        key: &ConnectKey,
        local: &Path,
        remote: &RemotePath,
        overwrite: OverwritePolicy,
        cancellation: &CancellationToken,
    ) -> Result<(), BridgeError> {
        self.require_key(key).await?;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(700)) => {}
            () = cancellation.cancelled() => return Err(BridgeError::new(ErrorCode::Cancelled, "操作已取消")),
        }
        let size = local.metadata().map(|m| m.len()).unwrap_or(1024);
        let mut state = self.state.lock().await;
        if overwrite == OverwritePolicy::Deny
            && let Some(parent) = remote.parent()
        {
            let parent = parent.as_str();
            let file_name = remote.file_name().unwrap_or("");
            if let Some((_, entries)) = state.directories.iter().find(|(dir, _)| dir == parent)
                && entries.iter().any(|entry| entry.name == file_name)
            {
                return Err(BridgeError::new(
                    ErrorCode::AlreadyExists,
                    "远端已存在(fake)",
                ));
            }
        }
        if let Some(parent) = remote.parent() {
            let name = remote.file_name().unwrap_or("file").to_owned();
            let parent = parent.as_str().to_owned();
            if let Some((_, entries)) = state.directories.iter_mut().find(|(dir, _)| *dir == parent)
            {
                entries.retain(|entry| entry.name != name);
                entries.push(FakeEntry {
                    name,
                    directory: false,
                    size,
                });
            }
        }
        Ok(())
    }

    async fn pull_file(
        &self,
        key: &ConnectKey,
        _remote: &RemotePath,
        local: &Path,
        overwrite: OverwritePolicy,
        cancellation: &CancellationToken,
    ) -> Result<(), BridgeError> {
        self.require_key(key).await?;
        // 与真后端同语义:本地已存在 + Deny(或符号链接)即拒绝,
        // 让 UI 的覆盖确认流程在假后端下也能演练。
        if local.exists() && (local.is_symlink() || overwrite == OverwritePolicy::Deny) {
            return Err(BridgeError::new(
                ErrorCode::AlreadyExists,
                format!("本地已存在: {}", local.display()),
            ));
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(700)) => {}
            () = cancellation.cancelled() => return Err(BridgeError::new(ErrorCode::Cancelled, "操作已取消")),
        }
        tokio::fs::write(local, b"fake downloaded file\n")
            .await
            .map_err(|error| {
                BridgeError::new(ErrorCode::Internal, format!("写入本地文件失败: {error}"))
            })
    }

    async fn create_directory(
        &self,
        key: &ConnectKey,
        path: &RemotePath,
    ) -> Result<(), BridgeError> {
        self.require_key(key).await?;
        let mut state = self.state.lock().await;
        if state
            .directories
            .iter()
            .any(|(dir, _)| dir == path.as_str())
        {
            return Err(BridgeError::new(
                ErrorCode::AlreadyExists,
                "目录已存在(fake)",
            ));
        }
        state
            .directories
            .push((path.as_str().to_owned(), Vec::new()));
        Ok(())
    }

    async fn rename_entry(
        &self,
        key: &ConnectKey,
        from: &RemotePath,
        to: &RemotePath,
    ) -> Result<(), BridgeError> {
        self.require_key(key).await?;
        let mut state = self.state.lock().await;
        let name = from.file_name().unwrap_or("").to_owned();
        let to_name = to.file_name().unwrap_or("").to_owned();
        let parent = from
            .parent()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_default();
        if let Some((_, entries)) = state.directories.iter_mut().find(|(dir, _)| *dir == parent) {
            if !entries.iter().any(|entry| entry.name == name) {
                return Err(BridgeError::new(
                    ErrorCode::PathNotFound,
                    "路径不存在(fake)",
                ));
            }
            for entry in entries.iter_mut() {
                if entry.name == name {
                    entry.name = to_name.clone();
                }
            }
            return Ok(());
        }
        Err(BridgeError::new(
            ErrorCode::PathNotFound,
            "目录不存在(fake)",
        ))
    }

    async fn delete_entry(&self, key: &ConnectKey, path: &RemotePath) -> Result<(), BridgeError> {
        self.require_key(key).await?;
        let mut state = self.state.lock().await;
        // 递归语义与真后端对齐:目录下的子目录一并移除。
        let prefix = format!("{}/", path.as_str().trim_end_matches('/'));
        state
            .directories
            .retain(|(dir, _)| dir != path.as_str() && !dir.starts_with(&prefix));
        if let Some(parent) = path.parent() {
            let name = path.file_name().unwrap_or("").to_owned();
            let parent = parent.as_str().to_owned();
            if let Some((_, entries)) = state.directories.iter_mut().find(|(dir, _)| *dir == parent)
            {
                entries.retain(|entry| entry.name != name);
            }
        }
        Ok(())
    }

    async fn capture_screenshot(&self, key: &ConnectKey) -> Result<Vec<u8>, BridgeError> {
        self.require_key(key).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        generate_fake_screenshot()
    }
}

/// 与真实后端同形状的 ps 解析,避免逻辑分叉。
fn fhdc_fake_parse_processes(output: &str) -> Vec<ProcessInfo> {
    // 复用真实解析器形状,避免逻辑分叉。
    let mut processes = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[0].eq_ignore_ascii_case("UID") {
            continue;
        }
        let Ok(pid) = fields[1].parse() else { continue };
        processes.push(ProcessInfo {
            pid,
            user: Some(fields[0].to_owned()),
            cpu_percent: fields[3].parse().ok(),
            command: fields[7..].join(" "),
        });
    }
    processes
}

fn generate_fake_screenshot() -> Result<Vec<u8>, BridgeError> {
    let width = 720u32;
    let height = 1280u32;
    let mut buffer = image::GrayImage::new(width, height);
    for (x, y, pixel) in buffer.enumerate_pixels_mut() {
        let value = ((x * 255) / width + (y * 255) / height) as u8;
        *pixel = image::Luma([value]);
    }
    let mut bytes = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85);
    image::DynamicImage::ImageLuma8(buffer)
        .write_with_encoder(encoder)
        .map_err(|error| {
            BridgeError::new(ErrorCode::Internal, format!("生成假截图失败: {error}"))
        })?;
    Ok(bytes)
}
