@echo off
REM ============================================================
REM  scripts/proxy-off.bat
REM  关闭 git 的 HTTP/HTTPS 代理，恢复直连。
REM
REM  用法（项目根目录执行）：
REM    scripts\proxy-off.bat
REM ============================================================

setlocal

echo === 关闭 Git 代理 ===
git config --global --unset http.proxy
git config --global --unset https.proxy

echo.
echo 当前代理配置：
git config --global --get http.proxy
git config --global --get https.proxy
if errorlevel 1 echo （已清除）
echo.
echo 已恢复直连
endlocal
exit /b 0
