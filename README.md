# turbo-dl — 超越 IDM 的多协议下载器

用 Rust 编写的高性能下载器：**图形界面 + 命令行双模式**，内置 **work-stealing 动态分段抢占带宽**，支持 **HTTP / FTP / BitTorrent** 多协议，断点续传、限速、校验、代理一应俱全。

## 特性

- 🖥 **图形界面**（egui 原生）：粘贴链接即可下载，任务列表实时显示进度/速度/状态，支持暂停、继续、取消、打开目录
- ⚡ **work-stealing 带宽抢占**：文件切成小块放入全局队列，所有连接共享领取——**快连接自动多干、慢连接自然少干**，不存在"等最慢段"的浪费（IDM 固定分段的致命弱点）
- 🔄 **断点续传**：chunk 位图精确记录每个数据块的完成状态，任意时刻中断（关程序/断网/暂停）都能无缝续传；ETag 变化自动重新下载
- 📦 **多协议**：HTTP/HTTPS（多连接并发）、FTP/FTPS（REST 续传）、磁力链接/BitTorrent（librqbit 引擎）
- 🐢 **精确限速**：按连接限速，实测误差 <1%
- 🚦 **全局总限速**：跨任务/跨连接共享限速器，所有下载合计不超设定值（如 `--global-rate-limit 5M`）
- 🔍 **任务搜索与过滤**：GUI 按文件名/URL 关键字搜索，按状态（下载中/已完成/失败/已暂停）过滤
- 🕘 **下载历史**：完成任务自动持久化到本地（`%APPDATA%\turbo-dl\history.json`），GUI 历史视图支持重新下载/打开目录/复制链接/清除
- ⚡ **完成后自动打开**：下载结束用系统默认程序打开文件（`--open-after` / GUI 勾选）
- 📤 **导出链接**：GUI 一键把当前任务 URL 列表导出为 `.txt`
- 🔐 **HTTP/FTP 认证**：`-u admin:123456`（Basic 认证，FTP URL 内嵌凭据优先）
- 🔔 **完成通知**：GUI 任务完成/失败即时提示 + 系统提示音
- 🧭 **代理一键检测**：GUI 代理输入框旁点「测试」即验证代理可用性
- 🔀 **Git 仓库克隆**：自动识别 `git://`、`ssh://git@`、`.git` 结尾 URL，以及任意主机上返回 git 智能协议响应的仓库页（GitHub/GitLab/Gitea 等），**用 libgit2 原生克隆，不调用外部 git**
- 🕸 **网站镜像**：`--mirror URL` 纯 Rust 递归克隆同域页面与静态资源（深度≤3、≤300 文件）
- 📦 **GitHub Release 解析**：输入 `.../releases/latest` 或 `.../releases/tag/x` 自动解析全部资产直链并下载第一个；`--release-info URL` 只列出所有直链
- 🗜 **自动解压**：完成后自动解压 zip/tar/tar.gz（`--extract`，可选解压后删除压缩包）
- 🗂 **文件自动分类**：按类型移入「视频/音频/压缩包/文档/图片」子目录（`--classify [--classify-dir DIR]`）
- 📺 **M3U8/HLS 流媒体**：输入 `.m3u8` 自动并发下载分片并合并为 `.ts`（纯 Rust，支持 **AES-128-CBC 加密流**，自动解析 `#EXT-X-KEY` 密钥与 IV）
- 🔗 **Referer/Cookie 防盗链**：`--referer URL` / `--cookie STRING`（GUI 设置栏直接填，所有请求自动带头）
- ✅ **旁车校验和**：`--checksum-sidecar` 自动尝试 `<url>.sha256/.sha1/.md5` 并校验
- 🌱 **.torrent 种子文件**：支持本地 `.torrent` 路径或 http 链接添加 BT 任务
- 📰 **RSS/Atom 订阅下载**：`--rss URL` 解析订阅源并自动批量下载所有条目（优先 enclosure）；`--rss-list URL` 只列出条目
- ⏱ **RSS 定时轮询**：`--rss URL --rss-interval 30` 定时检查订阅源，自动下载新条目（已下载的不会重复）
- 💾 **任务自动恢复**：进行中任务实时持久化，重启程序后自动恢复并断点续传
- 🔒 **校验和**：MD5 / SHA1 / SHA256
- 🧭 **代理支持**：HTTP / HTTPS / SOCKS5
- 📋 **批量队列**：CLI 支持 URL 队列文件并发下载
- 🧩 **智能分段**：小文件自动减少块数；服务器不支持 Range 自动退化为单连接
- 🔁 **失败重试**：指数退避自动重试；失败块自动放回队列由其他连接接管
- 💪 **弱网自愈**：分块读取 30s 空闲超时；超时/断连/5xx 归类为瞬时错误，指数退避自动重试（超过 `--retries` 后任务失败，避免无限空转）
- 🌊 **未知大小流式下载**：无 Content-Length / chunked 服务器也能完整下载（单连接顺序写盘）
- 🧹 **文件名安全清洗**：替换 Windows 非法字符、规避保留设备名、按 UTF-8 边界截断，杜绝路径穿越
- 🎨 **主题自适应**：状态色随深/浅主题调整对比度（WCAG AA）；支持拖入 `.txt` URL 列表或文本链接批量建任务
- ⚙️ **动态块大小自适应**：按带宽自动合并/拆分 Range 请求（256K~8M），高速链路减少请求开销，弱网降低往返占比
- 🔑 **关键尾部块优先**：首个连接先下载文件尾部，ZIP/MP4 等「边下边预览」，可用时刻大幅提前
- 📋 **剪贴板监视**：复制链接后自动填入输入框，零切换添加任务
- 🔌 **本地端口集成**：`http://127.0.0.1:31877/add?token=...&url=...` 一键「发送到 turbo-dl」（浏览器书签/小工具，需携带 token 防跨站触发）
- 🖱 **资源管理器右键菜单**：`.url` 网页快捷方式一键下载、`.txt` 链接列表批量发送（当前用户级注册，免管理员；自动携带 token）
- ⏰ **定时开始 + 完成后关机**：CLI `--start-at HH:MM` 或 GUI 设置定时；全部完成后自动关机省电
- 🔒 **同文件互斥**：同一输出文件同时只允许一个进程下载（文件锁 + 引擎代次），杜绝双开导致数据损坏

