//! 统一日志系统（tracing + tracing-subscriber）。
//!
//! - **控制台**：stderr，级别由 `RUST_LOG` 控制（默认 `info`）。
//! - **文件**：`%APPDATA%\turbo-dl\logs\turbo-dl.log`，级别由 `RUST_LOG_FILE` 控制
//!   （默认 `debug`，比控制台更详细，便于排查问题）。
//! - 文件超过 5 MiB 时自动轮转为 `turbo-dl.old.log`（覆盖上一次旧日志）。
//! - CLI 的最终结果（`✓/✗` 汇总）仍输出到 stdout，不受日志系统影响。

use std::fs::OpenOptions;
use std::path::PathBuf;
use tracing_subscriber::prelude::*;

/// 单个日志文件大小上限（超过后轮转）。
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// 日志目录：`%APPDATA%\turbo-dl\logs`。
fn log_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("turbo-dl")
        .join("logs")
}

/// 初始化日志系统（在 main 开头调用一次）。
pub fn init_logging() {
    let dir = log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("⚠ 创建日志目录失败: {e}");
        return;
    }
    let log_path = dir.join("turbo-dl.log");
    // 简单轮转：超过上限时把旧日志改名 .old（覆盖上一次的 .old）。
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > MAX_LOG_BYTES {
            let old = dir.join("turbo-dl.old.log");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&log_path, old);
        }
    }

    // 控制台层：RUST_LOG 控制（默认 info）。
    let console_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_filter(console_filter);

    let registry = tracing_subscriber::registry().with(console_layer);

    // 文件层：RUST_LOG_FILE 控制（默认 debug，比控制台更详细）。
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let file_filter = std::env::var("RUST_LOG_FILE")
            .unwrap_or_else(|_| "debug".to_string());
        let file_filter = tracing_subscriber::EnvFilter::new(file_filter);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(file)
            .with_ansi(false)
            .with_target(true)
            .with_filter(file_filter);
        registry.with(file_layer).init();
    } else {
        registry.init();
    }
}