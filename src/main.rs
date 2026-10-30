//! turbo-dl 入口：GUI 模式 / CLI 模式。

mod app;
mod args;
mod engine;
mod gui;
mod logging;
mod postprocess;
mod rate;
mod resume;
mod util;

use crate::app::{shutdown_computer, Settings, TaskManager, TaskStatus};
use crate::args::Cli;
use crate::engine::DownloadConfig;
use crate::util::{format_bytes, parse_headers};
use anyhow::{bail, Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    // 初始化日志系统（控制台 + 文件，RUST_LOG/RUST_LOG_FILE 控制级别）。
    crate::logging::init_logging();
    let rt = tokio::runtime::Runtime::new().expect("创建 tokio 运行时失败");
    let handle = rt.handle().clone();
    let cli = Cli::parse();

    // 右键菜单集成：解析 .url/.txt 中的链接并发送到运行中的 turbo-dl；
    // 若本地集成服务未运行，则启动 GUI 并加入任务。
    if let Some(path) = &cli.send {
        match crate::util::parse_links_file(path) {
            Ok(urls) if !urls.is_empty() => match send_to_local_server(&urls) {
                Ok(n) => {
                    println!("✅ 已发送 {n} 个链接到运行中的 turbo-dl");
                    return;
                }
                Err(e) => {
                    tracing::info!("ℹ 未发现运行中的 turbo-dl（{e}），将启动图形界面并加入任务。");
                    let manager = Arc::new(TaskManager::new(settings_from_cli(&cli), handle));
                    // 恢复上次未完成任务（断点续传）。
                    let restored = manager.restore_tasks();
                    if restored > 0 {
                        tracing::info!("↻ 已恢复 {restored} 个未完成任务");
                    }
                    for u in urls {
                        let _ = manager.add_download(u);
                    }
                    if let Err(e) = gui::run_gui(manager) {
                        tracing::error!("错误: {e:#}");
                        std::process::exit(2);
                    }
                    return;
                }
            },
            Ok(_) => {
                tracing::error!("错误: 文件中没有找到可用链接: {}", path.display());
                std::process::exit(2);
            }
            Err(e) => {
                tracing::error!("错误: {e:#}");
                std::process::exit(2);
            }
        }
    }

    // GUI 模式：--gui，或没有任何下载参数（且未使用 release-info/网站镜像/RSS 等 CLI 专用模式）。
    if cli.gui
        || (cli.urls.is_empty()
            && cli.queue.is_none()
            && cli.release_info.is_none()
            && cli.rss.is_none()
            && cli.rss_list.is_none()
            && !cli.mirror)
    {
        let manager = Arc::new(TaskManager::new(settings_from_cli(&cli), handle));
        // 恢复上次未完成任务（断点续传），再处理本次传入的 URL。
        let restored = manager.restore_tasks();
        if restored > 0 && !cli.quiet {
            tracing::info!("↻ 已恢复 {restored} 个未完成任务");
        }
        // CLI 传入 --shutdown 时，GUI 模式同样在全部任务完成后关机。
        if cli.shutdown {
            manager.settings.lock().unwrap().shutdown_after = true;
        }
        // 支持 `turbo-dl --gui URL...`：打开界面并自动加入下载任务。
        for u in &cli.urls {
            let _ = manager.add_download(u.clone());
        }
        // GUI 模式下 --queue 队列文件同样生效。
        if let Some(queue) = &cli.queue {
            if let Ok(content) = std::fs::read_to_string(queue) {
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() && !line.starts_with('#') {
                        let _ = manager.add_download(line.to_string());
                    }
                }
            }
        }
        if let Err(e) = gui::run_gui(manager) {
            tracing::error!("错误: {e:#}");
            std::process::exit(2);
        }
        return;
    }

    match rt.block_on(run(cli, handle)) {
        Ok(fail_count) => std::process::exit(if fail_count > 0 { 1 } else { 0 }),
        Err(e) => {
            tracing::error!("错误: {e:#}");
            std::process::exit(2);
        }
    }
}

