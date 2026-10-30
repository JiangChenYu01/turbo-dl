//! M3U8 / HLS 流媒体下载（纯 Rust，不调用外部 ffmpeg/wget）。
//!
//! 流程：下载主播放列表 → 解析分片（或选择最高带宽子列表）→ 并发下载分片
//! → 按顺序合并为单一 `.ts` 文件。支持 **AES-128-CBC 加密流**：
//! - 解析 `#EXT-X-KEY:METHOD=AES-128,URI="...",IV=0x...`；
//! - 下载密钥，对每个分片做 AES-128-CBC 解密（PKCS#7 去填充）；
//! - 未显式指定 IV 时使用分片媒体序列号（大端序 128 位）作为默认 IV。

use crate::app::{TaskState, TaskStatus};
use crate::engine::http::make_client;
use crate::engine::DownloadConfig;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::USER_AGENT;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use url::Url;

/// 判断 URL 是否为 m3u8 播放列表（路径以 .m3u8 结尾）。
pub fn is_m3u8_url(url: &Url) -> bool {
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

/// HLS AES-128 加密信息（`#EXT-X-KEY` 解析结果）。
#[derive(Clone, Debug)]
struct HlsEncryption {
    /// 密钥绝对 URL。
    key_uri: String,
    /// 显式 IV（16 字节）；None 时使用分片媒体序列号作为默认 IV。
    iv: Option<[u8; 16]>,
}

/// 解析后的播放列表。
struct ResolvedPlaylist {
    /// 分片绝对 URL（按播放顺序）。
    segments: Vec<Url>,
    /// 加密信息（无加密时为 None）。
    encryption: Option<HlsEncryption>,
    /// 媒体序列号起点（`#EXT-X-MEDIA-SEQUENCE`，默认 0）。
    media_sequence: u64,
}

/// 执行 M3U8 分片下载并合并为单一 .ts 文件。
/// `generation` 为启动时任务代次；若运行中被 resume 递增，旧引擎需主动退出。
pub async fn download_m3u8(
    task: Arc<Mutex<TaskState>>,
    cfg: DownloadConfig,
    generation: u64,
) -> Result<()> {
    let client = make_client(&cfg, false)?;
    let url = task.lock().unwrap().url.clone();
    let base = Url::parse(&url).with_context(|| format!("无效 URL: {url}"))?;

    // 1. 下载播放列表文本并解析分片与加密信息。
    let text = fetch_text(&client, &base, &cfg).await?;
    let resolved = resolve_segments(&client, &base, &text, &cfg, 0, None).await?;
    let segments = resolved.segments;
    let encryption = resolved.encryption;
    let media_sequence = resolved.media_sequence;

    // 若为加密流，下载 AES-128 密钥。
    let key: Option<[u8; 16]> = match &encryption {
        Some(enc) => {
            let key_bytes = fetch_key(&client, &enc.key_uri, &cfg).await?;
            if key_bytes.len() != 16 {
                bail!(
                    "AES-128 密钥长度必须为 16 字节，实际 {} 字节",
                    key_bytes.len()
                );
            }
            let mut k = [0u8; 16];
            k.copy_from_slice(&key_bytes);
            Some(k)
        }
        None => None,
    };

    // 2. 输出文件：m3u8 文件名去扩展名 + .ts。
    let output_name = base
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| {
            let s = s.trim_end_matches(".m3u8").trim_end_matches(".M3U8");
            format!("{s}.ts")
        })
        .unwrap_or_else(|| "stream.ts".to_string());
    let output = cfg.output_dir.join(&output_name);

    // 跨进程互斥：防止两个 turbo-dl 进程同时下载同一流到同一文件。
    let _lock = crate::util::FileLockGuard::acquire(&output)?;

    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    {
        let mut t = task.lock().unwrap();
        t.file_name = output_name.clone();
        t.output_path = output.clone();
        t.status = TaskStatus::Downloading;
    }

    // 3. 并发下载分片（8 并发），加密流先解密再写入临时目录文件。
    //    内存占用 = O(并发数 × 单分片大小)，而不是整个视频大小。
    let tmp_dir = output
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!(
            ".{}.parts",
            output
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "stream".to_string())
        ));
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .context("创建分片临时目录失败")?;

    let seg_count = segments.len();
    let results: Vec<(usize, Result<()>)> = futures_util::stream::iter(
        segments.into_iter().enumerate(),
    )
    .map(|(i, seg)| {
        let client = client.clone();
        let cfg = cfg.clone();
        let tmp_dir = tmp_dir.clone();
        let encryption = encryption.clone();
        let key = key.clone();
        async move {
            let r: Result<()> = async {
                let bytes = download_with_retry(&client, &seg, &cfg).await?;
                // AES-128 解密：显式 IV 优先，否则用媒体序列号作默认 IV。
                let bytes = match (&encryption, &key) {
                    (Some(enc), Some(key)) => {
                        let iv = match enc.iv {
                            Some(iv) => iv,
                            None => default_iv(media_sequence + i as u64),
                        };
                        decrypt_segment(&bytes, key, iv)?
                    }
                    _ => bytes,
                };
                let part_path = tmp_dir.join(format!("part_{:06}.ts", i));
                tokio::fs::write(&part_path, &bytes)
                    .await
                    .with_context(|| format!("写入分片 #{i} 失败"))?;
                Ok(())
            }
            .await;
            (i, r)
        }
    })
    .buffer_unordered(8)
    .collect()
    .await;

    // 任一分片失败：清理临时目录并返回错误。
    for (i, r) in results {
        r.with_context(|| format!("下载分片 #{i} 失败"))?;
    }

    // 4. 按原顺序合并分片为单一 .ts 文件。
    let mut file = tokio::fs::File::create(&output)
        .await
        .with_context(|| format!("创建输出文件失败: {}", output.display()))?;
    let mut downloaded: u64 = 0;
    for i in 0..seg_count {
        let part_path = tmp_dir.join(format!("part_{:06}.ts", i));
        let mut part = tokio::fs::File::open(&part_path)
            .await
            .with_context(|| format!("打开分片失败: {}", part_path.display()))?;
        let n = tokio::io::copy(&mut part, &mut file)
            .await
            .context("合并分片失败")?;
        downloaded += n;
        let gen_changed = {
            let mut t = task.lock().unwrap();
            t.downloaded = downloaded;
            t.total = downloaded;
            t.generation != generation
        };
        // 合并过程中被 resume：旧引擎中止并清理半成品，避免覆盖新引擎输出。
        if gen_changed {
            drop(file);
            let _ = tokio::fs::remove_file(&output).await;
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            return Ok(());
        }
    }
    file.flush().await.context("刷新输出文件失败")?;
    drop(file);

    // 清理临时分片目录。
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    {
        let mut t = task.lock().unwrap();
        t.downloaded = downloaded;
        t.total = downloaded;
        t.status = TaskStatus::Completed;
    }
    if !cfg.quiet {
        tracing::info!(
            "✓ M3U8 下载完成: {}（{} 分片，{} 字节{}）",
            output.display(),
            seg_count,
            downloaded,
            if encryption.is_some() { "，AES-128 已解密" } else { "" }
        );
    }
    Ok(())
}