## 使用

### 图形界面

```bash
# 直接运行打开 GUI（无参数）
turbo-dl

# 打开 GUI 并自动开始下载
turbo-dl --gui "https://example.com/big.zip"
turbo-dl --gui "magnet:?xt=urn:btih:..."
```

在界面顶部粘贴 http(s)://、ftp:// 或 magnet: 链接，回车即可添加任务。
设置栏可改保存目录、连接数、并发任务数、限速、代理，还可以设置**定时开始**（HH:MM）与**完成后关机**。

**剪贴板监视**：复制任意下载链接后，输入框会自动填入并高亮提示，点击「添加下载」即可。

**浏览器一键添加**：本地集成服务需要携带持久化 token（防止任意网页跨站触发下载）。
启动 turbo-dl 时会在终端打印带 token 的完整地址（token 保存在 `%APPDATA%\turbo-dl\local_token`）。
书签小工具示例（把 `TOKEN` 换成你的 token）：
`javascript:location.href='http://127.0.0.1:31877/add?token=TOKEN&url='+encodeURIComponent(location.href)`
（或直接访问 `http://127.0.0.1:31877/add?token=TOKEN&url=...`），即可把当前页面链接发送到 turbo-dl。
右键菜单发送链接时**自动携带 token**，无需手动配置。

**浏览器扩展**：`extension/` 目录提供 Chrome/Edge（MV3）扩展源码——
- 在任意链接上右键 →「发送到 turbo-dl 下载」
- 点击工具栏图标可查看任务列表、暂停/继续/取消/移除任务、直接粘贴链接添加
- 首次使用：`chrome://extensions` → 开启「开发者模式」→「加载已解压的扩展程序」→ 选择本项目 `extension/` 目录；
  然后点击扩展图标，填入 token 和端口（默认 31877）并保存。

**本地控制 API**（`http://127.0.0.1:31877`，需 `?token=...` 认证）：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/add?url=...` | 添加下载任务（文本响应） |
| GET | `/tasks` | 任务列表（JSON） |
| GET | `/history` | 下载历史（JSON） |
| POST | `/pause/{id}` | 暂停任务 |
| POST | `/resume/{id}` | 恢复任务 |
| POST | `/cancel/{id}` | 取消任务 |
| POST | `/remove/{id}` | 移除任务 |

示例（PowerShell / 脚本可调用）：
```bash
curl "http://127.0.0.1:31877/tasks?token=TOKEN"
curl -X POST "http://127.0.0.1:31877/pause/3?token=TOKEN"
```

**资源管理器右键菜单**：安装后，在 `.url` 网页快捷方式上右键可「使用 turbo-dl 下载」，
在 `.txt` 链接列表上右键可「使用 turbo-dl 批量下载链接」（每行一个链接，`#` 开头为注释）。
点击后如果 turbo-dl 已在运行，链接直接发送到任务列表；否则自动打开图形界面并加入任务。

