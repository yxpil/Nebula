//! 轻量运行诊断日志:分级(ERROR/WARN/INFO/DEBUG)追加写文件,支持按大小滚动。
//!
//! 设计原则:
//! - 日志是**可靠性附属品**,任何情况下都不能反过来把程序拖垮——
//!   未初始化时静默、磁盘写失败时降级 stderr、锁中毒时恢复而非 panic;
//! - 不依赖外部日志框架,时间戳用自实现的 UTC 格式化;
//! - 路径、级别、单文件上限全部来自 nebula.toml 的 `[logging]` 段。
//!
//! 日志行格式:`2026-09-24 14:35:37 INFO  [module::path] 消息`。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// 日志级别(数值越大越详细)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

impl LogLevel {
    /// 从配置文本解析(大小写不敏感)。
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" | "trace" => Some(LogLevel::Debug),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN ",
            LogLevel::Info => "INFO ",
            LogLevel::Debug => "DEBUG",
        }
    }
}

/// 文件日志器:输出文件、当前级别、滚动上限。
struct FileLogger {
    file: Option<File>,
    path: PathBuf,
    level: LogLevel,
    max_bytes: u64,
}

static GLOBAL: OnceLock<RwLock<FileLogger>> = OnceLock::new();

/// 初始化全局日志。整个进程只应初始化一次;重复初始化返回错误
/// (调用方可忽略——它不影响程序运行)。
pub fn init(path: impl AsRef<Path>, level: LogLevel, max_bytes: u64) -> Result<(), String> {
    let path = path.as_ref().to_path_buf();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create log dir: {e}"))?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open log file {}: {e}", path.display()))?;
    let logger = FileLogger {
        file: Some(file),
        path,
        level,
        max_bytes: max_bytes.max(1024),
    };
    if GLOBAL.set(RwLock::new(logger)).is_err() {
        return Err("logger already initialized".into());
    }
    Ok(())
}

/// 当前是否已初始化(测试/诊断用)。
pub fn is_enabled() -> bool {
    GLOBAL.get().is_some()
}

/// 写一条日志。级别高于配置级别、或日志未初始化时直接忽略。
pub fn log(level: LogLevel, target: &str, message: &str) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    // 锁中毒也不 panic:取出内部数据继续写。
    let mut logger = match global.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if (level as u8) > (logger.level as u8) {
        return;
    }
    let line = format!(
        "{} {} [{}] {}\n",
        timestamp_now(),
        level.tag(),
        target,
        message
    );
    if let Err(e) = write_with_rotation(&mut logger, line.as_bytes()) {
        // 文件写不进去:降级到 stderr,保证错误至少能被看到。
        eprintln!("[fallback log] {target}: {message} (log file error: {e})");
    }
}

/// 追加写入;文件超过上限时先滚动为 `.1`(覆盖旧备份)再新建。
fn write_with_rotation(logger: &mut FileLogger, bytes: &[u8]) -> std::io::Result<()> {
    let Some(file) = logger.file.as_mut() else {
        return Ok(());
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size >= logger.max_bytes {
        // 先关闭当前文件句柄,再重命名。
        logger.file = None;
        let rotated = logger.path.with_extension("log.1");
        // Windows 上替换目标已存在会失败,先尝试删除。
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&logger.path, &rotated)?;
        let fresh = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&logger.path)?;
        logger.file = Some(fresh);
    }
    let Some(file) = logger.file.as_mut() else {
        return Ok(());
    };
    file.write_all(bytes)?;
    let _ = file.flush();
    Ok(())
}

// ------- UTC 时间戳(自实现,无外部依赖)-------

/// 当前 UTC 时间,格式 `YYYY-MM-DD HH:MM:SS`。
fn timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

/// UNIX 秒数 → UTC 日期时间(Howard Hinnant civil-from-days 算法)。
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // 0..=365
    let mp = (5 * doy + 2) / 153; // 0..=11
    let day = doy - (153 * mp + 2) / 5 + 1; // 1..=31
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // 1..=12
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parse_accepts_aliases() {
        assert_eq!(LogLevel::parse("ERROR"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse(" Info "), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("trace"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("verbose"), None);
    }

    #[test]
    fn utc_known_epoch_values() {
        // 1970-01-01 00:00:00 UTC
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        // 2000-01-01 00:00:00 UTC(闰年/千禧年边界)
        assert_eq!(format_utc(946_684_800), "2000-01-01 00:00:00");
        // 2024-02-29 12:30:45 UTC(闰年 2 月 29 日)
        assert_eq!(
            format_utc(1_709_209_845),
            "2024-02-29 12:30:45"
        );
    }

    #[test]
    fn logging_uninitialized_is_safe() {
        // 没有 GLOBAL 的环境里调用任何级别都不应崩溃。
        log(LogLevel::Error, "test", "ignored");
    }

    #[test]
    fn file_logger_writes_and_rotates() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("nebula_logger_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.log");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(dir.join("test.log.1"));

        // 直接构造 FileLogger 测试写入与滚动(绕过全局 OnceLock)。
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut logger = FileLogger {
            file: Some(file),
            path: path.clone(),
            level: LogLevel::Debug,
            max_bytes: 20,
        };
        write_with_rotation(&mut logger, b"first line that should fit\n").unwrap();
        // 27 字节已超过 20 上限:本次写入(写前检查)触发滚动。
        write_with_rotation(&mut logger, b"second line lands in fresh file\n").unwrap();
        assert!(path.exists());
        // 滚动后旧内容应在 .1 中。
        let old = std::fs::read_to_string(dir.join("test.log.1")).unwrap();
        assert!(old.contains("first line"));
        // 新行写入滚动后的新文件。
        let current = std::fs::read_to_string(&path).unwrap();
        assert!(current.contains("second line"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
