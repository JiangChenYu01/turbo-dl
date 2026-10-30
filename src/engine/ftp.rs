//! FTP/FTPS 下载引擎（支持 REST 断点续传、限速）。

use crate::app::{TaskState, TaskStatus};
use crate::engine::DownloadConfig;
use crate::resume::ResumeState;
use crate::util::sanitize_file_name;
use anyhow::{anyhow, bail, Context, Result};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use suppaftp::FtpStream;
use url::Url;

/// 执行一次 FTP 下载。
/// `generation` 为启动时任务代次；若运行中被 resume 递增，旧引擎需主动退出。
pub async fn download_ftp(
    task: Arc<Mutex<TaskState>>,
    cfg: DownloadConfig,
    generation: u64,
) -> Result<()> {
    let url = Url::parse(&task.lock().unwrap().url.clone())
        .with_context(|| "无效 FTP URL")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("FTP URL 缺少主机"))?
        .to_string();
    let port = url.port().unwrap_or(21);
    let (user, password) = if !url.username().is_empty() {
        (
            percent_decode(url.username()),
            url.password().map(percent_decode).unwrap_or_default(),
        )
    } else if let Some((u, p)) = &cfg.auth {
        // CLI/GUI 设置的认证（URL 未内嵌凭据时使用）。
        (u.clone(), p.clone())
    } else {
        ("anonymous".to_string(), "anonymous@".to_string())
    };
    let path = url.path().to_string();
    let file_name = cfg
        .output_name
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut s| s.next_back())
                .filter(|s| !s.is_empty())
                .map(percent_decode)
        })
        .unwrap_or_else(|| "download".to_string());
    // Windows 兼容清洗：替换非法字符、规避保留设备名与路径穿越。
    let file_name = sanitize_file_name(&file_name);
    let output = cfg.output_dir.join(&file_name);

    // 跨进程互斥：防止两个 turbo-dl 进程同时下载同一文件。
    let _lock = crate::util::FileLockGuard::acquire(&output)?;

    if cfg.force_restart {
        let _ = std::fs::remove_file(&output);
        ResumeState::remove_meta(&output);
    }

    // 续传偏移（FTP 用 REST 从已下载字节继续）。
    let resume = cfg
        .resume_enabled
        .then(|| ResumeState::load(&output))
        .flatten();
    let start_offset = resume.as_ref().map(|r| r.downloaded_bytes).unwrap_or(0);

    {
        let mut t = task.lock().unwrap();
        t.file_name = file_name.clone();
        t.output_path = output.clone();
        t.status = TaskStatus::Downloading;
    }

    let task2 = task.clone();
    let cfg2 = cfg.clone();
    let host2 = host.clone();
    let user2 = user.clone();
    let pass2 = password.clone();
    let path2 = path.clone();
    let output2 = output.clone();

    let ftp_result = tokio::task::spawn_blocking(move || -> Result<u64> {
        ftp_blocking_download(
            &task2,
            &cfg2,
            &host2,
            port,
            &user2,
            &pass2,
            &path2,
            &output2,
            start_offset,
            generation,
        )
    })
    .await
    .context("FTP 工作线程异常")?;

    // 任务被 resume 重新开始：旧引擎静默退出，不覆盖新引擎状态。
    if task.lock().unwrap().generation != generation {
        return Ok(());
    }

    match ftp_result {
        Ok(total) => {
            ResumeState::remove_meta(&output);
            if let Some((algo, expected)) = &cfg.checksum {
                let ok = verify_checksum(&output, algo, expected).await?;
                if !ok {
                    let mut t = task.lock().unwrap();
                    t.status = TaskStatus::Failed;
                    t.error = Some(format!("校验和不一致（{algo}）"));
                    return Err(anyhow!("校验和不一致（{algo}）"));
                }
            }
            let mut t = task.lock().unwrap();
            t.downloaded = total;
            t.total = total;
            t.status = TaskStatus::Completed;
            Ok(())
        }
        Err(e) => {
            let mut t = task.lock().unwrap();
            if t.cancel {
                t.status = TaskStatus::Cancelled;
                Err(anyhow!("任务已取消"))
            } else if t.pause {
                // FTP 引擎在暂停时以 Err 形式退出（阻塞线程无法优雅返回 Ok），
                // 这里识别暂停标志，把状态修正为 Paused，避免任务卡在“下载中”。
                t.status = TaskStatus::Paused;
                Ok(())
            } else {
                t.status = TaskStatus::Failed;
                t.error = Some(format!("{e:#}"));
                Err(e)
            }
        }
    }
}