```powershell
# 安装（当前用户级，无需管理员）
powershell -ExecutionPolicy Bypass -File .\install-context-menu.ps1

# 卸载
powershell -ExecutionPolicy Bypass -File .\uninstall-context-menu.ps1
```

**拖拽添加**：把含 URL 列表的 `.txt` 文件（每行一个链接，`#` 开头为注释）拖入窗口，
即可批量建任务；从浏览器拖出的文本链接也会被自动识别。

### 命令行

```bash
# 多连接下载（默认 8 连接）
turbo-dl -d out -c 16 "https://example.com/big-file.zip"

# 限速 + 校验
turbo-dl -d out --rate-limit 1M --checksum sha256:HEX "https://example.com/file"

# 全局总限速（多个任务合计不超过 2M/s）
turbo-dl -d out -j 3 --global-rate-limit 2M "https://example.com/a.zip" "https://example.com/b.zip"

# 断点续传：中断后重新运行同一命令即可
turbo-dl -d out "https://example.com/big-file.zip"

# FTP 下载（支持 REST 续传）
turbo-dl -d out "ftp://user:pass@host/file.zip"

# 磁力链接下载
turbo-dl -d out "magnet:?xt=urn:btih:..."

# 批量队列（每行一个 URL，# 开头为注释）
turbo-dl -j 5 --queue urls.txt -d out

# 定时开始（HH:MM，已过顺延到明天）
turbo-dl --start-at 23:30 -d out "https://example.com/big-file.zip"

# 全部任务完成后自动关机（Windows: shutdown /s /t 60）
turbo-dl --shutdown -d out "https://example.com/big-file.zip"

# 下载完成后用默认程序打开文件
turbo-dl --open-after -d out "https://example.com/report.pdf"

# 需要认证的资源（Basic Auth）
turbo-dl -d out -u admin:123456 "https://example.com/private/file.zip"

# 克隆 Git 仓库（自动识别仓库页 / .git / git://）
turbo-dl -d code "https://github.com/user/repo"
turbo-dl -d code "git://git.example.com/project.git"

# 网站镜像（递归克隆同域页面与静态资源）
turbo-dl --mirror -d site "https://example.com/"

# GitHub Release：自动解析资产直链并下载第一个
turbo-dl -d out "https://github.com/aria2/aria2/releases/latest"

# 只列出 release 的所有资产直链
turbo-dl --release-info "https://github.com/aria2/aria2/releases/latest"

# 下载后自动解压 + 按类型分类
turbo-dl --extract --classify -d out "https://example.com/files.zip"

# 防盗链（Referer + Cookie）
turbo-dl --referer "https://example.com/page" --cookie "sid=abc" -d out "https://example.com/private.mp4"

# M3U8 流媒体下载（合并为 .ts）
turbo-dl -d out "https://example.com/live/index.m3u8"

# 自动旁车校验和
turbo-dl --checksum-sidecar -d out "https://example.com/file.bin"

# 本地 .torrent 文件
turbo-dl -d out "C:\downloads\ubuntu.torrent"

# RSS 订阅自动下载（解析 feed，批量下载所有条目链接）
turbo-dl --rss -d out "https://example.com/feed.xml"

# RSS 定时轮询：每 30 分钟检查一次，自动下载新条目
turbo-dl --rss -d out --rss-interval 30 "https://example.com/feed.xml"

# 只列出 RSS 条目
turbo-dl --rss-list "https://example.com/feed.xml"
```

## 命令行参数

