//! HTTP/HTTPS 下载引擎。
//!
//! 创新带宽抢占机制 —— work-stealing 动态块队列：
//! 文件按 min-segment-size 切成小块，所有连接共享一个原子全局队列。每个连接
//! 下载完一块后立即领取下一块，快连接自动获得更多块，慢连接自然少做，彻底消除
//! 传统「等最慢段」的带宽浪费。配合断点位图，任何时刻中断都能精确续传。

use crate::app::{TaskState, TaskStatus};
use crate::engine::DownloadConfig;
use crate::rate::RateLimiter;
use crate::resume::ResumeState;
use crate::util::sanitize_file_name;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use md5::Md5;
use reqwest::header::{
    HeaderValue, ACCEPT_RANGES, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_LENGTH,
    CONTENT_TYPE, COOKIE, ETAG, RANGE, REFERER, USER_AGENT,
};
use reqwest::{Client, StatusCode};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

/// 下载过程中共享的可变状态。
struct DlShared {
    task: Arc<Mutex<TaskState>>,
    client: Client,
    output: PathBuf,
    url: String,
    etag: Option<String>,
    total: u64,
    /// 基块大小（.rdmeta 位图记账单位，保持不变）。
    chunk_size: u64,
    num_chunks: u64,
    /// 每次领取的连续基块数（动态自适应：快则恢复、慢则减半）。
    batch_chunks: AtomicU64,
    /// 初始目标批量：动态调整的恢复上限，防止 batch 单向塌缩。
    target_batch: u64,
    /// 尾部块是否已被首个 worker 领取（关键尾部块优先）。
    tail_taken: AtomicBool,
    /// 服务器是否支持 Range；不支持时单连接整包下载（不发送 Range 头）。
    range_supported: bool,
    /// 全局块队列指针（work-stealing 核心）。
    next_chunk: AtomicU64,
    /// 完成位图（bit i 表示块 i 完成）。
    bitmap: AsyncMutex<Vec<u8>>,
    downloaded_total: AtomicU64,
    /// 断点定时保存状态。
    last_save: AsyncMutex<Instant>,
    /// 启动时任务代次：resume 后旧引擎据此识别自己已过期。
    generation: u64,
    cfg: DownloadConfig,
}

/// 每次读取响应的空闲超时：超过视为瞬时错误，交由弱网自愈逻辑重试。
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 动态块大小上下限：单个 Range 请求覆盖 [256K, 8M]。
const MIN_BATCH_BYTES: u64 = 256 * 1024;
const MAX_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// 慢阈值：单批耗时超过该值则减半批量；小于其一半则向初始目标恢复。
///
/// 阈值选择依据（多连接下载时每连接带宽 ≈ 总带宽/连接数）：
/// - 早期 3s 阈值：正常宽带也触发减半，且恢复条件苛刻导致 batch 单向塌缩；
/// - 6s：8MB 批（约 11s）触发一次减半到 4MB，4MB 恰好能被大多数大文件整除
///       （如 1.6GB/4MB=400=16×25），尾部均衡；弱网下仍可继续减半到最小。
const SLOW_THRESHOLD: Duration = Duration::from_secs(6);

/// 构造带内部标记的确定性错误（4xx 等），由 worker 识别后直接终止任务。
fn fatal_err(msg: String) -> anyhow::Error {
    anyhow!("[fatal] {msg}")
}

/// 判断错误是否为确定性错误（带内部标记）。
fn is_fatal_err(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("[fatal]")
}

/// 去掉内部标记，避免展示给用户时出现噪音。
fn strip_fatal_marker(e: anyhow::Error) -> anyhow::Error {
    anyhow!("{}", format!("{e:#}").replacen("[fatal] ", "", 1))
}

