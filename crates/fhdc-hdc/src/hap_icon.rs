//! 从 HAP 里解出应用图标。
//!
//! HAP 是 zip 容器,图标是 `resources/base/media/` 下的图片;这里实现一个
//! 只读的最小 zip 解析(EOCD + 中央目录 + 本地头),避免为此引入完整
//! zip 依赖。

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

/// EOCD 结尾签名 `PK\x05\x06`。
const EOCD_SIGNATURE: u32 = 0x0605_4B50;
/// 中央目录条目签名 `PK\x01\x02`。
const CD_SIGNATURE: u32 = 0x0201_4B50;
/// 本地文件头签名 `PK\x03\x04`。
const LOCAL_SIGNATURE: u32 = 0x0403_4B50;
/// zip 注释上限(64 KiB 减 1)。
const MAX_COMMENT_BYTES: u64 = 65_535;
/// 单张图标解压后的上限,防止异常包把内存打满。
const MAX_ICON_BYTES: u64 = 4 * 1024 * 1024;
/// 中央目录读取上限(单个 HAP 的 CD 远小于此)。
const MAX_CENTRAL_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;
/// 图标文件扩展名(按优先级)。
const ICON_EXTENSIONS: [&str; 4] = ["png", "webp", "jpg", "jpeg"];

#[derive(Debug, Clone)]
struct ZipEntry {
    name: String,
    method: u16,
    compressed_size: u64,
    local_offset: u64,
}

/// 解出 HAP 中的应用图标(原始图片字节);找不到返回 None。
pub fn extract_hap_icon(hap: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(hap).ok()?;
    let file_size = file.metadata().ok()?.len();
    let entries = read_central_directory(&mut file, file_size)?;
    let by_name: HashMap<&str, &ZipEntry> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();

    for candidate in icon_candidates(&mut file, &entries, &by_name) {
        let Some(entry) = by_name.get(candidate.as_str()).copied() else {
            continue;
        };
        let Some(bytes) = read_entry(&mut file, entry) else {
            continue;
        };
        if is_supported_image(&bytes) {
            return Some(bytes);
        }
    }
    None
}

/// 候选图标路径:先按 module.json 的 `$media:` 引用,再按常见文件名,
/// 最后扫描 media 目录里任何一个图片。保持顺序即优先级。
fn icon_candidates(
    file: &mut File,
    entries: &[ZipEntry],
    by_name: &HashMap<&str, &ZipEntry>,
) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();

    // module.json 的 app.icon / module.icon / ability.icon。
    if let Some(entry) = by_name.get("module.json").copied()
        && let Some(bytes) = read_entry(file, entry)
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(media) = module_icon_reference(&json)
    {
        for extension in ICON_EXTENSIONS {
            candidates.push(format!("resources/base/media/{media}.{extension}"));
        }
    }

    for base in ["app_icon", "icon", "main_icon"] {
        for extension in ICON_EXTENSIONS {
            candidates.push(format!("resources/base/media/{base}.{extension}"));
        }
    }

    // 兜底:media 目录下名字里带 icon 的最短路径优先,其次任意图片。
    let mut media: Vec<&ZipEntry> = entries
        .iter()
        .filter(|entry| entry.name.starts_with("resources/base/media/"))
        .filter(|entry| {
            ICON_EXTENSIONS
                .iter()
                .any(|ext| entry.name.ends_with(&format!(".{ext}")))
        })
        .collect();
    media.sort_by_key(|entry| {
        (
            !entry.name.to_lowercase().contains("icon"),
            entry.name.len(),
        )
    });
    candidates.extend(media.into_iter().map(|entry| entry.name.clone()));

    candidates.dedup();
    candidates
}

/// 从 module.json 里找图标媒体名(去掉 `$media:` 前缀)。
fn module_icon_reference(json: &serde_json::Value) -> Option<String> {
    let media = |value: Option<&serde_json::Value>| {
        value
            .and_then(|value| value.as_str())
            .and_then(|reference| reference.strip_prefix("$media:"))
            .map(str::to_owned)
    };
    media(json.get("app").and_then(|app| app.get("icon")))
        .or_else(|| media(json.get("module").and_then(|module| module.get("icon"))))
        .or_else(|| {
            json.get("module")
                .and_then(|module| module.get("abilities"))
                .and_then(|abilities| abilities.as_array())
                .and_then(|abilities| abilities.first())
                .and_then(|ability| media(ability.get("icon")))
        })
}