| 参数 | 说明 | 默认 |
| --- | --- | --- |
| `--gui` | 强制启动图形界面 | 无参数时自动 |
| `-d, --dir` | 输出目录 | `.` |
| `-o, --output` | 输出文件名（仅单任务） | 自动推断 |
| `-c, --connections` | 每任务连接数 | `8` |
| `--min-segment-size` | 数据块大小（`1M`/`500K`） | `1M` |
| `-j, --jobs` | 并发任务数 | `3` |
| `--rate-limit` | 每连接限速（`500K`/`2M`） | 不限 |
| `--global-rate-limit` | 全局总限速（所有任务合计，`5M`/`1M`） | 不限 |
| `-p, --proxy` | 代理 URL | 无 |
| `-u, --auth` | HTTP/FTP 认证（`user:pass`） | 无 |
| `-H, --header` | 自定义请求头（可重复） | 无 |
| `--user-agent` | User-Agent | `turbo-dl/<版本>` |
| `--retries` | 每块失败重试次数 | `5` |
| `--timeout` | 连接超时（秒） | `30` |
| `--checksum` | `algo:HEX` 校验和 | 无 |
| `--queue` | URL 队列文件 | 无 |
| `--force-restart` | 强制重新下载 | 关 |
| `--no-resume` | 禁用断点续传 | 关 |
| `-q, --quiet` | 安静模式 | 关 |
| `--start-at` | 定时开始（`HH:MM`，已过顺延到明天） | 立即 |
| `--shutdown` | 全部任务完成后自动关机 | 关 |
| `--open-after` | 下载完成后用系统默认程序打开文件 | 关 |
| `--mirror` | 网站镜像模式（递归克隆同域页面与资源） | 关 |
| `--extract` | 完成后自动解压 zip/tar/tar.gz | 关 |
| `--delete-archive` | 解压成功后删除原压缩包 | 关 |
| `--classify` | 完成后按类型自动分类 | 关 |
| `--classify-dir` | 分类根目录（默认=文件所在目录） | 无 |
| `--referer` | Referer 防盗链 | 无 |
| `--cookie` | Cookie 防盗链 | 无 |
| `--checksum-sidecar` | 自动尝试旁车校验和并校验 | 关 |
| `--rss` | RSS/Atom 订阅源，自动批量下载条目 | 无 |
| `--rss-interval` | RSS 轮询间隔（分钟，与 `--rss` 配合，自动下载新条目） | 无 |
| `--rss-list` | 只列出 RSS 条目（标题+链接） | 无 |

## work-stealing 动态分段原理

传统下载器（含 IDM）把文件平均切成 N 段，每个连接负责一段。问题是：
网络抖动/服务器限速导致某一段极慢时，其他连接早早完成却只能空等——整体带宽被最慢段拖死。

turbo-dl 的做法：

1. 文件按 `--min-segment-size` 切成 M 个小块（1MB 级）
2. 全局维护一个原子块队列指针 `next_chunk`
3. 每个连接下载完当前块后，立即 `fetch_add` 领取下一块
4. 快连接会领取更多块、慢连接自动少做；某连接失败时，失败块被放回队列，由其他连接接力
5. 断点状态用**位图**记录每个块是否完成，任何时刻中断都能精确续传，且续传时连接数可随意变化

效果：总下载时间只取决于「总字节数 / 总可用带宽」，而不是最慢连接的耗时。

## 断点文件格式

`.rdmeta`（与下载文件同目录，如 `big.zip.rdmeta`）：

```json
{
  "url": "https://...",
  "file_name": "big.zip",
  "total_size": 104857600,
  "etag": "\"abc123\"",
  "chunk_size": 1048576,
  "num_chunks": 100,
  "done_bitmap_hex": "ff00ff...",
  "next_chunk": 42,
  "downloaded_bytes": 52428800
}
```

`done_bitmap_hex` 的第 i 位表示第 i 块是否完成。删除 `.rdmeta` 即放弃断点。

## 架构

