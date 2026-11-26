# scripts/memory-clear.ps1
# 删除 Zero 的长期记忆文件（测试阶段专用清理脚本）：
#   - 项目级：<项目根>\.zero\memory.jsonl
#   - 用户级：~\.config\zero\memory.jsonl
#
# 用法：
#   .\scripts\memory-clear.ps1              # 交互确认后删除 项目级 + 用户级
#   .\scripts\memory-clear.ps1 -Yes         # 跳过确认
#   .\scripts\memory-clear.ps1 -Scope project
#   .\scripts\memory-clear.ps1 -Scope user
#   .\scripts\memory-clear.ps1 -Backup      # 删除前备份为 <文件>.bak-<时间戳>

param(
    [ValidateSet("all", "project", "user")]
    [string]$Scope = "all",
    [switch]$Yes,
    [switch]$Backup
)

$ProjectRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$ProjectFile = Join-Path $ProjectRoot ".zero\memory.jsonl"
$UserFile    = Join-Path $HOME ".config\zero\memory.jsonl"

function Remove-MemoryFile {
    param([string]$Label, [string]$Path)
    if (-not (Test-Path $Path)) {
        Write-Host "[$Label] 不存在，跳过：$Path"
        return
    }
    $lines = (Get-Content $Path | Measure-Object -Line).Lines
    Write-Host "[$Label] $Path"
    Write-Host "  当前记录数：$lines"
    if ($Backup) {
        $ts = Get-Date -Format "yyyyMMdd-HHmmss"
        Move-Item $Path "$Path.bak-$ts"
        Write-Host "  已备份为：$Path.bak-$ts"
    } else {
        Remove-Item $Path -Force
        Write-Host "  已删除"
    }
}

Write-Host "=== Zero 记忆清理 ==="
Write-Host "范围：$Scope（project=项目级 / user=用户级 / all=两者）"
Write-Host "注意：物理删除；下次对话记忆从零开始。"
Write-Host ""

if (-not $Yes) {
    $answer = Read-Host "确认删除？[y/N]"
    if ($answer -notmatch '^(y|Y|yes|YES)$') {
        Write-Host "已取消。"
        exit 0
    }
}

switch ($Scope) {
    "project" { Remove-MemoryFile "项目级" $ProjectFile }
    "user"    { Remove-MemoryFile "用户级" $UserFile }
    "all" {
        Remove-MemoryFile "项目级" $ProjectFile
        Write-Host ""
        Remove-MemoryFile "用户级" $UserFile
    }
}

Write-Host ""
Write-Host "完成。"
