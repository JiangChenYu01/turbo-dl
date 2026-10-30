//! 命令行参数解析。

use crate::util::parse_size;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "turbo-dl",
    version,
    about = "超越 IDM 的多协议下载器（HTTP/FTP/BT，图形界面）",
    long_about = "turbo-dl —— Rust 高性能下载器。\n图形界面 + 命令行双模式；work-stealing 动态分段抢占带宽；HTTP/FTP/BT 多协议；断点续传、限速、代理。"
)]
pub struct Cli {
    /// 下载 URL（支持多个）；留空则启动图形界面。
    #[arg(value_name = "URL")]
    pub urls: Vec<String>,

    /// 强制启动图形界面。
    #[arg(long)]
    pub gui: bool,

    /// 输出目录（CLI 模式；GUI 模式下作为默认保存目录，留空使用系统下载目录）。
    #[arg(short, long)]
    pub dir: Option<PathBuf>,

    /// 输出文件名（CLI 模式，仅单个任务）。
    #[arg(short, long)]
    pub output: Option<String>,

    /// 每任务连接数。
    #[arg(short, long, default_value_t = 8)]
    pub connections: usize,

    /// 最小分段大小，如 1M / 500K。
    #[arg(long, default_value = "1M")]
    pub min_segment_size: String,

    /// 并发任务数。
    #[arg(short, long, default_value_t = 3)]
    pub jobs: usize,

    /// 每连接限速，如 500K / 2M。
    #[arg(long)]
    pub rate_limit: Option<String>,

    /// 全局总限速（所有任务合计），如 5M / 1M。
    #[arg(long)]
    pub global_rate_limit: Option<String>,

    /// 代理，如 http://127.0.0.1:7890 或 socks5://127.0.0.1:1080。
    #[arg(short, long)]
    pub proxy: Option<String>,

    /// HTTP/FTP 认证，格式 user:pass（FTP URL 中的内嵌凭据优先）。
    #[arg(short = 'u', long = "auth", value_name = "USER:PASS")]
    pub auth: Option<String>,

    /// 自定义请求头（可重复）。
    #[arg(short = 'H', long = "header")]
    pub headers: Vec<String>,

    /// User-Agent。
    #[arg(long, default_value = concat!("turbo-dl/", env!("CARGO_PKG_VERSION")))]
    pub user_agent: String,

    /// 每块失败重试次数。
    #[arg(long, default_value_t = 5)]
    pub retries: u32,

    /// 连接超时秒数。
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,

    /// 下载完成后校验，如 sha256:HEX。
    #[arg(long)]
    pub checksum: Option<String>,

    /// URL 队列文件。
    #[arg(long)]
    pub queue: Option<PathBuf>,

    /// 解析 .url/.txt 链接文件并发送到运行中的 turbo-dl（右键菜单集成）。
    #[arg(long, value_name = "PATH")]
    pub send: Option<PathBuf>,

    /// 强制重新下载。
    #[arg(long)]
    pub force_restart: bool,

    /// 禁用断点续传。
    #[arg(long)]
    pub no_resume: bool,

    /// 安静模式：不显示进度条，仅输出结果。
    #[arg(short, long)]
    pub quiet: bool,

    /// 定时开始（HH:MM，24 小时制；到点自动开始，已过则顺延到明天）。
    #[arg(long, value_name = "HH:MM")]
    pub start_at: Option<String>,

    /// 全部任务完成后自动关机（Windows: shutdown /s /t 60）。
    #[arg(long)]
    pub shutdown: bool,

    /// 下载完成后用系统默认程序打开文件（CLI 模式）。
    #[arg(long)]
    pub open_after: bool,

    /// 网站镜像模式：递归克隆同域页面与静态资源（首个 URL）。
    #[arg(long)]
    pub mirror: bool,

    /// 列出 GitHub release 页面所有资产的直链（不下载），如 https://github.com/owner/repo/releases/latest。
    #[arg(long, value_name = "URL")]
    pub release_info: Option<String>,