/// 递归解析播放列表，返回分片、加密信息与媒体序列号。
/// 主列表（含 #EXT-X-STREAM-INF）选最高带宽子列表后递归；媒体列表收集 #EXTINF 分片。
/// `inherited` 为父列表继承的加密信息（子列表未声明 KEY 时沿用）。
async fn resolve_segments(
    client: &Client,
    base: &Url,
    text: &str,
    cfg: &DownloadConfig,
    depth: u32,
    inherited: Option<HlsEncryption>,
) -> Result<ResolvedPlaylist> {
    if depth > 3 {
        bail!("m3u8 嵌套层数过深（>3）");
    }
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();

    // 解析当前层声明的加密信息（#EXT-X-KEY）；未声明则继承父层。
    let encryption = match parse_key_encryption(&lines, base)? {
        Some(enc) => Some(enc),
        None => inherited,
    };

    // 主播放列表：选择 BANDWIDTH 最高的子列表并递归。
    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        let mut best: Option<(u64, String)> = None;
        for (i, line) in lines.iter().enumerate() {
            if line.starts_with("#EXT-X-STREAM-INF") {
                let bw = line
                    .split("BANDWIDTH=")
                    .nth(1)
                    .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                if let Some(next) = lines.get(i + 1) {
                    if !next.starts_with('#')
                        && best.as_ref().map(|(b, _)| bw > *b).unwrap_or(true)
                    {
                        best = Some((bw, (*next).to_string()));
                    }
                }
            }
        }
        if let Some((_, sub)) = best {
            let sub_url = base.join(&sub)?;
            let sub_text = fetch_text(client, &sub_url, cfg).await?;
            return Box::pin(resolve_segments(
                client,
                &sub_url,
                &sub_text,
                cfg,
                depth + 1,
                encryption,
            ))
            .await;
        }
        bail!("主播放列表中没有找到子列表");
    }

    // 媒体播放列表：解析媒体序列号起点。
    let mut media_sequence: u64 = 0;
    for line in &lines {
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest.trim().parse().unwrap_or(0);
            break;
        }
    }

    // 收集 #EXTINF 后的分片。
    let mut segs = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("#EXTINF") {
            if let Some(next) = lines.get(i + 1) {
                if !next.starts_with('#') {
                    segs.push(base.join(next)?);
                }
            }
        }
    }
    if segs.is_empty() {
        bail!("播放列表中没有找到任何分片");
    }
    Ok(ResolvedPlaylist {
        segments: segs,
        encryption,
        media_sequence,
    })
}