/// 从 CLI 参数构建初始设置（GUI 模式与 --send 模式共用）。
fn settings_from_cli(cli: &Cli) -> Settings {
    let mut settings = Settings::default();
    settings.download_dir = cli
        .dir
        .clone()
        .unwrap_or_else(|| settings.download_dir.clone());
    settings.connections = cli.connections.max(1);
    settings.jobs = cli.jobs.max(1);
    settings.retries = cli.retries;
    settings.user_agent = cli.user_agent.clone();
    if matches!(cli.rate_limit_value(), Ok(Some(_))) {
        if let Some(text) = &cli.rate_limit {
            settings.rate_limit_text = text.clone();
        }
    }
    if matches!(cli.global_rate_limit_value(), Ok(Some(_))) {
        if let Some(text) = &cli.global_rate_limit {
            settings.global_rate_limit_text = text.clone();
        }
    }
    if let Some(proxy) = &cli.proxy {
        settings.proxy_text = proxy.clone();
    }
    if let Some(auth) = &cli.auth {
        settings.auth_text = auth.clone();
    }
    if let Some(start_at) = &cli.start_at {
        settings.start_at_text = start_at.clone();
    }
    if let Ok(seg) = cli.min_segment_value() {
        settings.min_segment_size = seg.max(1);
    }
    settings.open_after_complete = cli.open_after;
    settings.auto_extract = cli.extract;
    settings.delete_archive_after_extract = cli.delete_archive;
    settings.auto_classify = cli.classify;
    if let Some(dir) = &cli.classify_dir {
        settings.classify_root = dir.clone();
    }
    if let Some(r) = &cli.referer {
        settings.referer_text = r.clone();
    }
    if let Some(c) = &cli.cookie {
        settings.cookie_text = c.clone();
    }
    settings.checksum_sidecar = cli.checksum_sidecar;
    settings
}

/// 把链接批量发送到已运行的 turbo-dl 本地集成服务（31877 起自动探测端口）。
/// 返回成功发送的链接数。
fn send_to_local_server(urls: &[String]) -> Result<usize> {
    use std::io::{Read, Write};

    // 本地集成服务需要持久化 token 认证（防跨站触发）。
    let token = crate::util::load_or_create_token();

    for port in 31_877u16..31_887 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            continue;
        }
        let mut sent = 0;
        for u in urls {
            let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
                continue;
            };
            // 用 form-urlencoded 编码，保证链接中的特殊字符在 query 中不被破坏。
            let encoded: String = url::form_urlencoded::byte_serialize(u.as_bytes()).collect();
            let req = format!(
                "GET /add?token={}&url={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                token, encoded, port
            );
            if stream.write_all(req.as_bytes()).is_err() {
                continue;
            }
            // 读取响应，确认服务端已处理（内容不重要）。
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            sent += 1;
        }
        return Ok(sent);
    }
    bail!("本地集成服务未启动")
}

