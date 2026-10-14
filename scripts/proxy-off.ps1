# scripts/proxy-off.ps1
# 关闭 git 的 HTTP/HTTPS 代理，恢复直连。
#
# 用法（在项目根目录的 PowerShell 中）：
#   .\scripts\proxy-off.ps1
#
# 若被 PowerShell 执行策略阻止，请改用：
#   scripts\proxy-off.bat

Write-Host '=== 关闭 Git 代理 ===' -ForegroundColor Cyan

git config --global --unset http.proxy
git config --global --unset https.proxy

Write-Host ''
Write-Host '当前代理配置：'
$http  = git config --global --get http.proxy
$https = git config --global --get https.proxy
if ($http)  { Write-Host "  http.proxy  = $http" }
else        { Write-Host '  http.proxy  = （已清除）' }
if ($https) { Write-Host "  https.proxy = $https" }
else        { Write-Host '  https.proxy = （已清除）' }
Write-Host ''
Write-Host '已恢复直连' -ForegroundColor Green
