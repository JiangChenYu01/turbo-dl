//! 统一任务模型：任务状态、设置、任务管理器（GUI 与 CLI 共用）。

use crate::engine::{self, DownloadConfig};
use crate::rate::GlobalRateLimiter;
use crate::util::parse_size;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Http,
    Ftp,
    Magnet,
    Git,
}

impl Protocol {
    pub fn detect(url: &str) -> Protocol {
        let u = url.trim().to_ascii_lowercase();
        if u.starts_with("magnet:") {
            Protocol::Magnet
        } else if u.starts_with("ftp://") || u.starts_with("ftps://") {
            Protocol::Ftp
        } else if crate::engine::git::is_git_url(&u) {
            Protocol::Git
        } else if u.ends_with(".torrent") {
            // .torrent 文件（本地路径或 http 链接）走 BT 引擎。
            Protocol::Magnet
        } else {
            Protocol::Http
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::Http => "HTTP",
            Protocol::Ftp => "FTP",
            Protocol::Magnet => "BT",
            Protocol::Git => "Git",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "排队中",
            TaskStatus::Downloading => "下载中",
            TaskStatus::Paused => "已暂停",
            TaskStatus::Completed => "已完成",
            TaskStatus::Failed => "失败",
            TaskStatus::Cancelled => "已取消",
        }
    }
}

/// 一条下载历史记录（持久化到本地 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub url: String,
    pub file_name: String,
    pub protocol: String,
    pub total: u64,
    pub downloaded: u64,
    pub status: String,
    pub created_at: u64,
    pub completed_at: u64,
    pub output_path: String,
    pub error: Option<String>,
}

/// 进行中任务的持久化快照（保存到 `%APPDATA%\turbo-dl\tasks.json`）。
/// 程序重启后由 TaskManager::restore_tasks 恢复，自动续传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub url: String,
    pub protocol: String,
    /// 输出目录：文件任务为文件所在目录，BT 任务为保存目录本身。
    pub output_dir: String,
    pub created_at: u64,
}

/// 下载历史的本地存储（最多保留 500 条）。
pub struct HistoryStore;

impl HistoryStore {
    pub fn path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("turbo-dl")
            .join("history.json")
    }

    pub fn load() -> Vec<HistoryEntry> {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn append(entry: HistoryEntry) {
        let mut entries = Self::load();
        entries.insert(0, entry);
        if entries.len() > 500 {
            entries.truncate(500);
        }
        Self::save(&entries);
    }

    pub fn save(entries: &[HistoryEntry]) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_string_pretty(entries) {
            let _ = std::fs::write(&path, data);
        }
    }

    pub fn clear() {
        let _ = std::fs::remove_file(Self::path());
    }
}

/// 单个下载任务的实时状态（GUI/CLI 通过 Arc<Mutex> 共享）。
pub struct TaskState {
    pub id: u64,
    pub url: String,
    pub protocol: Protocol,
    pub file_name: String,
    pub total: u64,
    pub downloaded: u64,
    pub speed: u64,
    pub status: TaskStatus,
    pub error: Option<String>,
    pub cancel: bool,
    pub pause: bool,
    pub output_path: PathBuf,
    /// 文件尾部块是否已就绪（ZIP/MP4 等可提前预览）。
    pub tail_ready: bool,
    /// 定时开始时间（Unix 时间戳；None 表示立即开始）。
    pub start_at: Option<u64>,
    /// 任务创建时间（Unix 时间戳）。
    pub created_at: u64,
    /// 是否已写入下载历史（防止重复记录）。
    pub history_recorded: bool,
    /// 引擎代次：每次 resume/重新开始时递增，旧引擎检测到不一致后主动退出，
    /// 避免「暂停后立即继续」导致两个引擎并发写同一文件。
    pub generation: u64,
}

impl TaskState {
    pub fn new(id: u64, url: String, protocol: Protocol) -> Self {
        Self {
            id,
            url: url.clone(),
            protocol,
            file_name: url,
            total: 0,
            downloaded: 0,
            speed: 0,
            status: TaskStatus::Queued,
            error: None,
            cancel: false,
            pause: false,
            output_path: PathBuf::new(),
            tail_ready: false,
            start_at: None,
            created_at: now_unix(),
            history_recorded: false,
            generation: 0,
        }
    }

