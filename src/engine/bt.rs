//! BitTorrent 引擎（磁力链接 / .torrent 文件），基于 librqbit。
//!
//! 支持三种输入：
//! - `magnet:` 磁力链接（`AddTorrent::Url`）；
//! - 本地 `.torrent` 文件路径（`AddTorrent::TorrentFileBytes`）；
//! - http(s) `.torrent` 链接（先下载字节再 `TorrentFileBytes`）。

use crate::app::{TaskState, TaskStatus};
use crate::engine::DownloadConfig;
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use librqbit::Session;
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 执行一次 BT 下载。
/// `generation` 为启动时任务代次；若运行中被 resume 递增，旧引擎需主动退出。
pub async fn download_bt(
    task: Arc<Mutex<TaskState>>,
    cfg: DownloadConfig,
    generation: u64,
) -> Result<()> {
    let url = task.lock().unwrap().url.clone();

    // 根据输入类型构造 AddTorrent。
    let add: librqbit::AddTorrent<'_> = if url.starts_with("magnet:") {
        librqbit::AddTorrent::Url(Cow::Owned(url.clone()))
    } else if is_local_torrent_file(&url) {
        let bytes = tokio::fs::read(&url)
            .await
            .with_context(|| format!("读取 .torrent 文件失败: {url}"))?;
        librqbit::AddTorrent::TorrentFileBytes(Bytes::from(bytes))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let resp = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, &cfg.user_agent)
            .send()
            .await
            .with_context(|| format!("下载 .torrent 失败: {url}"))?;
        if !resp.status().is_success() {
            bail!("下载 .torrent 失败: HTTP {}", resp.status());
        }
        let bytes = resp.bytes().await.context("读取 .torrent 失败")?;
        librqbit::AddTorrent::TorrentFileBytes(bytes)
    } else {
        anyhow::bail!("不支持的 BT 输入: {url}（仅支持 magnet: 或 .torrent 文件）");
    };

    let session = Session::new(cfg.output_dir.clone())
        .await
        .map_err(|e| anyhow!("创建 BT 会话失败: {e:#}"))?;

    let resp = session
        .add_torrent(add, None)
        .await
        .map_err(|e| anyhow!("添加 BT 任务失败: {e:#}"))?;
    let handle = resp
        .into_handle()
        .ok_or_else(|| anyhow!("BT 任务未返回句柄（可能已被管理）"))?;

    {
        let mut t = task.lock().unwrap();
        t.status = TaskStatus::Downloading;
        t.file_name = if url.starts_with("magnet:") {
            format!("BT 任务 #{}（磁力链接）", t.id)
        } else {
            // .torrent 文件名（去扩展名）
            url.rsplit(['/', '\\'])
                .next()
                .unwrap_or("torrent")
                .trim_end_matches(".torrent")
                .to_string()
        };
        // BT 任务没有单一输出文件，把保存目录记为 output_path，
        // 使 GUI「打开目录」可以直接打开下载目录。
        t.output_path = cfg.output_dir.clone();
    }

    loop {
        let (cancel, pause, gen_changed) = {
            let t = task.lock().unwrap();
            (t.cancel, t.pause, t.generation != generation)
        };
        // 任务被 resume 重新开始：旧引擎停止会话并静默退出。
        if gen_changed {
            session.stop().await;
            return Ok(());
        }
        if cancel {
            session.stop().await;
            let mut t = task.lock().unwrap();
            t.status = TaskStatus::Cancelled;
            return Err(anyhow!("任务已取消"));
        }
        if pause {
            session.stop().await;
            let mut t = task.lock().unwrap();
            t.status = TaskStatus::Paused;
            return Ok(());
        }

        let stats = handle.stats();
        let total = stats.total_bytes;
        let done = stats.progress_bytes;
        let finished = stats.finished;

        // 出错（解析失败/无可用 peer 等）：停止会话并标记失败，
        // 避免任务永远停留在“下载中”。
        if let Some(err) = &stats.error {
            session.stop().await;
            let mut t = task.lock().unwrap();
            t.status = TaskStatus::Failed;
            t.error = Some(err.clone());
            return Err(anyhow!("BT 任务失败: {err}"));
        }

        {
            let mut t = task.lock().unwrap();
            t.total = total;
            t.downloaded = done;
            t.status = TaskStatus::Downloading;
        }

        if finished {
            session.stop().await;
            let mut t = task.lock().unwrap();
            t.downloaded = total;
            t.status = TaskStatus::Completed;
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 判断字符串是否为本地存在的 `.torrent` 文件路径（不含协议前缀）。
fn is_local_torrent_file(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if !lower.ends_with(".torrent") || url.contains("://") {
        return false;
    }
    std::path::Path::new(url).is_file()
}