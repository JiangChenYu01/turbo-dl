//! Git 仓库克隆引擎（libgit2 静态链接实现，运行时不调用外部 git 命令）。
//!
//! 支持：
//! - 显式 Git URL：`git://`、`ssh://git@`、`git+https://`、以 `.git` 结尾的 http(s) URL；
//! - HTTP 智能协议自动识别：对常见 Git 托管平台（GitHub/GitLab/Gitee）的仓库页，
//!   通过 `GET <url>/info/refs?service=git-upload-pack` 探测，响应为
//!   `application/x-git-upload-pack-advertisement` 时判定为 Git 仓库并自动克隆。

use crate::app::{TaskState, TaskStatus};
use crate::engine::DownloadConfig;
use anyhow::{anyhow, Context, Result};
use std::sync::{Arc, Mutex};

/// 判断 URL 是否显式是 Git 仓库（协议前缀或 `.git` 后缀）。
pub fn is_git_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("git://")
        || lower.starts_with("ssh://git@")
        || lower.starts_with("git+")
        || lower.ends_with(".git")
}

/// 从仓库 URL 推断本地目录名（去掉 `.git` 后缀与末尾斜杠）。
pub fn repo_dir_name(url: &str) -> String {
    let name = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git")
        .to_string();
    if name.is_empty() {
        "repo".to_string()
    } else {
        name
    }
}

/// 执行 Git 克隆（git2 是阻塞 API，放入 spawn_blocking）。
/// `generation` 为启动时任务代次；若运行中被 resume 递增，旧引擎需主动退出。
pub async fn download_git(
    task: Arc<Mutex<TaskState>>,
    cfg: DownloadConfig,
    generation: u64,
) -> Result<()> {
    let url = task.lock().unwrap().url.clone();
    let repo_name = repo_dir_name(&url);
    let dest = cfg.output_dir.join(&repo_name);
    {
        let mut t = task.lock().unwrap();
        t.file_name = repo_name;
        t.output_path = dest.clone();
        t.status = TaskStatus::Downloading;
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    if !cfg.quiet {
        tracing::info!("🔀 检测到 Git 仓库，正在克隆: {url}");
    }

    let url2 = url.clone();
    let dest2 = dest.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        // libgit2 原生克隆（支持 http/https/git/本地路径），不调用外部 git。
        git2::Repository::clone(&url2, &dest2)
            .map(|_| ())
            .map_err(|e| anyhow!("Git 克隆失败: {e}"))
    })
    .await
    .context("Git 克隆线程异常")??;

    // 任务被 resume 重新开始：旧引擎静默退出，不覆盖新引擎状态。
    if task.lock().unwrap().generation != generation {
        return Ok(());
    }

    let mut t = task.lock().unwrap();
    t.downloaded = 0;
    t.total = 0;
    t.status = TaskStatus::Completed;
    if !cfg.quiet {
        tracing::info!("✓ Git 仓库已克隆到: {}", dest.display());
    }
    Ok(())
}