    /// 下载完成后自动解压压缩包（zip/tar/tar.gz）。
    #[arg(long)]
    pub extract: bool,

    /// 解压成功后删除原压缩包。
    #[arg(long)]
    pub delete_archive: bool,

    /// 下载完成后按类型自动分类到子目录。
    #[arg(long)]
    pub classify: bool,

    /// 分类根目录（默认 = 下载文件所在目录）。
    #[arg(long, value_name = "DIR")]
    pub classify_dir: Option<String>,

    /// Referer 防盗链。
    #[arg(long, value_name = "URL")]
    pub referer: Option<String>,

    /// Cookie 防盗链（原样放入 Cookie 头）。
    #[arg(long, value_name = "STRING")]
    pub cookie: Option<String>,

    /// 完成后自动尝试旁车校验和（<url>.sha256/.sha1/.md5）。
    #[arg(long)]
    pub checksum_sidecar: bool,

    /// RSS 订阅源：解析 feed 并自动批量下载所有条目链接。
    #[arg(long, value_name = "URL")]
    pub rss: Option<String>,

    /// RSS 轮询间隔（分钟，仅与 --rss 配合使用）：定时检查订阅源，
    /// 自动下载新条目（已下载的条目不会重复下载）。
    #[arg(long, value_name = "MINUTES")]
    pub rss_interval: Option<u64>,

    /// 只列出 RSS feed 的条目（标题+链接），不下载。
    #[arg(long, value_name = "URL")]
    pub rss_list: Option<String>,
}

impl Cli {
    /// 校验和参数 -> (算法, 期望值)。
    pub fn checksum_parts(&self) -> Result<Option<(String, String)>> {
        let Some(cs) = &self.checksum else {
            return Ok(None);
        };
        let (algo, val) = cs
            .split_once(':')
            .ok_or_else(|| anyhow!("校验和格式应为 algo:HEX，如 sha256:0123..."))?;
        let algo = algo.trim().to_ascii_lowercase();
        let val = val.trim().to_ascii_lowercase();
        if !matches!(algo.as_str(), "md5" | "sha1" | "sha256") {
            return Err(anyhow!("不支持的校验算法: {algo}（支持 md5/sha1/sha256）"));
        }
        if val.is_empty() {
            return Err(anyhow!("校验和值不能为空"));
        }
        Ok(Some((algo, val)))
    }

    /// 解析限速参数。
    pub fn rate_limit_value(&self) -> Result<Option<u64>> {
        match &self.rate_limit {
            Some(s) => parse_size(s).map(Some).with_context(|| "解析 --rate-limit 失败"),
            None => Ok(None),
        }
    }

    /// 解析全局总限速参数。
    pub fn global_rate_limit_value(&self) -> Result<Option<u64>> {
        match &self.global_rate_limit {
            Some(s) => parse_size(s).map(Some).with_context(|| "解析 --global-rate-limit 失败"),
            None => Ok(None),
        }
    }

    /// 解析认证参数（user:pass）。
    pub fn auth_parts(&self) -> Result<Option<(String, String)>> {
        match &self.auth {
            Some(s) => {
                let (u, p) = s
                    .split_once(':')
                    .ok_or_else(|| anyhow!("认证格式应为 user:pass，如 -u admin:123456"))?;
                let u = u.trim().to_string();
                if u.is_empty() {
                    return Err(anyhow!("认证用户名不能为空"));
                }
                Ok(Some((u, p.trim().to_string())))
            }
            None => Ok(None),
        }
    }

    /// 解析最小分段大小。
    pub fn min_segment_value(&self) -> Result<u64> {
        parse_size(&self.min_segment_size).with_context(|| "解析 --min-segment-size 失败")
    }

    /// 解析定时开始参数（HH:MM -> Unix 时间戳）。
    pub fn start_at_value(&self) -> Result<Option<u64>> {
        match &self.start_at {
            Some(s) => crate::util::parse_start_at(s).map(Some),
            None => Ok(None),
        }
    }
}