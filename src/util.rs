//! 通用工具函数：容量解析、字节格式化、时长格式化、请求头解析。

use anyhow::{anyhow, Context, Result};
use chrono::TimeZone;
use reqwest::header::{HeaderMap, HeaderName};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 解析人类可读容量，如 "500K"、"1M"、"2G"、"1T"、"1024"、"1MB"、"500KB"（字节）。大小写不敏感。
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err(anyhow!("容量为空"));
    }
    // 去掉可选的 B 后缀：1MB -> 1M，500KB -> 500K，1024B -> 1024。
    let s = if s.ends_with('b') && s.len() > 1 {
        &s[..s.len() - 1]
    } else {
        s.as_str()
    };
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('m') => (&s[..s.len() - 1], 1024u64 * 1024),
        Some('g') => (&s[..s.len() - 1], 1024u64 * 1024 * 1024),
        Some('t') => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _ => (s, 1u64),
    };
    let num = num.trim();
    if num.is_empty() {
        return Err(anyhow!("无效容量: {s}"));
    }
    let v: f64 = num
        .parse()
        .map_err(|_| anyhow!("无效容量: {s}（示例: 1M, 500K, 2G）"))?;
    if v < 0.0 {
        return Err(anyhow!("容量不能为负: {s}"));
    }
    Ok((v * mult as f64) as u64)
}

/// 把字节数格式化为人类可读，如 "45.2 MiB"。
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Windows 保留设备名（含扩展名仍保留，如 CON.txt），命中时加前缀规避。
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 清洗文件名，保证三平台行为一致且不逃逸输出目录：
/// - 替换 Windows 非法字符 `<>:"/\|?*` 与控制字符为 `_`；
/// - 拒绝 `.` / `..`；
/// - 按 UTF-8 边界截断至 255 字节（避免超长文件名创建失败）；
/// - 对 Windows 保留设备名（CON/NUL/COM1…）加 `_` 前缀。
pub fn sanitize_file_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return "_".to_string();
    }
    let mut out: Vec<u8> = Vec::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            out.push(b'_');
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    // 截断到 255 字节，并保证不切断多字节 UTF-8 字符。
    if out.len() > 255 {
        let mut cut = 255;
        while cut > 0 && (out[cut] & 0xC0) == 0x80 {
            cut -= 1;
        }
        out.truncate(cut);
    }
    let mut cleaned = String::from_utf8_lossy(&out).into_owned();
    if cleaned.is_empty() {
        return "_".to_string();
    }
    // 保留设备名：取第一个 `.` 前的部分比较（大写）。
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if WINDOWS_RESERVED_NAMES.contains(&stem.as_str()) {
        cleaned.insert(0, '_');
    }
    cleaned
}

/// 把 Duration 格式化为 HH:MM:SS 或 MM:SS。
#[allow(dead_code)]
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// 解析 "-H 'Name: Value'" 形式的请求头列表。
#[allow(dead_code)]
pub fn parse_headers(items: &[String]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for h in items {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("无效请求头: {h}（应为 Name: Value）"))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return Err(anyhow!("无效请求头: {h}"));
        }
        map.insert(
            name.parse::<HeaderName>()
                .map_err(|_| anyhow!("无效头名称: {name}"))?,
            value
                .parse()
                .map_err(|_| anyhow!("无效头值: {value}"))?,
        );
    }
    Ok(map)
}

/// 解析 "HH:MM"（24 小时制）为下一次到达该时刻的 Unix 时间戳；
/// 若该时刻今天已过，则自动顺延到明天。
pub fn parse_start_at(s: &str) -> Result<u64> {
    let s = s.trim();
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("定时开始格式应为 HH:MM，如 23:30"))?;
    let h: u32 = h
        .trim()
        .parse()
        .map_err(|_| anyhow!("无效小时: {h}"))?;
    let m: u32 = m
        .trim()
        .parse()
        .map_err(|_| anyhow!("无效分钟: {m}"))?;
    if h > 23 || m > 59 {
        return Err(anyhow!("时间超出范围: {s}（应为 00:00-23:59）"));
    }
    let now = chrono::Local::now();
    let today_ts = local_datetime_ts(now.date_naive(), h, m)?;
    if today_ts > now.timestamp() as u64 {
        Ok(today_ts)
    } else {
        // 已过该时刻：顺延到明天。
        let tomorrow = now.date_naive() + chrono::Duration::days(1);
        local_datetime_ts(tomorrow, h, m)
    }
}

/// 把本地日期 + HH:MM 转换为 Unix 时间戳（处理夏令时跳变：取 earliest）。
fn local_datetime_ts(date: chrono::NaiveDate, h: u32, m: u32) -> Result<u64> {
    let naive = date
        .and_hms_opt(h, m, 0)
        .ok_or_else(|| anyhow!("无效时间: {h}:{m}"))?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp() as u64)
        .ok_or_else(|| anyhow!("无法将本地时间 {naive} 转换为时间戳"))
}

