//! 下载引擎：协议识别与任务分发。

pub mod bt;
pub mod ftp;
pub mod git;
pub mod http;
pub mod m3u8;
pub mod mirror;
pub mod release;
pub mod rss;

use crate::app::{Protocol, Settings, TaskState};
use crate::rate::GlobalRateLimiter;
use crate::util::parse_size;
use anyhow::Result;
use reqwest::header::HeaderMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 下载引擎配置（由 Settings 派生）。
#[derive(Clone, Debug)]
pub struct DownloadConfig {
    pub output_dir: PathBuf,
    pub output_name: Option<String>,
    pub connections: usize,
    pub min_segment_size: u64,
    pub headers: HeaderMap,
    pub user_agent: String,
    pub proxy: Option<String>,
    /// HTTP/FTP 认证（user, pass）。
    pub auth: Option<(String, String)>,
    /// Referer（防盗链）。
    pub referer: Option<String>,
    /// Cookie（防盗链）。
    pub cookie: Option<String>,
    /// 完成后自动尝试旁车校验和。
    pub checksum_sidecar: bool,
    pub retries: u32,
    pub timeout: Duration,
    pub rate_limit: Option<u64>,
    /// 全局总限速器（跨任务共享；由 TaskManager 注入，引擎直接使用）。
    pub global_limiter: Option<Arc<GlobalRateLimiter>>,
    pub checksum: Option<(String, String)>,
    pub force_restart: bool,
    pub resume_enabled: bool,
    pub quiet: bool,
    /// 定时开始（Unix 时间戳；None 表示立即开始）。
    pub start_at: Option<u64>,
}

impl DownloadConfig {
    pub fn from_settings(s: &Settings) -> Self {
        let rate_limit = if s.rate_limit_text.trim().is_empty() {
            None
        } else {
            parse_size(&s.rate_limit_text).ok()
        };
        let proxy = if s.proxy_text.trim().is_empty() {
            None
        } else {
            Some(s.proxy_text.trim().to_string())
        };
        // HTTP/FTP 认证：user:pass；缺失冒号时视为无效，忽略。
        let auth = if s.auth_text.trim().is_empty() {
            None
        } else {
            s.auth_text
                .split_once(':')
                .map(|(u, p)| (u.trim().to_string(), p.trim().to_string()))
        };
        let referer = if s.referer_text.trim().is_empty() {
            None
        } else {
            Some(s.referer_text.trim().to_string())
        };
        let cookie = if s.cookie_text.trim().is_empty() {
            None
        } else {
            Some(s.cookie_text.trim().to_string())
        };
        // 定时开始：设置栏文本（HH:MM），非法/为空则立即开始。
        let start_at = crate::util::parse_start_at(&s.start_at_text).ok();
        Self {
            output_dir: s.download_dir.clone(),
            output_name: None,
            connections: s.connections.max(1),
            min_segment_size: s.min_segment_size.max(1),
            headers: HeaderMap::new(),
            user_agent: s.user_agent.clone(),
            proxy,
            auth,
            referer,
            cookie,
            checksum_sidecar: s.checksum_sidecar,
            retries: s.retries,
            timeout: Duration::from_secs(30),
            rate_limit,
            global_limiter: None, // 由 TaskManager 注入全局总限速器
            checksum: None,
            force_restart: false,
            resume_enabled: true,
            quiet: false,
            start_at,
        }
    }
}

/// 按协议分发到对应引擎。
/// `generation` 在引擎启动时读取：resume 会递增任务代次，旧引擎在运行中
/// 检测到代次不一致即主动退出，避免并发写同一文件。
pub async fn dispatch(task: Arc<Mutex<TaskState>>, cfg: DownloadConfig) -> Result<()> {
    let (protocol, generation) = {
        let t = task.lock().unwrap();
        (t.protocol, t.generation)
    };
    match protocol {
        Protocol::Http => http::download_http(task, cfg, generation).await,
        Protocol::Ftp => ftp::download_ftp(task, cfg, generation).await,
        Protocol::Magnet => bt::download_bt(task, cfg, generation).await,
        Protocol::Git => git::download_git(task, cfg, generation).await,
    }
}