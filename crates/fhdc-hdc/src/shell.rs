//! 交互式 `hdc shell` 会话:通道化的句柄让进程后端与假后端共享同一套语义。

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use fhdc_domain::{BridgeError, ConnectKey, ErrorCode, ShellOutputChunk, ShellStream};

const CHUNK_BYTES: usize = 8 * 1024;
const CHANNEL_CAPACITY: usize = 128;
const MAX_INPUT_BATCH_CHUNKS: usize = 16;
/// 关闭后等待进程退出的宽限期。
const CLOSE_GRACE: Duration = Duration::from_millis(750);

/// 进程无关的交互式会话句柄:GUI 只见通道,不见子进程。
pub struct ShellSessionHandle {
    input: mpsc::Sender<Vec<u8>>,
    output: mpsc::Receiver<ShellOutputChunk>,
    cancellation: CancellationToken,
    completion: tokio::task::JoinHandle<Result<Option<i32>, BridgeError>>,
}

impl ShellSessionHandle {
    /// 用自定义驱动函数构造(假后端与 hilog 流也走这里)。
    pub fn from_handler<F, Fut>(handler: F) -> Self
    where
        F: FnOnce(
            mpsc::Receiver<Vec<u8>>,
            mpsc::Sender<ShellOutputChunk>,
            CancellationToken,
        ) -> Fut,
        Fut: std::future::Future<Output = Result<Option<i32>, BridgeError>> + Send + 'static,
    {
        let (input_tx, input_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (output_tx, output_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let cancellation = CancellationToken::new();
        let completion = tokio::spawn(handler(input_rx, output_tx, cancellation.clone()));
        Self {
            input: input_tx,
            output: output_rx,
            cancellation,
            completion,
        }
    }

    pub fn input(&self) -> mpsc::Sender<Vec<u8>> {
        self.input.clone()
    }

    pub async fn next_chunk(&mut self) -> Option<ShellOutputChunk> {
        self.output.recv().await
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn close(mut self) -> Result<Option<i32>, BridgeError> {
        self.cancel();
        let result = (&mut self.completion).await;
        match result {
            Ok(result) => result,
            Err(error) => Err(BridgeError::new(
                ErrorCode::Internal,
                format!("shell 任务异常结束: {error}"),
            )),
        }
    }

    /// 等待驱动任务结束并取回退出码(不取消,输出通道关闭后调用)。
    pub async fn wait(&mut self) -> Result<Option<i32>, BridgeError> {
        let result = (&mut self.completion).await;
        match result {
            Ok(result) => result,
            Err(error) => Err(BridgeError::new(
                ErrorCode::Internal,
                format!("shell 任务异常结束: {error}"),
            )),
        }
    }
}

impl Drop for ShellSessionHandle {
    fn drop(&mut self) {
        self.cancel();
        self.completion.abort();
    }
}

fn shell_arguments(key: &ConnectKey) -> Vec<OsString> {
    vec![
        OsString::from("-t"),
        OsString::from(key.as_str()),
        OsString::from("shell"),
    ]
}

/// 启动一个交互式 `hdc -t <key> shell` 会话。
pub fn start_shell(
    executable: PathBuf,
    key: &ConnectKey,
) -> Result<ShellSessionHandle, BridgeError> {
    let mut command = Command::new(&executable);
    command
        .args(shell_arguments(key))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    super::process::configure_command(&mut command);
    let mut child = command.spawn().map_err(|error| {
        BridgeError::new(
            ErrorCode::HdcFailed,
            format!("启动 hdc shell 失败: {error}"),
        )
    })?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    Ok(ShellSessionHandle::from_handler(
        move |input, output, cancellation| async move {
            run_shell(child, stdin, input, output, stdout, stderr, cancellation).await
        },
    ))
}

async fn run_shell(
    mut child: Child,
    mut stdin: ChildStdin,
    mut input_rx: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<ShellOutputChunk>,
    stdout: impl AsyncRead + Send + Unpin + 'static,
    stderr: impl AsyncRead + Send + Unpin + 'static,
    cancellation: CancellationToken,
) -> Result<Option<i32>, BridgeError> {
    let stdout_task = tokio::spawn(forward_output(
        stdout,
        ShellStream::Stdout,
        output.clone(),
        cancellation.clone(),
    ));
    let stderr_task = tokio::spawn(forward_output(
        stderr,
        ShellStream::Stderr,
        output,
        cancellation.clone(),
    ));

    loop {
        tokio::select! {
            () = cancellation.cancelled() => break,
            input = input_rx.recv() => {
                let Some(input) = input else { break };
                let batch = collect_input_batch(input, &mut input_rx);
                // ChildStdin 无缓冲,egui 一帧可能连发多段,合并后一次写完。
                let joined: Vec<u8> = batch.concat();
                if let Err(error) = stdin.write_all(&joined).await {
                    cancellation.cancel();
                    finish_reader(stdout_task).await?;
                    finish_reader(stderr_task).await?;
                    return Err(BridgeError::new(ErrorCode::HdcFailed, format!("写入 shell stdin 失败: {error}")));
                }
                let _ = stdin.flush().await;
            }
            status = child.wait() => {
                cancellation.cancel();
                finish_reader(stdout_task).await?;
                finish_reader(stderr_task).await?;
                return Ok(status.ok().and_then(|status| status.code()));
            }
        }
    }

    drop(stdin);
    if wait_exit(&mut child, CLOSE_GRACE).await {
        finish_reader(stdout_task).await?;
        finish_reader(stderr_task).await?;
        return Ok(child.wait().await.ok().and_then(|status| status.code()));
    }
    let _ = child.start_kill();
    if wait_exit(&mut child, CLOSE_GRACE).await {
        finish_reader(stdout_task).await?;
        finish_reader(stderr_task).await?;
        return Ok(child.wait().await.ok().and_then(|status| status.code()));
    }
    finish_reader(stdout_task).await?;
    finish_reader(stderr_task).await?;
    Err(BridgeError::new(
        ErrorCode::TimedOut,
        "shell 关闭超时,已强制结束",
    ))
}

/// 把一段输出持续转发到通道;通道关闭则安静退出。
pub(crate) async fn forward_output<R: AsyncRead + Unpin>(
    mut pipe: R,
    stream: ShellStream,
    sender: mpsc::Sender<ShellOutputChunk>,
    cancellation: CancellationToken,
) {
    let mut chunk = vec![0u8; CHUNK_BYTES];
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return,
            read = pipe.read(&mut chunk) => match read {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    let piece = ShellOutputChunk { stream, bytes: chunk[..read].to_vec() };
                    if sender.send(piece).await.is_err() {
                        return;
                    }
                }
            },
        }
    }
}

async fn finish_reader(task: tokio::task::JoinHandle<()>) -> Result<(), BridgeError> {
    match tokio::time::timeout(CLOSE_GRACE, task).await {
        Ok(_) => Ok(()),
        Err(_) => Err(BridgeError::new(
            ErrorCode::TimedOut,
            "shell 输出转发未在宽限期内结束",
        )),
    }
}

fn collect_input_batch(first: Vec<u8>, rest: &mut mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut batch = vec![first];
    while batch.len() < MAX_INPUT_BATCH_CHUNKS {
        match rest.try_recv() {
            Ok(chunk) => batch.push(chunk),
            Err(_) => break,
        }
    }
    batch
}

async fn wait_exit(child: &mut Child, grace: Duration) -> bool {
    matches!(tokio::time::timeout(grace, child.wait()).await, Ok(Ok(_)))
}
