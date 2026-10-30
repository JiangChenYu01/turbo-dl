//! RSS / Atom 订阅源解析：提取每个条目的下载链接。
//!
//! - RSS 2.0：优先取 `<enclosure url>`，否则取 `<link>`；
//! - Atom：优先取 `rel="enclosure"` 的 `<link href>`，否则取第一个 `<link href>`。
//!
//! 同时提供已下载条目的持久化去重（`rss_seen.json`），供定时轮询模式使用。

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

/// 已下载条目 URL 集合的持久化文件（`%APPDATA%\turbo-dl\rss_seen.json`）。
pub fn seen_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("turbo-dl")
        .join("rss_seen.json")
}

/// 加载已下载条目集合。
pub fn load_seen() -> HashSet<String> {
    std::fs::read_to_string(seen_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 保存已下载条目集合。
pub fn save_seen(seen: &HashSet<String>) {
    let path = seen_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(data) = serde_json::to_string(seen) {
        let _ = std::fs::write(&path, data);
    }
}

/// 一个订阅条目（标题 + 下载地址）。
#[derive(Debug, Clone)]
pub struct FeedItem {
    pub title: String,
    pub url: String,
}

/// 下载并解析 RSS/Atom feed，返回条目列表。
pub async fn parse_feed(url: &Url, client: &Client, user_agent: &str) -> Result<Vec<FeedItem>> {
    let resp = client
        .get(url.clone())
        .header(reqwest::header::USER_AGENT, user_agent)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("请求 RSS feed 失败")?;
    if !resp.status().is_success() {
        bail!("下载 RSS feed 失败: HTTP {}", resp.status());
    }
    let text = resp.text().await.context("读取 RSS 文本失败")?;

    // 根据根元素精确选择解析器：<rss 为 RSS 2.0，<feed 为 Atom。
    let root_tag = detect_root_tag(&text);
    if root_tag == "rss" {
        parse_rss(&text)
    } else {
        parse_atom(&text)
    }
}

/// 检测 feed 根元素名（跳过 XML 声明与注释）。
fn detect_root_tag(text: &str) -> &'static str {
    let t = text.trim_start();
    // 跳过 XML 声明（<?xml ... ?>）。
    let body = if t.starts_with("<?xml") {
        t.find("?>").map(|i| &t[i + 2..]).unwrap_or(t)
    } else {
        t
    };
    let body = body.trim_start();
    if body.starts_with("<rss") {
        "rss"
    } else if body.starts_with("<feed") {
        "atom"
    } else {
        "unknown"
    }
}

/// 解析 RSS 2.0。
fn parse_rss(text: &str) -> Result<Vec<FeedItem>> {
    let channel = rss::Channel::read_from(text.as_bytes()).context("解析 RSS 失败")?;
    let mut items = Vec::new();
    for item in channel.items() {
        let title = item.title().unwrap_or("").to_string();
        let url = item
            .enclosure()
            .map(|e| e.url().to_string())
            .or_else(|| item.link().map(|s| s.to_string()))
            .unwrap_or_default();
        if !url.is_empty() {
            items.push(FeedItem { title, url });
        }
    }
    Ok(items)
}

/// 解析 Atom。
fn parse_atom(text: &str) -> Result<Vec<FeedItem>> {
    let feed: atom_syndication::Feed = text
        .parse()
        .map_err(|e: &'static str| anyhow!("解析 Atom 失败: {e}"))?;
    let mut items = Vec::new();
    for entry in &feed.entries {
        let title = entry.title.clone();
        let url = entry
            .links
            .iter()
            .find(|l| l.rel.as_deref() == Some("enclosure"))
            .map(|l| l.href.to_string())
            .or_else(|| entry.links.first().map(|l| l.href.to_string()))
            .unwrap_or_default();
        if !url.is_empty() {
            items.push(FeedItem { title, url });
        }
    }
    Ok(items)
}