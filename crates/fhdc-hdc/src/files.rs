//! 远端文件操作:目录列表用带定界符的设备端脚本,传输用 `hdc file send/recv`。

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use fhdc_domain::{
    BridgeError, ConnectKey, ErrorCode, OverwritePolicy, RemoteFileEntry, RemoteFileKind,
    RemotePath,
};

use crate::ProcessHdcTransport;
use crate::process::run_bounded_cancellable;

/// 传输类命令的最短超时:大文件可能很久。
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(300);

/// 设备端列表脚本:kind\034name\034size\034modified\034perms\036 记录流,
/// 对任意文件名字符安全。字段用 FS(0x1C),记录用 RS(0x1E)——均为
/// "信息分隔符"控制字符,且刻意避开 NUL(部分通道会吞掉 \000)。
/// 转义用八进制而非 \xHH:八进制是 POSIX printf 标准写法,不依赖扩展。
/// 字段:'l'=符号链接 'd'=目录 'f'=普通文件 'o'=其它。
const LIST_SCRIPT: &str = r#"for entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
  [ -e "$entry" ] || [ -L "$entry" ] || continue
  if [ -L "$entry" ]; then kind=l
  elif [ -d "$entry" ]; then kind=d
  elif [ -f "$entry" ]; then kind=f
  else kind=o
  fi
  set -- $(stat -c '%s %Y %A' "$entry" 2>/dev/null || echo '0 0 ?')
  name=${entry##*/}
  printf '%s\034%s\034%s\034%s\034%s\036' "$kind" "$name" "$1" "$2" "$3"
done"#;

/// POSIX 单引号包裹,内部单引号转成 `'\''`。
pub(crate) fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// 把脚本与位置参数组成单个预引用命令串(规避旧式参数重解释)。
fn script_arguments(script: &str, positional: &[&str]) -> String {
    let mut line = format!("sh -c {}", shell_quote(script));
    for argument in positional {
        line.push(' ');
        line.push_str(&shell_quote(argument));
    }
    line
}

pub(crate) async fn list_directory(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    path: &RemotePath,
) -> Result<Vec<RemoteFileEntry>, BridgeError> {
    let command = script_arguments(LIST_SCRIPT, &["fhdc-files", path.as_str()]);
    let output = core.shell_text(key, &command).await?;
    let entries = parse_directory_entries(path, &output)?;
    Ok(entries)
}

async fn remote_exists(core: &ProcessHdcTransport, key: &ConnectKey, path: &RemotePath) -> bool {
    // hdc 不透传设备端退出码(实测 3.2.0d 恒为 0),存在性探测必须用文本
    // 标记;退出码判存在会让"不覆盖"模式的上传永远被误判为已存在。
    const SCRIPT: &str = r#"if [ -e "$1" ] || [ -L "$1" ]; then echo FHDC_EXISTS_MARK; fi"#;
    let command = script_arguments(SCRIPT, &["fhdc-files", path.as_str()]);
    core.shell_text(key, &command)
        .await
        .map(|output| output.contains("FHDC_EXISTS_MARK"))
        .unwrap_or(false)
}

pub(crate) async fn push_file(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    local: &Path,
    remote: &RemotePath,
    overwrite: OverwritePolicy,
    cancellation: &CancellationToken,
) -> Result<(), BridgeError> {
    if !local.is_file() {
        return Err(BridgeError::new(
            ErrorCode::PathNotFound,
            format!("本地文件不存在: {}", local.display()),
        ));
    }
    if overwrite == OverwritePolicy::Deny && remote_exists(core, key, remote).await {
        return Err(BridgeError::new(
            ErrorCode::AlreadyExists,
            format!("远端已存在: {}", remote.as_str()),
        ));
    }
    let arguments = vec![
        OsString::from("-t"),
        OsString::from(key.as_str()),
        OsString::from("file"),
        OsString::from("send"),
        local.as_os_str().to_os_string(),
        OsString::from(remote.as_str()),
    ];
    let output = run_bounded_cancellable(
        core.executable(),
        arguments,
        TRANSFER_TIMEOUT,
        64 * 1024,
        1024 * 1024,
        Some(cancellation),
    )
    .await?;
    interpret_transfer(&output, "上传")
}

pub(crate) async fn pull_file(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    remote: &RemotePath,
    local: &Path,
    overwrite: OverwritePolicy,
    cancellation: &CancellationToken,
) -> Result<(), BridgeError> {
    if local.exists() && (local.is_symlink() || overwrite == OverwritePolicy::Deny) {
        return Err(BridgeError::new(
            ErrorCode::AlreadyExists,
            format!("本地已存在: {}", local.display()),
        ));
    }
    let arguments = vec![
        OsString::from("-t"),
        OsString::from(key.as_str()),
        OsString::from("file"),
        OsString::from("recv"),
        OsString::from(remote.as_str()),
        local.as_os_str().to_os_string(),
    ];
    let output = run_bounded_cancellable(
        core.executable(),
        arguments,
        TRANSFER_TIMEOUT,
        64 * 1024,
        1024 * 1024,
        Some(cancellation),
    )
    .await?;
    interpret_transfer(&output, "下载")
}

pub(crate) async fn create_directory(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    path: &RemotePath,
) -> Result<(), BridgeError> {
    const SCRIPT: &str = r#"[ ! -e "$1" ] && [ ! -L "$1" ] || exit 17
mkdir -- "$1""#;
    run_mutation_script(core, key, SCRIPT, path, None).await
}

pub(crate) async fn rename_entry(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    from: &RemotePath,
    to: &RemotePath,
) -> Result<(), BridgeError> {
    const SCRIPT: &str = r#"[ -e "$1" ] || [ -L "$1" ] || exit 2
[ ! -e "$2" ] && [ ! -L "$2" ] || exit 17
mv -- "$1" "$2""#;
    run_mutation_script(core, key, SCRIPT, from, Some(to)).await
}

pub(crate) async fn delete_entry(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    path: &RemotePath,
) -> Result<(), BridgeError> {
    if path.as_str() == "/" {
        return Err(BridgeError::invalid_input("拒绝删除根目录"));
    }
    const SCRIPT: &str = r#"[ -e "$1" ] || [ -L "$1" ] || exit 22
rm -r -- "$1""#;
    run_mutation_script(core, key, SCRIPT, path, None).await
}

async fn run_mutation_script(
    core: &ProcessHdcTransport,
    key: &ConnectKey,
    script: &str,
    path: &RemotePath,
    second: Option<&RemotePath>,
) -> Result<(), BridgeError> {
    let to = second.map(|p| p.as_str()).unwrap_or("");
    let command = script_arguments(script, &["fhdc-files", path.as_str(), to]);
    let output = core.shell_exit_code(key, &command).await?;
    classify_mutation_output(&output.stdout, &output.stderr, output.exit_code)
}

/// 设备端 mutation(mkdir/mv/rm)的结果判定。实测 hdc 不透传设备端退出
/// 码(失败也退出 0),所以以设备端输出为唯一事实:成功时输出为空,
/// 失败时是 busybox 风格的 `mkdir: '...': File exists` 等文本。
fn classify_mutation_output(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
) -> Result<(), BridgeError> {
    let stderr = String::from_utf8_lossy(stderr);
    let stdout = String::from_utf8_lossy(stdout);
    let lowered = format!("{stderr}{stdout}").to_lowercase();
    if lowered.contains("file exists") {
        return Err(BridgeError::new(ErrorCode::AlreadyExists, "目标已存在"));
    }
    if lowered.contains("permission denied") || lowered.contains("read-only") {
        return Err(BridgeError::new(
            ErrorCode::PermissionDenied,
            format!("权限不足: {}", stderr.trim()),
        ));
    }
    if lowered.contains("no such file") || lowered.contains("not found") {
        return Err(BridgeError::new(ErrorCode::PathNotFound, "路径不存在"));
    }
    if !lowered.trim().is_empty() || exit_code.is_some_and(|code| code != 0) {
        return Err(map_command_error(stderr.as_bytes(), stdout.as_bytes()));
    }
    Ok(())
}

fn interpret_transfer(
    output: &crate::process::ProcessOutput,
    what: &str,
) -> Result<(), BridgeError> {
    if output.exit_code == Some(0) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}{stderr}").to_lowercase();
        if combined.contains("[fail]") || combined.contains("error") {
            return Err(BridgeError::new(
                ErrorCode::HdcFailed,
                format!(
                    "{what}失败: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        return Ok(());
    }
    Err(map_command_error(&output.stderr, &output.stdout))
}

fn map_command_error(stderr: &[u8], stdout: &[u8]) -> BridgeError {
    let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(stdout).trim().to_owned();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    let lowered = detail.to_lowercase();
    if lowered.contains("permission denied") || lowered.contains("read-only") {
        return BridgeError::new(ErrorCode::PermissionDenied, format!("权限不足: {detail}"));
    }
    if lowered.contains("no such file") || lowered.contains("not found") {
        return BridgeError::new(ErrorCode::PathNotFound, format!("路径不存在: {detail}"));
    }
    BridgeError::hdc(if detail.is_empty() {
        "hdc 命令失败".to_owned()
    } else {
        detail
    })
}

/// 解析 FS(0x1C) 分隔字段、RS(0x1E) 结尾记录的记录流。
pub(crate) fn parse_directory_entries(
    directory: &RemotePath,
    output: &str,
) -> Result<Vec<RemoteFileEntry>, BridgeError> {
    let mut entries = Vec::new();
    for record in output.split('\u{1e}') {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.split('\u{1c}').collect();
        if fields.len() != 5 {
            continue;
        }
        let kind = match fields[0] {
            "d" => RemoteFileKind::Directory,
            "f" => RemoteFileKind::File,
            "l" => RemoteFileKind::Symlink,
            _ => RemoteFileKind::Other,
        };
        let name = fields[1].to_owned();
        if name.is_empty() {
            continue;
        }
        let path = directory.join_name(&name)?;
        let size_bytes = fields[2].parse().ok();
        let modified_unix_seconds = fields[3].parse().ok();
        let permissions = (fields[4] != "?").then(|| fields[4].to_owned());
        entries.push(RemoteFileEntry {
            path,
            name,
            kind,
            size_bytes,
            modified_unix_seconds,
            permissions,
        });
    }
    entries.sort_by(|a, b| match (a.kind, b.kind) {
        (RemoteFileKind::Directory, RemoteFileKind::Directory) => {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
        (RemoteFileKind::Directory, _) => std::cmp::Ordering::Less,
        (_, RemoteFileKind::Directory) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records_with_control_separators() {
        let dir = RemotePath::new("/data/local/tmp").unwrap();
        let output = "d\x1Cetc\x1C4096\x1C1700000000\x1Cdrwxr-xr-x\x1ef\x1Ca b.txt\x1C12\x1C1700000001\x1C-rw-r--r--\x1e";
        let entries = parse_directory_entries(&dir, output).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "etc");
        assert_eq!(entries[0].kind, RemoteFileKind::Directory);
        assert_eq!(entries[0].path.as_str(), "/data/local/tmp/etc");
        assert_eq!(entries[1].name, "a b.txt");
        assert_eq!(entries[1].size_bytes, Some(12));
        // 目录永远排前
        assert!(matches!(entries[0].kind, RemoteFileKind::Directory));
    }

    #[test]
    fn tolerates_partial_records() {
        let dir = RemotePath::new("/data").unwrap();
        let entries =
            parse_directory_entries(&dir, "garbage\x1ed\x1Cok\x1C1\x1C2\x1Cdrwx\x1e\x1e").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "ok");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}
