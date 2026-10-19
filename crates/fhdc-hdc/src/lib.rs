//! fhdc-hdc:封装 `hdc` 命令行工具的真实后端。
//!
//! 分层仿照 fadb:本 crate 持有 [`HdcTransport`] trait 与进程实现,
//! 假后端(fhdc-fake)实现同一 trait 供无设备开发使用。

mod files;
mod hap_icon;
mod hilog;
mod process;
mod shell;

pub use hap_icon::extract_hap_icon;
pub use shell::ShellSessionHandle;

use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use fhdc_domain::{
    AppAction, BridgeError, BundleDetails, BundleHap, BundleName, BundlePackage, BundleRecord,
    ConnectKey, DeviceDescriptor, DeviceOverview, DeviceState, Endpoint, ErrorCode,
    OverwritePolicy, ProcessInfo, RemotePath, ShellSize,
};

/// 单命令默认超时。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
/// 单命令默认输出上限(与 domain 的常量同源,避免两处魔数)。
const DEFAULT_OUTPUT_LIMIT: usize = fhdc_domain::MAX_OUTPUT_BYTES;
/// 安装 HAP 的超时。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
/// 枚举全部包(每包一次 bm dump -l)是慢命令,单独给更长的超时。
const PACKAGES_TIMEOUT: Duration = Duration::from_secs(120);

/// 设备端包清单脚本:输出 `bundle\034system\036` 记录流。
/// 系统/三方的判据是安装根目录——系统应用装在 `/system/app`,三方应用
/// 装在 `/data/app/el1/bundle/public`;这比解析 `bm dump` 的 JSON 稳,
/// 各系统版本的字段名与嵌套结构差异较大。
const BUNDLE_LIST_SCRIPT: &str = r#"for b in $(bm dump -a 2>/dev/null | sed -n 's/^[[:space:]]*//p' | grep -v '^ID:' | grep -v '^$'); do
  case "$b" in
    *.*) ;;
    *) continue ;;
  esac
  sys=0
  if [ -d "/system/app/$b" ]; then
    sys=1
  elif [ -d "/data/app/el1/bundle/public/$b" ]; then
    sys=0
  else
    # 两个常见根都没命中时,退回包名启发式:com.ohos.* 多为系统应用。
    case "$b" in
      com.ohos.*|com.huawei.*|com.hihonor.*) sys=1 ;;
    esac
  fi
  printf '%s\034%s\036' "$b" "$sys"
done"#;

