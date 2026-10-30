//! 图形界面（egui/eframe）。

use crate::app::{shutdown_computer, HistoryEntry, HistoryStore, TaskManager, TaskStatus};
use crate::util::{format_bytes, format_unix_hhmm, looks_like_url};
use anyhow::{anyhow, Result};
use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run_gui(manager: Arc<TaskManager>) -> Result<()> {
    // 启动本地端口集成服务：浏览器/书签可一键「发送到 turbo-dl」。
    start_local_server(manager.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "turbo-dl —— 超越 IDM 的下载器",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(TurboApp::new(manager.clone())))
        }),
    )
    .map_err(|e| anyhow!("GUI 启动失败: {e}"))
}

/// 本地集成服务：监听 127.0.0.1 上的端口（默认 31877，被占用时自动递增），
/// GET /add?url=... 即可添加下载任务。多实例可各自占用不同端口。
fn start_local_server(manager: Arc<TaskManager>) {
    let rt = manager.runtime();
    let token = crate::util::load_or_create_token();
    rt.spawn(async move {
        // 从 31877 开始尝试，最多 10 个端口；端口冲突时自动 +1，避免新实例集成服务失效。
        let mut port = 31_877u16;
        let listener = loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => break l,
                Err(_) if port < 31_877 + 10 => {
                    port += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("⚠ 本地集成端口 {port} 启动失败（不影响下载）: {e}");
                    return;
                }
            }
        };
        tracing::info!("ℹ 本地集成服务已启动：http://127.0.0.1:{port}/add?token={token}&url=...");
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let mgr = manager.clone();
            let token = token.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let mut request_line = String::new();
                let mut reader = BufReader::new(&mut sock);
                if reader.read_line(&mut request_line).await.is_err() {
                    return;
                }
                // 读取剩余请求头直到真正的空行（CRLF），消耗掉请求头避免干扰响应。
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_err() {
                        break;
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let body = handle_local_request(request_line.trim(), &token, &mgr);
                let resp = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.status,
                    body.content_type,
                    body.text.len(),
                    body.text
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
}

/// 本地请求响应。
struct LocalResponse {
    status: &'static str,
    content_type: &'static str,
    text: String,
}

impl LocalResponse {
    fn text(status: &'static str, text: String) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            text,
        }
    }

    fn json(status: &'static str, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            text: value.to_string(),
        }
    }
}

/// 本地集成 REST API（全部需要 `?token=...` 认证）：
/// - `GET  /add?url=...`          添加下载任务（文本响应，兼容书签/右键菜单）
/// - `GET  /tasks`                任务列表（JSON）
/// - `GET  /history`             下载历史（JSON）
/// - `POST /pause/{id}`          暂停任务
/// - `POST /resume/{id}`         恢复任务
/// - `POST /cancel/{id}`         取消任务
/// - `POST /remove/{id}`         移除任务
fn handle_local_request(request_line: &str, token: &str, manager: &TaskManager) -> LocalResponse {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_ascii_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let parsed = match url::Url::parse(&format!("http://127.0.0.1{path}")) {
        Ok(u) => u,
        Err(_) => {
            return LocalResponse::json("400 Bad Request", &serde_json::json!({"error": "无效请求路径"}))
        }
    };
    let query: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let supplied_token = query
        .iter()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if supplied_token != token {
        return LocalResponse::json("403 Forbidden", &serde_json::json!({
            "error": "缺少或错误的 token，已拒绝（本地集成服务需携带 ?token=...）"
        }));
    }
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();

    match (method.as_str(), segs.as_slice()) {
        ("GET", ["add"]) => {
            let target = query
                .iter()
                .find(|(k, _)| k == "url")
                .map(|(_, v)| v.clone())
                .filter(|v| !v.trim().is_empty());
            match target {
                Some(u) => match manager.add_download(u.trim().to_string()) {
                    Ok(id) => LocalResponse::text("200 OK", format!("OK 任务 #{id} 已添加：{u}")),
                    Err(e) => {
                        LocalResponse::text("400 Bad Request", format!("添加任务失败: {e:#}"))
                    }
                },
                _ => LocalResponse::text(
                    "400 Bad Request",
                    "缺少 url 参数。用法: http://127.0.0.1:31877/add?token=...&url=https://example.com/file.zip"
                        .to_string(),
                ),
            }
        }
        ("GET", ["tasks"]) => {
            let tasks_arc = manager.tasks();
            let list = tasks_arc.lock().unwrap();
            let tasks: Vec<_> = list
                .iter()
                .map(|t| {
                    let t = t.lock().unwrap();
                    serde_json::json!({
                        "id": t.id,
                        "url": t.url,
                        "protocol": t.protocol.name(),
                        "file_name": t.file_name,
                        "total": t.total,
                        "downloaded": t.downloaded,
                        "speed": t.speed,
                        "status": format!("{:?}", t.status),
                        "error": t.error,
                        "start_at": t.start_at,
                        "output_path": t.output_path.display().to_string(),
                    })
                })
                .collect();
            LocalResponse::json("200 OK", &serde_json::json!({ "tasks": tasks }))
        }
        ("GET", ["history"]) => {
            let history = HistoryStore::load();
            LocalResponse::json("200 OK", &serde_json::json!({ "history": history }))
        }
        ("POST", ["pause", id]) => {
            let id: u64 = id.parse().unwrap_or(0);
            manager.set_pause(id, true);
            LocalResponse::json(
                "200 OK",
                &serde_json::json!({ "ok": true, "id": id, "action": "pause" }),
            )
        }
        ("POST", ["resume", id]) => {
            let id: u64 = id.parse().unwrap_or(0);
            match manager.resume(id) {
                Ok(()) => LocalResponse::json(
                    "200 OK",
                    &serde_json::json!({ "ok": true, "id": id, "action": "resume" }),
                ),
                Err(e) => LocalResponse::json(
                    "400 Bad Request",
                    &serde_json::json!({ "ok": false, "id": id, "error": format!("{e:#}") }),
                ),
            }
        }
        ("POST", ["cancel", id]) => {
            let id: u64 = id.parse().unwrap_or(0);
            manager.cancel(id);
            LocalResponse::json(
                "200 OK",
                &serde_json::json!({ "ok": true, "id": id, "action": "cancel" }),
            )
        }
        ("POST", ["remove", id]) => {
            let id: u64 = id.parse().unwrap_or(0);
            manager.remove_task(id);
            LocalResponse::json(
                "200 OK",
                &serde_json::json!({ "ok": true, "id": id, "action": "remove" }),
            )
        }
        _ => LocalResponse::json(
            "404 Not Found",
            &serde_json::json!({ "error": "未知接口", "method": method, "path": path }),
        ),
    }
}

