//! fhdc 领域词汇层:类型、错误与后端命令/事件。
//!
//! 本 crate 不做任何 I/O,仅定义 fhdc 各层共享的数据形状。仿照 fadb 的
//! 分层:校验型 newtype 在构造处拒绝非法值,从而充当注入防护。

use std::path::PathBuf;

use thiserror::Error;
use uuid::Uuid;

/// 单次写入 shell stdin 的上限,防止一次事件撑爆通道。
pub const MAX_SHELL_INPUT_BYTES: usize = 64 * 1024;
/// 单条命令输出上限(每流)。
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    HdcNotFound,
    HdcFailed,
    DeviceNotFound,
    DeviceUnavailable,
    InvalidInput,
    TimedOut,
    OutputLimit,
    PathNotFound,
    AlreadyExists,
    PermissionDenied,
    Cancelled,
    Internal,
}

/// 后端统一错误:code 供程序分支,detail 是可直接展示给人看的中文说明。
#[derive(Debug, Clone, Error)]
#[error("{detail}")]
pub struct BridgeError {
    pub code: ErrorCode,
    pub detail: String,
}

impl BridgeError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub fn invalid_input(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidInput, detail)
    }

    pub fn hdc(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::HdcFailed, detail)
    }

    pub fn device_unavailable() -> Self {
        Self::new(
            ErrorCode::DeviceUnavailable,
            "设备已断开或连接已变化,操作被取消",
        )
    }
}

// ---------------------------------------------------------------------------
// 设备与连接
// ---------------------------------------------------------------------------

/// hdc 的连接键(connect key):USB 设备是序列号,网络设备是 "IP:端口"。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectKey(String);

