//! C# `ClipHostLogFileHelper` — rolling log pick + truncated export.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Local;

/// C# ClipHost uses `cliphost`; Tauri keeps an isolated base name under BetterClipTauri.
pub const LOG_FILE_BASE_NAME: &str = "cliphost-tauri";
/// C# `ClipHostLogFileHelper.MaxExportChars` — applied as a byte seek, not a char count.
pub const MAX_EXPORT_CHARS: u64 = 512 * 1024;

pub fn current_log_file_path(log_folder: &Path) -> PathBuf {
    log_folder.join(format!("{LOG_FILE_BASE_NAME}.log"))
}

/// C# `Global.GetLogFiles`: `{LogFileBaseName}*.log` by LastWriteTime descending.
pub fn get_latest_log_path(log_folder: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let rd = std::fs::read_dir(log_folder).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !matches_rolling_log_name(name) {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .map(|(time, _)| modified > *time)
            .unwrap_or(true)
        {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

fn matches_rolling_log_name(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let prefix = LOG_FILE_BASE_NAME.to_ascii_lowercase();
    lower.starts_with(&prefix) && lower.ends_with(".log")
}

/// C# `TryReadLatestLogText`. Error text includes the log folder (no newline).
pub fn try_read_latest_log_text(log_folder: &Path) -> Result<(String, PathBuf, bool), String> {
    let Some(path) = get_latest_log_path(log_folder) else {
        return Err(format!("未找到日志文件。目录：{}", log_folder.display()));
    };
    match read_log_text(&path) {
        Ok((text, truncated)) => Ok((text, path, truncated)),
        Err(err) => Err(err.to_string()),
    }
}

fn read_log_text(log_path: &Path) -> std::io::Result<(String, bool)> {
    let header = format!(
        "// Log file: {}{nl}// Exported: {}{nl}{nl}",
        log_path.display(),
        Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
        nl = "\r\n",
    );
    let mut stream = open_shared_read(log_path)?;
    let length = stream.seek(SeekFrom::End(0))?;
    if length <= MAX_EXPORT_CHARS {
        stream.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes)?;
        let body = String::from_utf8_lossy(strip_utf8_bom(&bytes)).into_owned();
        return Ok((header + &body, false));
    }

    stream.seek(SeekFrom::Start(length - MAX_EXPORT_CHARS))?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    let tail = String::from_utf8_lossy(&bytes);
    let truncate_note = format!(
        "// [Truncated: last ~{}KB of {}KB]\r\n\r\n",
        MAX_EXPORT_CHARS / 1024,
        length / 1024
    );
    Ok((header + &truncate_note + &tail, true))
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    }
}

fn open_shared_read(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE — Serilog keeps the file open.
        opts.share_mode(0x0000_0007);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn missing_log_includes_folder_like_csharp() {
        let dir = tempdir().unwrap();
        let err = try_read_latest_log_text(dir.path()).unwrap_err();
        assert!(err.starts_with("未找到日志文件。目录："), "{err}");
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
    }

    #[test]
    fn ignores_jsonl_and_picks_latest_rolling_log() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("cloud-sync.jsonl"), "jsonl").unwrap();
        fs::write(dir.path().join("other.log"), "nope").unwrap();
        fs::write(dir.path().join("cliphost-tauri.log"), "older").unwrap();
        thread::sleep(Duration::from_millis(15));
        fs::write(dir.path().join("cliphost-tauri20260918.log"), "newer").unwrap();
        let path = get_latest_log_path(dir.path()).unwrap();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("cliphost-tauri20260918.log")
        );
        let (text, _, truncated) = try_read_latest_log_text(dir.path()).unwrap();
        assert!(!truncated);
        assert!(text.contains("// Log file:"));
        assert!(text.contains("// Exported:"));
        assert!(text.contains("newer"));
        assert!(!text.contains("jsonl"));
        assert!(!text.contains("nope"));
    }

    #[test]
    fn truncates_last_512kb_by_byte_seek() {
        let dir = tempdir().unwrap();
        let extra = 3000u64;
        let mut body = vec![b'H'; extra as usize];
        body.extend(std::iter::repeat(b'T').take(MAX_EXPORT_CHARS as usize));
        let path = dir.path().join("cliphost-tauri.log");
        fs::write(&path, &body).unwrap();
        let (text, _, truncated) = try_read_latest_log_text(dir.path()).unwrap();
        assert!(truncated);
        assert!(text.contains("// [Truncated: last ~512KB of 514KB]"));
        let tail = text.rsplit("\r\n\r\n").next().unwrap_or("");
        assert!(!tail.contains('H'), "prefix bytes leaked into tail; {tail:.32}");
        assert!(tail.contains('T'));
    }
}