/// FTP 数据流空闲超时：服务器断连/停止发送时，阻塞 read 不再永久挂起。
const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[allow(clippy::too_many_arguments)]
fn ftp_blocking_download(
    task: &Arc<Mutex<TaskState>>,
    cfg: &DownloadConfig,
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    path: &str,
    output: &PathBuf,
    start_offset: u64,
    generation: u64,
) -> Result<u64> {
    let mut ftp = FtpStream::connect((host, port))
        .with_context(|| format!("连接 FTP {host}:{port} 失败"))?;
    // FTP 控制/数据连接默认没有读超时，服务器半开连接会让 read 永久阻塞，
    // 导致任务无法暂停/取消/失败。这里设置 30s 空闲超时。
    if let Err(e) = ftp.get_ref().set_read_timeout(Some(READ_TIMEOUT)) {
        bail!("设置 FTP 读超时失败: {e}");
    }
    ftp.login(user, pass).context("FTP 登录失败")?;
    ftp.set_mode(suppaftp::types::Mode::Passive);
    ftp.transfer_type(suppaftp::types::FileType::Binary)
        .context("设置二进制传输模式失败")?;
    let total: u64 = ftp
        .size(path)
        .context("获取 FTP 文件大小失败")?
        .try_into()
        .context("FTP 文件大小超出范围")?;

    // 文件已存在且完整，无断点需要 → 直接完成。
    if start_offset == 0 {
        let existing = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
        if existing == total {
            if !cfg.quiet {
                tracing::info!("✓ 文件已存在且完整: {}", output.display());
            }
            let _ = ftp.quit();
            return Ok(total);
        }
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(output)
        .with_context(|| format!("打开输出文件失败: {}", output.display()))?;
    file.seek(SeekFrom::Start(start_offset))?;

    if start_offset > 0 {
        ftp.resume_transfer(
            start_offset
                .try_into()
                .context("FTP 续传偏移超出范围")?,
        )
        .context("设置 REST 续传失败")?;
    }
    if !cfg.quiet {
        tracing::info!(
            "↻ FTP 下载 {} -> {}（{} 字节，从偏移 {} 开始）",
            path,
            output.display(),
            total,
            start_offset
        );
    }

    let mut reader = ftp.retr_as_stream(path).context("开始 FTP RETR 传输失败")?;
    // FTP 数据连接是独立 socket（PASV），控制连接的读超时不影响它；
    // 这里对数据流本身设置读超时，服务器停止发送时才能正常退出。
    if let Err(e) = reader.get_ref().set_read_timeout(Some(READ_TIMEOUT)) {
        bail!("设置 FTP 数据流读超时失败: {e}");
    }
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded = start_offset;
    let mut limiter = cfg.rate_limit.map(BlockingRateLimiter::new);
    let mut last_save = Instant::now();

    loop {
        let (cancel, pause, gen_changed) = {
            let t = task.lock().unwrap();
            (t.cancel, t.pause, t.generation != generation)
        };
        if gen_changed {
            let _ = ftp.quit();
            bail!("任务已重新开始，旧引擎退出");
        }
        if cancel || pause {
            let _ = ftp.quit();
            if cancel {
                bail!("任务已取消");
            }
            return Err(anyhow!("任务已暂停"));
        }

        let n = reader
            .read(&mut buf)
            .context("读取 FTP 数据流失败")?;
        if n == 0 {
            break;
        }
        // 全局限速优先；未设置时回退到每连接限速（阻塞线程内直接 sleep）。
        if let Some(g) = &cfg.global_limiter {
            let dur = g.register(n as u64);
            if !dur.is_zero() {
                std::thread::sleep(dur);
            }
        } else if let Some(l) = limiter.as_mut() {
            l.throttle(n as u64);
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        downloaded += n as u64;

        {
            let mut t = task.lock().unwrap();
            t.downloaded = downloaded;
            t.total = total;
        }
        if last_save.elapsed() >= Duration::from_secs(2) {
            last_save = Instant::now();
            let _ = save_ftp_resume(task, output, total, downloaded);
        }
    }
    file.flush().context("刷新文件缓冲失败")?;
    ftp.finalize_retr_stream(reader)
        .context("结束 FTP 传输失败")?;
    let _ = ftp.quit();

    let (cancel, pause) = {
        let t = task.lock().unwrap();
        (t.cancel, t.pause)
    };
    if cancel || pause {
        bail!("传输已中止");
    }
    if downloaded < total {
        bail!("下载不完整：期望 {total} 字节，实际 {downloaded} 字节");
    }
    Ok(total)
}

fn save_ftp_resume(
    task: &Arc<Mutex<TaskState>>,
    output: &Path,
    total: u64,
    downloaded: u64,
) -> anyhow::Result<()> {
    let t = task.lock().unwrap();
    let state = ResumeState {
        url: t.url.clone(),
        file_name: t.file_name.clone(),
        total_size: total,
        etag: None,
        chunk_size: total.max(1),
        num_chunks: 1,
        done_bitmap_hex: hex::encode(vec![if downloaded >= total { 1 } else { 0 }]),
        next_chunk: if downloaded >= total { 1 } else { 0 },
        downloaded_bytes: downloaded,
        updated_at: now_unix(),
    };
    state.save(output)
}

/// blocking 版限速器（FTP 运行在 spawn_blocking 线程中）。
struct BlockingRateLimiter {
    limit: u64,
    start: Instant,
    sent: u64,
}

impl BlockingRateLimiter {
    fn new(limit: u64) -> Self {
        Self {
            limit: limit.max(1),
            start: Instant::now(),
            sent: 0,
        }
    }

    fn throttle(&mut self, bytes: u64) {
        self.sent += bytes;
        let expected = self.sent as f64 / self.limit as f64;
        let elapsed = self.start.elapsed().as_secs_f64();
        if expected > elapsed {
            let gap = expected - elapsed;
            if gap > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(gap.min(5.0)));
            }
        }
    }
}

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
                use md5::Md5;
                use sha2::Digest;
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
                use sha1::Sha1;
                use sha2::Digest;
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
                use sha2::{Digest, Sha256};
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