impl ConnectKey {
    pub fn new(value: impl Into<String>) -> Result<Self, BridgeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(BridgeError::invalid_input("连接键不能为空"));
        }
        if value.len() > 512 || value.chars().any(char::is_control) {
            return Err(BridgeError::invalid_input("连接键包含非法字符"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 网络设备键形如 "192.168.1.10:5555"。
    pub fn is_network(&self) -> bool {
        self.0.contains(':')
    }
}

/// 设备目标 = 连接键 + 连接代数。代数在设备消失/重现时递增,
/// 携带旧代数的请求会被后端拒绝,防止跨连接误操作。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceTarget {
    pub key: ConnectKey,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Connected,
    Offline,
    Unknown,
}

impl DeviceState {
    pub fn is_online(self) -> bool {
        matches!(self, DeviceState::Connected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub key: ConnectKey,
    pub state: DeviceState,
}

impl DeviceDescriptor {
    pub fn display_name(&self) -> &str {
        self.key.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub descriptor: DeviceDescriptor,
    pub generation: u64,
}

impl DeviceRecord {
    pub fn target(&self) -> DeviceTarget {
        DeviceTarget {
            key: self.descriptor.key.clone(),
            generation: self.generation,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub devices: Vec<DeviceRecord>,
    pub selected: Option<ConnectKey>,
}

// ---------------------------------------------------------------------------
// 设备概览
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceOverview {
    pub model: Option<String>,
    pub brand: Option<String>,
    pub device_type: Option<String>,
    pub software_version: Option<String>,
    pub api_version: Option<String>,
    pub kernel_version: Option<String>,
    pub cpu_cores: Option<u32>,
    pub memory_total_kib: Option<u64>,
    pub memory_available_kib: Option<u64>,
    pub storage_total_kib: Option<u64>,
    pub storage_used_kib: Option<u64>,
}

// ---------------------------------------------------------------------------
// 文件
// ---------------------------------------------------------------------------

/// 远端绝对路径 newtype:规范化分隔符、拒绝 `..` 逃逸与控制字符。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn root() -> Self {
        Self("/".to_owned())
    }

    pub fn new(value: impl AsRef<str>) -> Result<Self, BridgeError> {
        let raw = value.as_ref();
        if raw.is_empty() || !raw.starts_with('/') {
            return Err(BridgeError::invalid_input(format!(
                "远端路径必须是绝对路径: {raw}"
            )));
        }
        if raw.len() > 4096 || raw.chars().any(char::is_control) {
            return Err(BridgeError::invalid_input("远端路径包含非法字符或过长"));
        }
        let mut parts = Vec::new();
        for segment in raw.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    return Err(BridgeError::invalid_input(format!(
                        "远端路径不允许包含 ..: {raw}"
                    )));
                }
                other => parts.push(other),
            }
        }
        let normalized = format!("/{}", parts.join("/"));
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parent(&self) -> Option<Self> {
        if self.0 == "/" {
            return None;
        }
        let trimmed = self.0.trim_end_matches('/');
        let idx = trimmed.rfind('/')?;
        if idx == 0 {
            Some(Self::root())
        } else {
            Some(Self(trimmed[..idx].to_owned()))
        }
    }

    pub fn join_name(&self, name: &str) -> Result<Self, BridgeError> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(BridgeError::invalid_input(format!("非法的文件名: {name}")));
        }
        let base = self.0.trim_end_matches('/');
        Ok(Self(format!("{base}/{name}")))
    }

    pub fn file_name(&self) -> Option<&str> {
        if self.0 == "/" {
            return None;
        }
        self.0.rsplit('/').next()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFileKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileEntry {
    pub path: RemotePath,
    pub name: String,
    pub kind: RemoteFileKind,
    pub size_bytes: Option<u64>,
    pub modified_unix_seconds: Option<i64>,
    pub permissions: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    Deny,
    ReplaceConfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMutationKind {
    CreateDirectory,
    Rename,
    Delete,
}

// ---------------------------------------------------------------------------
// 应用(bundle)
// ---------------------------------------------------------------------------

/// 包名 newtype:字符白名单 [A-Za-z0-9_.],同时是注入防护。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BundleName(String);

impl BundleName {
    pub fn new(value: impl Into<String>) -> Result<Self, BridgeError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 255
            && value
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
        if !valid {
            return Err(BridgeError::invalid_input(format!("非法的包名: {value}")));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRecord {
    pub name: BundleName,
    pub system: bool,
}

/// 一个包的一个安装物(HAP)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleHap {
    pub path: String,
    pub size_bytes: u64,
    pub modified_unix_seconds: u64,
}

/// 一个包的安装信息:本地化名称与全部 HAP。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePackage {
    pub bundle: BundleName,
    /// `bm dump -l` 给出的本地化名称;可能为空。
    pub label: Option<String>,
    pub haps: Vec<BundleHap>,
}

impl BundlePackage {
    /// 图标缓存指纹:HAP 大小与修改时间的组合,包升级后自动失效。
    pub fn stamp(&self) -> String {
        self.haps
            .iter()
            .map(|hap| format!("{}-{}", hap.size_bytes, hap.modified_unix_seconds))
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleDetails {
    pub name: BundleName,
    pub version_name: Option<String>,
    pub version_code: Option<String>,
    pub main_ability: Option<String>,
    pub app_id: Option<String>,
    pub install_time: Option<String>,
    pub update_time: Option<String>,
    pub system: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    Launch,
    ForceStop,
    ClearData,
    Uninstall,
}

impl AppAction {
    /// 破坏性操作在 UI 层必须弹出确认框。
    pub fn destructive(self) -> bool {
        matches!(self, AppAction::Uninstall | AppAction::ClearData)
    }
}

// ---------------------------------------------------------------------------
// 进程
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub user: Option<String>,
    pub cpu_percent: Option<f32>,
    pub command: String,
}

// ---------------------------------------------------------------------------
// 网络端点
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn parse(value: &str) -> Result<Self, BridgeError> {
        let value = value.trim();
        let (host, port) = value.rsplit_once(':').ok_or_else(|| {
            BridgeError::invalid_input(format!("端点格式应为 IP:端口,收到: {value}"))
        })?;
        if host.is_empty() {
            return Err(BridgeError::invalid_input("主机地址不能为空"));
        }
        let port: u16 = port
            .parse()
            .map_err(|_| BridgeError::invalid_input(format!("非法端口: {port}")))?;
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    pub fn target_string(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

// ---------------------------------------------------------------------------
// 会话与操作标识
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub Uuid);

impl OperationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct ShellOutputChunk {
    pub stream: ShellStream,
    pub bytes: Vec<u8>,
}

/// 交互式终端的行列尺寸,会话建立时传给设备端一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellSize {
    columns: u16,
    rows: u16,
}

impl ShellSize {
    pub fn new(columns: u16, rows: u16) -> Result<Self, BridgeError> {
        if columns == 0 || rows == 0 {
            return Err(BridgeError::invalid_input("终端行列数必须大于 0"));
        }
        Ok(Self { columns, rows })
    }

    pub const fn columns(self) -> u16 {
        self.columns
    }

    pub const fn rows(self) -> u16 {
        self.rows
    }
}

// ---------------------------------------------------------------------------
// 应用更新
// ---------------------------------------------------------------------------

/// 一次可用更新的元数据;asset 缺失时降级为纯链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub asset_url: Option<String>,
    pub asset_size: Option<u64>,
    pub checksum_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheckOutcome {
    UpToDate,
    Available(UpdateInfo),
}

// ---------------------------------------------------------------------------
// 后端命令(UI → 后端)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum BackendCommand {
    RefreshDevices,
    SelectDevice {
        key: Option<ConnectKey>,
    },
    ConnectEndpoint(Endpoint),
    DisconnectEndpoint(Endpoint),
    EnableTcpListen {
        target: DeviceTarget,
        port: u16,
    },
    KillServer,
    LoadOverview(DeviceTarget),
    LoadProcesses(DeviceTarget),
    LoadBundles(DeviceTarget),
    LoadBundleDetails(DeviceTarget, BundleName),
    /// 批量枚举包与 HAP,逐个加载图标(带本地缓存)。
    LoadBundleIcons(DeviceTarget),
    /// 单个包的按需图标加载(批量阶段跳过大包时由选中动作触发)。
    LoadBundleIcon {
        target: DeviceTarget,
        package: BundlePackage,
    },
    RunAppAction {
        target: DeviceTarget,
        bundle: BundleName,
        action: AppAction,
    },
    InstallHap(DeviceTarget, PathBuf),
    OpenShell {
        target: DeviceTarget,
        session: SessionId,
        size: ShellSize,
    },
    WriteShell {
        session: SessionId,
        input: Vec<u8>,
    },
    CloseShell {
        session: SessionId,
    },
    StartHilog {
        target: DeviceTarget,
        session: SessionId,
    },
    StopHilog {
        session: SessionId,
    },
    ClearDeviceHilog(DeviceTarget),
    /// 一次性执行设备端命令(如结束进程),结果以 Toast 事件回报。
    RunShellCommand {
        target: DeviceTarget,
        command: String,
        purpose: String,
    },
    CaptureScreenshot(DeviceTarget),
    ListDirectory {
        target: DeviceTarget,
        path: RemotePath,
    },
    UploadFile {
        target: DeviceTarget,
        local: PathBuf,
        remote: RemotePath,
        overwrite: OverwritePolicy,
        operation: OperationId,
    },
    DownloadFile {
        target: DeviceTarget,
        remote: RemotePath,
        local: PathBuf,
        overwrite: OverwritePolicy,
        operation: OperationId,
    },
    CancelFileOperation(OperationId),
    CreateDirectory {
        target: DeviceTarget,
        path: RemotePath,
    },
    RenameRemoteEntry {
        target: DeviceTarget,
        from: RemotePath,
        to: RemotePath,
    },
    DeleteRemoteEntry {
        target: DeviceTarget,
        path: RemotePath,
    },
    CheckForUpdates,
    DownloadUpdate,
    CancelUpdateDownload,
}

// ---------------------------------------------------------------------------
// 后端事件(后端 → UI)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum BackendEvent {
    HdcReady {
        path: String,
        version: String,
    },
    HdcUnavailable {
        reason: String,
    },
    DevicesChanged(DeviceSnapshot),
    Toast(String),
    EndpointConnectFinished {
        endpoint: Endpoint,
        message: String,
    },
    EndpointConnectFailed {
        endpoint: Endpoint,
        error: String,
    },
    EndpointDisconnected {
        endpoint: Endpoint,
        message: String,
    },
    TcpListenEnabled {
        message: String,
    },
    TcpListenFailed {
        error: String,
    },
    ServerKilled {
        message: String,
    },
    OverviewLoading(DeviceTarget),
    OverviewLoaded(DeviceTarget, DeviceOverview),
    OverviewFailed(DeviceTarget, BridgeError),
    ProcessesLoading(DeviceTarget),
    ProcessesLoaded(DeviceTarget, Vec<ProcessInfo>),
    ProcessesFailed(DeviceTarget, BridgeError),
    BundlesLoading(DeviceTarget),
    BundlesLoaded(DeviceTarget, Vec<BundleRecord>),
    BundlesFailed(DeviceTarget, BridgeError),
    /// 图标加载:先给出包清单(含本地化名称与 HAP 路径),随后逐个给图标。
    BundlePackagesLoaded(DeviceTarget, Vec<BundlePackage>),
    BundlePackagesFailed(DeviceTarget, BridgeError),
    /// 单个包的图标字节(PNG/JPEG/WebP);None 表示该包没有可用图标。
    BundleIconLoaded(DeviceTarget, BundleName, Option<Vec<u8>>),
    /// 批量阶段跳过了超大 HAP;选中该包时可再按需拉取。
    BundleIconSkipped(DeviceTarget, BundleName),
    BundleIconsFinished(DeviceTarget),
    BundleDetailsLoading(DeviceTarget, BundleName),
    BundleDetailsLoaded(DeviceTarget, BundleDetails),
    BundleDetailsFailed(DeviceTarget, BundleName, BridgeError),
    AppActionStarted(DeviceTarget, BundleName, AppAction),
    AppActionCompleted(DeviceTarget, BundleName, AppAction, String),
    AppActionFailed(DeviceTarget, BundleName, AppAction, BridgeError),
    HapInstallStarted(DeviceTarget, String),
    HapInstallFinished(DeviceTarget, String),
    HapInstallFailed(DeviceTarget, BridgeError),
    ShellOpened {
        target: DeviceTarget,
        session: SessionId,
    },
    ShellOutput {
        session: SessionId,
        bytes: Vec<u8>,
    },
    ShellClosed {
        session: SessionId,
        exit_code: Option<i32>,
    },
    ShellFailed {
        session: SessionId,
        error: BridgeError,
    },
    HilogStarted {
        target: DeviceTarget,
        session: SessionId,
    },
    HilogOutput {
        session: SessionId,
        bytes: Vec<u8>,
    },
    HilogClosed {
        session: SessionId,
    },
    HilogFailed {
        session: SessionId,
        error: BridgeError,
    },
    ScreenshotLoading(DeviceTarget),
    ScreenshotCaptured(DeviceTarget, Vec<u8>),
    ScreenshotFailed(DeviceTarget, BridgeError),
    DirectoryLoading {
        target: DeviceTarget,
        path: RemotePath,
    },
    DirectoryLoaded {
        target: DeviceTarget,
        path: RemotePath,
        entries: Vec<RemoteFileEntry>,
    },
    DirectoryFailed {
        target: DeviceTarget,
        path: RemotePath,
        error: BridgeError,
    },
    FileTransferStarted {
        operation: OperationId,
        direction: FileTransferDirection,
        remote: RemotePath,
        local: PathBuf,
    },
    FileTransferFinished {
        operation: OperationId,
    },
    FileTransferFailed {
        operation: OperationId,
        error: BridgeError,
    },
    FileTransferCancelled {
        operation: OperationId,
    },
    FileMutationStarted {
        target: DeviceTarget,
        kind: FileMutationKind,
        path: RemotePath,
    },
    FileMutationCompleted {
        target: DeviceTarget,
        kind: FileMutationKind,
    },
    FileMutationFailed {
        target: DeviceTarget,
        kind: FileMutationKind,
        error: BridgeError,
    },
    UpdateChecked(Result<UpdateCheckOutcome, BridgeError>),
    UpdateDownloadProgress {
        received: u64,
        total: Option<u64>,
    },
    UpdateDownloadFinished(Result<(), BridgeError>),
    OperationFailed(BridgeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_rejects_escape_and_normalizes() {
        assert!(RemotePath::new("../etc").is_err());
        assert!(
            RemotePath::new("/data/local/../tmp").is_err(),
            "不允许路径穿越"
        );
        assert!(RemotePath::new("relative").is_err());
        assert_eq!(
            RemotePath::new("/data//local/").unwrap().as_str(),
            "/data/local"
        );
        assert_eq!(
            RemotePath::new("/data/./tmp").unwrap().as_str(),
            "/data/tmp"
        );
        assert_eq!(RemotePath::root().parent(), None);
        assert_eq!(
            RemotePath::new("/data").unwrap().parent().unwrap().as_str(),
            "/"
        );
        let joined = RemotePath::new("/data")
            .unwrap()
            .join_name("local tmp")
            .unwrap();
        assert_eq!(joined.as_str(), "/data/local tmp");
        assert!(RemotePath::new("/data").unwrap().join_name("../x").is_err());
    }

    #[test]
    fn bundle_name_whitelist() {
        assert!(BundleName::new("com.example.app_1").is_ok());
        assert!(BundleName::new(".hidden").is_err());
        assert!(BundleName::new("com.example;rm -rf").is_err());
        assert!(BundleName::new("").is_err());
    }

    #[test]
    fn endpoint_parsing() {
        let ep = Endpoint::parse(" 192.168.1.10:5555 ").unwrap();
        assert_eq!((ep.host.as_str(), ep.port), ("192.168.1.10", 5555));
        assert_eq!(ep.target_string(), "192.168.1.10:5555");
        assert!(Endpoint::parse("192.168.1.10").is_err());
        assert!(Endpoint::parse("host:abc").is_err());
    }
}