    pub fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.downloaded as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

/// 全局设置。
#[derive(Clone)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub connections: usize,
    pub jobs: usize,
    pub rate_limit_text: String,
    /// 全局总限速（所有任务合计），留空不限速。
    pub global_rate_limit_text: String,
    pub user_agent: String,
    pub proxy_text: String,
    /// HTTP/FTP 认证（user:pass，可空）。
    pub auth_text: String,
    pub retries: u32,
    pub min_segment_size: u64,
    /// 定时开始（HH:MM，留空立即开始）。
    pub start_at_text: String,
    /// 全部任务完成后自动关机。
    pub shutdown_after: bool,
    /// 下载完成后自动打开文件/目录。
    pub open_after_complete: bool,
    /// 完成后自动解压压缩包（zip/tar/tar.gz）。
    pub auto_extract: bool,
    /// 解压成功后删除原压缩包。
    pub delete_archive_after_extract: bool,
    /// 完成后按类型自动分类到子目录。
    pub auto_classify: bool,
    /// 分类根目录（留空 = 下载文件所在目录）。
    pub classify_root: String,
    /// Referer（防盗链）。
    pub referer_text: String,
    /// Cookie（防盗链）。
    pub cookie_text: String,
    /// 完成后自动尝试旁车校验和（<file>.sha256 等）。
    pub checksum_sidecar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let download_dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            download_dir,
            connections: 8,
            jobs: 3,
            rate_limit_text: String::new(),
            global_rate_limit_text: String::new(),
            user_agent: format!("turbo-dl/{}", env!("CARGO_PKG_VERSION")),
            proxy_text: String::new(),
            auth_text: String::new(),
            retries: 5,
            min_segment_size: 1024 * 1024,
            start_at_text: String::new(),
            shutdown_after: false,
            open_after_complete: false,
            auto_extract: false,
            delete_archive_after_extract: false,
            auto_classify: false,
            classify_root: String::new(),
            referer_text: String::new(),
            cookie_text: String::new(),
            checksum_sidecar: false,
        }
    }
}

/// 任务管理器：持有 tokio 运行时句柄，统一调度 HTTP/FTP/BT 引擎。
/// Runtime 本身由调用方（main / GUI）创建并保持存活，避免在 async 上下文 drop 阻塞。
pub struct TaskManager {
    tasks: Arc<Mutex<Vec<Arc<Mutex<TaskState>>>>>,
    rt: tokio::runtime::Handle,
    next_id: AtomicU64,
    /// 并发任务数限制。
    semaphore: Arc<tokio::sync::Semaphore>,
    /// 全局总限速器（所有任务共享；未设置时为 None）。
    global_limiter: Option<Arc<GlobalRateLimiter>>,
    /// 待执行的下载后处理队列（(输出文件, 设置)），由 GUI 异步或 CLI 同步消费。
    postprocess_queue: Arc<Mutex<Vec<(PathBuf, Settings)>>>,
    pub settings: Arc<Mutex<Settings>>,
}

impl TaskManager {
    pub fn new(settings: Settings, rt: tokio::runtime::Handle) -> Self {
        let jobs = settings.jobs.max(1);
        let global_limiter = if settings.global_rate_limit_text.trim().is_empty() {
            None
        } else {
            parse_size(&settings.global_rate_limit_text)
                .ok()
                .map(|v| Arc::new(GlobalRateLimiter::new(v)))
        };
        // 信号量从 0 开始，随后 add_permits 注入初始并发额度；
        // 这样 GUI 修改「并发任务」时可动态增减许可，无需重启生效。
        let semaphore = Arc::new(tokio::sync::Semaphore::new(0));
        semaphore.add_permits(jobs);
        Self {
            tasks: Arc::new(Mutex::new(Vec::new())),
            rt,
            next_id: AtomicU64::new(1),
            semaphore,
            global_limiter,
            postprocess_queue: Arc::new(Mutex::new(Vec::new())),
            settings: Arc::new(Mutex::new(settings)),
        }
    }

    /// 动态调整并发任务数（GUI 修改设置时调用，立即生效）。
    pub fn update_jobs(&self, new_jobs: usize) {
        let new_jobs = new_jobs.max(1);
        let old_jobs = self.settings.lock().unwrap().jobs.max(1);
        if new_jobs == old_jobs {
            return;
        }
        self.settings.lock().unwrap().jobs = new_jobs;
        if new_jobs > old_jobs {
            self.semaphore.add_permits(new_jobs - old_jobs);
        } else {
            self.semaphore.forget_permits(old_jobs - new_jobs);
        }
    }

    pub fn tasks(&self) -> Arc<Mutex<Vec<Arc<Mutex<TaskState>>>>> {
        self.tasks.clone()
    }

