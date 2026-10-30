//! 断点续传：chunk 位图模型。
//!
//! 下载器把文件按固定 chunk 大小切分为若干数据块，所有连接共享一个全局块队列
//! （work-stealing）：快连接自动领取更多块，慢连接自然少做。断点状态记录
//! 「哪些块已完成 + 下一个待领取块索引」，中断后可精确续传，且连接数可随时变化。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 断点续传控制文件（.rdmeta）内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub url: String,
    pub file_name: String,
    pub total_size: u64,
    /// 服务器 ETag，用于检测远端文件是否变化。
    pub etag: Option<String>,
    /// 每个数据块的大小（字节）。
    pub chunk_size: u64,
    /// 数据块总数。
    pub num_chunks: u64,
    /// 完成位图（hex 编码，bit i = 块 i 已完成）。
    pub done_bitmap_hex: String,
    /// 下一个待领取的块索引。
    pub next_chunk: u64,
    /// 已下载总字节（用于进度显示与校验）。
    pub downloaded_bytes: u64,
    pub updated_at: u64,
}

impl ResumeState {
    /// 新建空状态。
    pub fn new(
        url: &str,
        file_name: &str,
        total_size: u64,
        etag: Option<String>,
        chunk_size: u64,
    ) -> Self {
        let num_chunks = total_size.div_ceil(chunk_size.max(1)).max(1);
        let bitmap_len = num_chunks.div_ceil(8) as usize;
        Self {
            url: url.to_string(),
            file_name: file_name.to_string(),
            total_size,
            etag,
            chunk_size: chunk_size.max(1),
            num_chunks,
            done_bitmap_hex: hex::encode(vec![0u8; bitmap_len]),
            next_chunk: 0,
            downloaded_bytes: 0,
            updated_at: now_unix(),
        }
    }

    /// 控制文件路径：<输出文件>.rdmeta
    pub fn meta_path_for(output: &Path) -> PathBuf {
        let mut os = output.as_os_str().to_os_string();
        os.push(".rdmeta");
        PathBuf::from(os)
    }

    /// 加载控制文件；不存在或损坏返回 None（损坏文件将被忽略并重建）。
    pub fn load(output: &Path) -> Option<ResumeState> {
        let path = Self::meta_path_for(output);
        let data = std::fs::read(&path).ok()?;
        match serde_json::from_slice::<ResumeState>(&data) {
            Ok(s) => Some(s),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                None
            }
        }
    }

    /// 保存控制文件。
    pub fn save(&self, output: &Path) -> Result<()> {
        let path = Self::meta_path_for(output);
        let data = serde_json::to_vec_pretty(self)
            .map_err(|e| anyhow!("序列化断点状态失败: {e}"))?;
        std::fs::write(&path, data)
            .map_err(|e| anyhow!("写入断点文件 {} 失败: {e}", path.display()))
    }

    /// 删除控制文件。
    pub fn remove_meta(output: &Path) {
        let _ = std::fs::remove_file(Self::meta_path_for(output));
    }

    /// URL 与 ETag 同时匹配才认为是同一资源。
    pub fn matches(&self, url: &str, etag: Option<&str>) -> bool {
        if self.url != url {
            return false;
        }
        match (self.etag.as_deref(), etag) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    /// 查询块是否已完成。
    #[allow(dead_code)]
    pub fn chunk_done(&self, idx: u64) -> bool {
        if idx >= self.num_chunks {
            return true;
        }
        let bytes = match hex::decode(&self.done_bitmap_hex) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let byte = bytes[(idx / 8) as usize];
        (byte >> (idx % 8)) & 1 == 1
    }

    /// 标记块完成。
    #[allow(dead_code)]
    pub fn mark_chunk_done(&mut self, idx: u64) {
        if idx >= self.num_chunks {
            return;
        }
        let mut bytes = hex::decode(&self.done_bitmap_hex).unwrap_or_default();
        while (bytes.len() as u64) < self.num_chunks.div_ceil(8) {
            bytes.push(0);
        }
        bytes[(idx / 8) as usize] |= 1 << (idx % 8);
        self.done_bitmap_hex = hex::encode(bytes);
    }

    /// 已完成的块数。
    #[allow(dead_code)]
    pub fn done_chunks_count(&self) -> u64 {
        let bytes = hex::decode(&self.done_bitmap_hex).unwrap_or_default();
        bytes
            .iter()
            .take(self.num_chunks.div_ceil(8) as usize)
            .map(|b| b.count_ones() as u64)
            .sum()
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_mark_and_query() {
        let mut st = ResumeState::new("http://example.com/f.bin", "f.bin", 1000, None, 100);
        // 1000 / 100 = 10 块
        assert_eq!(st.num_chunks, 10);
        assert!(!st.chunk_done(0));
        st.mark_chunk_done(0);
        st.mark_chunk_done(9);
        assert!(st.chunk_done(0));
        assert!(st.chunk_done(9));
        assert!(!st.chunk_done(5));
        assert_eq!(st.done_chunks_count(), 2);
    }

    #[test]
    fn save_load_roundtrip() {
        let output = std::env::temp_dir().join("turbo_dl_test_resume.bin");
        let mut st = ResumeState::new(
            "http://example.com/f.bin",
            "f.bin",
            500,
            Some("\"etag1\"".to_string()),
            50,
        );
        st.mark_chunk_done(3);
        st.save(&output).unwrap();

        let loaded = ResumeState::load(&output).expect("应能加载断点");
        assert_eq!(loaded.url, "http://example.com/f.bin");
        assert_eq!(loaded.etag.as_deref(), Some("\"etag1\""));
        assert!(loaded.chunk_done(3));
        assert_eq!(loaded.downloaded_bytes, 0);
        assert_eq!(loaded.total_size, 500);

        // matches：URL 与 ETag 一致才算同一资源
        assert!(loaded.matches("http://example.com/f.bin", Some("\"etag1\"")));
        assert!(!loaded.matches("http://example.com/other.bin", Some("\"etag1\"")));

        ResumeState::remove_meta(&output);
        let meta = ResumeState::meta_path_for(&output);
        assert!(!meta.exists(), "断点文件应已删除");
    }

    #[test]
    fn corrupt_meta_ignored_and_removed() {
        let output = std::env::temp_dir().join("turbo_dl_test_corrupt.bin");
        let meta = ResumeState::meta_path_for(&output);
        std::fs::write(&meta, "{not valid json").unwrap();
        assert!(ResumeState::load(&output).is_none());
        assert!(!meta.exists(), "损坏的断点文件应被删除");
    }
}