/// 加载中文字体：优先系统常见字体，Linux 下额外用 fc-match 定位，避免硬编码路径失效。
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(path) = find_cjk_font() {
        if let Ok(data) = std::fs::read(&path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(data));
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .get_mut(&egui::FontFamily::Monospace)
                .unwrap()
                .insert(0, "cjk".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

/// 在常见平台路径中查找中文字体；Linux 下额外尝试 fc-match。
fn find_cjk_font() -> Option<std::path::PathBuf> {
    let candidates = [
        // Windows
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyhl.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
        "C:\\Windows\\Fonts\\msyhbd.ttc",
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        // Linux（覆盖多发行版路径）
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/truetype/arphic/uming.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    ];
    for path in candidates {
        let p = std::path::PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    #[cfg(target_os = "linux")]
    {
        for family in ["Noto Sans CJK SC", "WenQuanYi Micro Hei", "sans-serif"] {
            if let Ok(out) = std::process::Command::new("fc-match")
                .args(["-f", "%{file}", family])
                .output()
            {
                if out.status.success() {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !path.is_empty() && path != "/dev/null" {
                        let p = std::path::PathBuf::from(path);
                        if p.is_file() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

struct TurboApp {
    manager: Arc<TaskManager>,
    url_input: String,
    last_snapshot: Instant,
    last_bytes: HashMap<u64, u64>,
    /// 剪贴板监视（复制即提示）。
    clipboard: Option<arboard::Clipboard>,
    last_clipboard_text: String,
    last_clipboard_poll: Instant,
    clipboard_hint: Option<(String, Instant)>,
    /// 完成后关机是否已触发（防止重复执行 shutdown）。
    shutdown_triggered: bool,
    /// 添加任务的结果提示：(文本, 是否错误)。
    status_hint: Option<(String, bool)>,
    /// 任务搜索关键字（匹配文件名/URL）。
    filter_text: String,
    /// 任务状态过滤。
    filter_status: FilterStatus,
    /// 当前视图：任务列表 / 下载历史。
    view: AppView,
    /// 下载历史缓存。
    history: Vec<HistoryEntry>,
    /// 历史是否需要重新加载。
    history_dirty: bool,
    /// 完成/失败通知：(文本, 时间)。
    notification: Option<(String, Instant)>,
    /// 代理测试结果（异步任务更新）。
    proxy_test: Arc<Mutex<Option<String>>>,
}

/// 主界面视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppView {
    Tasks,
    History,
}

/// 任务列表状态过滤条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterStatus {
    All,
    Downloading,
    Completed,
    Failed,
    Paused,
}

impl FilterStatus {
    fn label(&self) -> &'static str {
        match self {
            FilterStatus::All => "全部",
            FilterStatus::Downloading => "下载中",
            FilterStatus::Completed => "已完成",
            FilterStatus::Failed => "失败/取消",
            FilterStatus::Paused => "已暂停",
        }
    }

    fn matches(&self, status: TaskStatus) -> bool {
        match self {
            FilterStatus::All => true,
            FilterStatus::Downloading => {
                matches!(status, TaskStatus::Downloading | TaskStatus::Queued)
            }
            FilterStatus::Completed => status == TaskStatus::Completed,
            FilterStatus::Failed => {
                matches!(status, TaskStatus::Failed | TaskStatus::Cancelled)
            }
            FilterStatus::Paused => status == TaskStatus::Paused,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Resume(u64),
    Pause(u64),
    Cancel(u64),
    Remove(u64),
    Open(u64),
}

impl TurboApp {
    fn new(manager: Arc<TaskManager>) -> Self {
        let mut clipboard = arboard::Clipboard::new().ok();
        let last_clipboard_text = clipboard
            .as_mut()
            .and_then(|c| c.get_text().ok())
            .unwrap_or_default();
        Self {
            manager,
            url_input: String::new(),
            last_snapshot: Instant::now(),
            last_bytes: HashMap::new(),
            clipboard,
            last_clipboard_text,
            last_clipboard_poll: Instant::now(),
            clipboard_hint: None,
            shutdown_triggered: false,
            status_hint: None,
            filter_text: String::new(),
            filter_status: FilterStatus::All,
            view: AppView::Tasks,
            history: HistoryStore::load(),
            history_dirty: false,
            notification: None,
            proxy_test: Arc::new(Mutex::new(None)),
        }
    }

    /// 提交输入框中的 URL 添加任务；成功/失败都在界面给出明确提示。
    fn submit_url(&mut self) {
        let url = self.url_input.trim().to_string();
        if url.is_empty() {
            return;
        }
        match self.manager.add_download(url) {
            Ok(id) => {
                self.url_input.clear();
                self.status_hint = Some((format!("✅ 任务 #{id} 已添加"), false));
            }
            Err(e) => {
                self.status_hint = Some((format!("❌ 添加失败: {e:#}"), true));
            }
        }
    }

    fn update_speeds(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_snapshot).as_secs_f64().max(0.1);
        self.last_snapshot = now;
        let list = self.manager.tasks().lock().unwrap().clone();
        for task in list {
            let mut t = task.lock().unwrap();
            let prev = self.last_bytes.get(&t.id).copied().unwrap_or(t.downloaded);
            t.speed = ((t.downloaded.saturating_sub(prev)) as f64 / dt) as u64;
            self.last_bytes.insert(t.id, t.downloaded);
        }
    }

    fn summary(&self) -> (usize, usize, usize, u64) {
        let list = self.manager.tasks().lock().unwrap().clone();
        let mut active = 0;
        let mut done = 0;
        let mut total = 0;
        let mut speed = 0u64;
        for task in list {
            let t = task.lock().unwrap();
            match t.status {
                TaskStatus::Downloading | TaskStatus::Queued => active += 1,
                TaskStatus::Completed => done += 1,
                _ => {}
            }
            total += 1;
            speed += t.speed;
        }
        (active, done, total, speed)
    }

    /// 每 2 秒轮询一次剪贴板：发现新的 URL 自动填入输入框并提示。
    fn poll_clipboard(&mut self) {
        if self.last_clipboard_poll.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_clipboard_poll = Instant::now();
        let Some(cb) = &mut self.clipboard else {
            return;
        };
        let Ok(text) = cb.get_text() else {
            return;
        };
        let text = text.trim().to_string();
        if text.is_empty() || text == self.last_clipboard_text {
            return;
        }
        self.last_clipboard_text = text.clone();
        if looks_like_url(&text) {
            self.url_input = text;
            self.clipboard_hint = Some((
                "📋 已从剪贴板获取链接，点击「添加下载」开始".to_string(),
                Instant::now(),
            ));
        }
    }

    /// 全部任务【成功完成】后，若勾选了「完成后关机」则触发系统关机（仅一次）。
    /// 存在失败/取消/进行中的任务时不关机，避免误触发。
    fn maybe_shutdown(&mut self) {
        if self.shutdown_triggered {
            return;
        }
        let list = self.manager.tasks().lock().unwrap().clone();
        if list.is_empty() {
            return;
        }
        let all_completed = list
            .iter()
            .all(|t| t.lock().unwrap().status == TaskStatus::Completed);
        if !all_completed {
            return;
        }
        let shutdown = self.manager.settings.lock().unwrap().shutdown_after;
        if shutdown {
            self.shutdown_triggered = true;
            shutdown_computer();
        }
    }

    fn render_add_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("🚀 turbo-dl");
            ui.separator();
            ui.label("多协议 · 动态分段抢占带宽 · 断点续传");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.view == AppView::History, "🕘 历史")
                    .clicked()
                {
                    self.view = AppView::History;
                    self.history_dirty = true;
                }
                if ui
                    .selectable_label(self.view == AppView::Tasks, "📥 任务")
                    .clicked()
                {
                    self.view = AppView::Tasks;
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            // 输入框占剩余宽度，按钮固定 90px 在右侧，保证按钮始终可见不被挤出。
            let avail = ui.available_width();
            let edit_width = (avail - 110.0).max(120.0);
            let edit = ui.add_sized(
                [edit_width, 24.0],
                egui::TextEdit::singleline(&mut self.url_input)
                    .hint_text("粘贴 http(s):// / ftp:// / magnet:?xt=... 链接"),
            );
            let add = ui.add_enabled(
                !self.url_input.trim().is_empty(),
                egui::Button::new("⬇ 添加下载").min_size(egui::vec2(90.0, 24.0)),
            );
            // 回车（输入框聚焦时）与点击按钮都直接提交。
            let enter_pressed = edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter_pressed || add.clicked() {
                self.submit_url();
            }
        });
        // 添加结果提示（成功/失败都给用户明确反馈）。
        if let Some((text, is_err)) = &self.status_hint {
            let color = if *is_err {
                if ui.visuals().dark_mode {
                    egui::Color32::from_rgb(240, 110, 110)
                } else {
                    egui::Color32::from_rgb(180, 20, 20)
                }
            } else if ui.visuals().dark_mode {
                egui::Color32::from_rgb(80, 200, 120)
            } else {
                egui::Color32::from_rgb(0, 130, 60)
            };
            ui.colored_label(color, text);
            ui.add_space(2.0);
        }
        ui.add_space(6.0);
        if let Some((hint, at)) = &self.clipboard_hint {
            if at.elapsed() < Duration::from_secs(6) {
                ui.colored_label(
                    if ui.visuals().dark_mode {
                        egui::Color32::from_rgb(120, 200, 255)
                    } else {
                        egui::Color32::from_rgb(0, 90, 180)
                    },
                    hint,
                );
            }
        }
        // 设置栏
        let mut jobs_to_update: Option<usize> = None;
        ui.horizontal_wrapped(|ui| {
            let mut settings = self.manager.settings.lock().unwrap();
            ui.label("保存到:");
            let mut dir_str = settings.download_dir.display().to_string();
            if ui
                .add(egui::TextEdit::singleline(&mut dir_str).desired_width(180.0))
                .changed()
            {
                // 清空时回退系统下载目录，避免后续任务输出到当前目录。
                let trimmed = dir_str.trim();
                settings.download_dir = if trimmed.is_empty() {
                    dirs::download_dir().unwrap_or_else(|| PathBuf::from("."))
                } else {
                    PathBuf::from(trimmed)
                };
            }
            if ui.button("浏览").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    settings.download_dir = dir;
                }
            }
            ui.separator();
            ui.label("连接数:");
            ui.add(egui::DragValue::new(&mut settings.connections).range(1..=64));
            ui.separator();
            ui.label("并发任务:");
            if ui
                .add(egui::DragValue::new(&mut settings.jobs).range(1..=16))
                .changed()
            {
                jobs_to_update = Some(settings.jobs);
            }
            ui.separator();
            ui.label("限速/连接:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.rate_limit_text)
                    .hint_text("如 1M / 500K")
                    .desired_width(70.0),
            );
            ui.separator();
            ui.label("总限速:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.global_rate_limit_text)
                    .hint_text("如 5M / 1M")
                    .desired_width(70.0),
            );
            ui.separator();
            ui.label("代理:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.proxy_text)
                    .hint_text("http://... 或 socks5://...")
                    .desired_width(120.0),
            );
            if ui.button("测试").clicked() {
                let rt = self.manager.runtime();
                let result = self.proxy_test.clone();
                let proxy_text = settings.proxy_text.trim().to_string();
                *self.proxy_test.lock().unwrap() = Some("测试中...".to_string());
                rt.spawn(async move {
                    let r = test_proxy(&proxy_text).await;
                    *result.lock().unwrap() = Some(r);
                });
            }
            if let Some(r) = self.proxy_test.lock().unwrap().as_ref() {
                ui.colored_label(egui::Color32::from_rgb(120, 190, 255), r);
            }
            ui.separator();
            ui.label("认证:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.auth_text)
                    .hint_text("user:pass")
                    .desired_width(90.0),
            );
            ui.separator();
            ui.label("定时开始:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.start_at_text)
                    .hint_text("HH:MM，留空立即")
                    .desired_width(70.0),
            );
            ui.checkbox(&mut settings.shutdown_after, "完成后关机");
            ui.checkbox(&mut settings.open_after_complete, "完成后打开");
            ui.checkbox(&mut settings.auto_extract, "自动解压");
            ui.checkbox(&mut settings.delete_archive_after_extract, "解压后删包");
            ui.checkbox(&mut settings.auto_classify, "自动分类");
            ui.label("分类到:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.classify_root)
                    .hint_text("留空=同目录")
                    .desired_width(100.0),
            );
            ui.checkbox(&mut settings.checksum_sidecar, "旁车校验");
            ui.separator();
            ui.label("Referer:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.referer_text)
                    .hint_text("https://...")
                    .desired_width(120.0),
            );
            ui.separator();
            ui.label("Cookie:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.cookie_text)
                    .hint_text("name=value; ...")
                    .desired_width(150.0),
            );
        });
        // 锁已释放：同步并发任务数到 TaskManager 信号量（立即生效）。
        if let Some(jobs) = jobs_to_update {
            self.manager.update_jobs(jobs);
        }
        ui.add_space(6.0);
        ui.separator();
    }

    fn render_tasks(&mut self, ui: &mut egui::Ui) {
        let raw_list = self.manager.tasks().lock().unwrap().clone();
        // 过滤栏：搜索关键字 + 状态过滤。
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.filter_text)
                    .hint_text("搜索文件名/URL...")
                    .desired_width(200.0),
            );
            ui.separator();
            egui::ComboBox::from_id_salt("filter_status")
                .selected_text(self.filter_status.label())
                .show_ui(ui, |ui| {
                    for f in [
                        FilterStatus::All,
                        FilterStatus::Downloading,
                        FilterStatus::Completed,
                        FilterStatus::Failed,
                        FilterStatus::Paused,
                    ] {
                        ui.selectable_value(&mut self.filter_status, f, f.label());
                    }
                });
            if !self.filter_text.is_empty() || self.filter_status != FilterStatus::All {
                if ui.button("✕ 清除").clicked() {
                    self.filter_text.clear();
                    self.filter_status = FilterStatus::All;
                }
            }
            ui.separator();
            if ui.button("📤 导出链接").clicked() {
                let list = self.manager.tasks().lock().unwrap().clone();
                let lines: Vec<String> = list
                    .iter()
                    .map(|t| t.lock().unwrap().url.clone())
                    .filter(|u| !u.is_empty())
                    .collect();
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("txt", &["txt"])
                    .save_file()
                {
                    let _ = std::fs::write(path, lines.join("\n"));
                }
            }
        });
        ui.add_space(2.0);

        if raw_list.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("暂无任务。在上方输入框粘贴下载链接，回车或点击「添加下载」。");
            });
            return;
        }

        // 按关键字与状态过滤。
        let list: Vec<_> = raw_list
            .into_iter()
            .filter(|task| {
                let t = task.lock().unwrap();
                if !self.filter_text.is_empty() {
                    let kw = self.filter_text.to_lowercase();
                    if !t.file_name.to_lowercase().contains(&kw)
                        && !t.url.to_lowercase().contains(&kw)
                    {
                        return false;
                    }
                }
                self.filter_status.matches(t.status)
            })
            .collect();

        if list.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("没有匹配的任务。");
            });
            return;
        }
        let mut actions: Vec<Action> = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for task in list {
                    let t = task.lock().unwrap();
                    let group = ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("[{}]", t.protocol.name()));
                            ui.strong(truncate(&t.file_name, 60));
                            ui.label(format!("({})", format_bytes(t.total)));
                            ui.separator();
                            // 状态色随深/浅主题自适应，保证 WCAG AA 对比度。
                            let dark = ui.visuals().dark_mode;
                            let status_color = match t.status {
                                TaskStatus::Completed => {
                                    if dark {
                                        egui::Color32::from_rgb(80, 200, 120)
                                    } else {
                                        egui::Color32::from_rgb(0, 130, 60)
                                    }
                                }
                                TaskStatus::Failed | TaskStatus::Cancelled => {
                                    if dark {
                                        egui::Color32::from_rgb(240, 110, 110)
                                    } else {
                                        egui::Color32::from_rgb(180, 20, 20)
                                    }
                                }
                                TaskStatus::Paused => {
                                    if dark {
                                        egui::Color32::from_rgb(230, 190, 80)
                                    } else {
                                        egui::Color32::from_rgb(150, 100, 0)
                                    }
                                }
                                _ => {
                                    if dark {
                                        egui::Color32::from_gray(160)
                                    } else {
                                        egui::Color32::from_gray(80)
                                    }
                                }
                            };
                            ui.colored_label(status_color, t.status.label());
                        });
                        ui.add_space(2.0);
                        // 定时开始 / 关键尾部块就绪提示。
                        ui.horizontal(|ui| {
                            if let Some(at) = t.start_at {
                                if t.status == TaskStatus::Queued {
                                    ui.label(format!("⏰ 将于 {} 开始", format_unix_hhmm(at)));
                                }
                            }
                            if t.tail_ready
                                && t.status == TaskStatus::Downloading
                                && is_previewable(&t.file_name)
                            {
                                let c = if ui.visuals().dark_mode {
                                    egui::Color32::from_rgb(80, 200, 120)
                                } else {
                                    egui::Color32::from_rgb(0, 130, 60)
                                };
                                ui.colored_label(c, "🔑 尾部就绪，可预览");
                            }
                        });
                        ui.horizontal(|ui| {
                            let progress = t.progress();
                            let bar_width = (ui.available_width() - 300.0).max(120.0);
                            ui.add(
                                egui::ProgressBar::new(progress)
                                    .desired_width(bar_width)
                                    .show_percentage(),
                            );
                            ui.label(format!(
                                "{} / {}",
                                format_bytes(t.downloaded),
                                format_bytes(t.total)
                            ));
                            ui.label(format!("{}/s", format_bytes(t.speed)));
                            if let Some(err) = &t.error {
                                if t.status == TaskStatus::Failed {
                                    let err_color = if ui.visuals().dark_mode {
                                        egui::Color32::from_rgb(240, 110, 110)
                                    } else {
                                        egui::Color32::from_rgb(180, 20, 20)
                                    };
                                    ui.colored_label(err_color, err);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            let url_color = if ui.visuals().dark_mode {
                                egui::Color32::from_gray(180)
                            } else {
                                egui::Color32::from_gray(90)
                            };
                            ui.label(
                                egui::RichText::new(truncate(&t.url, 90))
                                    .color(url_color),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("🗑 移除").clicked() {
                                    actions.push(Action::Remove(t.id));
                                }
                                if !t.output_path.as_os_str().is_empty()
                                    && t.status == TaskStatus::Completed
                                {
                                    if ui.button("📂 打开目录").clicked() {
                                        actions.push(Action::Open(t.id));
                                    }
                                }
                                if t.status == TaskStatus::Paused {
                                    if ui.button("▶ 继续").clicked() {
                                        actions.push(Action::Resume(t.id));
                                    }
                                } else if t.status == TaskStatus::Downloading
                                    || t.status == TaskStatus::Queued
                                {
                                    if ui.button("⏸ 暂停").clicked() {
                                        actions.push(Action::Pause(t.id));
                                    }
                                    if ui.button("✕ 取消").clicked() {
                                        actions.push(Action::Cancel(t.id));
                                    }
                                }
                            });
                        });
                    });
                // 右键上下文菜单：暂停/继续/取消/打开目录/复制链接等。
                let id = t.id;
                let status = t.status;
                let has_output = !t.output_path.as_os_str().is_empty();
                let url = t.url.clone();
                let file_name = t.file_name.clone();
                let mgr = self.manager.clone();
                group.response.context_menu(move |ui| {
                    match status {
                        TaskStatus::Downloading | TaskStatus::Queued => {
                            if ui.button("⏸ 暂停").clicked() {
                                mgr.set_pause(id, true);
                                ui.close_menu();
                            }
                            if ui.button("✕ 取消").clicked() {
                                mgr.cancel(id);
                                ui.close_menu();
                            }
                        }
                        TaskStatus::Paused => {
                            if ui.button("▶ 继续").clicked() {
                                let _ = mgr.resume(id);
                                ui.close_menu();
                            }
                        }
                        TaskStatus::Failed | TaskStatus::Cancelled => {
                            if ui.button("🔄 重新下载").clicked() {
                                let _ = mgr.resume(id);
                                ui.close_menu();
                            }
                        }
                        _ => {}
                    }
                    if has_output && status == TaskStatus::Completed {
                        if ui.button("📂 打开目录").clicked() {
                            mgr.open_folder(id);
                            ui.close_menu();
                        }
                    }
                    if ui.button("📋 复制链接").clicked() {
                        let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(url.clone()));
                        ui.close_menu();
                    }
                    if ui.button("📝 复制文件名").clicked() {
                        let _ =
                            arboard::Clipboard::new().and_then(|mut c| c.set_text(file_name.clone()));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("🗑 移除任务").clicked() {
                        mgr.remove_task(id);
                        ui.close_menu();
                    }
                });
                }
            });
        // 释放所有锁后执行动作，避免死锁。
        for action in actions {
            match action {
                Action::Resume(id) => {
                    let _ = self.manager.resume(id);
                }
                Action::Pause(id) => self.manager.set_pause(id, true),
                Action::Cancel(id) => self.manager.cancel(id),
                Action::Remove(id) => self.manager.remove_task(id),
                Action::Open(id) => self.manager.open_folder(id),
            }
        }
    }

    /// 渲染下载历史视图。
    fn render_history(&mut self, ui: &mut egui::Ui) {
        if self.history.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("暂无下载历史。任务进入完成/失败/取消状态后会自动记录到这里。");
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.label(format!("🕘 下载历史（{} 条）", self.history.len()));
            if ui.button("🗑 清除历史").clicked() {
                HistoryStore::clear();
                self.history.clear();
            }
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut to_remove: Option<usize> = None;
                for (idx, h) in self.history.iter().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("[{}]", h.protocol));
                            ui.strong(truncate(&h.file_name, 50));
                            ui.label(format!("({})", format_bytes(h.total)));
                            ui.separator();
                            ui.colored_label(
                                history_status_color(&h.status, ui.visuals().dark_mode),
                                history_status_label(&h.status),
                            );
                        });
                        ui.horizontal(|ui| {
                            let url_color = if ui.visuals().dark_mode {
                                egui::Color32::from_gray(180)
                            } else {
                                egui::Color32::from_gray(90)
                            };
                            ui.label(egui::RichText::new(truncate(&h.url, 80)).color(url_color));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("🗑 删除").clicked() {
                                        to_remove = Some(idx);
                                    }
                                    if ui.button("📋 复制链接").clicked() {
                                        let _ = arboard::Clipboard::new()
                                            .and_then(|mut c| c.set_text(h.url.clone()));
                                    }
                                    if !h.output_path.is_empty() {
                                        if ui.button("📂 打开目录").clicked() {
                                            let p = std::path::PathBuf::from(&h.output_path);
                                            // BT 任务的 output_path 是保存目录，直接打开；文件任务打开其父目录。
                                            if p.is_dir() {
                                                crate::app::open_in_explorer(&p);
                                            } else if let Some(dir) = p.parent() {
                                                crate::app::open_in_explorer(dir);
                                            }
                                        }
                                    }
                                    if h.status == "Completed" && !h.output_path.is_empty() {
                                        if ui.button("🔄 重新下载").clicked() {
                                            let _ = self.manager.add_download(h.url.clone());
                                            self.view = AppView::Tasks;
                                        }
                                    }
                                },
                            );
                        });
                    });
                }
                if let Some(idx) = to_remove {
                    self.history.remove(idx);
                    HistoryStore::save(&self.history);
                }
            });
    }

    /// 任务进入终态时显示界面通知并播放提示音。
    fn notify(&mut self, e: &HistoryEntry) {
        let msg = match e.status.as_str() {
            "Completed" => format!("✅ 下载完成: {}", e.file_name),
            "Failed" => format!("❌ 下载失败: {}", e.file_name),
            "Cancelled" => format!("⏹ 已取消: {}", e.file_name),
            _ => format!("任务结束: {}", e.file_name),
        };
        self.notification = Some((msg, Instant::now()));
        // 系统提示音（失败用低音，成功用高音）。
        let freq = if e.status == "Completed" { 880 } else { 300 };
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("[console]::beep({freq},200)"),
            ])
            .spawn();
    }

    /// 处理拖入的 URL 列表文件（.txt）或文本链接（text/uri-list / text/plain）。
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let mut urls: Vec<String> = Vec::new();
        for file in dropped {
            // .txt 文件按行解析（# 开头为注释）。
            if let Some(path) = &file.path {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext == "txt" {
                    if let Ok(content) = std::fs::read_to_string(path) {
                        for line in content.lines() {
                            let line = line.trim();
                            if !line.is_empty() && !line.starts_with('#') {
                                urls.push(line.to_string());
                            }
                        }
                    }
                    continue;
                }
            }
            // 文本型拖放（如从浏览器拖出链接）直接解析内容。
            if file.mime == "text/uri-list" || file.mime.starts_with("text/plain") {
                if let Some(bytes) = &file.bytes {
                    let text = String::from_utf8_lossy(bytes).into_owned();
                    for line in text.lines() {
                        let line = line.trim();
                        if line.starts_with("http://")
                            || line.starts_with("https://")
                            || line.starts_with("ftp://")
                            || line.starts_with("ftps://")
                            || line.starts_with("magnet:")
                        {
                            urls.push(line.to_string());
                        }
                    }
                }
            }
        }
        for url in urls {
            let _ = self.manager.add_download(url);
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    // 按字符截断（而非字节），避免在中文/emoji 的多字节边界 panic。
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

/// 历史记录状态（英文枚举字符串）转中文显示。
fn history_status_label(s: &str) -> &'static str {
    match s {
        "Completed" => "已完成",
        "Failed" => "失败",
        "Cancelled" => "已取消",
        "Paused" => "已暂停",
        "Downloading" => "下载中",
        _ => "未知",
    }
}