/// 解析播放列表中的 `#EXT-X-KEY` 行，返回加密信息（无 KEY 行返回 None）。
fn parse_key_encryption(
    lines: &[&str],
    base: &Url,
) -> Result<Option<HlsEncryption>> {
    let mut result: Option<HlsEncryption> = None;
    for line in lines {
        if !line.starts_with("#EXT-X-KEY") {
            continue;
        }
        let attrs: HashMap<String, String> = parse_key_attributes(line)?
            .into_iter()
            .collect();
        match attrs.get("METHOD").map(|s| s.as_str()) {
            Some("NONE") => result = None,
            Some("AES-128") => {
                let uri = attrs.get("URI").cloned().unwrap_or_default();
                if uri.is_empty() {
                    bail!("#EXT-X-KEY 缺少 URI");
                }
                let key_url = base
                    .join(&uri)
                    .with_context(|| format!("无效密钥 URI: {uri}"))?;
                let iv = match attrs.get("IV") {
                    Some(s) => parse_iv_hex(s)?,
                    None => None,
                };
                result = Some(HlsEncryption {
                    key_uri: key_url.to_string(),
                    iv,
                });
            }
            Some(other) => bail!("不支持的加密方法: {other}（仅支持 AES-128/NONE）"),
            None => bail!("#EXT-X-KEY 缺少 METHOD"),
        }
    }
    Ok(result)
}

/// 解析 `#EXT-X-KEY:METHOD=...,URI="...",IV=0x...` 的属性列表。
fn parse_key_attributes(line: &str) -> Result<Vec<(String, String)>> {
    let body = line
        .trim()
        .strip_prefix("#EXT-X-KEY")
        .unwrap_or(line)
        .trim_start_matches(':');
    let chars: Vec<char> = body.chars().collect();
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // 跳过前导逗号。
        while i < chars.len() && chars[i] == ',' {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        // 属性名。
        let name_start = i;
        while i < chars.len() && chars[i] != '=' {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let name: String = chars[name_start..i].iter().collect();
        i += 1; // 跳过 '='
        // 属性值：引号包裹则读到匹配引号（URI 可能含逗号），否则读到逗号。
        let value: String = if i < chars.len() && chars[i] == '"' {
            i += 1;
            let v_start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let v: String = chars[v_start..i].iter().collect();
            i += 1; // 跳过闭合引号
            v
        } else {
            let v_start = i;
            while i < chars.len() && chars[i] != ',' {
                i += 1;
            }
            chars[v_start..i].iter().collect()
        };
        attrs.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(attrs)
}

/// 解析 `0x...` 形式的 16 字节 IV。
fn parse_iv_hex(s: &str) -> Result<Option<[u8; 16]>> {
    let s = s
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if s.is_empty() {
        return Ok(None);
    }
    let bytes = hex::decode(s).with_context(|| format!("无效 IV: {s}"))?;
    if bytes.len() != 16 {
        bail!("IV 长度必须为 16 字节（0x + 32 个 hex 字符），实际 {} 字节", bytes.len());
    }
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&bytes);
    Ok(Some(iv))
}

/// 默认 IV：媒体序列号的大端序 128 位表示。
fn default_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

/// 下载 AES-128 密钥。
async fn fetch_key(client: &Client, key_uri: &str, cfg: &DownloadConfig) -> Result<Vec<u8>> {
    let resp = client
        .get(key_uri)
        .header(USER_AGENT, &cfg.user_agent)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("下载 AES 密钥失败: {key_uri}"))?;
    if !resp.status().is_success() {
        bail!("下载 AES 密钥失败: HTTP {}", resp.status());
    }
    resp.bytes().await.context("读取 AES 密钥失败").map(|b| b.to_vec())
}

