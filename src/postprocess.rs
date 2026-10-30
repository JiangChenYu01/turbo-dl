//! 下载后处理：自动解压压缩包 + 按文件类型分类（纯 Rust，不调用外部工具）。
//!
//! - 解压支持：zip、tar、tar.gz/tgz；
//! - 分类：按扩展名移动到「视频/音频/压缩包/文档/图片」子目录；
//! - 全部阻塞 IO 放入 spawn_blocking，避免阻塞 GUI/CLI 主循环。

use crate::app::Settings;
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

/// 对已完成的下载文件执行后处理（解压/分类）。失败只打印警告，不影响任务状态。
pub async fn run(output_path: PathBuf, settings: Settings) {
    if settings.auto_extract {
        if let Err(e) = extract_archive(&output_path, &settings).await {
            tracing::warn!("⚠ 自动解压失败 {}: {e:#}", output_path.display());
        }
    }
    if settings.auto_classify {
        if let Err(e) = classify_file(&output_path, &settings).await {
            tracing::warn!("⚠ 自动分类失败 {}: {e:#}", output_path.display());
        }
    }
}

/// 自动解压压缩包到「压缩包同目录/同名目录」，成功后按设置删除原压缩包。
async fn extract_archive(path: &Path, settings: &Settings) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "zip" | "tar" | "gz" | "tgz") {
        return Ok(());
    }
    let path = path.to_path_buf();
    let dest = archive_dest_dir(&path);
    tokio::fs::create_dir_all(&dest)
        .await
        .with_context(|| format!("创建解压目录失败: {}", dest.display()))?;

    // 判断是否 tar.gz：文件名以 .tar.gz 或 .tgz 结尾。
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let is_tar_gz = name.ends_with(".tar.gz") || name.ends_with(".tgz") || ext == "tgz";

    let dest2 = dest.clone();
    let path2 = path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        match ext.as_str() {
            "zip" => extract_zip(&path2, &dest2),
            "tar" => extract_tar(&path2, &dest2, false),
            _ => extract_tar(&path2, &dest2, is_tar_gz),
        }
    })
    .await
    .context("解压线程异常")??;

    // 解压成功后可选择删除原压缩包。
    if settings.delete_archive_after_extract {
        let _ = tokio::fs::remove_file(&path).await;
    }
    Ok(())
}

/// 解压目标目录：`<压缩包目录>/<文件名去扩展名>/`。
fn archive_dest_dir(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "extracted".to_string());
    // file.tar.gz -> stem 是 file.tar，需要再去掉 .tar
    let stem = stem.trim_end_matches(".tar").to_string();
    path.parent().unwrap_or_else(|| Path::new(".")).join(stem)
}

/// 解压 zip（阻塞），防 zip-slip 路径穿越。
fn extract_zip(path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("打开 zip 失败: {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("解析 zip 失败")?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("读取 zip 条目 #{i} 失败"))?;
        let out_path = sanitize_archive_path(dest, entry.name())?;
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

/// 解压 tar / tar.gz（阻塞），tar crate 自带路径穿越防护。
fn extract_tar(path: &Path, dest: &Path, gz: bool) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("打开 tar 失败: {}", path.display()))?;
    let reader: Box<dyn Read> = if gz {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    archive.unpack(dest).context("解压 tar 失败")?;
    Ok(())
}

/// 规范化压缩包内路径，拒绝绝对路径与 `..` 穿越。
fn sanitize_archive_path(dest: &Path, name: &str) -> Result<PathBuf> {
    let clean = name.replace('\\', "/");
    let rel = Path::new(&clean);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_)))
    {
        bail!("压缩包包含不安全路径: {name}");
    }
    Ok(dest.join(rel))
}

/// 按扩展名分类移动文件。
async fn classify_file(path: &Path, settings: &Settings) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let Some(category) = category_for_ext(&ext) else {
        return Ok(());
    };

    let root = if settings.classify_root.trim().is_empty() {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(settings.classify_root.trim())
    };
    let dest_dir = root.join(category);
    tokio::fs::create_dir_all(&dest_dir).await?;
    let dest = dest_dir.join(path.file_name().unwrap_or_default());
    if dest == path {
        return Ok(());
    }
    tokio::fs::rename(path, &dest)
        .await
        .with_context(|| format!("移动文件到 {} 失败", dest.display()))?;
    Ok(())
}

/// 扩展名 → 分类目录名。
fn category_for_ext(ext: &str) -> Option<&'static str> {
    const VIDEO: &[&str] = &["mp4", "mkv", "mov", "webm", "avi", "flv", "ts", "m4v", "wmv"];
    const AUDIO: &[&str] = &["mp3", "flac", "wav", "m4a", "aac", "ogg", "opus"];
    const ARCHIVE: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "tgz"];
    const DOC: &[&str] = &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "epub"];
    const IMAGE: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico"];
    if VIDEO.contains(&ext) {
        Some("视频")
    } else if AUDIO.contains(&ext) {
        Some("音频")
    } else if ARCHIVE.contains(&ext) {
        Some("压缩包")
    } else if DOC.contains(&ext) {
        Some("文档")
    } else if IMAGE.contains(&ext) {
        Some("图片")
    } else {
        None
    }
}