/// 历史记录状态颜色（深/浅主题自适应）。
fn history_status_color(s: &str, dark: bool) -> egui::Color32 {
    match s {
        "Completed" => {
            if dark {
                egui::Color32::from_rgb(80, 200, 120)
            } else {
                egui::Color32::from_rgb(0, 130, 60)
            }
        }
        "Failed" | "Cancelled" => {
            if dark {
                egui::Color32::from_rgb(240, 110, 110)
            } else {
                egui::Color32::from_rgb(180, 20, 20)
            }
        }
        "Paused" => {
            if dark {
                egui::Color32::from_rgb(230, 190, 80)
            } else {
                egui::Color32::from_rgb(150, 100, 0)
            }
        }
        _ => {
            if dark {
                egui::Color32::from_gray(160)
            } else {
                egui::Color32::from_gray(80)
            }
        }
    }
}

/// 通过代理访问 gstatic 204 端点，检测代理是否可用。
async fn test_proxy(proxy: &str) -> String {
    if proxy.trim().is_empty() {
        return "未配置代理".to_string();
    }
    let proxy = match reqwest::Proxy::all(proxy) {
        Ok(p) => p,
        Err(e) => return format!("❌ 无效代理: {e}"),
    };
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .connect_timeout(Duration::from_secs(5))
        .build();
    match client {
        Ok(c) => match c.get("http://www.gstatic.com/generate_204").send().await {
            Ok(r) if r.status().is_success() => "✅ 代理可用".to_string(),
            Ok(r) => format!("⚠️ 代理响应异常: HTTP {}", r.status()),
            Err(e) => format!("❌ 代理连接失败: {e}"),
        },
        Err(e) => format!("❌ 构建代理客户端失败: {e}"),
    }
}