/// AES-128-CBC 解密一个分片（PKCS#7 去填充）。
fn decrypt_segment(data: &[u8], key: &[u8; 16], iv: [u8; 16]) -> Result<Vec<u8>> {
    use cbc::cipher::{
        block_padding::Pkcs7, generic_array::GenericArray, BlockDecryptMut, KeyIvInit,
    };
    use cbc::Decryptor;

    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data.len() % 16 != 0 {
        bail!(
            "加密分片长度不是 16 的倍数（{} 字节），无法 AES-CBC 解密",
            data.len()
        );
    }
    let key_arr = GenericArray::from_slice(key);
    let iv_arr = GenericArray::from_slice(&iv);
    let mut buf = data.to_vec();
    let plain = Decryptor::<aes::Aes128>::new(key_arr, iv_arr)
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("AES-128-CBC 解密失败（密钥或 IV 错误？）"))?;
    Ok(plain.to_vec())
}

/// 下载文本（播放列表）。
async fn fetch_text(client: &Client, url: &Url, cfg: &DownloadConfig) -> Result<String> {
    let resp = client
        .get(url.clone())
        .header(USER_AGENT, &cfg.user_agent)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("请求 m3u8 失败")?;
    if !resp.status().is_success() {
        bail!("下载 m3u8 失败: HTTP {}", resp.status());
    }
    resp.text().await.context("读取 m3u8 文本失败")
}

/// 下载单个分片，失败重试 3 次（指数退避）。
async fn download_with_retry(client: &Client, url: &Url, cfg: &DownloadConfig) -> Result<Vec<u8>> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        match client
            .get(url.clone())
            .header(USER_AGENT, &cfg.user_agent)
            .timeout(Duration::from_secs(60))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                return resp.bytes().await.map(|b| b.to_vec()).context("读取分片失败");
            }
            Ok(resp) => last_err = Some(anyhow!("分片下载失败: HTTP {}", resp.status())),
            Err(e) => last_err = Some(anyhow!("分片下载失败: {e}")),
        }
        tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
    }
    Err(last_err.unwrap_or_else(|| anyhow!("分片下载失败")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_attributes_parsing() {
        let line = r#"#EXT-X-KEY:METHOD=AES-128,URI="https://cdn.example.com/key?token=a,b",IV=0x112233445566778899aabbccddeeff00"#;
        let attrs: HashMap<String, String> = parse_key_attributes(line).unwrap().into_iter().collect();
        assert_eq!(attrs.get("METHOD").map(|s| s.as_str()), Some("AES-128"));
        assert_eq!(
            attrs.get("URI").map(|s| s.as_str()),
            Some("https://cdn.example.com/key?token=a,b") // 引号内逗号不截断
        );
        assert_eq!(
            attrs.get("IV").map(|s| s.as_str()),
            Some("0x112233445566778899aabbccddeeff00")
        );
    }

    #[test]
    fn iv_parsing() {
        assert_eq!(
            parse_iv_hex("0x112233445566778899aabbccddeeff00").unwrap(),
            Some([
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff, 0x00
            ])
        );
        assert!(parse_iv_hex("0x12").is_err()); // 长度错误应 Err
        assert_eq!(parse_iv_hex("").unwrap(), None);
    }

    #[test]
    fn default_iv_from_sequence() {
        let iv = default_iv(0);
        assert_eq!(&iv[..8], &[0u8; 8]);
        assert_eq!(&iv[8..], &0u64.to_be_bytes());
        let iv2 = default_iv(1);
        assert_eq!(&iv2[8..], &1u64.to_be_bytes());
    }

    #[test]
    fn aes_roundtrip() {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        use cbc::Encryptor;

        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let plain = b"hello turbo-dl AES-128 segment data 0123456789";
        // 输出缓冲区 = 明文 + 一个块（PKCS#7 最大填充 16 字节）。
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let ct = Encryptor::<aes::Aes128>::new(
            cbc::cipher::generic_array::GenericArray::from_slice(&key),
            cbc::cipher::generic_array::GenericArray::from_slice(&iv),
        )
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
        .expect("AES 加密失败")
        .to_vec();
        assert_eq!(ct.len() % 16, 0);
        let dec = decrypt_segment(&ct, &key, iv).unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn key_none_clears_encryption() {
        let base = Url::parse("https://example.com/stream.m3u8").unwrap();
        let lines = [
            r#"#EXT-X-KEY:METHOD=AES-128,URI="key.bin""#,
            r#"#EXT-X-KEY:METHOD=NONE"#,
        ];
        let enc = parse_key_encryption(&lines, &base).unwrap();
        assert!(enc.is_none());
    }
}