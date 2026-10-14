# 代理脚本（scripts/）

通过本地代理软件（默认 `127.0.0.1:7890`）加速 `git push / pull / clone` 等网络操作。

## 快速使用

在 **项目根目录** 下：

| 平台 | 开启 | 关闭 |
|---|---|---|
| PowerShell | `.\scripts\proxy-on.ps1`  | `.\scripts\proxy-off.ps1`  |
| cmd / 双击 | `scripts\proxy-on.bat`    | `scripts\proxy-off.bat`    |

> 推荐用 `.bat` 版本——不依赖 PowerShell 执行策略。

## 修改代理地址

如代理端口变更，编辑四个脚本中的：

```
PROXY_HOST = 127.0.0.1
PROXY_PORT = 7890
```

## 它做了什么

- `proxy-on`：`git config --global http.proxy  http://127.0.0.1:7890` + `https.proxy`
- `proxy-off`：`git config --global --unset http.proxy` + `https.proxy`

仅影响 git 的 HTTP(S) 协议；不影响 SSH 协议（SSH 用 `~/.ssh/config` 或 `~/.gitconfig` 的 `[core]` 别处配置）。

## 故障排查

| 现象 | 处理 |
|---|---|
| `git push` 仍超时 | 先 `scripts\proxy-off.bat` 再 `scripts\proxy-on.bat` 一次 |
| 代理软件端口变化 | 编辑脚本中的 `PROXY_HOST` / `PROXY_PORT` |
| 想临时绕过代理 | `git push ... --config http.proxy=` |
