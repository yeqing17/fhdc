//! `hdc shell` 会话:宿主侧伪控制台(ConPTY/openpty)承载的持久交互会话。
//!
//! 实测 hdc 3.2.0d 在管道 stdin 下不消费输入(只打印
//! "Not support stdio TTY mode"),只有挂上真控制台才进入 TTY 模式,
//! 所以这里用 portable-pty 给 hdc 分配伪终端;GUI 侧拿到的是与
//! 假后端一致的通道化句柄,字节进、字节出。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use fhdc_domain::{BridgeError, ConnectKey, ErrorCode, ShellOutputChunk, ShellSize, ShellStream};

/// 关闭会话时等待转发线程收尾的宽限期。
const CLOSE_GRACE: Duration = Duration::from_millis(750);
/// 读转发线程的缓冲块大小。
const CHUNK_BYTES: usize = 8 * 1024;

/// 进程无关的交互式会话句柄:GUI 只见通道,不见子进程。
pub struct ShellSessionHandle {
    input: mpsc::Sender<Vec<u8>>,
    output: mpsc::Receiver<ShellOutputChunk>,
    cancellation: CancellationToken,
    completion: tokio::task::JoinHandle<Result<Option<i32>, BridgeError>>,
    /// 丢弃句柄时同步执行的兜底清理(杀掉 hdc 子进程,避免泄漏)。
    kill_hook: Option<Box<dyn FnOnce() + Send>>,
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
        let (input_tx, input_rx) = mpsc::channel(128);
        let (output_tx, output_rx) = mpsc::channel(128);
        let cancellation = CancellationToken::new();
        let completion = tokio::spawn(handler(input_rx, output_tx, cancellation.clone()));
        Self {
            input: input_tx,
            output: output_rx,
            cancellation,
            completion,
            kill_hook: None,
        }
    }

    /// 附带一个丢弃句柄时同步执行的清理动作。驱动任务可能来不及走到
    /// 取消分支就被 abort,子进程必须在这里兜底收掉。
    pub fn with_kill_hook(mut self, hook: impl FnOnce() + Send + 'static) -> Self {
        self.kill_hook = Some(Box::new(hook));
        self
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
        if let Some(hook) = self.kill_hook.take() {
            hook();
        }
    }
}

/// 启动一个挂在伪控制台上的 `hdc -t <key> shell` 持久会话。
pub fn start_shell(
    executable: PathBuf,
    key: &ConnectKey,
    size: ShellSize,
) -> Result<ShellSessionHandle, BridgeError> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: size.rows(),
            cols: size.columns(),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| {
            BridgeError::new(ErrorCode::HdcFailed, format!("分配伪终端失败: {error}"))
        })?;
    let mut command = CommandBuilder::new(executable.as_os_str());
    command.args(["-t", key.as_str(), "shell"]);
    let child = pair.slave.spawn_command(command).map_err(|error| {
        BridgeError::new(
            ErrorCode::HdcFailed,
            format!("启动 hdc shell 失败: {error}"),
        )
    })?;
    // slave 句柄只在 spawn 时需要;留着会妨碍后续的 EOF 与退出。
    drop(pair.slave);
    let writer = pair.master.take_writer().map_err(|error| {
        BridgeError::new(ErrorCode::HdcFailed, format!("打开终端输入失败: {error}"))
    })?;
    let reader = pair.master.try_clone_reader().map_err(|error| {
        BridgeError::new(ErrorCode::HdcFailed, format!("打开终端输出失败: {error}"))
    })?;
    let master = pair.master;
    let child = Arc::new(Mutex::new(child));
    let kill_child = Arc::clone(&child);

    Ok(
        ShellSessionHandle::from_handler(move |input, output, cancellation| async move {
            run_pty(child, master, writer, reader, input, output, cancellation).await
        })
        .with_kill_hook(move || {
            // 已退出的进程 kill 是无害的空操作。
            if let Ok(mut child) = kill_child.lock() {
                let _ = child.kill();
            }
        }),
    )
}

/// 会话驱动:两个阻塞线程搬运主/从两侧的字节,协程只负责等取消或退出。
async fn run_pty(
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut writer: Box<dyn std::io::Write + Send>,
    mut reader: Box<dyn std::io::Read + Send>,
    mut input_rx: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<ShellOutputChunk>,
    cancellation: CancellationToken,
) -> Result<Option<i32>, BridgeError> {
    // 输出侧:阻塞读 master,经 blocking_send 送进通道。
    let reader_cancel = cancellation.clone();
    let reader_task = tokio::task::spawn_blocking(move || {
        let mut buffer = vec![0u8; CHUNK_BYTES];
        loop {
            if reader_cancel.is_cancelled() {
                break;
            }
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let piece = ShellOutputChunk {
                        stream: ShellStream::Stdout,
                        bytes: buffer[..read].to_vec(),
                    };
                    if output.blocking_send(piece).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // 输入侧:通道字节写到 master writer(PTY 语义:键入直达设备 shell)。
    let writer_task = tokio::task::spawn_blocking(move || {
        while let Some(mut bytes) = input_rx.blocking_recv() {
            // 终端惯例:Enter/换行以 CR 进入 PTY,由设备端行规程(icrnl)
            // 转回 NL;快捷指令与多行粘贴的 \n 在这里统一翻译。
            if bytes.contains(&b'\n') {
                bytes = bytes
                    .iter()
                    .map(|&byte| if byte == b'\n' { b'\r' } else { byte })
                    .collect();
            }
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    // 等子进程退出;取消则先杀再收尸。
    let wait_child = Arc::clone(&child);
    let mut wait_task = tokio::task::spawn_blocking(move || {
        let mut child = wait_child.lock().unwrap();
        child
            .wait()
            .ok()
            .and_then(|status| i32::try_from(status.exit_code()).ok())
    });
    let status;
    tokio::select! {
        waited = &mut wait_task => {
            status = waited.unwrap_or_default();
            tracing::debug!(?status, "pty child exited");
            cancellation.cancel();
        }
        () = cancellation.cancelled() => {
            tracing::debug!("pty session cancelled, killing child");
            let _ = child.lock().unwrap().kill();
            status = match tokio::time::timeout(Duration::from_secs(2), &mut wait_task).await {
                Ok(Ok(code)) => code,
                _ => None,
            };
        }
    }

    // master 掉线让 reader 立刻 EOF;输入通道关闭让 writer 退出。
    drop(master);
    let reader_finished = tokio::time::timeout(CLOSE_GRACE, reader_task).await.is_ok();
    tracing::debug!(reader_finished, "pty reader finished");
    let _ = tokio::time::timeout(CLOSE_GRACE, writer_task).await;
    Ok(status)
}

/// 把一段输出持续转发到通道;通道关闭则安静退出(hilog 流使用)。
pub(crate) async fn forward_output<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: R,
    stream: ShellStream,
    sender: mpsc::Sender<ShellOutputChunk>,
    cancellation: CancellationToken,
) {
    use tokio::io::AsyncReadExt;
    let mut chunk = vec![0u8; 8 * 1024];
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
