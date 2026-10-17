//! `hdc shell hilog` 流式日志会话:复用 shell 通道语义,输入通道被忽略。

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

use fhdc_domain::{BridgeError, ConnectKey, ErrorCode};

use crate::shell::ShellSessionHandle;

/// 启动 hilog 流。断连时 hilog 非零退出属正常,退出码被吞掉,
/// UI 保留已缓冲的行。
pub fn start_hilog(
    executable: PathBuf,
    key: &ConnectKey,
) -> Result<ShellSessionHandle, BridgeError> {
    let mut command = Command::new(&executable);
    command
        .args(hilog_arguments(key))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process::configure_command(&mut command);
    let mut child = command.spawn().map_err(|error| {
        BridgeError::new(ErrorCode::HdcFailed, format!("启动 hilog 失败: {error}"))
    })?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    Ok(ShellSessionHandle::from_handler(
        move |input, output, cancellation| async move {
            drop(input);
            let stdout_task = tokio::spawn(crate::shell::forward_output(
                stdout,
                fhdc_domain::ShellStream::Stdout,
                output.clone(),
                cancellation.clone(),
            ));
            let stderr_task = tokio::spawn(crate::shell::forward_output(
                stderr,
                fhdc_domain::ShellStream::Stderr,
                output,
                cancellation.clone(),
            ));
            let _ = child.wait().await;
            cancellation.cancel();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            Ok(None)
        },
    ))
}

fn hilog_arguments(key: &ConnectKey) -> Vec<OsString> {
    vec![
        OsString::from("-t"),
        OsString::from(key.as_str()),
        OsString::from("shell"),
        OsString::from("hilog"),
    ]
}
