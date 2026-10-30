//! GitHub Release 解析：从 release 页面 URL 获取资产直链。
//!
//! GitHub release 页面（如 `https://github.com/owner/repo/releases/latest`）是 HTML，
//! 资产真实下载地址在 release 的 `assets[].browser_download_url` 中。
//! 本模块通过 GitHub API 解析：
//! - `GET /repos/{owner}/{repo}/releases/latest`
//! - `GET /repos/{owner}/{repo}/releases/tags/{tag}`

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use url::Url;

/// 一个 release 资产（文件名 + 直链）。
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
    #[serde(default)]
    zipball_url: Option<String>,
    #[serde(default)]
    tarball_url: Option<String>,
}

/// 判断 URL 是否为 GitHub release 页面（`/owner/repo/releases/latest` 或
/// `/owner/repo/releases/tag/<tag>`）。注意 `releases/download/...` 是资产直链，
/// 不是 release 页面，不能误判。
pub fn is_release_url(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("");
    if host != "github.com" && !host.ends_with(".github.com") {
        return false;
    }
    let segs: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
    segs.len() >= 4 && segs[2] == "releases" && (segs[3] == "latest" || segs[3] == "tag")
}

/// 解析 GitHub release 资产列表（含源码包兜底）。
pub async fn resolve_release(url: &Url, client: &Client) -> Result<Vec<ReleaseAsset>> {
    let segs: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
    if segs.len() < 4 || segs[2] != "releases" {
        bail!("不是 GitHub release 页面: {url}");
    }
    let owner = segs[0];
    let repo = segs[1];

    let api_url = if segs.len() >= 5 && segs[3] == "tag" {
        format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/tags/{}",
            segs[4]
        )
    } else {
        // latest（或 /releases 列表页）→ 最新 release。
        format!("https://api.github.com/repos/{owner}/{repo}/releases/latest")
    };

    let resp = client
        .get(&api_url)
        .header("User-Agent", "turbo-dl/0.2")
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("请求 GitHub API 失败")?;
    if !resp.status().is_success() {
        bail!(
            "GitHub API 返回 HTTP {}（可能仓库不存在或没有 release）",
            resp.status()
        );
    }
    let rel: GithubRelease = resp
        .json()
        .await
        .context("解析 GitHub release JSON 失败")?;

    let mut assets: Vec<ReleaseAsset> = rel
        .assets
        .into_iter()
        .map(|a| ReleaseAsset {
            name: a.name,
            url: a.browser_download_url,
        })
        .collect();

    // 没有二进制资产时，提供源码包直链。
    if assets.is_empty() {
        if let Some(z) = rel.zipball_url {
            assets.push(ReleaseAsset {
                name: format!("{}-{}.zip", repo, rel.tag_name),
                url: z,
            });
        }
        if let Some(t) = rel.tarball_url {
            assets.push(ReleaseAsset {
                name: format!("{}-{}.tar.gz", repo, rel.tag_name),
                url: t,
            });
        }
    }
    Ok(assets)
}