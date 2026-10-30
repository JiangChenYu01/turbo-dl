# turbo-dl 右键菜单安装脚本（当前用户级，无需管理员权限）
# 用法: 右键“使用 PowerShell 运行”，或：
#   powershell -ExecutionPolicy Bypass -File .\install-context-menu.ps1
$ErrorActionPreference = "Stop"

$exe = Join-Path $PSScriptRoot "target\release\turbo-dl.exe"
if (-not (Test-Path $exe)) {
    $exe = Join-Path $PSScriptRoot "target\debug\turbo-dl.exe"
}
if (-not (Test-Path $exe)) {
    Write-Host "未找到 turbo-dl.exe，请先构建：cargo build --release" -ForegroundColor Red
    exit 1
}
$exe = (Resolve-Path $exe).Path

# 当前用户级注册表根：HKCU\Software\Classes（资源管理器与 HKCR 合并视图一致）
$root = "Registry::HKEY_CURRENT_USER\Software\Classes"

function Add-Menu([string]$progId, [string]$menuName, [string]$displayName) {
    $key = "$root\$progId\shell\$menuName"
    New-Item -Path $key -Force | Out-Null
    Set-ItemProperty -Path $key -Name "(Default)" -Value $displayName
    Set-ItemProperty -Path $key -Name "Icon" -Value "`"$exe`",0"
    $cmdKey = "$key\command"
    New-Item -Path $cmdKey -Force | Out-Null
    Set-ItemProperty -Path $cmdKey -Name "(Default)" -Value "`"$exe`" --send `"%1`""
    Write-Host "已添加: $displayName  [$progId]" -ForegroundColor Green
}

try {
    Add-Menu "InternetShortcut" "TurboDLDownload" "使用 turbo-dl 下载"
    Add-Menu "txtfile" "TurboDLBatch" "使用 turbo-dl 批量下载链接"
    Write-Host ""
    Write-Host "右键菜单安装完成。现在可以：" -ForegroundColor Green
    Write-Host "  • 在 .url 网页快捷方式上右键 → 使用 turbo-dl 下载"
    Write-Host "  • 在 .txt 链接列表上右键 → 使用 turbo-dl 批量下载链接"
} catch {
    Write-Host ""
    Write-Host "安装失败: $_" -ForegroundColor Red
    exit 1
}
