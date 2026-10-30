//! HTTP 引擎黑盒集成测试：直接运行 turbo-dl 可执行文件，
//! 针对内嵌的 Range 服务器验证「多连接下载 / 断点续传 / 文件锁互斥 / M3U8 AES-128 解密」。

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

const FILE_SIZE: usize = 2 * 1024 * 1024; // 2 MiB
const CHUNK: u64 = 256 * 1024; // 测试用 256K 分段（与 --min-segment-size 256K 对应）

/// 解析 Range 头，返回 (start, end)。无 Range 时返回整个文件。
fn parse_range(range: &str, len: usize) -> (usize, usize) {
    if range.is_empty() {
        return (0, len - 1);
    }
    let spec = range.trim_start_matches("bytes=");
    let (s, e) = spec.split_once('-').unwrap_or((spec, spec));
    let start: usize = s.trim().parse().unwrap_or(0);
    let end: usize = if e.trim().is_empty() {
        len - 1
    } else {
        e.trim().parse().unwrap_or(len - 1)
    };
    (start.min(len - 1), end.min(len - 1).max(start))
}

/// 启动一个支持 Range 的极简文件服务器（路径 → 内容）。
async fn start_file_server(
    files: HashMap<String, Vec<u8>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let files = Arc::new(files);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let files2 = files.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let files = files2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let mut lines = req.lines();
                let request_line = lines.next().unwrap_or("");
                let method = request_line
                    .split_whitespace()
                    .next()
                    .unwrap_or("GET")
                    .to_string();
                let raw_path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let path = raw_path.split('?').next().unwrap_or(raw_path).to_string();
                let mut range = String::new();
                for line in req.lines().skip(1) {
                    let lower = line.to_ascii_lowercase();
                    if lower.starts_with("range:") {
                        range = line
                            .split_once(':')
                            .map(|(_, v)| v.trim().to_string())
                            .unwrap_or_default();
                    }
                }
                let Some(data) = files.get(path.as_str()) else {
                    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                    return;
                };
                let (start, end) = parse_range(&range, data.len());
                let status = if range.is_empty() {
                    "200 OK"
                } else {
                    "206 Partial Content"
                };
                let body = &data[start..=end];
                let mut head = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n",
                    status,
                    body.len()
                );
                if !range.is_empty() {
                    head.push_str(&format!(
                        "Content-Range: bytes {}-{}/{}\r\n",
                        start,
                        end,
                        data.len()
                    ));
                }
                head.push_str("Connection: close\r\n\r\n");
                let _ = sock.write_all(head.as_bytes()).await;
                if method != "HEAD" {
                    let _ = sock.write_all(body).await;
                }
            });
        }
    });
    (format!("http://{addr}"), handle)
}

