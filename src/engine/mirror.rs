//! 网站镜像（整站克隆）：纯 Rust 递归下载同域页面与静态资源。
//!
//! 不使用 wget/httrack 等外部工具；基于 reqwest + scraper 实现 BFS 爬取：
//! - 只下载与入口同主机（host）的 http/https 资源；
//! - HTML 页面解析 `a[href]`、`img/src/script/link/source` 并递归；
//! - 深度上限 3 层、总文件数上限 300，防止失控；
//! - 本地路径按 URL 路径映射（逐段清洗，防路径穿越），目录页保存为 index.html。

use crate::engine::http::make_client;
use crate::engine::DownloadConfig;
use crate::util::sanitize_file_name;
use anyhow::{Context, Result};
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use scraper::{Html, Selector};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use url::Url;

/// 镜像最大层数（入口为第 0 层）。
const MAX_DEPTH: usize = 3;
/// 镜像最大文件数。
const MAX_PAGES: usize = 300;

/// 执行网站镜像，返回下载的文件数。
pub async fn mirror_site(url: &str, out_dir: &Path, cfg: &DownloadConfig) -> Result<usize> {
    let client = make_client(cfg, false)?;
    let base = Url::parse(url).with_context(|| format!("无效 URL: {url}"))?;
    let host = base
        .host_str()
        .unwrap_or("localhost")
        .to_string();
    let root = out_dir.join(&host);
    tokio::fs::create_dir_all(&root)
        .await
        .context("创建镜像目录失败")?;

    let mut queue: VecDeque<(Url, usize)> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    queue.push_back((base, 0));
    let mut downloaded = 0usize;

    while let Some((u, depth)) = queue.pop_front() {
        // 达到文件数上限：立即停止爬取，避免队列继续膨胀。
        if downloaded >= MAX_PAGES {
            queue.clear();
            break;
        }
        let key = u.as_str().to_string();
        if !visited.insert(key) {
            continue;
        }

        let resp = match client
            .get(u.clone())
            .header(USER_AGENT, &cfg.user_agent)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        let ct = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => continue,
        };

        let local = url_to_local_path(&u, &root, &ct);
        if let Some(parent) = local.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::write(&local, &body).await {
            tracing::warn!("⚠ 写入失败 {}: {e}", local.display());
            continue;
        }
        downloaded += 1;
        if !cfg.quiet {
            tracing::debug!("⧉ {}", local.display());
        }

        // HTML 页面：解析链接并递归（限制深度）。
        if ct.contains("html") && depth < MAX_DEPTH {
            let html_text = String::from_utf8_lossy(&body).into_owned();
            for link in extract_links(&html_text) {
                let Ok(link_url) = u.join(&link) else {
                    continue;
                };
                if (link_url.scheme() == "http" || link_url.scheme() == "https")
                    && link_url.host_str() == Some(host.as_str())
                {
                    queue.push_back((link_url, depth + 1));
                }
            }
        }
    }

    Ok(downloaded)
}

/// 从 HTML 中提取所有资源链接（a[href]、img/script/source[src]、link[href]）。
fn extract_links(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut links = Vec::new();
    if let Ok(sel) = Selector::parse("a[href]") {
        for el in doc.select(&sel) {
            if let Some(v) = el.value().attr("href") {
                links.push(v.to_string());
            }
        }
    }
    if let Ok(sel) = Selector::parse("img[src],script[src],source[src]") {
        for el in doc.select(&sel) {
            if let Some(v) = el.value().attr("src") {
                links.push(v.to_string());
            }
        }
    }
    if let Ok(sel) = Selector::parse("link[href]") {
        for el in doc.select(&sel) {
            if let Some(v) = el.value().attr("href") {
                links.push(v.to_string());
            }
        }
    }
    links
}

/// 把 URL 映射为本地文件路径（逐段清洗防路径穿越；目录页保存为 index.html）。
fn url_to_local_path(u: &Url, root: &Path, ct: &str) -> PathBuf {
    let path = u.path().trim_start_matches('/');
    let mut rel: PathBuf = if path.is_empty() {
        PathBuf::from("index.html")
    } else {
        let safe: Vec<String> = path
            .split('/')
            .map(|seg| sanitize_file_name(seg))
            .collect();
        PathBuf::from(safe.join("/"))
    };

    if rel.extension().is_none() {
        let is_html = ct.contains("html");
        if u.path().ends_with('/') {
            rel = rel.join("index.html");
        } else if is_html {
            // 无扩展名的 HTML：保存为 <path>.html
            let mut os = rel.as_os_str().to_os_string();
            os.push(".html");
            rel = PathBuf::from(os);
        }
    }
    root.join(rel)
}