/// 执行一次 HTTP 下载。
/// `generation` 为启动时任务代次；若运行中被 resume 递增，旧引擎需主动退出。
pub async fn download_http(
    task: Arc<Mutex<TaskState>>,
    cfg: DownloadConfig,
    generation: u64,
) -> Result<()> {
    // 探测用客户端：允许 ALPN 协商（兼容 HTTP/1.1 与 HTTP/2）。
    let client = make_client(&cfg, false)?;
    let url = task.lock().unwrap().url.clone();
    let parsed = Url::parse(&url).with_context(|| format!("无效 URL: {url}"))?;

    // 自动识别 Git 仓库：任意主机上「路径像仓库页」的 URL，若 HTTP 智能协议
    // 探测返回 git-upload-pack（GitHub/GitLab/Gitee/自建 Gitea 等均适用），
    // 则转原生 git 克隆，避免把仓库页当成 HTML 文件下载。
    if looks_like_repo_path(&parsed) && detect_git_repo(&client, &parsed, &cfg).await? {
        {
            let mut t = task.lock().unwrap();
            t.url = parsed.as_str().to_string();
            t.protocol = crate::app::Protocol::Git;
        }
        return crate::engine::git::download_git(task, cfg.clone(), generation).await;
    }

    // GitHub release 页面自动解析资产直链并下载。
    if crate::engine::release::is_release_url(&parsed) {
        match crate::engine::release::resolve_release(&parsed, &client).await {
            Ok(assets) if !assets.is_empty() => {
                if !cfg.quiet {
                    tracing::info!("📦 GitHub Release「{}」资产（{} 个）:", parsed, assets.len());
                    for a in &assets {
                        tracing::info!("   {}", a.url);
                    }
                }
                // 下载第一个资产；其余资产的直链已打印，用户可另行使用。
                let first_url = assets[0].url.clone();
                {
                    let mut t = task.lock().unwrap();
                    t.url = first_url.clone();
                }
                // 递归下载资产（只发生一次：资产直链不会再命中 release 分支）。
                return Box::pin(download_http(task, cfg.clone(), generation)).await;
            }
            Ok(_) => {}
            Err(e) => {
                if !cfg.quiet {
                    tracing::warn!("⚠ 解析 GitHub Release 失败: {e:#}，按普通网页下载");
                }
            }
        }
    }

    // M3U8 / HLS 流媒体：识别后转 HLS 下载引擎（分片下载+合并）。
    if crate::engine::m3u8::is_m3u8_url(&parsed) {
        return crate::engine::m3u8::download_m3u8(task, cfg.clone(), generation).await;
    }

    // 1. 探测资源。
    let (total, server_name, range_supported, final_url, etag) =
        probe(&parsed, &cfg, &client).await?;

    // 未知大小（无 Content-Length / chunked）→ 流式单连接下载。
    let Some(total) = total else {
        return download_streaming(
            task,
            cfg.clone(),
            client.clone(),
            final_url,
            server_name,
            generation,
        )
        .await;
    };

    // 传输协议自适应：HTTP/2 在部分 CDN 上会把多连接退化为单连接限速，
    // 这里用一个小 Range 竞速实测，选择实际更快的协议，不做一刀切禁用。
    let client = if range_supported && total > 0 {
        match pick_client(&final_url, &cfg, total).await {
            Ok(c) => c,
            Err(_) => client, // 竞速失败时回退探测用客户端
        }
    } else {
        client
    };

    // 2. 解析输出路径。
    let file_name = resolve_file_name(&parsed, server_name.as_deref(), cfg.output_name.as_deref())?;
    let output = cfg.output_dir.join(&file_name);
    debug_assert_eq!(
        output.parent(),
        Some(cfg.output_dir.as_path()),
        "文件名清洗后不得逃逸输出目录"
    );

    let _lock = crate::util::FileLockGuard::acquire(&output)?;

    {
        let mut t = task.lock().unwrap();
        t.file_name = file_name.clone();
        t.total = total;
        t.output_path = output.clone();
        t.status = TaskStatus::Downloading;
    }

    // 3. 断点状态。
    if cfg.force_restart {
        let _ = std::fs::remove_file(&output);
        ResumeState::remove_meta(&output);
    }
    let chunk_size = cfg.min_segment_size.max(1);
    let resume = cfg
        .resume_enabled
        .then(|| ResumeState::load(&output))
        .flatten();
    let rstate = match resume {
        Some(st)
            if st.matches(final_url.as_str(), etag.as_deref())
                && resume_state_valid(&st, total, chunk_size, &output) =>
        {
            st
        }
        _ => {
            // 断点无效（数据文件被删/下载参数变化/位图损坏）：清理旧断点，
            // 避免把磁盘上并不存在的数据当作“已完成”，导致静默数据损坏。
            ResumeState::remove_meta(&output);
            if output.exists() && !cfg.force_restart {
                let existing = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
                if existing == total {
                    let mut t = task.lock().unwrap();
                    t.downloaded = total;
                    t.status = TaskStatus::Completed;
                    t.output_path = output;
                    return Ok(());
                }
                if cfg.resume_enabled && !cfg.force_restart {
                    bail!(
                        "文件已存在但无有效断点（{} ≠ {}）。如需覆盖请开启强制重启",
                        existing,
                        total
                    );
                }
            }
            ResumeState::new(
                final_url.as_str(),
                &file_name,
                total,
                etag.clone(),
                chunk_size,
            )
        }
    };

    // 4. 确保输出目录存在，再预分配文件（避免写入时反复扩展，提速 + 减少磁盘碎片）。
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建输出目录失败: {}", parent.display()))?;
    }
    {
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&output)
            .await
            .with_context(|| format!("创建输出文件失败: {}", output.display()))?;
        f.set_len(total).await.context("预分配文件失败")?;
    }

    // 5. 构建共享状态。不支持 Range 时退化为整块单连接。
    let (chunk_size, num_chunks) = if range_supported && total > 0 {
        let cs = cfg.min_segment_size.max(1);
        (cs, total.div_ceil(cs).max(1))
    } else {
        (total.max(1), 1u64)
    };

    // 动态块大小：初始批量按「每连接约 8 块」估算，clamp 到 [256K, 8M]。
    let min_batch = (MIN_BATCH_BYTES).div_ceil(chunk_size).max(1);
    let max_batch = (MAX_BATCH_BYTES).div_ceil(chunk_size).max(min_batch);
    let est_batch = if total > 0 {
        (total / (cfg.connections.max(1) as u64 * 8))
            .div_ceil(chunk_size)
            .max(1)
    } else {
        1
    };
    let initial_batch = est_batch.clamp(min_batch, max_batch);

    let shared = Arc::new(DlShared {
        task: task.clone(),
        client: client.clone(),
        output: output.clone(),
        url: final_url.as_str().to_string(),
        etag,
        total,
        chunk_size,
        num_chunks,
        batch_chunks: AtomicU64::new(initial_batch),
        target_batch: initial_batch,
        tail_taken: AtomicBool::new(false),
        range_supported,
        // 位图是已完成块的唯一权威。续传时队列指针统一从 0 开始：
        // worker 会通过 chunk_done 跳过已完成块，立即补齐位图中的空洞；
        // 若沿用上次保存的 next_chunk，指针之前的未完成块要等整个队列耗尽
        // 才会被 has_incomplete_chunk 发现并重置，导致续传初期带宽空转。
        next_chunk: AtomicU64::new(0),
        bitmap: AsyncMutex::new(hex::decode(&rstate.done_bitmap_hex).unwrap_or_default()),
        downloaded_total: AtomicU64::new(rstate.downloaded_bytes),
        last_save: AsyncMutex::new(Instant::now()),
        generation,
        cfg: cfg.clone(),
    });

    // 无条件保存初始断点（quiet 模式下也保存，避免启动即崩溃丢失断点）。
    save_resume(&shared).await;

    // 6. 启动连接池（每个连接一个独立任务）。
    let n_workers = if range_supported { cfg.connections.max(1) } else { 1 };
    let mut handles = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let shared = shared.clone();
        handles.push(tokio::spawn(async move { worker(shared).await }));
    }

    let mut first_err: Option<anyhow::Error> = None;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(anyhow!("下载任务异常: {e}"));
                }
            }
        }
    }

    // 7. 依据结束原因处理状态。
    let (cancel, pause, gen_changed) = {
        let t = task.lock().unwrap();
        (t.cancel, t.pause, t.generation != generation)
    };
    // 任务已被 resume 重新开始：旧引擎静默退出，不覆盖新引擎的状态。
    if gen_changed {
        return Ok(());
    }
    if cancel {
        save_resume(&shared).await;
        let mut t = task.lock().unwrap();
        t.status = TaskStatus::Cancelled;
        return Err(anyhow!("任务已取消"));
    }
    if pause {
        save_resume(&shared).await;
        let mut t = task.lock().unwrap();
        t.status = TaskStatus::Paused;
        return Ok(());
    }
    if let Some(e) = first_err {
        save_resume(&shared).await;
        let mut t = task.lock().unwrap();
        t.status = TaskStatus::Failed;
        t.error = Some(format!("{e:#}"));
        return Err(e);
    }

    // 8. 全部完成：清理断点，校验和。
    ResumeState::remove_meta(&output);
    let downloaded = shared.downloaded_total.load(Ordering::Relaxed);
    {
        let mut t = task.lock().unwrap();
        t.downloaded = downloaded;
    }
    // 校验和：显式 --checksum 优先；否则若开启旁车校验，尝试 <url>.sha256/.sha1/.md5。
    let checksum = if cfg.checksum.is_none() && cfg.checksum_sidecar {
        fetch_sidecar_checksum(&client, &final_url, &cfg).await?
    } else {
        cfg.checksum.clone()
    };
    if let Some((algo, expected)) = &checksum {
        if !cfg.quiet {
            tracing::info!("🔒 校验 {algo} ...");
        }
        let ok = verify_checksum(&output, algo, expected).await?;
        if !ok {
            let mut t = task.lock().unwrap();
            t.status = TaskStatus::Failed;
            t.error = Some(format!("校验和不一致（{algo}）"));
            return Err(anyhow!("校验和不一致（{algo}）"));
        }
    }
    {
        let mut t = task.lock().unwrap();
        t.downloaded = total;
        t.status = TaskStatus::Completed;
    }
    Ok(())
}