/// 创建唯一临时目录（测试间互不冲突）。
fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "turbo_dl_test_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn basic_download_matches_source() {
    let data: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    let mut files = HashMap::new();
    files.insert("/file.bin".to_string(), data.clone());
    let (base, _jh) = start_file_server(files).await;
    let dir = tmp_dir("basic");

    let out = Command::new(env!("CARGO_BIN_EXE_turbo-dl"))
        .args([
            "-d",
            dir.to_str().unwrap(),
            "-c",
            "4",
            "--min-segment-size",
            "256K",
            "-q",
            &format!("{}/file.bin", base),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "下载失败 exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let file = std::fs::read(dir.join("file.bin")).unwrap();
    assert_eq!(file.len(), FILE_SIZE);
    assert_eq!(file, data, "下载内容应与源一致");
    // 完成后的断点文件应被清理
    assert!(!dir.join("file.bin.rdmeta").exists(), "完成后应删除 .rdmeta");
}

#[tokio::test]
async fn resume_from_partial_bitmap() {
    let data: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    let mut files = HashMap::new();
    files.insert("/file.bin".to_string(), data.clone());
    let (base, _jh) = start_file_server(files).await;
    let dir = tmp_dir("resume");
    let total = data.len() as u64;
    let num_chunks = total.div_ceil(CHUNK);

    // 手动构造「下载了前 4 块」的断点场景：预分配文件 + 写入前 4 块内容 + .rdmeta
    let path = dir.join("file.bin");
    let mut f = std::fs::File::create(&path).unwrap();
    f.set_len(total).unwrap();
    for i in 0..4u64 {
        let start = (i * CHUNK) as usize;
        f.seek(SeekFrom::Start(start as u64)).unwrap();
        f.write_all(&data[start..start + CHUNK as usize]).unwrap();
    }
    f.flush().unwrap();

    let bitmap_len = (num_chunks as usize).div_ceil(8);
    let mut bitmap = vec![0u8; bitmap_len];
    for i in 0..4u64 {
        bitmap[(i / 8) as usize] |= 1 << (i % 8);
    }
    let rmeta = serde_json::json!({
        "url": format!("{}/file.bin", base),
        "file_name": "file.bin",
        "total_size": total,
        "etag": null,
        "chunk_size": CHUNK,
        "num_chunks": num_chunks,
        "done_bitmap_hex": hex::encode(&bitmap),
        "next_chunk": 4,
        "downloaded_bytes": 4 * CHUNK,
        "updated_at": 0,
    });
    std::fs::write(dir.join("file.bin.rdmeta"), rmeta.to_string()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_turbo-dl"))
        .args([
            "-d",
            dir.to_str().unwrap(),
            "-c",
            "4",
            "--min-segment-size",
            "256K",
            "-q",
            &format!("{}/file.bin", base),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "续传失败 exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let file = std::fs::read(dir.join("file.bin")).unwrap();
    assert_eq!(file, data, "断点续传后文件应完整且一致");
}

#[tokio::test]
async fn file_lock_prevents_duplicate_process() {
    let data: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    let mut files = HashMap::new();
    files.insert("/file.bin".to_string(), data);
    let (base, _jh) = start_file_server(files).await;
    let dir = tmp_dir("lock");

    // 进程 A：限速慢速下载，保持运行以持有文件锁。
    let mut child_a = Command::new(env!("CARGO_BIN_EXE_turbo-dl"))
        .args([
            "-d",
            dir.to_str().unwrap(),
            "-c",
            "1",
            "--rate-limit",
            "100K",
            "-q",
            &format!("{}/file.bin", base),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // 等待进程 A 创建锁文件（最长 10s）。
    let lock_path = dir.join("file.bin.lock");
    let mut waited = 0;
    while !lock_path.exists() && waited < 100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        waited += 1;
    }
    assert!(lock_path.exists(), "进程 A 应已创建锁文件");

    // 进程 B 尝试下载同一文件：应被文件锁拒绝。
    // 注意：-q 模式下错误汇总打印到 stdout，因此同时检查 stdout 与 stderr。
    let out_b = Command::new(env!("CARGO_BIN_EXE_turbo-dl"))
        .args([
            "-d",
            dir.to_str().unwrap(),
            "-c",
            "1",
            "-q",
            &format!("{}/file.bin", base),
        ])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out_b.stdout);
    let stderr = String::from_utf8_lossy(&out_b.stderr);
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("文件正在被另一个 turbo-dl 进程"),
        "进程 B 应被文件锁拒绝，实际 exit={:?} stdout={stdout} stderr={stderr}",
        out_b.status.code()
    );

    // 停止进程 A。
    child_a.kill().await.unwrap();
    let _ = child_a.wait().await;
}

/// 生成 AES-128-CBC 加密分片（测试辅助：与实现使用同一套 RustCrypto crate）。
fn aes_encrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    use aes::Aes128;
    use cbc::cipher::{
        block_padding::Pkcs7, generic_array::GenericArray, BlockEncryptMut, KeyIvInit,
    };
    use cbc::Encryptor;
    let mut buf = vec![0u8; data.len() + 16];
    buf[..data.len()].copy_from_slice(data);
    Encryptor::<Aes128>::new(GenericArray::from_slice(key), GenericArray::from_slice(iv))
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .expect("AES 加密失败")
        .to_vec()
}

/// 媒体序列号默认 IV（与实现一致）。
fn default_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

/// 构造一个加密 m3u8 测试流（3 个分片，明文非 16 字节倍数）。
fn encrypted_stream_files(
    key: &[u8; 16],
    use_explicit_iv: bool,
) -> (HashMap<String, Vec<u8>>, Vec<u8>) {
    let segments: Vec<Vec<u8>> = (0..3)
        .map(|seed| (0..300 * 1024 + 7).map(|i| ((seed * 97 + i) % 251) as u8).collect())
        .collect();
    let explicit_iv = [0x24u8; 16];

    let enc_segments: Vec<Vec<u8>> = segments
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let iv = if use_explicit_iv {
                explicit_iv
            } else {
                default_iv(i as u64)
            };
            aes_encrypt(seg, key, &iv)
        })
        .collect();

    let key_line = if use_explicit_iv {
        r#"#EXT-X-KEY:METHOD=AES-128,URI="enc_key.bin",IV=0x24242424242424242424242424242424"#
    } else {
        r#"#EXT-X-KEY:METHOD=AES-128,URI="enc_key.bin""#
    };
    let m3u8 = format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n{key_line}\n#EXTINF:1.0,\nseg_enc_1.ts\n#EXTINF:1.0,\nseg_enc_2.ts\n#EXTINF:1.0,\nseg_enc_3.ts\n#EXT-X-ENDLIST\n"
    );

    let mut files = HashMap::new();
    files.insert("/enc_key.bin".to_string(), key.to_vec());
    files.insert("/enc_stream.m3u8".to_string(), m3u8.into_bytes());
    for (i, enc) in enc_segments.iter().enumerate() {
        files.insert(format!("/seg_enc_{}.ts", i + 1), enc.clone());
    }
    (files, segments.concat())
}

#[tokio::test]
async fn aes128_encrypted_m3u8_explicit_iv() {
    let key = [0x13u8; 16];
    let (files, expected) = encrypted_stream_files(&key, true);
    let (base, _jh) = start_file_server(files).await;
    let dir = tmp_dir("aes_iv");

    let out = Command::new(env!("CARGO_BIN_EXE_turbo-dl"))
        .args(["-d", dir.to_str().unwrap(), "-q", &format!("{}/enc_stream.m3u8", base)])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "加密流下载失败 exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let file = std::fs::read(dir.join("enc_stream.ts")).unwrap();
    assert_eq!(file, expected, "显式 IV 解密后的输出应与明文拼接一致");
}

#[tokio::test]
async fn aes128_encrypted_m3u8_default_iv() {
    let key = [0x37u8; 16];
    let (files, expected) = encrypted_stream_files(&key, false);
    let (base, _jh) = start_file_server(files).await;
    let dir = tmp_dir("aes_seq");

    let out = Command::new(env!("CARGO_BIN_EXE_turbo-dl"))
        .args(["-d", dir.to_str().unwrap(), "-q", &format!("{}/enc_stream.m3u8", base)])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "加密流下载失败 exit={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let file = std::fs::read(dir.join("enc_stream.ts")).unwrap();
    assert_eq!(file, expected, "媒体序列号默认 IV 解密后的输出应与明文拼接一致");
}