/// 设备端枚举脚本:输出 `bundle\034label\034size\034mtime\034hap\036` 记录流。
const BUNDLE_PACKAGES_SCRIPT: &str = r#"for b in $(bm dump -a 2>/dev/null | sed -n 's/^[[:space:]]*//p' | grep -v '^ID:' | grep -v '^$'); do
  label=''
  found=0
  for root in /data/app/el1/bundle/public /system/app; do
    d="$root/$b"
    [ -d "$d" ] || continue
    if [ "$found" = 0 ]; then
      label=$(bm dump -n "$b" -l 2>/dev/null | head -1)
      found=1
    fi
    for hap in "$d"/*.hap; do
      [ -f "$hap" ] || continue
      set -- $(stat -c '%s %Y' "$hap" 2>/dev/null || echo '0 0')
      printf '%s\034%s\034%s\034%s\034%s\036' "$b" "$label" "$1" "$2" "$hap"
    done
  done
  if [ "$found" = 0 ]; then
    cp=$(bm dump -n "$b" 2>/dev/null | sed -n 's/.*"codePath": "\([^"]*\)".*/\1/p' | head -1)
    label=$(bm dump -n "$b" -l 2>/dev/null | head -1)
    [ -n "$cp" ] || continue
    for hap in "$cp"/*.hap; do
      [ -f "$hap" ] || continue
      set -- $(stat -c '%s %Y' "$hap" 2>/dev/null || echo '0 0')
      printf '%s\034%s\034%s\034%s\034%s\036' "$b" "$label" "$1" "$2" "$hap"
    done
  fi
done"#;
/// 截图流水线各步超时。
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
/// 截图字节上限。
const SCREENSHOT_LIMIT: usize = 32 * 1024 * 1024;

/// fhdc 与 hdc 后端交互的统一接口;真实与假后端共享。
#[async_trait]
pub trait HdcTransport: Send + Sync {
    async fn version(&self) -> Result<String, BridgeError>;
    async fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, BridgeError>;
    async fn connect_endpoint(&self, endpoint: &Endpoint) -> Result<String, BridgeError>;
    async fn disconnect_endpoint(&self, endpoint: &Endpoint) -> Result<String, BridgeError>;
    async fn enable_tcp_listen(&self, key: &ConnectKey, port: u16) -> Result<String, BridgeError>;
    async fn kill_server(&self) -> Result<String, BridgeError>;

    async fn device_overview(&self, key: &ConnectKey) -> Result<DeviceOverview, BridgeError>;
    async fn list_processes(&self, key: &ConnectKey) -> Result<Vec<ProcessInfo>, BridgeError>;

    async fn list_bundles(&self, key: &ConnectKey) -> Result<Vec<BundleRecord>, BridgeError>;
    /// 枚举所有包的安装物(HAP 路径 + 本地化名称),供图标加载使用。
    async fn list_bundle_packages(
        &self,
        key: &ConnectKey,
    ) -> Result<Vec<BundlePackage>, BridgeError>;
    async fn bundle_details(
        &self,
        key: &ConnectKey,
        name: &BundleName,
    ) -> Result<BundleDetails, BridgeError>;
    async fn run_app_action(
        &self,
        key: &ConnectKey,
        name: &BundleName,
        action: AppAction,
    ) -> Result<String, BridgeError>;
    async fn install_hap(&self, key: &ConnectKey, path: &Path) -> Result<String, BridgeError>;

    async fn start_shell(
        &self,
        key: &ConnectKey,
        size: ShellSize,
    ) -> Result<ShellSessionHandle, BridgeError>;
    async fn start_hilog(&self, key: &ConnectKey) -> Result<ShellSessionHandle, BridgeError>;
    async fn clear_hilog(&self, key: &ConnectKey) -> Result<(), BridgeError>;
    /// 一次性执行设备端命令并返回文本输出。
    async fn run_shell_once(&self, key: &ConnectKey, command: &str) -> Result<String, BridgeError>;

    async fn list_directory(
        &self,
        key: &ConnectKey,
        path: &RemotePath,
    ) -> Result<Vec<fhdc_domain::RemoteFileEntry>, BridgeError>;
    async fn push_file(
        &self,
        key: &ConnectKey,
        local: &Path,
        remote: &RemotePath,
        overwrite: OverwritePolicy,
        cancellation: &CancellationToken,
    ) -> Result<(), BridgeError>;
    async fn pull_file(
        &self,
        key: &ConnectKey,
        remote: &RemotePath,
        local: &Path,
        overwrite: OverwritePolicy,
        cancellation: &CancellationToken,
    ) -> Result<(), BridgeError>;
    async fn create_directory(
        &self,
        key: &ConnectKey,
        path: &RemotePath,
    ) -> Result<(), BridgeError>;
    async fn rename_entry(
        &self,
        key: &ConnectKey,
        from: &RemotePath,
        to: &RemotePath,
    ) -> Result<(), BridgeError>;
    async fn delete_entry(&self, key: &ConnectKey, path: &RemotePath) -> Result<(), BridgeError>;

    async fn capture_screenshot(&self, key: &ConnectKey) -> Result<Vec<u8>, BridgeError>;
}

/// 查找 hdc 可执行文件。优先级:环境变量 FHDC_HDC > 用户设置 > SDK 环境变量 > DevEco 默认安装位置 > PATH。
#[derive(Debug, Clone, Default)]
pub struct HdcLocator {
    /// 设置界面持久化的用户指定路径。
    pub override_path: Option<PathBuf>,
}

fn hdc_executable_name() -> &'static str {
    if cfg!(windows) { "hdc.exe" } else { "hdc" }
}

impl HdcLocator {
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = Vec::new();
        let mut push = |path: PathBuf| {
            if !found.iter().any(|existing| paths_equal(existing, &path)) {
                found.push(path);
            }
        };

        if let Ok(env_path) = env::var("FHDC_HDC") {
            push(PathBuf::from(env_path));
        }
        if let Some(explicit) = &self.override_path {
            push(explicit.clone());
        }
        for variable in ["HDC_HOME", "OHOS_SDK_HOME", "DEVECO_SDK_HOME"] {
            if let Ok(base) = env::var(variable) {
                let base = PathBuf::from(base);
                push(base.join(hdc_executable_name()));
                push(base.join("toolchains").join(hdc_executable_name()));
                push(
                    base.join("default")
                        .join("openharmony")
                        .join("toolchains")
                        .join(hdc_executable_name()),
                );
            }
        }
        if let Ok(program_files) = env::var("ProgramFiles") {
            push(
                PathBuf::from(&program_files)
                    .join("Huawei")
                    .join("DevEco Studio")
                    .join("sdk")
                    .join("default")
                    .join("openharmony")
                    .join("toolchains")
                    .join(hdc_executable_name()),
            );
        }
        if let Ok(program_files_x86) = env::var("ProgramFiles(x86)") {
            push(
                PathBuf::from(&program_files_x86)
                    .join("Huawei")
                    .join("DevEco Studio")
                    .join("sdk")
                    .join("default")
                    .join("openharmony")
                    .join("toolchains")
                    .join(hdc_executable_name()),
            );
        }
        if let Ok(path_var) = env::var("PATH") {
            for dir in env::split_paths(&path_var) {
                push(dir.join(hdc_executable_name()));
            }
        }
        found
    }

    pub fn locate(&self) -> Option<PathBuf> {
        self.candidates().into_iter().find(|path| path.is_file())
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// 基于子进程的真实后端。
pub struct ProcessHdcTransport {
    executable: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
}

impl ProcessHdcTransport {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_OUTPUT_LIMIT,
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// 执行任意 hdc 参数;exit 0 → stdout,否则带 stderr 细节报错。
    async fn run(&self, arguments: Vec<OsString>) -> Result<String, BridgeError> {
        let output = process::run_bounded(
            &self.executable,
            arguments,
            self.timeout,
            self.max_output_bytes,
            self.max_output_bytes,
        )
        .await?;
        if output.exit_code != Some(0) {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let detail = if detail.is_empty() { stdout } else { detail };
            return Err(BridgeError::new(
                ErrorCode::HdcFailed,
                if detail.is_empty() {
                    format!("hdc 退出码 {:?}", output.exit_code)
                } else {
                    detail
                },
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// 设备端 shell 命令(整条命令作为单个参数,规避参数重解释)。
    pub(crate) async fn shell_text(
        &self,
        key: &ConnectKey,
        command: &str,
    ) -> Result<String, BridgeError> {
        self.run(vec![
            OsString::from("-t"),
            OsString::from(key.as_str()),
            OsString::from("shell"),
            OsString::from(command),
        ])
        .await
    }

    /// 执行 shell 命令但只关心退出码;spawn/超时等异常返回 None。
    pub(crate) async fn shell_optional(&self, key: &ConnectKey, command: &str) -> Option<i32> {
        let output = process::run_bounded(
            &self.executable,
            vec![
                OsString::from("-t"),
                OsString::from(key.as_str()),
                OsString::from("shell"),
                OsString::from(command),
            ],
            self.timeout,
            self.max_output_bytes,
            self.max_output_bytes,
        )
        .await
        .ok()?;
        output.exit_code
    }

    /// 执行 shell 命令并返回完整输出(退出码 + 双流)。
    pub(crate) async fn shell_exit_code(
        &self,
        key: &ConnectKey,
        command: &str,
    ) -> Result<process::ProcessOutput, BridgeError> {
        process::run_bounded(
            &self.executable,
            vec![
                OsString::from("-t"),
                OsString::from(key.as_str()),
                OsString::from("shell"),
                OsString::from(command),
            ],
            self.timeout,
            self.max_output_bytes,
            self.max_output_bytes,
        )
        .await
    }

    /// 概览探测:失败返回 None,不阻断其它字段。
    async fn probe(&self, key: &ConnectKey, command: &str) -> Option<String> {
        self.shell_text(key, command)
            .await
            .ok()
            .map(|out| out.trim().to_owned())
            .filter(|out| !out.is_empty())
    }
}

#[async_trait]
impl HdcTransport for ProcessHdcTransport {
    async fn version(&self) -> Result<String, BridgeError> {
        let output = self.run(vec![OsString::from("-v")]).await?;
        Ok(output.lines().next().unwrap_or_default().trim().to_owned())
    }

    async fn list_devices(&self) -> Result<Vec<DeviceDescriptor>, BridgeError> {
        let output = self
            .run(vec![OsString::from("list"), OsString::from("targets")])
            .await?;
        Ok(parse_targets(&output))
    }

    async fn connect_endpoint(&self, endpoint: &Endpoint) -> Result<String, BridgeError> {
        let output = self
            .run(vec![
                OsString::from("tconn"),
                OsString::from(endpoint.target_string()),
            ])
            .await?;
        let trimmed = output.trim();
        let lowered = trimmed.to_lowercase();
        if output_failed(&lowered) {
            return Err(BridgeError::hdc(format!("连接失败: {trimmed}")));
        }
        Ok(trimmed.to_owned())
    }

    async fn disconnect_endpoint(&self, endpoint: &Endpoint) -> Result<String, BridgeError> {
        let output = self
            .run(vec![
                OsString::from("tdisconn"),
                OsString::from(endpoint.target_string()),
            ])
            .await?;
        let trimmed = output.trim();
        if output_failed(&trimmed.to_lowercase()) {
            return Err(BridgeError::hdc(format!("断开失败: {trimmed}")));
        }
        Ok(trimmed.to_owned())
    }

    async fn enable_tcp_listen(&self, key: &ConnectKey, port: u16) -> Result<String, BridgeError> {
        let output = self
            .run(vec![
                OsString::from("-t"),
                OsString::from(key.as_str()),
                OsString::from("tmode"),
                OsString::from("port"),
                OsString::from(port.to_string()),
            ])
            .await?;
        Ok(output.trim().to_owned())
    }

    async fn kill_server(&self) -> Result<String, BridgeError> {
        let output = self.run(vec![OsString::from("kill")]).await?;
        Ok(output.trim().to_owned())
    }

    async fn device_overview(&self, key: &ConnectKey) -> Result<DeviceOverview, BridgeError> {
        // 十条探测并发执行:总耗时从 10 次往返压缩到约 1 次。
        let (
            model,
            brand,
            device_type,
            software_version,
            api_version,
            kernel_version,
            cores,
            meminfo,
            df,
        ) = tokio::join!(
            self.probe(key, "param get const.product.model"),
            self.probe(key, "param get const.product.brand"),
            self.probe(key, "param get const.product.devicetype"),
            self.probe(key, "param get const.product.software.version"),
            self.probe(key, "param get const.ohos.apiversion"),
            self.probe(key, "uname -r"),
            self.probe(key, "grep -c '^processor' /proc/cpuinfo"),
            self.probe(key, "cat /proc/meminfo"),
            self.probe(key, "df -k /data"),
        );
        let cpu_cores = cores.and_then(|value| value.parse().ok());
        let (memory_total_kib, memory_available_kib) = parse_meminfo(&meminfo.unwrap_or_default());
        let (storage_total_kib, storage_used_kib) = parse_df_data(&df.unwrap_or_default());
        Ok(DeviceOverview {
            model,
            brand,
            device_type,
            software_version,
            api_version,
            kernel_version,
            cpu_cores,
            memory_total_kib,
            memory_available_kib,
            storage_total_kib,
            storage_used_kib,
        })
    }

    async fn list_processes(&self, key: &ConnectKey) -> Result<Vec<ProcessInfo>, BridgeError> {
        let output = self.shell_text(key, "ps -ef").await?;
        Ok(parse_processes(&output))
    }

    async fn list_bundles(&self, key: &ConnectKey) -> Result<Vec<BundleRecord>, BridgeError> {
        // 一次 shell 调用同时拿到包名与系统/三方标记。
        let output = self.shell_text(key, BUNDLE_LIST_SCRIPT).await?;
        Ok(parse_bundles(&output))
    }

    async fn list_bundle_packages(
        &self,
        key: &ConnectKey,
    ) -> Result<Vec<BundlePackage>, BridgeError> {
        // 一次 shell 跑完枚举:每个包一次 bm dump -l 拿本地化名称
        // (约 0.1s/包),再逐个 HAP 取大小与 mtime 作缓存指纹。
        let output = process::run_bounded(
            &self.executable,
            vec![
                OsString::from("-t"),
                OsString::from(key.as_str()),
                OsString::from("shell"),
                OsString::from(BUNDLE_PACKAGES_SCRIPT),
            ],
            PACKAGES_TIMEOUT,
            self.max_output_bytes,
            self.max_output_bytes,
        )
        .await?;
        if output.exit_code != Some(0) {
            return Err(BridgeError::new(
                ErrorCode::HdcFailed,
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        Ok(parse_bundle_packages(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    async fn bundle_details(
        &self,
        key: &ConnectKey,
        name: &BundleName,
    ) -> Result<BundleDetails, BridgeError> {
        let output = self
            .shell_text(key, &format!("bm dump -n {}", name.as_str()))
            .await?;
        parse_bundle_details(&output, name.clone())
    }

    async fn run_app_action(
        &self,
        key: &ConnectKey,
        name: &BundleName,
        action: AppAction,
    ) -> Result<String, BridgeError> {
        let bundle = name.as_str();
        let command = match action {
            AppAction::Launch => {
                let ability = self
                    .bundle_details(key, name)
                    .await
                    .ok()
                    .and_then(|details| details.main_ability)
                    .unwrap_or_else(|| "EntryAbility".to_owned());
                format!("aa start -a {ability} -b {bundle}")
            }
            AppAction::ForceStop => format!("aa force-stop {bundle}"),
            AppAction::ClearData => format!("bm clean -n {bundle} -d"),
            AppAction::Uninstall => {
                let output = self
                    .run(vec![
                        OsString::from("-t"),
                        OsString::from(key.as_str()),
                        OsString::from("uninstall"),
                        OsString::from(bundle),
                    ])
                    .await?;
                let trimmed = output.trim();
                // 卸载失败时 hdc 退出码仍为 0,报错文本形如
                // "msg:error: failed to uninstall bundle."。
                if output_failed(&trimmed.to_lowercase()) {
                    return Err(BridgeError::hdc(format!("卸载失败: {trimmed}")));
                }
                return Ok(trimmed.to_owned());
            }
        };
        let output = self.shell_text(key, &command).await?;
        let trimmed = output.trim();
        if output_failed(&trimmed.to_lowercase()) {
            return Err(BridgeError::hdc(format!("操作失败: {trimmed}")));
        }
        Ok(if trimmed.is_empty() {
            "完成".to_owned()
        } else {
            trimmed.to_owned()
        })
    }

    async fn install_hap(&self, key: &ConnectKey, path: &Path) -> Result<String, BridgeError> {
        let output = process::run_bounded(
            &self.executable,
            vec![
                OsString::from("-t"),
                OsString::from(key.as_str()),
                OsString::from("install"),
                path.as_os_str().to_os_string(),
            ],
            INSTALL_TIMEOUT,
            1024 * 1024,
            1024 * 1024,
        )
        .await?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        // 实测(3.2.0d)失败的安装也退出 0,报错在文本里:
        // "msg:error: failed to install bundle. code:9568320 error: no signature file."
        // 成功输出只含 "install bundle successfully" / "AppMod finish",不含 error。
        if output.exit_code == Some(0)
            && !output_failed(&format!("{stdout} {stderr}").to_lowercase())
        {
            return Ok(if stdout.is_empty() {
                "安装完成".to_owned()
            } else {
                stdout
            });
        }
        Err(BridgeError::hdc(format!(
            "安装失败: {}",
            if stdout.is_empty() { stderr } else { stdout }
        )))
    }

    async fn start_shell(
        &self,
        key: &ConnectKey,
        size: ShellSize,
    ) -> Result<ShellSessionHandle, BridgeError> {
        shell::start_shell(self.executable.clone(), key, size)
    }

    async fn start_hilog(&self, key: &ConnectKey) -> Result<ShellSessionHandle, BridgeError> {
        hilog::start_hilog(self.executable.clone(), key)
    }

    async fn clear_hilog(&self, key: &ConnectKey) -> Result<(), BridgeError> {
        self.shell_text(key, "hilog -r").await.map(|_| ())
    }

    async fn run_shell_once(&self, key: &ConnectKey, command: &str) -> Result<String, BridgeError> {
        self.shell_text(key, command).await
    }

    async fn list_directory(
        &self,
        key: &ConnectKey,
        path: &RemotePath,
    ) -> Result<Vec<fhdc_domain::RemoteFileEntry>, BridgeError> {
        files::list_directory(self, key, path).await
    }

    async fn push_file(
        &self,
        key: &ConnectKey,
        local: &Path,
        remote: &RemotePath,
        overwrite: OverwritePolicy,
        cancellation: &CancellationToken,
    ) -> Result<(), BridgeError> {
        files::push_file(self, key, local, remote, overwrite, cancellation).await
    }

    async fn pull_file(
        &self,
        key: &ConnectKey,
        remote: &RemotePath,
        local: &Path,
        overwrite: OverwritePolicy,
        cancellation: &CancellationToken,
    ) -> Result<(), BridgeError> {
        files::pull_file(self, key, remote, local, overwrite, cancellation).await
    }

    async fn create_directory(
        &self,
        key: &ConnectKey,
        path: &RemotePath,
    ) -> Result<(), BridgeError> {
        files::create_directory(self, key, path).await
    }

    async fn rename_entry(
        &self,
        key: &ConnectKey,
        from: &RemotePath,
        to: &RemotePath,
    ) -> Result<(), BridgeError> {
        files::rename_entry(self, key, from, to).await
    }

    async fn delete_entry(&self, key: &ConnectKey, path: &RemotePath) -> Result<(), BridgeError> {
        files::delete_entry(self, key, path).await
    }

    async fn capture_screenshot(&self, key: &ConnectKey) -> Result<Vec<u8>, BridgeError> {
        let remote = "/data/local/tmp/fhdc_screen.jpeg";
        // snapshot_display 失败(路径非法/不支持)时退出码仍为 0,且成功输出
        // 恒含 "success: snapshot display";文本判定才可靠。
        let snap = self
            .shell_text(key, &format!("snapshot_display -f {remote}"))
            .await?;
        let snap_lower = snap.to_lowercase();
        if snap_lower.contains("error") || !snap_lower.contains("success") {
            return Err(BridgeError::hdc(format!(
                "snapshot_display 截图失败(设备可能不支持): {}",
                snap.trim().lines().next_back().unwrap_or_default()
            )));
        }

        let local = std::env::temp_dir().join(format!(
            "fhdc_screen_{}.jpeg",
            uuid::Uuid::new_v4().simple()
        ));
        let recv = process::run_bounded(
            &self.executable,
            vec![
                OsString::from("-t"),
                OsString::from(key.as_str()),
                OsString::from("file"),
                OsString::from("recv"),
                OsString::from(remote),
                local.as_os_str().to_os_string(),
            ],
            SCREENSHOT_TIMEOUT,
            64 * 1024,
            64 * 1024,
        )
        .await;
        let cleanup_remote = self.shell_optional(key, &format!("rm -f {remote}")).await;
        let _ = cleanup_remote;
        let output = recv?;
        // file recv 失败(远端不存在)时退出码也是 0,靠 [Fail]/error 文本判定。
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_lowercase();
        if output.exit_code != Some(0) || output_failed(&combined) {
            return Err(BridgeError::hdc(format!(
                "拉取截图失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let bytes = match tokio::fs::read(&local).await {
            Ok(bytes) => bytes,
            Err(error) => {
                // 读取失败要如实上报,不能混进"不是有效 JPEG"误导排查。
                return Err(BridgeError::new(
                    ErrorCode::Internal,
                    format!("读取拉取到本地的截图失败({}): {error}", local.display()),
                ));
            }
        };
        let _ = tokio::fs::remove_file(&local).await;
        if bytes.len() > SCREENSHOT_LIMIT || !bytes.starts_with(&[0xFF, 0xD8]) {
            return Err(BridgeError::hdc("设备返回的不是有效 JPEG 图像"));
        }
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// 解析器
// ---------------------------------------------------------------------------

/// 解析 `hdc list targets` 输出;全部视为已连接。
pub(crate) fn parse_targets(output: &str) -> Vec<DeviceDescriptor> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.eq_ignore_ascii_case("[empty]"))
        .filter_map(|line| ConnectKey::new(line).ok())
        .map(|key| DeviceDescriptor {
            state: DeviceState::Connected,
            key,
        })
        .collect()
}

/// 解析 /proc/meminfo 的 MemTotal/MemAvailable(单位 KiB)。
pub(crate) fn parse_meminfo(output: &str) -> (Option<u64>, Option<u64>) {
    let mut total = None;
    let mut available = None;
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:") => total = parts.next().and_then(|v| v.parse().ok()),
            Some("MemAvailable:") => available = parts.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    (total, available)
}

/// 解析 `df -k /data` 最后一行:文件系统 1K-blocks Used ...
pub(crate) fn parse_df_data(output: &str) -> (Option<u64>, Option<u64>) {
    let Some(last) = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
    else {
        return (None, None);
    };
    let fields: Vec<&str> = last.split_whitespace().collect();
    if fields.len() < 3 {
        return (None, None);
    }
    (fields[1].parse().ok(), fields[2].parse().ok())
}

/// 解析 `ps -ef`:UID PID PPID C STIME TTY TIME CMD。
pub(crate) fn parse_processes(output: &str) -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8
            || fields[0].eq_ignore_ascii_case("UID")
            || fields[0].eq_ignore_ascii_case("USER")
        {
            continue;
        }
        let Some(pid) = fields[1].parse().ok() else {
            continue;
        };
        processes.push(ProcessInfo {
            pid,
            user: Some(fields[0].to_owned()),
            cpu_percent: fields[3].parse().ok(),
            command: fields[7..].join(" "),
        });
    }
    processes.sort_by(|a, b| {
        b.cpu_percent
            .unwrap_or(0.0)
            .partial_cmp(&a.cpu_percent.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.pid.cmp(&b.pid))
    });
    processes
}

fn looks_like_bundle(line: &str) -> bool {
    let line = line.trim().trim_end_matches(',');
    !line.is_empty()
        && line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        && line.contains('.')
        && line.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

/// 解析枚举脚本的 `bundle\034label\034size\034mtime\034hap\036` 记录流。
pub(crate) fn parse_bundle_packages(output: &str) -> Vec<BundlePackage> {
    let mut packages: Vec<BundlePackage> = Vec::new();
    for record in output.split('\u{1e}') {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\u{1c}').collect();
        if fields.len() != 5 {
            continue;
        }
        let Ok(bundle) = BundleName::new(fields[0].trim()) else {
            continue;
        };
        let label = fields[1].trim();
        let hap = BundleHap {
            path: fields[4].trim().to_owned(),
            size_bytes: fields[2].trim().parse().unwrap_or(0),
            modified_unix_seconds: fields[3].trim().parse().unwrap_or(0),
        };
        if hap.path.is_empty() {
            continue;
        }
        match packages.iter_mut().find(|package| package.bundle == bundle) {
            Some(existing) => existing.haps.push(hap),
            None => packages.push(BundlePackage {
                bundle,
                label: (!label.is_empty()).then(|| label.to_owned()),
                haps: vec![hap],
            }),
        }
    }
    packages
}

/// 解析包清单。
///
/// 首选设备端脚本的 `bundle\034system\036` 记录流(带系统/三方标记);
/// 若拿到的是 `bm dump -a` 的原始逐行输出,则退回旧路径:只列包名,
/// 一律按三方应用处理。
pub(crate) fn parse_bundles(output: &str) -> Vec<BundleRecord> {
    if output.contains('\u{1e}') {
        let mut records = Vec::new();
        for record in output.split('\u{1e}') {
            if record.is_empty() {
                continue;
            }
            let mut fields = record.split('\u{1c}');
            let Some(name) = fields.next().map(str::trim) else {
                continue;
            };
            let Ok(name) = BundleName::new(name) else {
                continue;
            };
            let system = fields.next().map(str::trim) == Some("1");
            records.push(BundleRecord { name, system });
        }
        records.sort_by(|a, b| a.name.cmp(&b.name));
        records.dedup_by(|a, b| a.name == b.name);
        return records;
    }

    let mut names: Vec<BundleName> = output
        .lines()
        .map(str::trim)
        .filter(|line| looks_like_bundle(line))
        .filter_map(|line| BundleName::new(line).ok())
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| BundleRecord {
            system: false,
            name,
        })
        .collect()
}

/// hdc 的退出码不区分成败(实测 3.2.0d 失败也退出 0),文本标记是唯一依据。
/// "error" 单独用作失败词只在安装/卸载/aa/bm 这类输出上使用——它们的
/// 成功输出不含该词;注意不要推广到任意命令。
pub(crate) fn output_failed(lowered: &str) -> bool {
    lowered.contains("[fail]") || lowered.contains("error") || lowered.contains("failure")
}

/// 解析 `bm dump -n <bundle>` 的详情:优先 JSON,退化为键值扫描。
pub(crate) fn parse_bundle_details(
    output: &str,
    name: BundleName,
) -> Result<BundleDetails, BridgeError> {
    let trimmed = output.trim();
    if trimmed.to_lowercase().contains("[fail]") {
        return Err(BridgeError::hdc(format!(
            "查询包信息失败: {}",
            trimmed.lines().next().unwrap_or_default()
        )));
    }
    let mut details = BundleDetails {
        name: name.clone(),
        version_name: None,
        version_code: None,
        main_ability: None,
        app_id: None,
        install_time: None,
        update_time: None,
        system: None,
    };
    // 真机(3.2.0d)输出以 `包名:` 前缀行开头,JSON 从首个 `{`/`[` 起;
    // 切片后兼容前缀与无前缀两种形态。
    let json_body = trimmed
        .find(['{', '['])
        .map(|index| &trimmed[index..])
        .unwrap_or(trimmed);
    let parsed: Option<serde_json::Value> = serde_json::from_str(json_body).ok();
    let object = match parsed {
        Some(serde_json::Value::Array(items)) => items.into_iter().next(),
        Some(value @ serde_json::Value::Object(_)) => Some(value),
        _ => None,
    };
    if let Some(serde_json::Value::Object(map)) = object {
        let text = |key: &str| map.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        let number = |key: &str| map.get(key).and_then(|v| v.as_f64()).map(|v| v.to_string());
        details.version_name = text("versionName");
        details.version_code = number("versionCode").or_else(|| text("versionCode"));
        details.main_ability = text("mainAbility")
            .or_else(|| text("entryAbility"))
            .or_else(|| {
                // 真机根层没有 mainAbility,在 hapModuleInfos[0] 里。
                map.get("hapModuleInfos")
                    .and_then(|v| v.as_array())
                    .and_then(|items| items.first())
                    .and_then(|module| module.get("mainAbility"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
        details.app_id = text("appId").or_else(|| text("appIdentifier"));
        details.install_time = number("installTime").or_else(|| text("installTime"));
        details.update_time = number("updateTime").or_else(|| text("updateTime"));
        // 真机根层没有 isSystemApp,在 applicationInfo 里。
        details.system = map
            .get("isSystemApp")
            .and_then(|v| v.as_bool())
            .or_else(|| {
                map.get("applicationInfo")
                    .and_then(|v| v.get("isSystemApp"))
                    .and_then(|v| v.as_bool())
            });
        return Ok(details);
    }
    // 退化为 `key: value` 行扫描(旧版本纯文本输出;容忍 JSON 风格的引号键)。
    for line in trimmed.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('"').to_lowercase();
        let value = value.trim().trim_matches('"').to_owned();
        match key.as_str() {
            "versionname" => details.version_name = Some(value),
            "versioncode" => details.version_code = Some(value),
            "mainability" => details.main_ability = Some(value),
            "appid" | "appidentifier" => details.app_id = Some(value),
            "installtime" => details.install_time = Some(value),
            "updatetime" => details.update_time = Some(value),
            "issystemapp" => details.system = value.to_lowercase().parse().ok(),
            _ => {}
        }
    }
    Ok(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_skips_empty_marker() {
        let devices =
            parse_targets("\r\nUSER_default__abcdef123\r\n[Empty]\n192.168.1.10:5555\n\n");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].key.as_str(), "USER_default__abcdef123");
        assert!(devices[0].state.is_online());
        assert!(devices[1].key.is_network());
    }

    #[test]
    fn meminfo_and_df_parse() {
        let (total, available) = parse_meminfo(
            "MemTotal:       3992948 kB\nMemFree:         123456 kB\nMemAvailable:   2100000 kB\n",
        );
        assert_eq!((total, available), (Some(3_992_948), Some(2_100_000)));
        let (total, used) =
            parse_df_data("/dev/block/sda10  25394624 12345676  11775148  52% /data");
        assert_eq!((total, used), (Some(25_394_624), Some(12_345_676)));
        assert_eq!(parse_df_data(""), (None, None));
    }

    #[test]
    fn processes_parse_and_sort() {
        let output = "UID            PID  PPID C STIME TTY    TIME     CMD\nroot             1     0 0 12:00 ?      00:00:02 init\nsystem         666     1 5 12:00 ?      00:00:09 com.ohos.my app --started\nroot           250     1 1 12:00 ?      00:00:01 foundation";
        let processes = parse_processes(output);
        assert_eq!(processes.len(), 3);
        assert_eq!(processes[0].pid, 666);
        assert_eq!(processes[0].cpu_percent, Some(5.0));
        assert_eq!(processes[0].command, "com.ohos.my app --started");
        assert_eq!(processes[1].pid, 250);
    }

    #[test]
    fn bundles_parse_filters_headers() {
        let output = "All bundles:\n\ncom.ohos.launcher\ncom.example.demo_app\n/system/bin/xxx is not bundle\n12345\n";
        let bundles = parse_bundles(output);
        let names: Vec<&str> = bundles.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["com.example.demo_app", "com.ohos.launcher"]);
        // 旧路径拿不到系统标记,一律按三方应用处理。
        assert!(bundles.iter().all(|bundle| !bundle.system));
    }

    #[test]
    fn bundles_parse_system_flag_from_record_stream() {
        // 设备端脚本输出:包名 + 系统标记。
        let output = "com.ohos.launcher\u{1c}1\u{1e}com.example.demo\u{1c}0\u{1e}com.ohos.settings\u{1c}1\u{1e}";
        let bundles = parse_bundles(output);
        let names: Vec<&str> = bundles.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["com.example.demo", "com.ohos.launcher", "com.ohos.settings"]
        );
        let system: Vec<&str> = bundles
            .iter()
            .filter(|b| b.system)
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(system, vec!["com.ohos.launcher", "com.ohos.settings"]);
    }

    #[test]
    fn bundle_details_json_and_text() {
        let json = r#"[
  {
    "appId": "com.example.demo___y",
    "versionCode": 1000000,
    "versionName": "1.0.0",
    "mainAbility": "EntryAbility",
    "installTime": 1719000000000,
    "isSystemApp": false
  }
]"#;
        let details =
            parse_bundle_details(json, BundleName::new("com.example.demo").unwrap()).unwrap();
        assert_eq!(details.version_name.as_deref(), Some("1.0.0"));
        assert_eq!(details.version_code.as_deref(), Some("1000000"));
        assert_eq!(details.main_ability.as_deref(), Some("EntryAbility"));
        assert_eq!(details.system, Some(false));

        let text =
            " BundleName : com.example.demo\n versionName: 2.0.0\n installTime : 1719000000000\n";
        let details =
            parse_bundle_details(text, BundleName::new("com.example.demo").unwrap()).unwrap();
        assert_eq!(details.version_name.as_deref(), Some("2.0.0"));

        assert!(
            parse_bundle_details("[Fail]BusMana error", BundleName::new("com.x.y").unwrap())
                .is_err()
        );
    }

    #[test]
    fn bundle_details_handles_device_prefix_and_nested_fields() {
        // 真机 3.2.0d:首行是 `包名:` 前缀;根层没有 mainAbility 与
        // isSystemApp,分别在 hapModuleInfos[0] 与 applicationInfo 里。
        let device = r#"com.ohos.camera:
{
    "appId": "com.ohos.camera_BAjVq",
    "appIdentifier": "",
    "applicationInfo": {
        "isSystemApp": true,
        "versionCode": 50000110,
        "versionName": "5.0.0.110"
    },
    "hapModuleInfos": [
        {
            "mainAbility": "com.ohos.camera.MainAbility",
            "name": "entry"
        }
    ],
    "installTime": 20828,
    "updateTime": 20828,
    "versionCode": 50000110,
    "versionName": "5.0.0.110"
}"#;
        let details =
            parse_bundle_details(device, BundleName::new("com.ohos.camera").unwrap()).unwrap();
        assert_eq!(details.version_name.as_deref(), Some("5.0.0.110"));
        assert_eq!(details.version_code.as_deref(), Some("50000110"));
        assert_eq!(
            details.main_ability.as_deref(),
            Some("com.ohos.camera.MainAbility")
        );
        assert_eq!(details.system, Some(true));
        assert_eq!(details.install_time.as_deref(), Some("20828"));
    }

    #[test]
    fn output_failure_detection() {
        assert!(output_failed(
            "[info]... msg:error: failed to install bundle."
        ));
        assert!(output_failed("[fail]error opening file"));
        assert!(output_failed("operation failure"));
        assert!(!output_failed("install bundle successfully. appmod finish"));
        // 防回归:成功输出不能因普通词误判。
        assert!(!output_failed(
            "FileTransfer finish, Size:45235, File count = 1"
        ));
        assert!(!output_failed("start ability successfully."));
    }
}