/// 断点状态是否可信：资源大小、块大小、块数与当前探测一致，位图长度正确，
/// 且数据文件必须实际存在（否则位图指向的数据已丢失，不能续传）。
fn resume_state_valid(st: &ResumeState, total: u64, chunk_size: u64, output: &Path) -> bool {
    let num_chunks = total.div_ceil(chunk_size).max(1);
    if st.total_size != total || st.chunk_size != chunk_size || st.num_chunks != num_chunks {
        return false;
    }
    let bm_len = num_chunks.div_ceil(8) as usize;
    match hex::decode(&st.done_bitmap_hex) {
        Ok(b) if b.len() == bm_len => {}
        _ => return false,
    }
    output.exists()
}

/// 未知大小（无 Content-Length / chunked）资源的流式下载路径。
///
/// 这类服务器通常不支持 Range，无法使用 work-stealing 分段；这里退化为
/// 单连接顺序消费响应体并追加写盘，EOF 后以实际字节数作为文件大小。
/// 断点续传对未知大小资源不适用（无总长度无法切块），中断后重新下载。
async fn download_streaming(
    task: Arc<Mutex<TaskState>>,
    cfg: DownloadConfig,
    client: Client,
    final_url: Url,
    server_name: Option<String>,
    generation: u64,
) -> Result<()> {
    let file_name =
        resolve_file_name(&final_url, server_name.as_deref(), cfg.output_name.as_deref())?;
    let output = cfg.output_dir.join(&file_name);
    debug_assert_eq!(
        output.parent(),
        Some(cfg.output_dir.as_path()),
        "文件名清洗后不得逃逸输出目录"
    );

    // 跨进程互斥：防止两个 turbo-dl 进程同时下载同一文件。
    let _lock = crate::util::FileLockGuard::acquire(&output)?;

    {
        let mut t = task.lock().unwrap();
        t.file_name = file_name.clone();
        t.total = 0;
        t.output_path = output.clone();
        t.status = TaskStatus::Downloading;
    }

    if cfg.force_restart {
        let _ = std::fs::remove_file(&output);
        ResumeState::remove_meta(&output);
    }
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    let mut req = client
        .get(final_url.clone())
        .header(USER_AGENT, &cfg.user_agent);
    if !cfg.headers.is_empty() {
        req = req.headers(cfg.headers.clone());
    }
    let resp = req.send().await.context("下载请求失败")?;
    if !resp.status().is_success() {
        if resp.status().is_client_error() {
            return Err(fatal_err(format!("下载失败: HTTP {}", resp.status())));
        }
        bail!("下载失败: HTTP {}", resp.status());
    }

    // 流式下载不支持续传：始终从零开始覆盖写，避免重复下载时向旧文件追加导致损坏。
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&output)
        .await
        .with_context(|| format!("创建输出文件失败: {}", output.display()))?;
    file.set_len(0)
        .await
        .context("重置输出文件失败")?;

    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut limiter = cfg.rate_limit.map(RateLimiter::new);
    loop {
        let (cancel, pause, gen_changed) = {
            let t = task.lock().unwrap();
            (t.cancel, t.pause, t.generation != generation)
        };
        // 任务被 resume 重新开始：旧引擎静默退出，不覆盖新引擎状态。
        if gen_changed {
            return Ok(());
        }
        if cancel {
            return Err(anyhow!("任务已取消"));
        }
        if pause {
            let mut t = task.lock().unwrap();
            t.status = TaskStatus::Paused;
            return Ok(());
        }
        let next = tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| anyhow!("读取响应流超时（{}s 无数据）", READ_IDLE_TIMEOUT.as_secs()))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.context("响应流中断")?;
        // 全局限速优先（所有连接共享总带宽）；未设置时回退到每连接限速。
        if let Some(g) = &cfg.global_limiter {
            let dur = g.register(chunk.len() as u64);
            if !dur.is_zero() {
                tokio::time::sleep(dur).await;
            }
        } else if let Some(l) = limiter.as_mut() {
            l.throttle(chunk.len() as u64).await;
        }
        file.write_all(&chunk).await.context("写入文件失败")?;
        downloaded += chunk.len() as u64;
        {
            let mut t = task.lock().unwrap();
            t.downloaded = downloaded;
            t.speed = 0;
        }
    }
    file.flush().await.context("刷新缓冲失败")?;

    let (cancel, pause, gen_changed) = {
        let t = task.lock().unwrap();
        (t.cancel, t.pause, t.generation != generation)
    };
    // 任务被 resume 重新开始：旧引擎静默退出。
    if gen_changed {
        return Ok(());
    }
    if cancel {
        return Err(anyhow!("任务已取消"));
    }
    if pause {
        let mut t = task.lock().unwrap();
        t.status = TaskStatus::Paused;
        return Ok(());
    }

    // EOF：以实际字节数为准，正常校验。
    ResumeState::remove_meta(&output);
    // 校验和：显式参数优先；否则旁车文件（<url>.sha256 等）。
    let checksum = if cfg.checksum.is_none() && cfg.checksum_sidecar {
        fetch_sidecar_checksum(&client, &final_url, &cfg).await?
    } else {
        cfg.checksum.clone()
    };
    if let Some((algo, expected)) = &checksum {
        if !cfg.quiet {
            tracing::info!("🔒 校验 {algo} ...");
        }
        let ok = verify_checksum(&output, algo, expected).await?;
        if !ok {
            let mut t = task.lock().unwrap();
            t.status = TaskStatus::Failed;
            t.error = Some(format!("校验和不一致（{algo}）"));
            return Err(anyhow!("校验和不一致（{algo}）"));
        }
    }
    {
        let mut t = task.lock().unwrap();
        t.downloaded = downloaded;
        t.total = downloaded;
        t.status = TaskStatus::Completed;
    }
    if !cfg.quiet {
        tracing::info!("✓ 下载完成: {}（{} 字节）", output.display(), downloaded);
    }
    Ok(())
}

