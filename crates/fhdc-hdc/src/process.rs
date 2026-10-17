//! hdc 子进程的统一封装:输出上限、超时、取消与 Windows 无窗口。

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use fhdc_domain::{BridgeError, ErrorCode};

/// 读取流的块大小。
const CHUNK_BYTES: usize = 8 * 1024;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 防止每次 spawn hdc.exe 闪出控制台窗口。
pub(crate) fn configure_command(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
}

/// 逐块读取直到 EOF 或达到上限;二进制安全,保留原始字节。
async fn read_limited<R: AsyncRead + Unpin>(
    mut pipe: R,
    limit: usize,
    stream: &'static str,
) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; CHUNK_BYTES];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return buffer,
            Ok(read) => {
                if buffer.len() + read > limit {
                    tracing::warn!(stream, limit, "hdc 输出超过上限,截断");
                    return buffer;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
        }
    }
}

/// 构造一个输出已管道化的 hdc 子进程命令(交互式会话使用)。
pub(crate) fn prepare_command(executable: &Path, arguments: &[OsString]) -> Command {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_command(&mut command);
    command
}

/// 有界执行一条 hdc 命令:stdout/stderr 各自限长,超时即杀。
pub(crate) async fn run_bounded(
    executable: &Path,
    arguments: Vec<OsString>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<ProcessOutput, BridgeError> {
    run_bounded_cancellable(
        executable,
        arguments,
        timeout,
        stdout_limit,
        stderr_limit,
        None,
    )
    .await
}

/// 同 [`run_bounded`],另支持通过取消令牌中止(传输类长操作使用)。
pub(crate) async fn run_bounded_cancellable(
    executable: &Path,
    arguments: Vec<OsString>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<ProcessOutput, BridgeError> {
    let mut command = prepare_command(executable, &arguments);
    command.stdin(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        BridgeError::new(ErrorCode::HdcFailed, format!("启动 hdc 失败: {error}"))
    })?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(read_limited(stdout, stdout_limit, "stdout"));
    let stderr_task = tokio::spawn(read_limited(stderr, stderr_limit, "stderr"));

    enum Outcome {
        Exited(Option<i32>),
        TimedOut,
        Cancelled,
    }
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(match status {
            Ok(status) => status.code(),
            Err(_) => None,
        }),
        () = tokio::time::sleep(timeout) => Outcome::TimedOut,
        () = async {
            match cancellation {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        } => Outcome::Cancelled,
    };
    match outcome {
        Outcome::Exited(exit_code) => {
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            Ok(ProcessOutput {
                stdout,
                stderr,
                exit_code,
            })
        }
        Outcome::TimedOut => {
            let _ = child.start_kill();
            Err(BridgeError::new(
                ErrorCode::TimedOut,
                format!("hdc 命令超时({timeout:?})"),
            ))
        }
        Outcome::Cancelled => {
            let _ = child.start_kill();
            Err(BridgeError::new(ErrorCode::Cancelled, "操作已取消"))
        }
    }
}