async fn run(cli: Cli, handle: tokio::runtime::Handle) -> Result<usize> {
    // --release-info：列出 GitHub release 资产直链（不下载）。
    if let Some(ru) = &cli.release_info {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        let url = url::Url::parse(ru).with_context(|| format!("无效 URL: {ru}"))?;
        let assets = crate::engine::release::resolve_release(&url, &client).await?;
        println!("📦 {} 个资产:", assets.len());
        for a in &assets {
            println!("{}  {}", a.name, a.url);
        }
        return Ok(0);
    }

    // --rss-list：列出 RSS/Atom 订阅源条目（不下载）。
    if let Some(feed_url) = &cli.rss_list {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let url = url::Url::parse(feed_url).with_context(|| format!("无效 RSS URL: {feed_url}"))?;
        let items = crate::engine::rss::parse_feed(&url, &client, &cli.user_agent).await?;
        println!("📰 {} 个条目:", items.len());
        for it in &items {
            println!("{}  {}", it.title, it.url);
        }
        return Ok(0);
    }

    // 收集 URL。
    let mut urls = cli.urls.clone();
    if let Some(queue) = &cli.queue {
        let content = std::fs::read_to_string(queue)
            .with_context(|| format!("读取队列文件失败: {}", queue.display()))?;
        for line in content.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                urls.push(line.to_string());
            }
        }
    }

    // --rss：解析订阅源并批量追加条目下载链接。
    // 若指定 --rss-interval，则进入定时轮询模式（自动下载新条目）。
    if let Some(feed_url) = &cli.rss {
        if let Some(interval_min) = cli.rss_interval {
            let feed_url = feed_url.clone();
            if !cli.quiet {
                tracing::info!(
                    "📰 RSS 轮询模式：每 {} 分钟检查一次「{feed_url}」",
                    interval_min
                );
            }
            return run_rss_polling(cli, feed_url, interval_min, handle).await;
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let url = url::Url::parse(feed_url).with_context(|| format!("无效 RSS URL: {feed_url}"))?;
        let items = crate::engine::rss::parse_feed(&url, &client, &cli.user_agent).await?;
        if items.is_empty() {
            bail!("RSS feed 中没有找到可下载条目: {feed_url}");
        }
        if !cli.quiet {
            tracing::info!("📰 RSS「{feed_url}」共 {} 个条目", items.len());
        }
        for it in &items {
            urls.push(it.url.clone());
        }
    }
    if urls.is_empty() {
        bail!("没有可下载的 URL。请传入 URL、--queue 队列文件，或直接运行本程序启动图形界面。");
    }
    if cli.output.is_some() && urls.len() > 1 {
        bail!("-o/--output 只能用于单个下载任务。");
    }

    let manager = Arc::new(TaskManager::new(settings_from_cli(&cli), handle));

    let cfg = DownloadConfig {
        output_dir: cli.dir.clone().unwrap_or_else(|| PathBuf::from(".")),
        output_name: cli.output.clone(),
        connections: cli.connections.max(1),
        min_segment_size: cli.min_segment_value()?,
        headers: parse_headers(&cli.headers)?,
        user_agent: cli.user_agent.clone(),
        proxy: cli.proxy.clone(),
        auth: cli.auth_parts()?,
        referer: cli.referer.clone(),
        cookie: cli.cookie.clone(),
        checksum_sidecar: cli.checksum_sidecar,
        retries: cli.retries,
        timeout: Duration::from_secs(cli.timeout),
        rate_limit: cli.rate_limit_value()?,
        global_limiter: None, // CLI 全局限速由 TaskManager 注入
        checksum: cli.checksum_parts()?,
        force_restart: cli.force_restart,
        resume_enabled: !cli.no_resume,
        quiet: cli.quiet,
        start_at: cli.start_at_value()?,
    };

    // 网站镜像模式：不进入任务引擎，直接递归下载同域页面与资源。
    if cli.mirror {
        let Some(first) = urls.first() else {
            bail!("--mirror 需要提供一个 URL");
        };
        let n = crate::engine::mirror::mirror_site(first, &cfg.output_dir, &cfg).await?;
        println!(
            "✓ 网站镜像完成：{n} 个文件 -> {}",
            cfg.output_dir.display()
        );
        return Ok(0);
    }

    let mut ids = Vec::new();
    for u in &urls {
        if let Ok(id) = manager.add_download_cfg(u.clone(), cfg.clone()) {
            if !cli.quiet {
                tracing::info!("添加任务 #{id}: {u}");
            }
            ids.push(id);
        }
    }
    if ids.is_empty() {
        bail!("所有任务添加失败，请检查 URL。");
    }

    // CLI 进度条：轮询任务状态，等待全部结束。
    wait_cli_tasks(&manager).await;

    // 全部任务结束后按 --shutdown 关机。
    if cli.shutdown {
        shutdown_computer();
    }

    // 汇总结果。
    let mut fail = 0usize;
    let tasks = manager.tasks().lock().unwrap().clone();
    for task in tasks {
        let t = task.lock().unwrap();
        match t.status {
            TaskStatus::Completed => {
                println!(
                    "✓ {} -> {}（{}）",
                    t.url,
                    t.output_path.display(),
                    format_bytes(t.total)
                );
            }
            TaskStatus::Failed => {
                fail += 1;
                println!(
                    "✗ {} 失败: {}",
                    t.url,
                    t.error.clone().unwrap_or_else(|| "未知错误".to_string())
                );
            }
            TaskStatus::Cancelled => {
                fail += 1;
                println!("✗ {} 已取消", t.url);
            }
            _ => {}
        }
    }
    Ok(fail)
}

