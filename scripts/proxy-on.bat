@echo off
REM ============================================================
REM  scripts/proxy-on.bat
REM  开启 git 的 HTTP/HTTPS 代理（127.0.0.1:7890）
REM  用于通过本地代理软件加速 git push / pull / clone。
REM
REM  用法（项目根目录执行）：
REM    scripts\proxy-on.bat
REM ============================================================

setlocal

set "PROXY_HOST=127.0.0.1"
set "PROXY_PORT=7890"
set "PROXY_URL=http://%PROXY_HOST%:%PROXY_PORT%"

echo === Git 代理配置 ===
git config --global http.proxy  %PROXY_URL%
if errorlevel 1 goto :fail

git config --global https.proxy %PROXY_URL%
if errorlevel 1 goto :fail

echo.
echo 已设置：
git config --global --get http.proxy
git config --global --get https.proxy
echo.
echo 提示：如需关闭，运行 scripts\proxy-off.bat
endlocal
exit /b 0

:fail
echo [错误] 设置代理失败
endlocal
exit /b 1