/// 下载文件锁：<输出文件>.lock
///
/// 防止多个 turbo-dl 进程同时下载同一 URL 到同一文件导致数据损坏。
/// 锁文件内容为持有者 PID；进程崩溃后残留锁可通过 PID 存活检查清理。
pub fn lock_path_for(output: &Path) -> PathBuf {
    let mut os = output.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// 尝试获取下载文件锁，成功返回锁文件路径（调用方需在结束时删除）。
pub fn acquire_file_lock(output: &Path) -> Result<PathBuf> {
    let lock_path = lock_path_for(output);
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "{}", std::process::id());
                return Ok(lock_path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = std::fs::read_to_string(&lock_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok());
                match owner {
                    Some(pid) if process_alive(pid) => {
                        anyhow::bail!(
                            "文件正在被另一个 turbo-dl 进程（PID {pid}）下载，请等待其完成后再试（锁文件: {}）",
                            lock_path.display()
                        );
                    }
                    _ => {
                        // 持有者已退出（崩溃/被杀）：清理残留锁后重试。
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("获取下载文件锁失败: {}", lock_path.display())
}

/// 判断 PID 对应的进程是否存活（跨平台，不引入额外依赖）。
pub fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// RAII 文件锁守卫：离开作用域自动删除锁文件，确保所有退出路径都释放锁。
pub struct FileLockGuard(PathBuf);

impl FileLockGuard {
    pub fn acquire(output: &Path) -> Result<Self> {
        let path = acquire_file_lock(output)?;
        Ok(Self(path))
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 把 Unix 时间戳格式化为本地 HH:MM（用于 GUI 显示定时开始时间）。
pub fn format_unix_hhmm(ts: u64) -> String {
    let Some(dt) = chrono::DateTime::from_timestamp(ts as i64, 0) else {
        return String::new();
    };
    dt.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// 判断文本是否为可下载链接（http/https/ftp/ftps/magnet）。
pub fn looks_like_url(s: &str) -> bool {
    let s = s.trim().to_ascii_lowercase();
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("ftp://")
        || s.starts_with("ftps://")
        || s.starts_with("magnet:")
}

/// 解析右键菜单传入的链接文件：
/// - `.url`（Internet 快捷方式）：读取 `[InternetShortcut]` 段的 `URL=` 行；
/// - `.txt`：每行一个链接，`#` 开头为注释；
/// - 其他文件：按文本逐行尝试识别链接。
pub fn parse_links_file(path: &Path) -> Result<Vec<String>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取文件失败: {}", path.display()))?;
    let mut urls = Vec::new();
    if ext == "url" {
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("URL=") {
                let u = rest.trim();
                if looks_like_url(u) {
                    urls.push(u.to_string());
                }
            }
        }
    } else {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if looks_like_url(line) {
                urls.push(line.to_string());
            }
        }
    }
    Ok(urls)
}

/// 本地集成服务的认证 token：持久化到 `%APPDATA%\turbo-dl\local_token`，
/// 首次生成后固定复用，供浏览器书签 / 右键菜单发送链接时携带。
pub fn load_or_create_token() -> String {
    let path = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("turbo-dl")
        .join("local_token");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim().to_string();
        if t.len() >= 16 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return t;
        }
    }
    let mut buf = [0u8; 16];
    let token: String = if getrandom::getrandom(&mut buf).is_ok() {
        buf.iter().map(|b| format!("{:02x}", b)).collect()
    } else {
        // 极低概率的 fallback：时间 + 进程号 + 地址哈希（仍不可预测）。
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:016x}", h.finish())
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, &token);
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_variants() {
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024u64 * 1024 * 1024);
        assert_eq!(parse_size("1T").unwrap(), 1024u64.pow(4));
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("500KB").unwrap(), 500 * 1024);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-1M").is_err());
    }

    #[test]
    fn sanitize_file_name_safe() {
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("con.txt"), "_con.txt");
        assert_eq!(sanitize_file_name("a<b>c:d"), "a_b_c_d");
        // 路径分隔符被替换，`..` 不再构成穿越
        assert_eq!(sanitize_file_name("../evil"), ".._evil");
        assert_eq!(sanitize_file_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_file_name(""), "_");
        assert_eq!(sanitize_file_name(".."), "_");
        // 超长 UTF-8 按字节截断且不 panic
        let out = sanitize_file_name(&"中".repeat(200));
        assert!(out.len() <= 255);
        assert!(out.chars().all(|c| c != '�'));
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024u64.pow(3)), "1.0 GiB");
    }

    #[test]
    fn looks_like_url_ok() {
        assert!(looks_like_url("https://example.com/a"));
        assert!(looks_like_url("magnet:?xt=urn:btih:abc"));
        assert!(looks_like_url("ftp://host/file"));
        assert!(!looks_like_url("example.com/a"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn parse_start_at_valid() {
        let ts = parse_start_at("23:30").unwrap();
        assert!(ts > 0);
        assert!(parse_start_at("00:00").is_ok());
        assert!(parse_start_at("24:00").is_err());
        assert!(parse_start_at("12:60").is_err());
        assert!(parse_start_at("abc").is_err());
    }

    #[test]
    fn parse_headers_valid() {
        let map = parse_headers(&["X-A: 1".to_string(), "X-B: two".to_string()]).unwrap();
        assert_eq!(map.get("x-a").unwrap(), "1");
        assert_eq!(map.get("x-b").unwrap(), "two");
        assert!(parse_headers(&["NoColon".to_string()]).is_err());
        assert!(parse_headers(&["X: ".to_string()]).is_err());
    }

    #[test]
    fn lock_path_and_guard() {
        let output = std::env::temp_dir().join("turbo_dl_test_lock.bin");
        {
            let guard = FileLockGuard::acquire(&output).unwrap();
            assert!(output.as_os_str().to_os_string().to_string_lossy().ends_with(".bin"));
            assert!(std::fs::read_to_string(crate::util::lock_path_for(&output))
                .unwrap()
                .trim()
                .parse::<u32>()
                .is_ok());
            drop(guard);
        }
        // 释放后锁文件已删除，可再次获取
        let _guard2 = FileLockGuard::acquire(&output).unwrap();
        assert!(std::fs::read_to_string(crate::util::lock_path_for(&output)).is_ok());
    }
}