/// 连接工作循环：动态批量领取数据块（含尾部块优先），直到所有块完成。
async fn worker(shared: Arc<DlShared>) -> Result<()> {
    let mut limiter = shared.cfg.rate_limit.map(RateLimiter::new);
    let mut attempts: u32 = 0;
    let max_attempts = shared.cfg.retries.max(1);

    // 关键尾部块优先：首个 worker 先领取最后一块，让 ZIP/MP4 的尾部尽早就绪。
    if !shared.tail_taken.swap(true, Ordering::Relaxed) && shared.num_chunks > 1 {
        let tail = shared.num_chunks - 1;
        if !chunk_done(&shared, tail).await {
            match download_range(&shared, tail, tail + 1, limiter.as_mut()).await {
                Ok(()) => {
                    mark_chunk_done(&shared, tail).await;
                    shared.task.lock().unwrap().tail_ready = true;
                    attempts = 0;
                }
                Err(e) => {
                    // 确定性错误（4xx 等）直接终止任务并保存断点。
                    if is_fatal_err(&e) {
                        return Err(strip_fatal_marker(e));
                    }
                    attempts += 1;
                    if attempts > max_attempts {
                        return Err(anyhow!(
                            "尾部块 #{tail} 连续失败超过 {} 次，任务终止: {}",
                            max_attempts,
                            strip_fatal_marker(e)
                        ));
                    }
                    let backoff = backoff_ms(&shared, attempts);
                    if !shared.cfg.quiet {
                        tracing::warn!(
                            "⚠ 尾部块 #{tail} 下载失败（第 {attempts} 次重试，{backoff}ms 退避）: {e}"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                }
            }
        }
    }

    loop {
        let (cancel, pause, gen_changed) = {
            let t = shared.task.lock().unwrap();
            (t.cancel, t.pause, t.generation != shared.generation)
        };
        // 任务被 resume 重新开始：旧引擎退出，避免双引擎并发写文件。
        if gen_changed {
            return Err(anyhow!("任务已重新开始，旧引擎退出"));
        }
        if cancel {
            return Err(anyhow!("任务已取消"));
        }
        if pause {
            return Ok(()); // 暂停：正常退出，断点已由主流程保存
        }

        // 动态批量：一次领取 batch_chunks 个连续基块（合并为一个 Range 请求）。
        let batch = shared.batch_chunks.load(Ordering::Relaxed).max(1);
        let start = shared.next_chunk.fetch_add(batch, Ordering::Relaxed);
        if start >= shared.num_chunks {
            // 所有块已被领取。若仍有未完成块（如失败未回队、退化路径中断后续传），
            // 重置队列继续补齐，避免文件不完整却被标记完成。
            if has_incomplete_chunk(&shared).await {
                shared.next_chunk.store(0, Ordering::Relaxed);
                continue;
            }
            break;
        }
        let end = (start + batch).min(shared.num_chunks);
        let batch_started = Instant::now();
        let mut pos = start;
        let mut batch_ok = true;

        // 本批内跳过已完成块，把连续未完成块合并成一个 Range 下载。
        while pos < end {
            if chunk_done(&shared, pos).await {
                pos += 1;
                continue;
            }
            let run_start = pos;
            let mut run_end = pos + 1;
            while run_end < end && !chunk_done(&shared, run_end).await {
                run_end += 1;
            }
            match download_range(&shared, run_start, run_end, limiter.as_mut()).await {
                Ok(()) => {
                    for k in run_start..run_end {
                        mark_chunk_done(&shared, k).await;
                    }
                    if run_end == shared.num_chunks {
                        shared.task.lock().unwrap().tail_ready = true;
                    }
                    pos = run_end;
                }
                Err(e) => {
                    // 确定性错误（4xx 等）直接终止任务并保存断点。
                    if is_fatal_err(&e) {
                        return Err(strip_fatal_marker(e));
                    }
                    // 瞬时错误（超时/连接重置/5xx）：本批未完成部分放回队列，退避后重试。
                    attempts += 1;
                    if attempts > max_attempts {
                        return Err(anyhow!(
                            "块 #{}-{} 连续失败超过 {} 次，任务终止: {}",
                            run_start,
                            run_end - 1,
                            max_attempts,
                            strip_fatal_marker(e)
                        ));
                    }
                    let unprocessed = end - run_start;
                    shared.next_chunk.fetch_sub(unprocessed, Ordering::Relaxed);
                    let backoff = backoff_ms(&shared, attempts);
                    if !shared.cfg.quiet {
                        tracing::warn!(
                            "⚠ 块 #{run_start}-{} 下载失败（第 {attempts} 次重试，{backoff}ms 退避）: {e}",
                            run_end - 1
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    batch_ok = false;
                    break;
                }
            }
        }

        // 整批成功：按耗时自适应调整批量大小。
        if batch_ok {
            attempts = 0;
            adjust_batch(&shared, batch_started.elapsed()).await;
        }
    }
    Ok(())
}

/// 计算退避毫秒数：前几次指数退避，之后稳定在 30s 长退避。
fn backoff_ms(shared: &DlShared, attempts: u32) -> u64 {
    let exp_backoff = (300 * 2u64.pow(attempts.min(8))).min(30_000);
    if attempts <= shared.cfg.retries.max(1) {
        exp_backoff
    } else {
        30_000
    }
}

/// 动态调整批量大小：
/// - 单批耗时 > SLOW_THRESHOLD：减半（弱网自适应）；
/// - 当前批量塌缩到目标的一半以下：向目标恢复
///   （恢复只发生在「明显低于目标」时，避免在目标附近反复振荡）；
/// - 其余情况保持。
async fn adjust_batch(shared: &DlShared, elapsed: Duration) {
    let cur = shared.batch_chunks.load(Ordering::Relaxed);
    let min_batch = MIN_BATCH_BYTES.div_ceil(shared.chunk_size).max(1);
    let target = shared.target_batch.clamp(min_batch, MAX_BATCH_BYTES.div_ceil(shared.chunk_size).max(min_batch));
    let next = if elapsed > SLOW_THRESHOLD {
        (cur / 2).max(min_batch)
    } else if cur < target / 2 {
        // 明显小于目标且当前不算慢：向目标恢复。
        cur.saturating_mul(2).min(target).max(min_batch)
    } else {
        cur
    };
    if next != cur {
        shared.batch_chunks.store(next, Ordering::Relaxed);
    }
}

/// 下载 [start_idx, end_idx) 连续基块（单个 Range 请求）。
async fn download_range(
    shared: &DlShared,
    start_idx: u64,
    end_idx: u64,
    mut limiter: Option<&mut RateLimiter>,
) -> Result<()> {
    // 空文件无需下载。
    if shared.total == 0 {
        return Ok(());
    }
    let start = start_idx * shared.chunk_size;
    let end = (end_idx * shared.chunk_size - 1).min(shared.total - 1);
    let expected = end - start + 1;

    let mut req = shared
        .client
        .get(&shared.url)
        .header(USER_AGENT, &shared.cfg.user_agent);
    // 仅支持 Range 的服务器发送 Range 头；否则整包下载（单连接退化路径）。
    if shared.range_supported {
        req = req.header(RANGE, format!("bytes={start}-{end}"));
    }
    if !shared.cfg.headers.is_empty() {
        req = req.headers(shared.cfg.headers.clone());
    }
    let resp = req.send().await.context("请求分段失败")?;
    if shared.range_supported {
        if resp.status() != StatusCode::PARTIAL_CONTENT {
            if resp.status().is_success() {
                // 探测阶段声明支持 Range，实际请求却返回 200 全量：继续按瞬时错误
                // 重试只会无限循环（配合队列重置永不结束），直接终止任务并给出明确原因。
                return Err(fatal_err(
                    "服务器未返回 206（不支持 Range），任务终止。可尝试 --no-resume 或更换服务器".to_string(),
                ));
            }
            // 4xx 为确定性错误（如 403 拒绝 Range），不再无限重试。
            if resp.status().is_client_error() {
                return Err(fatal_err(format!("下载分段失败: HTTP {}", resp.status())));
            }
            bail!("下载分段失败: HTTP {}", resp.status());
        }
    } else if !resp.status().is_success() {
        if resp.status().is_client_error() {
            return Err(fatal_err(format!("下载失败: HTTP {}", resp.status())));
        }
        bail!("下载失败: HTTP {}", resp.status());
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&shared.output)
        .await
        .with_context(|| format!("打开输出文件失败: {}", shared.output.display()))?;
    file.seek(SeekFrom::Start(start)).await.context("定位偏移失败")?;

    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    // 每次读取带空闲超时：服务器半开连接不再让 worker 永久挂起。
    while written < expected {
        let next = tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| anyhow!("读取响应流超时（{}s 无数据）", READ_IDLE_TIMEOUT.as_secs()))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.context("响应流中断")?;
        // 全局限速优先（所有连接共享总带宽）；未设置时回退到每连接限速。
        if let Some(g) = &shared.cfg.global_limiter {
            let dur = g.register(chunk.len() as u64);
            if !dur.is_zero() {
                tokio::time::sleep(dur).await;
            }
        } else if let Some(l) = limiter.as_mut() {
            l.throttle(chunk.len() as u64).await;
        }
        file.write_all(&chunk).await.context("写入文件失败")?;
        written += chunk.len() as u64;
        shared.downloaded_total.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        update_progress(shared).await;
        // 定期保存断点，保证中断后可从精确位置续传。
        maybe_save(shared).await;
    }
    file.flush().await.context("刷新缓冲失败")?;
    if written < expected {
        bail!(
            "块 {start_idx}-{} 数据不完整：期望 {expected} 字节，实际 {written} 字节",
            end_idx - 1
        );
    }
    Ok(())
}

/// 同步任务进度字段。
async fn update_progress(shared: &DlShared) {
    // 并发 worker 可能重复下载同一块（失败块回队后），计数会暂时超过 total，
    // 这里 clamp 到总大小，避免进度条/速度显示超过 100%。
    let dl = shared
        .downloaded_total
        .load(Ordering::Relaxed)
        .min(shared.total);
    let mut t = shared.task.lock().unwrap();
    t.downloaded = dl;
    t.total = shared.total;
    t.speed = 0; // 速度由 UI/CLI 侧按时间差计算
}

/// 保存断点控制文件。
async fn save_resume(shared: &DlShared) {
    let bitmap = shared.bitmap.lock().await.clone();
    let state = ResumeState {
        url: shared.url.clone(),
        file_name: shared.task.lock().unwrap().file_name.clone(),
        total_size: shared.total,
        etag: shared.etag.clone(),
        chunk_size: shared.chunk_size,
        num_chunks: shared.num_chunks,
        done_bitmap_hex: hex::encode(bitmap),
        next_chunk: shared.next_chunk.load(Ordering::Relaxed),
        // 与进度显示一致，避免重复下载导致断点中的字节数虚高。
        downloaded_bytes: shared
            .downloaded_total
            .load(Ordering::Relaxed)
            .min(shared.total),
        updated_at: now_unix(),
    };
    if let Err(e) = state.save(&shared.output) {
        tracing::warn!("⚠ 保存断点状态失败: {e}");
    }
}

/// 每 2 秒保存一次断点状态（避免频繁磁盘 IO，同时保证中断损失不超过 2 秒进度）。
async fn maybe_save(shared: &DlShared) {
    let mut last = shared.last_save.lock().await;
    if last.elapsed() >= Duration::from_secs(2) {
        *last = Instant::now();
        drop(last);
        save_resume(shared).await;
    }
}

async fn chunk_done(shared: &DlShared, idx: u64) -> bool {
    let bm = shared.bitmap.lock().await;
    if (idx / 8) as usize >= bm.len() {
        return true;
    }
    (bm[(idx / 8) as usize] >> (idx % 8)) & 1 == 1
}

async fn mark_chunk_done(shared: &DlShared, idx: u64) {
    let mut bm = shared.bitmap.lock().await;
    if ((idx / 8) as usize) < bm.len() {
        bm[(idx / 8) as usize] |= 1 << (idx % 8);
    }
}

/// 是否存在尚未完成的块（位图中任一有效位为 0）。
async fn has_incomplete_chunk(shared: &DlShared) -> bool {
    let bm = shared.bitmap.lock().await;
    for i in 0..shared.num_chunks {
        if ((i / 8) as usize) >= bm.len() {
            return true;
        }
        if (bm[(i / 8) as usize] >> (i % 8)) & 1 == 0 {
            return true;
        }
    }
    false
}

/// 构建 HTTP 客户端。
/// `http1_only` 为 true 时强制 HTTP/1.1（多连接下载通常更快，CDN 普遍按连接限速）；
/// 为 false 时按 ALPN 协商（支持 HTTP/2 的服务器会自动使用 h2）。
pub(crate) fn make_client(cfg: &DownloadConfig, http1_only: bool) -> Result<Client> {
    let mut builder = Client::builder()
        .connect_timeout(cfg.timeout)
        .user_agent(cfg.user_agent.clone())
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        .http2_initial_stream_window_size(8 * 1024 * 1024)
        .http2_initial_connection_window_size(16 * 1024 * 1024);
    if http1_only {
        builder = builder.http1_only();
    }
    if let Some(p) = &cfg.proxy {
        builder = builder.proxy(reqwest::Proxy::all(p).with_context(|| format!("无效代理: {p}"))?);
    }
    // HTTP Basic 认证：作为默认 Authorization 头附加到所有请求。
    if let Some((user, pass)) = &cfg.auth {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let token = STANDARD.encode(format!("{user}:{pass}"));
        if let Ok(v) = HeaderValue::from_str(&format!("Basic {token}")) {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(AUTHORIZATION, v);
            builder = builder.default_headers(headers);
        }
    }
    // 防盗链：Referer / Cookie 作为默认头附加到所有请求。
    let mut extra_headers = reqwest::header::HeaderMap::new();
    if let Some(r) = &cfg.referer {
        if let Ok(v) = HeaderValue::from_str(r) {
            extra_headers.insert(REFERER, v);
        }
    }
    if let Some(c) = &cfg.cookie {
        if let Ok(v) = HeaderValue::from_str(c) {
            extra_headers.insert(COOKIE, v);
        }
    }
    if !extra_headers.is_empty() {
        builder = builder.default_headers(extra_headers);
    }
    builder
        .build()
        .map_err(|e| anyhow!("构建 HTTP 客户端失败: {e}"))
}

/// 传输协议自适应选择：HTTP/1.1 多连接在大多数 CDN 上带宽更高（按连接限速），
/// 但部分服务器 HTTP/2 单连接更快。这里用一个小 Range 请求实测，自动选择更快者，
/// 不做一刀切禁用。
async fn pick_client(url: &Url, cfg: &DownloadConfig, total: u64) -> Result<Client> {
    let h1 = make_client(cfg, true)?;
    let h2 = make_client(cfg, false)?;
    // 竞速区间：最多 2MiB；文件太小则竞速开销不划算，直接用默认客户端。
    let race_len = (2 * 1024 * 1024).min(total.max(1));
    if race_len < 256 * 1024 {
        return Ok(h2);
    }
    // 两个协议并行竞速，结果更公平且总等待时间只取决于较慢的一方。
    let (r1, r2) = tokio::join!(
        race_download(&h1, url, cfg, race_len),
        race_download(&h2, url, cfg, race_len)
    );
    match (r1, r2) {
        (Ok(t1), Ok(t2)) => Ok(if t1 <= t2 { h1 } else { h2 }),
        (Ok(_), Err(_)) => Ok(h1),
        (Err(_), Ok(_)) => Ok(h2),
        // 竞速失败（网络波动等）：回退默认客户端，正式下载仍有重试机制。
        (Err(_), Err(_)) => Ok(h2),
    }
}

/// 用指定客户端下载 [0, len) 区间并返回耗时（用于协议竞速）。
async fn race_download(
    client: &Client,
    url: &Url,
    cfg: &DownloadConfig,
    len: u64,
) -> Result<Duration> {
    let start = Instant::now();
    let resp = client
        .get(url.clone())
        .header(USER_AGENT, &cfg.user_agent)
        .header(RANGE, format!("bytes=0-{}", len - 1))
        .send()
        .await
        .context("竞速请求失败")?;
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        bail!("竞速请求未返回 206");
    }
    let mut stream = resp.bytes_stream();
    let mut got = 0u64;
    while got < len {
        let next = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .map_err(|_| anyhow!("竞速读取超时"))?;
        match next {
            Some(Ok(chunk)) => got += chunk.len() as u64,
            Some(Err(e)) => return Err(e.into()),
            None => break,
        }
    }
    Ok(start.elapsed())
}

/// URL 路径是否像 Git 仓库页（至少两段路径，最后一段不像带扩展名的普通文件）。
fn looks_like_repo_path(url: &Url) -> bool {
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    if segments.len() < 2 {
        return false;
    }
    if let Some(last) = segments.last() {
        if last.contains('.') && !last.ends_with(".git") {
            return false;
        }
    }
    true
}

/// 用 HTTP 智能协议探测 URL 是否为 Git 仓库：
/// `GET <url>/info/refs?service=git-upload-pack`，响应 Content-Type 含
/// `git-upload-pack` 即判定为 Git 仓库。
async fn detect_git_repo(client: &Client, url: &Url, cfg: &DownloadConfig) -> Result<bool> {
    let base = url.as_str().trim_end_matches('/').to_string();
    let candidates = [
        format!("{base}/info/refs?service=git-upload-pack"),
        format!("{base}.git/info/refs?service=git-upload-pack"),
    ];
    for cand in candidates {
        let resp = client
            .get(&cand)
            .header(USER_AGENT, &cfg.user_agent)
            .send()
            .await;
        if let Ok(resp) = resp {
            let ct = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if ct.contains("git-upload-pack") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// 探测资源：总大小（未知为 None）、文件名、Range 支持、最终 URL、ETag。
async fn probe(
    url: &Url,
    cfg: &DownloadConfig,
    client: &Client,
) -> Result<(Option<u64>, Option<String>, bool, Url, Option<String>)> {
    // 先 HEAD。
    let mut head_req = client
        .head(url.clone())
        .header(USER_AGENT, &cfg.user_agent);
    if !cfg.headers.is_empty() {
        head_req = head_req.headers(cfg.headers.clone());
    }
    if let Ok(resp) = head_req.send().await {
        let status = resp.status();
        let total = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let ranges_ok = resp
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);
        let cd_name = content_disposition_filename(resp.headers().get(CONTENT_DISPOSITION));
        let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let final_url = resp.url().clone();
        if status.is_success() && total.is_some() {
            // 无 Accept-Ranges 头但可能有实际 Range 支持（如部分服务器），继续用 GET 探测确认。
            if ranges_ok {
                return Ok((total, cd_name, true, final_url, etag));
            }
        }
    }

    // HEAD 失败/无长度/无 Accept-Ranges → GET + Range: bytes=0-0 实测。
    let mut get_req = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .header(USER_AGENT, &cfg.user_agent);
    if !cfg.headers.is_empty() {
        get_req = get_req.headers(cfg.headers.clone());
    }
    let resp = get_req.send().await.context("探测请求失败")?;
    let status = resp.status();
    let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
    let cd_name = content_disposition_filename(resp.headers().get(CONTENT_DISPOSITION));
    let final_url = resp.url().clone();

    if status == StatusCode::PARTIAL_CONTENT {
        let total = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split('/').nth(1))
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| {
                resp.headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
            })
            .ok_or_else(|| anyhow!("无法从响应确定文件大小"))?;
        // 206 只返回 1 字节，读取以复用连接。
        let _ = resp.bytes().await;
        Ok((Some(total), cd_name, true, final_url, etag))
    } else if status.is_success() {
        let total = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        // 服务器忽略 Range（返回 200 全量）：只取头部立即断开，不消费 body，
        // 避免大文件探测时产生整包流量（原实现会 O(文件大小) 读入内存）。
        drop(resp);
        Ok((total, cd_name, false, final_url, etag))
    } else {
        bail!("探测请求失败: HTTP {status} {}", resp.url())
    }
}

/// 解析最终文件名（统一经过 Windows 兼容清洗，杜绝路径穿越）。
fn resolve_file_name(url: &Url, server_name: Option<&str>, user_name: Option<&str>) -> Result<String> {
    if let Some(name) = user_name {
        if !name.is_empty() {
            return Ok(sanitize_file_name(name));
        }
    }
    if let Some(name) = server_name {
        let name = name.trim().trim_matches('"');
        if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
            return Ok(sanitize_file_name(name));
        }
    }
    let name = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty() && *s != "/")
        .map(percent_decode)
        .ok_or_else(|| anyhow!("URL 中没有文件名，请使用 -o/--output 指定"))?;
    Ok(sanitize_file_name(&name))
}