/// 扩展名可边下边预览的文件类型（尾部就绪后即可打开/预览）。
fn is_previewable(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    [
        ".zip", ".7z", ".rar", ".tar", ".gz", ".bz2", ".xz",
        ".mp4", ".mkv", ".mov", ".webm", ".avi", ".flv", ".ts",
        ".mp3", ".flac", ".wav", ".m4a", ".aac",
        ".iso", ".epub", ".pdf",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

impl eframe::App for TurboApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            self.render_add_bar(ui);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // 完成/失败通知（短暂显示）。
            if let Some((text, at)) = &self.notification {
                if at.elapsed() < Duration::from_secs(4) {
                    ui.colored_label(egui::Color32::from_rgb(80, 200, 120), text);
                } else {
                    self.notification = None;
                }
            }
            match self.view {
                AppView::Tasks => self.render_tasks(ui),
                AppView::History => self.render_history(ui),
            }
        });

        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.add_space(2.0);
            let (active, done, total, speed) = self.summary();
            ui.horizontal(|ui| {
                ui.label(format!(
                    "活动: {}   已完成: {}   任务总数: {}   全局速度: {}/s",
                    active,
                    done,
                    total,
                    format_bytes(speed)
                ));
            });
            ui.add_space(2.0);
        });

        // 拖拽添加任务：.txt URL 列表 / 浏览器拖出的文本链接。
        self.handle_dropped_files(ctx);

        // 周期把终态任务写入下载历史，并触发完成通知。
        let new_entries = self.manager.checkpoint_history();
        for e in new_entries {
            self.notify(&e);
        }

        // 消费下载后处理队列（自动解压/分类）：异步执行，不阻塞 UI。
        for (p, s) in self.manager.drain_postprocess() {
            let rt = self.manager.runtime();
            rt.spawn(async move {
                crate::postprocess::run(p, s).await;
            });
        }

        // 历史视图需要刷新时重新加载。
        if self.history_dirty {
            self.history = HistoryStore::load();
            self.history_dirty = false;
        }

        // 剪贴板监视：复制即提示。
        self.poll_clipboard();

        self.update_speeds();
        // 全部完成后按设置关机。
        self.maybe_shutdown();
        // 持续刷新进度。
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}