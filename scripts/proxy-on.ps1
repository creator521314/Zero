# scripts/proxy-on.ps1
# 开启 git 的 HTTP/HTTPS 代理（127.0.0.1:7890）
# 用于通过本地代理软件加速 git push / pull / clone。
#
# 用法（在项目根目录的 PowerShell 中）：
#   .\scripts\proxy-on.ps1
#
# 若被 PowerShell 执行策略阻止，请改用：
#   scripts\proxy-on.bat

$ProxyHost = '127.0.0.1'
$ProxyPort = '7890'
$ProxyUrl  = "http://${ProxyHost}:${ProxyPort}"

Write-Host '=== Git 代理配置 ===' -ForegroundColor Cyan

git config --global http.proxy  $ProxyUrl
if ($LASTEXITCODE -ne 0) { Write-Host '[错误] 设置 http.proxy 失败' -ForegroundColor Red; exit 1 }

git config --global https.proxy $ProxyUrl
if ($LASTEXITCODE -ne 0) { Write-Host '[错误] 设置 https.proxy 失败' -ForegroundColor Red; exit 1 }

Write-Host ''
Write-Host '已设置：'
git config --global --get http.proxy
git config --global --get https.proxy
Write-Host ''
Write-Host '提示：如需关闭，运行 scripts\proxy-off.bat' -ForegroundColor Yellow