/// 解析 Content-Disposition 中的文件名。
fn content_disposition_filename(hv: Option<&HeaderValue>) -> Option<String> {
    let s = hv?.to_str().ok()?;
    if let Some(idx) = s.find("filename*=") {
        let rest = &s[idx + "filename*=".len()..];
        let val = rest.split(';').next().unwrap_or("").trim();
        if let Some(eq) = val.find("''") {
            let encoded = &val[eq + 2..];
            if !encoded.is_empty() {
                return Some(percent_decode(encoded));
            }
        }
    }
    if let Some(idx) = s.find("filename=") {
        let rest = &s[idx + "filename=".len()..];
        let val = rest.split(';').next().unwrap_or("").trim();
        let val = val.trim_matches('"');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
}

/// 简单百分号解码。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 尝试从 `<url>.sha256` / `<url>.sha1` / `<url>.md5` 旁车文件解析期望校验和。
/// 全部 404 或格式无效时返回 None（不失败）。
async fn fetch_sidecar_checksum(
    client: &Client,
    url: &Url,
    cfg: &DownloadConfig,
) -> Result<Option<(String, String)>> {
    let base = url.as_str().trim_end_matches('/').to_string();
    for (ext, algo) in [("sha256", "sha256"), ("sha1", "sha1"), ("md5", "md5")] {
        let cand = format!("{base}.{ext}");
        let Ok(resp) = client
            .get(&cand)
            .header(USER_AGENT, &cfg.user_agent)
            .timeout(Duration::from_secs(15))
            .send()
            .await
        else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Ok(text) = resp.text().await {
            if let Some((a, hex)) = parse_sidecar(&text, algo) {
                return Ok(Some((a, hex)));
            }
        }
    }
    Ok(None)
}

/// 解析旁车校验和文件：首个 token 为 hex（32/40/64 位），文件名可省略。
fn parse_sidecar(text: &str, default_algo: &str) -> Option<(String, String)> {
    let token = text.lines().next()?.trim().split_whitespace().next()?;
    let len = token.len();
    if !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let algo = match len {
        32 => "md5",
        40 => "sha1",
        64 => "sha256",
        _ => default_algo,
    };
    Some((algo.to_string(), token.to_ascii_lowercase()))
}

/// 校验文件哈希。
async fn verify_checksum(path: &Path, algo: &str, expected_hex: &str) -> Result<bool> {
    let path = path.to_path_buf();
    let algo = algo.to_string();
    let expected_hex = expected_hex.to_string();
    let actual = tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::Read;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .with_context(|| format!("打开文件校验失败: {}", path.display()))?;
        let mut buf = vec![0u8; 1024 * 1024];
        match algo.as_str() {
            "md5" => {
                let mut h = Md5::new();
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                Ok(hex::encode(h.finalize()))
            }
            "sha1" => {
                let mut h = Sha1::new();
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                Ok(hex::encode(h.finalize()))
            }
            "sha256" => {
                let mut h = Sha256::new();
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                Ok(hex::encode(h.finalize()))
            }
            other => bail!("不支持的校验算法: {other}"),
        }
    })
    .await
    .context("校验任务异常")??;
    Ok(actual.eq_ignore_ascii_case(&expected_hex))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%zz"), "%zz"); // 非法 hex 原样保留
    }

    #[test]
    fn parse_sidecar_detects_algo() {
        let md5 = "d41d8cd98f00b204e9800998ecf8427e";
        let (algo, hex) = parse_sidecar(&format!("{md5}  file.bin"), "sha256").unwrap();
        assert_eq!(algo, "md5");
        assert_eq!(hex, md5);

        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let (algo, _) = parse_sidecar(sha256, "sha256").unwrap();
        assert_eq!(algo, "sha256");

        assert!(parse_sidecar("not-hex", "sha256").is_none());
        assert!(parse_sidecar("", "sha256").is_none());
    }

    #[test]
    fn content_disposition_parsing() {
        let hv = HeaderValue::from_static("attachment; filename=\"report.pdf\"");
        assert_eq!(content_disposition_filename(Some(&hv)).unwrap(), "report.pdf");

        let hv2 = HeaderValue::from_static("attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.pdf");
        assert_eq!(content_disposition_filename(Some(&hv2)).unwrap(), "中文.pdf");

        assert!(content_disposition_filename(None).is_none());
        assert!(content_disposition_filename(Some(&HeaderValue::from_static("inline"))).is_none());
    }

    #[test]
    fn resolve_file_name_prefers_server_name() {
        let url = Url::parse("https://example.com/path/actual.bin?x=1").unwrap();
        let name = resolve_file_name(&url, Some("server_name.zip"), None).unwrap();
        assert_eq!(name, "server_name.zip");
        // 无 Content-Disposition 时回退 URL 最后路径段
        let name2 = resolve_file_name(&url, None, None).unwrap();
        assert_eq!(name2, "actual.bin");
        // 用户显式指定优先
        let name3 = resolve_file_name(&url, Some("server.zip"), Some("user.txt")).unwrap();
        assert_eq!(name3, "user.txt");
    }

    #[test]
    fn fatal_marker_roundtrip() {
        let e = fatal_err("403 Forbidden".to_string());
        assert!(is_fatal_err(&e));
        let stripped = strip_fatal_marker(e);
        assert!(!format!("{stripped:#}").contains("[fatal]"));
    }
}