/// 读 EOCD 并解析中央目录。
fn read_central_directory(file: &mut File, file_size: u64) -> Option<Vec<ZipEntry>> {
    let tail_length = (MAX_COMMENT_BYTES + 22).min(file_size);
    let tail_start = file_size - tail_length;
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut tail = vec![0u8; usize::try_from(tail_length).ok()?];
    file.read_exact(&mut tail).ok()?;

    let eocd = find_eocd(&tail)?;
    let entries_in_directory = u16::from_le_bytes([eocd[10], eocd[11]]) as u64;
    let directory_size = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]) as u64;
    let directory_offset = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]) as u64;
    if directory_offset == u64::from(u32::MAX) || directory_size == u64::from(u32::MAX) {
        // zip64:单 HAP 不会到这个量级,直接放弃。
        return None;
    }
    if directory_size > MAX_CENTRAL_DIRECTORY_BYTES
        || directory_offset.saturating_add(directory_size) > file_size
    {
        return None;
    }

    file.seek(SeekFrom::Start(directory_offset)).ok()?;
    let mut directory = vec![0u8; usize::try_from(directory_size).ok()?];
    file.read_exact(&mut directory).ok()?;

    let mut entries = Vec::new();
    let mut cursor = 0usize;
    while cursor + 46 <= directory.len() && entries.len() < entries_in_directory as usize {
        if read_u32(&directory, cursor)? != CD_SIGNATURE {
            break;
        }
        let method = read_u16(&directory, cursor + 10)?;
        let compressed_size = read_u32(&directory, cursor + 20)? as u64;
        let name_length = read_u16(&directory, cursor + 28)? as usize;
        let extra_length = read_u16(&directory, cursor + 30)? as usize;
        let comment_length = read_u16(&directory, cursor + 32)? as usize;
        let local_offset = read_u32(&directory, cursor + 42)? as u64;
        let name_start = cursor + 46;
        let name_end = name_start.checked_add(name_length)?;
        if name_end > directory.len() {
            return None;
        }
        let name = String::from_utf8_lossy(&directory[name_start..name_end]).into_owned();
        entries.push(ZipEntry {
            name,
            method,
            compressed_size,
            local_offset,
        });
        cursor = name_end
            .checked_add(extra_length)?
            .checked_add(comment_length)?;
    }
    Some(entries)
}

/// 在尾部缓冲里从后往前找 EOCD 签名。
fn find_eocd(tail: &[u8]) -> Option<&[u8]> {
    if tail.len() < 22 {
        return None;
    }
    (0..=tail.len() - 22)
        .rev()
        .find(|&index| read_u32(tail, index) == Some(EOCD_SIGNATURE))
        .map(|index| &tail[index..])
}

/// 解出单个条目:method 0 原样,method 8 走 deflate。
fn read_entry(file: &mut File, entry: &ZipEntry) -> Option<Vec<u8>> {
    if entry.compressed_size > MAX_ICON_BYTES {
        return None;
    }
    file.seek(SeekFrom::Start(entry.local_offset)).ok()?;
    let mut header = [0u8; 30];
    file.read_exact(&mut header).ok()?;
    if u32::from_le_bytes([header[0], header[1], header[2], header[3]]) != LOCAL_SIGNATURE {
        return None;
    }
    let name_length = u16::from_le_bytes([header[26], header[27]]) as u64;
    let extra_length = u16::from_le_bytes([header[28], header[29]]) as u64;
    let data_offset = entry
        .local_offset
        .checked_add(30)?
        .checked_add(name_length)?
        .checked_add(extra_length)?;
    file.seek(SeekFrom::Start(data_offset)).ok()?;

    let mut compressed = vec![0u8; usize::try_from(entry.compressed_size).ok()?];
    file.read_exact(&mut compressed).ok()?;
    match entry.method {
        0 => Some(compressed),
        8 => {
            let mut decoder =
                flate2::read::DeflateDecoder::new(compressed.as_slice()).take(MAX_ICON_BYTES);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output).ok()?;
            Some(output)
        }
        _ => None,
    }
}

/// 只接受 image crate 能解的位图格式。
fn is_supported_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || bytes.starts_with(&[0xFF, 0xD8])
        || (bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 夹具:一个最小 HAP,module.json 引用 `$media:app_icon`,
    /// 图标是 1x1 红点 PNG(deflate 压缩)。
    const TINY_HAP: &[u8] = include_bytes!("../testdata/tiny.hap");

    #[test]
    fn extracts_icon_from_module_json_reference() {
        // 写进 target/ 而不是系统临时目录:后者的位置由环境变量决定,
        // 在 TMP/TEMP 配置异常的机器上(如指向受限的 C:\WINDOWS\TEMP)
        // 会写不进去。
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        // 某些安全软件会拦截新编译的未签名进程写文件(本机 Windows
        // Defender 转主动后出现过)。环境不允许时跳过,不算失败。
        if std::fs::create_dir_all(&directory).is_err() {
            eprintln!("跳过:无法创建测试目录(安全软件可能拦截了新编译的进程)");
            return;
        }
        let path = directory.join("fhdc_tiny.bin");
        let Err(error) = std::fs::write(&path, TINY_HAP) else {
            let icon = extract_hap_icon(&path).expect("icon");
            assert!(is_supported_image(&icon));
            assert!(icon.starts_with(&[0x89, b'P', b'N', b'G']));
            let _ = std::fs::remove_file(&path);
            return;
        };
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            eprintln!("跳过:环境拒绝写入(安全软件可能拦截了新编译的进程)");
            return;
        }
        panic!("write fixture to {}: {error}", path.display());
    }

    /// 用真实的设备 HAP 验证(需要 `FHDC_TEST_HAP` 指向本地 .hap)。
    #[test]
    #[ignore]
    fn extracts_icon_from_real_hap() {
        let Ok(path) = std::env::var("FHDC_TEST_HAP") else {
            return;
        };
        let icon = extract_hap_icon(Path::new(&path)).expect("real icon");
        assert!(is_supported_image(&icon));
        eprintln!("real hap icon: {} bytes", icon.len());
    }

    #[test]
    fn missing_file_returns_none() {
        assert!(extract_hap_icon(Path::new("Z:/definitely/not/here.hap")).is_none());
    }

    #[test]
    fn image_magic_detection() {
        assert!(is_supported_image(&[0x89, b'P', b'N', b'G', 0]));
        assert!(is_supported_image(&[0xFF, 0xD8, 0xFF]));
        assert!(!is_supported_image(b"<svg></svg>"));
        assert!(!is_supported_image(b""));
    }
}
