# turbo-dl 右键菜单卸载脚本（当前用户级）
$ErrorActionPreference = "SilentlyContinue"
$root = "Registry::HKEY_CURRENT_USER\Software\Classes"
Remove-Item -Path "$root\InternetShortcut\shell\TurboDLDownload" -Recurse -Force
Remove-Item -Path "$root\txtfile\shell\TurboDLBatch" -Recurse -Force
Write-Host "右键菜单已卸载。"