```
src/
├── main.rs           入口（GUI / CLI 双模式调度；RSS 定时轮询 run_rss_polling；CLI 等待循环 wait_cli_tasks）
├── args.rs           CLI 参数
├── app.rs            任务模型、设置、TaskManager（并发限制、任务快照持久化/恢复、引擎代次防双引擎）
├── gui.rs            egui 图形界面（中文界面；本地集成服务 + token 认证）
├── engine/
│   ├── mod.rs        协议识别与任务分发（携带引擎代次）
│   ├── http.rs       HTTP 引擎（work-stealing 动态分段 + 自适应块大小 + 尾部块优先 + 文件锁 + 有限重试）
│   ├── ftp.rs        FTP 引擎（REST 续传）
│   ├── bt.rs         BitTorrent 引擎（librqbit，磁力/.torrent）
│   ├── git.rs        Git 仓库克隆（libgit2，HTTP 智能协议自动识别）
│   ├── m3u8.rs       HLS 流媒体（分片落盘 + 按序合并，内存常数级）
│   ├── mirror.rs     网站镜像（同域 BFS，≤3 层 / ≤300 文件）
│   ├── release.rs    GitHub Release 资产解析
│   └── rss.rs        RSS/Atom 解析 + 已下载条目去重（rss_seen.json）
├── postprocess.rs    下载后处理（自动解压 / 分类）
├── resume.rs         chunk 位图断点模型（.rdmeta）
├── rate.rs           每连接限速器 + 全局总限速器
└── util.rs           工具函数（容量解析、文件名清洗、跨进程文件锁、本地服务 token）
tests/
└── http_download.rs  黑盒集成测试（内嵌 Range 服务器：多连接下载 / 断点续传 / 文件锁）
```

**持久化文件**（`%APPDATA%\turbo-dl\`）：

| 文件 | 内容 |
|---|---|
| `history.json` | 已完成/失败/取消任务历史（最多 500 条） |
| `tasks.json` | 进行中任务快照，重启后自动恢复续传 |
| `rss_seen.json` | RSS 轮询已下载条目 URL，防止重复下载 |
| `local_token` | 本地集成服务认证 token（浏览器书签/右键菜单携带） |

## 日志与调试

turbo-dl 内置基于 `tracing` 的日志系统，同时输出到**控制台**和**文件**：

| 输出 | 位置 | 默认级别 | 环境变量 |
|---|---|---|---|
| 控制台（stderr） | 终端 | `info` | `RUST_LOG` |
| 文件 | `%APPDATA%\turbo-dl\logs\turbo-dl.log` | `debug` | `RUST_LOG_FILE` |

- 文件超过 5 MiB 自动轮转为 `turbo-dl.old.log`（覆盖上一次旧日志）。
- CLI 的最终结果（`✓/✗` 汇总）始终输出到 stdout，**不受日志级别影响**。
- turbo-dl 自身日志的 target 为 `turbo_dl`（模块如 `turbo_dl::app`、`turbo_dl::engine::http`）。

常用示例：

```bash
# 控制台显示详细过程（网络连接、分块领取、重试退避等）
RUST_LOG=debug turbo-dl -d out "https://example.com/big.zip"

# 只显示警告与错误
RUST_LOG=warn turbo-dl -d out "https://example.com/big.zip"

# 文件日志只记录 info 及以上（默认 debug 更详细，便于排查）
RUST_LOG_FILE=info turbo-dl -d out "https://example.com/big.zip"

# 只看 turbo-dl 自身的 debug，屏蔽 reqwest/hyper 等依赖库噪音
RUST_LOG=turbo_dl=debug,reqwest=warn hyper=warn turbo-dl -d out "https://example.com/big.zip"
```

## 构建

```bash
cargo build --release
# 产物 target/release/turbo-dl.exe（GUI + CLI 二合一）
```

> **Windows 构建提示**：BT 引擎（librqbit → reqwest 0.13 → rustls）默认会引入
> `aws-lc-rs`，其在 Windows 上的默认构建路径可能失败（`missing DEP_AWS_LC include`）。
> 官方 workaround 是启用 CMake 构建路径（需安装 CMake 与 NASM）：
>
> ```powershell
> $env:AWS_LC_SYS_CMAKE_BUILDER = "1"
> cargo build --release
> ```
>
> 或在 bash 中：`AWS_LC_SYS_CMAKE_BUILDER=1 cargo build --release`。

## 与 IDM 对比

| 能力 | IDM | turbo-dl |
| --- | --- | --- |
| 多连接分段 | ✅ 固定分段 | ✅ work-stealing 动态抢占 |
| 断点续传 | ✅ | ✅ 位图级精确续传 |
| 图形界面 | ✅ | ✅（原生 egui） |
| 命令行/无头 | ❌ | ✅ |
| FTP | ✅ | ✅（REST 续传） |
| 磁力/BT | ❌ | ✅ |
| 每连接限速 | 部分 | ✅ 精确到连接 |
| 校验和验证 | ❌ | ✅（MD5/SHA1/SHA256） |
| 跨平台 | Windows | Windows / Linux / macOS |

## License

MIT