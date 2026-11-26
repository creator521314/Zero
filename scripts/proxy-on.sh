#!/usr/bin/env bash
# scripts/proxy-on.sh
# 开启 git 的 HTTP/HTTPS 代理（127.0.0.1:7890）
# 用于通过本地代理软件加速 git push / pull / clone。
#
# 用法（项目根目录执行）：
#   ./scripts/proxy-on.sh
#   # 或
#   bash scripts/proxy-on.sh

set -euo pipefail

PROXY_HOST="${PROXY_HOST:-127.0.0.1}"
PROXY_PORT="${PROXY_PORT:-7890}"
PROXY_URL="http://${PROXY_HOST}:${PROXY_PORT}"

echo '=== Git 代理配置 ==='

git config --global http.proxy "$PROXY_URL"
git config --global https.proxy "$PROXY_URL"

echo
echo '已设置：'
git config --global --get http.proxy
git config --global --get https.proxy
echo
echo '提示：如需关闭，运行 scripts/proxy-off.sh'
echo '提示：端口不是 7890？PROXY_PORT=xxxx ./scripts/proxy-on.sh 即可，无需改脚本。'
