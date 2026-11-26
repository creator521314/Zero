#!/usr/bin/env bash
# scripts/proxy-off.sh
# 关闭 git 的 HTTP/HTTPS 代理，恢复直连。
#
# 用法（项目根目录执行）：
#   ./scripts/proxy-off.sh
#   # 或
#   bash scripts/proxy-off.sh

set -euo pipefail

echo '=== 关闭 Git 代理 ==='

git config --global --unset http.proxy || true
git config --global --unset https.proxy || true

echo
echo '当前代理配置：'
http=$(git config --global --get http.proxy || true)
https=$(git config --global --get https.proxy || true)
if [ -n "$http" ]; then
    echo "  http.proxy  = $http"
else
    echo '  http.proxy  = （已清除）'
fi
if [ -n "$https" ]; then
    echo "  https.proxy = $https"
else
    echo '  https.proxy = （已清除）'
fi
echo
echo '已恢复直连'
