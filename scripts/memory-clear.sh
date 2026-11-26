#!/usr/bin/env bash
# scripts/memory-clear.sh
# 删除 Zero 的长期记忆文件（测试阶段专用清理脚本）：
#   - 项目级：<项目根>/.zero/memory.jsonl
#   - 用户级：~/.config/zero/memory.jsonl
#
# 记忆为 append-only JSONL，逻辑删除（Archived）只是打标记；
# 本脚本直接移除物理文件，让记忆系统回到全新状态。
#
# 用法：
#   ./scripts/memory-clear.sh            # 交互确认后删除 项目级 + 用户级
#   ./scripts/memory-clear.sh -y         # 跳过确认
#   ./scripts/memory-clear.sh project    # 只删项目级
#   ./scripts/memory-clear.sh user       # 只删用户级
#   ./scripts/memory-clear.sh --backup   # 删除前备份为 <文件>.bak-<时间戳>

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_FILE="${PROJECT_ROOT}/.zero/memory.jsonl"
USER_FILE="${HOME}/.config/zero/memory.jsonl"

ASSUME_YES=0
BACKUP=0
SCOPE="all"
for arg in "$@"; do
    case "$arg" in
        -y|--yes)    ASSUME_YES=1 ;;
        --backup)    BACKUP=1 ;;
        project|user|all) SCOPE="$arg" ;;
        *)
            echo "未知参数：$arg"
            echo "用法：memory-clear.sh [-y] [--backup] [project|user|all]"
            exit 1
            ;;
    esac
done

count_lines() {
    # 统计有效条目数（含历史操作记录）；文件不存在输出 -。
    if [ -f "$1" ]; then
        grep -c . "$1" || true
    else
        echo "-"
    fi
}

backup_file() {
    local ts
    ts="$(date +%Y%m%d-%H%M%S)"
    mv "$1" "${1}.bak-${ts}"
    echo "  已备份为：${1}.bak-${ts}"
}

remove_file() {
    local label="$1" path="$2"
    if [ ! -f "$path" ]; then
        echo "[$label] 不存在，跳过：$path"
        return
    fi
    echo "[$label] ${path}"
    echo "  当前记录数：$(count_lines "$path")"
    if [ "$BACKUP" -eq 1 ]; then
        backup_file "$path"
    else
        rm -f "$path"
        echo "  已删除"
    fi
}

echo '=== Zero 记忆清理 ==='
echo "范围：${SCOPE}（project=项目级 / user=用户级 / all=两者）"
echo "注意：物理删除，不影响已生效任务；下次对话记忆从零开始。"
echo

if [ "$ASSUME_YES" -ne 1 ]; then
    printf '确认删除？[y/N] '
    read -r answer
    case "$answer" in
        y|Y|yes|YES) ;;
        *) echo '已取消。'; exit 0 ;;
    esac
fi

case "$SCOPE" in
    project) remove_file '项目级' "$PROJECT_FILE" ;;
    user)    remove_file '用户级' "$USER_FILE" ;;
    all)
        remove_file '项目级' "$PROJECT_FILE"
        echo
        remove_file '用户级' "$USER_FILE"
        ;;
esac

echo
echo '完成。'