    /// 返回 tokio 运行时句柄（GUI 本地集成服务等使用）。
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    /// 进行中任务快照文件路径。
    fn tasks_path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("turbo-dl")
            .join("tasks.json")
    }

    /// 持久化当前未完成任务列表（Completed/Failed/Cancelled 不保存）。
    /// 任务状态变化（添加/完成/暂停/移除）后调用，供重启自动恢复。
    pub fn persist_tasks(&self) {
        let list = self.tasks.lock().unwrap();
        let snapshots: Vec<TaskSnapshot> = list
            .iter()
            .filter(|t| {
                !matches!(
                    t.lock().unwrap().status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                )
            })
            .map(|t| {
                let t = t.lock().unwrap();
                // 文件任务的 output_path 是文件；BT/Git 任务的 output_path 是目录本身。
                let output_dir = if t.output_path.is_dir() {
                    t.output_path.display().to_string()
                } else {
                    t.output_path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                };
                TaskSnapshot {
                    url: t.url.clone(),
                    protocol: t.protocol.name().to_string(),
                    output_dir,
                    created_at: t.created_at,
                }
            })
            .collect();
        drop(list);
        let path = Self::tasks_path();
        if snapshots.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_string_pretty(&snapshots) {
            let _ = std::fs::write(&path, data);
        }
    }

    /// 从 tasks.json 恢复未完成任务，返回恢复的任务数。
    /// 恢复的任务使用当前设置并立即开始（续传），不套用旧的定时开始。
    pub fn restore_tasks(&self) -> usize {
        let path = Self::tasks_path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return 0;
        };
        let Ok(snapshots) = serde_json::from_str::<Vec<TaskSnapshot>>(&content) else {
            return 0;
        };
        let mut restored = 0;
        for snap in snapshots {
            if snap.url.trim().is_empty() {
                continue;
            }
            let settings = self.settings.lock().unwrap().clone();
            let mut cfg = DownloadConfig::from_settings(&settings);
            if !snap.output_dir.is_empty() {
                cfg.output_dir = PathBuf::from(&snap.output_dir);
            }
            cfg.start_at = None;
            if self.add_download_cfg(snap.url, cfg).is_ok() {
                restored += 1;
            }
        }
        restored
    }

    pub fn add_download(&self, url: String) -> Result<u64> {
        let settings = self.settings.lock().unwrap().clone();
        let cfg = DownloadConfig::from_settings(&settings);
        self.add_download_cfg(url, cfg)
    }

    /// 使用完整自定义配置添加任务（CLI 模式：-o/-H/--checksum 等参数全量生效）。
    pub fn add_download_cfg(&self, url: String, cfg: DownloadConfig) -> Result<u64> {
        // 注入全局总限速器（跨任务共享；任务级配置未显式设置时生效）。
        let mut cfg = cfg;
        if cfg.global_limiter.is_none() {
            cfg.global_limiter = self.global_limiter.clone();
        }
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("URL 不能为空");
        }
        // 显式校验协议，避免把任意文本当成 HTTP 任务静默添加后失败。
        let lower = url.to_ascii_lowercase();
        let supported = lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("ftp://")
            || lower.starts_with("ftps://")
            || lower.starts_with("magnet:")
            || lower.starts_with("git://")
            || lower.starts_with("ssh://git@")
            || lower.starts_with("git+")
            || (lower.ends_with(".torrent") && Path::new(&url).is_file());
        if !supported {
            anyhow::bail!("不支持的 URL: {url}（仅支持 http:// https:// ftp:// magnet: git://）");
        }
        let protocol = Protocol::detect(&url);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let task = Arc::new(Mutex::new(TaskState::new(
            id,
            url.clone(),
            protocol,
        )));
        {
            let mut t = task.lock().unwrap();
            t.start_at = cfg.start_at;
        }
        self.tasks.lock().unwrap().push(task.clone());
        self.spawn_engine(task.clone(), cfg);
        // 立即持久化任务快照，保证重启可恢复。
        self.persist_tasks();
        Ok(id)
    }

    /// 继续（恢复）一个已暂停/失败的任务。
    pub fn resume(&self, id: u64) -> Result<()> {
        let task = self.find(id).ok_or_else(|| anyhow!("任务不存在: {id}"))?;
        {
            let mut t = task.lock().unwrap();
            t.cancel = false;
            t.pause = false;
            t.error = None;
            t.status = TaskStatus::Queued;
            // 递增代次：仍在运行的旧引擎检测到不一致后会主动退出，
            // 避免「暂停后立即继续」导致双引擎并发写同一文件。
            t.generation += 1;
        }
        let settings = self.settings.lock().unwrap().clone();
        let mut cfg = DownloadConfig::from_settings(&settings);
        // 恢复任务应立刻开始，不再套用全局定时设置。
        cfg.start_at = None;
        self.spawn_engine(task.clone(), cfg);
        Ok(())
    }

    fn spawn_engine(&self, task: Arc<Mutex<TaskState>>, cfg: DownloadConfig) {
        let sem = self.semaphore.clone();
        let start_at = cfg.start_at;
        self.rt.spawn(async move {
            // 定时开始：到点前保持排队状态，不占用并发额度。
            if let Some(at) = start_at {
                let now = now_unix();
                if at > now {
                    tokio::time::sleep(Duration::from_secs(at - now)).await;
                }
            }
            let _permit = sem.acquire_owned().await;
            let result = engine::dispatch(task.clone(), cfg).await;
            if let Err(e) = result {
                let mut t = task.lock().unwrap();
                if t.cancel {
                    t.status = TaskStatus::Cancelled;
                } else if !t.pause {
                    t.status = TaskStatus::Failed;
                    let err_text = format!("{e:#}");
                    t.error = Some(err_text.clone());
                    tracing::error!(
                        task_id = t.id,
                        protocol = ?t.protocol,
                        url = %t.url,
                        "任务失败: {err_text}"
                    );
                }
            }
        });
    }

    pub fn cancel(&self, id: u64) {
        if let Some(t) = self.find(id) {
            t.lock().unwrap().cancel = true;
        }
    }

    pub fn set_pause(&self, id: u64, pause: bool) {
        if let Some(t) = self.find(id) {
            t.lock().unwrap().pause = pause;
        }
    }

    pub fn remove_task(&self, id: u64) {
        let mut list = self.tasks.lock().unwrap();
        // 先标记取消（底层引擎检测到后停止下载并保存断点），再从列表移除。
        list.retain(|t| {
            let keep = t.lock().unwrap().id != id;
            if !keep {
                t.lock().unwrap().cancel = true;
            }
            keep
        });
        drop(list);
        // 移除任务后同步任务快照。
        self.persist_tasks();
    }

    /// 取走待执行的下载后处理队列（GUI/CLI 各自消费）。
    pub fn drain_postprocess(&self) -> Vec<(PathBuf, Settings)> {
        std::mem::take(&mut self.postprocess_queue.lock().unwrap())
    }

    /// 将已进入终态且未记录的任务写入下载历史（GUI/CLI 周期调用）。
    /// 若设置「完成后自动打开」，同时触发打开文件。
    /// 返回本次新写入的历史条目（用于完成通知）。
    pub fn checkpoint_history(&self) -> Vec<HistoryEntry> {
        let list = self.tasks.lock().unwrap().clone();
        let settings = self.settings.lock().unwrap().clone();
        let mut recorded = Vec::new();
        for task in list {
            let entry = {
                let mut t = task.lock().unwrap();
                if t.history_recorded {
                    continue;
                }
                let finished = matches!(
                    t.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                );
                if !finished {
                    continue;
                }
                t.history_recorded = true;
                Some(HistoryEntry {
                    id: t.id,
                    url: t.url.clone(),
                    file_name: t.file_name.clone(),
                    protocol: t.protocol.name().to_string(),
                    total: t.total,
                    downloaded: t.downloaded,
                    status: format!("{:?}", t.status),
                    created_at: t.created_at,
                    completed_at: now_unix(),
                    output_path: t.output_path.display().to_string(),
                    error: t.error.clone(),
                })
            };
            let Some(entry) = entry else {
                continue;
            };
            // 释放任务锁后再做 IO/后处理，避免持锁阻塞。
            let p = PathBuf::from(&entry.output_path);
            if settings.open_after_complete && entry.status == "Completed" {
                if !p.as_os_str().is_empty() {
                    open_file(&p);
                }
            }
            // 下载后处理（自动解压/分类）：放入待处理队列，
            // 由 GUI（异步）或 CLI（退出前同步）消费执行。
            if (settings.auto_extract || settings.auto_classify)
                && entry.status == "Completed"
                && !p.as_os_str().is_empty()
            {
                self.postprocess_queue.lock().unwrap().push((p, settings.clone()));
            }
            HistoryStore::append(entry.clone());
            recorded.push(entry);
        }
        // 任务状态可能已变化（完成/失败/取消），同步任务快照。
        self.persist_tasks();
        recorded
    }

    pub fn open_folder(&self, id: u64) {
        if let Some(t) = self.find(id) {
            let path = t.lock().unwrap().output_path.clone();
            // BT 任务的 output_path 直接就是保存目录；文件任务则打开其父目录。
            if path.is_dir() {
                open_in_explorer(&path);
            } else {
                let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or(path);
                open_in_explorer(&dir);
            }
        }
    }

    fn find(&self, id: u64) -> Option<Arc<Mutex<TaskState>>> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.lock().unwrap().id == id)
            .cloned()
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn open_in_explorer(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// 用系统默认程序打开文件（下载完成后自动打开）。
///
/// 不使用 `cmd /c start`：文件名来自服务器（Content-Disposition 可控），
/// 经 cmd 解析时 `%` 会被环境变量展开（如 `%TEMP%`），存在路径劫持风险。
/// Windows 下用 explorer 直接打开（相当于双击，不经过 shell）。
pub fn open_file(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// 全部任务完成后关机/休眠（GUI「完成后动作」与 CLI --shutdown 共用）。
pub fn shutdown_computer() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("shutdown")
            .args(["/s", "/t", "60"])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("systemctl").arg("poweroff").spawn();
    }
}