/// 等待 CLI 模式的所有任务进入终态：周期写历史、显示进度条，
/// 结束后执行下载后处理（解压/分类）。
async fn wait_cli_tasks(manager: &Arc<TaskManager>) {
    let mp = MultiProgress::new();
    let mut bars: HashMap<u64, ProgressBar> = HashMap::new();
    let style = ProgressStyle::with_template(
        "{spinner:.green} [{bar:30.cyan/blue}] {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} {msg}",
    )
    .unwrap()
    .progress_chars("=>-");

    loop {
        // 周期把终态任务写入下载历史（并触发完成后打开）。
        manager.checkpoint_history();
        let tasks = manager.tasks().lock().unwrap().clone();
        let mut all_done = true;
        for task in tasks {
            let t = task.lock().unwrap();
            let finished = matches!(
                t.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            );
            if !finished {
                all_done = false;
            }
            let bar = bars.entry(t.id).or_insert_with(|| {
                let b = mp.add(ProgressBar::new(t.total.max(1)));
                b.set_style(style.clone());
                b.set_message(t.file_name.clone());
                b
            });
            bar.set_length(t.total.max(1));
            bar.set_position(t.downloaded);
            bar.set_message(format!(
                "{} {} {}/s",
                t.status.label(),
                t.file_name,
                format_bytes(t.speed)
            ));
            if finished {
                bar.finish_and_clear();
            }
        }
        if all_done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = mp.clear();

    // 任务全部结束后再记录一次历史（最后一批任务的终态可能晚于轮询退出点）。
    manager.checkpoint_history();

    // CLI 退出前同步执行下载后处理（解压/分类），确保命令返回时已完成。
    for (p, s) in manager.drain_postprocess() {
        crate::postprocess::run(p, s).await;
    }
}

/// RSS 定时轮询：每隔 interval_min 分钟解析 feed，自动下载新条目。
/// 已下载条目记录在 rss_seen.json，避免重复下载。
async fn run_rss_polling(
    cli: Cli,
    feed_url: String,
    interval_min: u64,
    handle: tokio::runtime::Handle,
) -> Result<usize> {
    use crate::engine::rss::{load_seen, parse_feed, save_seen};

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = url::Url::parse(&feed_url).with_context(|| format!("无效 RSS URL: {feed_url}"))?;
    let mut seen = load_seen();
    let interval = Duration::from_secs(interval_min.max(1) * 60);

    loop {
        let items = parse_feed(&url, &client, &cli.user_agent).await?;
        let new_items: Vec<_> = items
            .into_iter()
            .filter(|it| !seen.contains(&it.url))
            .collect();

        if new_items.is_empty() {
            if !cli.quiet {
                tracing::info!(
                    "📰 无新条目，{interval_min} 分钟后重试（已跟踪 {} 条）",
                    seen.len()
                );
            }
        } else {
            if !cli.quiet {
                tracing::info!("📰 发现 {} 个新条目，开始下载", new_items.len());
                for it in &new_items {
                    tracing::info!("   - {}", it.title);
                }
            }
            let manager = Arc::new(TaskManager::new(settings_from_cli(&cli), handle.clone()));
            for it in &new_items {
                if manager.add_download(it.url.clone()).is_ok() {
                    seen.insert(it.url.clone());
                }
            }
            save_seen(&seen);
            wait_cli_tasks(&manager).await;
        }

        tokio::time::sleep(interval).await;
    }
}