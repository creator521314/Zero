# 代理脚本（scripts/）

通过本地代理软件（默认 `127.0.0.1:7890`）加速 `git push / pull / clone` 等网络操作。

## 快速使用

在 **项目根目录** 下：

| 平台 | 开启 | 关闭 |
|---|---|---|
| Linux / macOS | `./scripts/proxy-on.sh`  | `./scripts/proxy-off.sh`  |
| PowerShell | `.\scripts\proxy-on.ps1`  | `.\scripts\proxy-off.ps1`  |
| cmd / 双击 | `scripts\proxy-on.bat`    | `scripts\proxy-off.bat`    |

> Linux/macOS 首次使用若提示无执行权限：`chmod +x scripts/*.sh`。
> Windows 推荐用 `.bat` 版本——不依赖 PowerShell 执行策略。

## 修改代理地址

Linux / macOS 版支持环境变量临时覆盖（无需改脚本）：

```bash
PROXY_PORT=10809 ./scripts/proxy-on.sh
PROXY_HOST=192.168.1.10 PROXY_PORT=7890 ./scripts/proxy-on.sh
```

Windows 版如代理端口变更，编辑脚本中的：

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
| `git push` 仍超时 | 先 `proxy-off` 再 `proxy-on` 一次 |
| 代理软件端口变化 | 见「修改代理地址」 |
| 想临时绕过代理 | `git push ... --config http.proxy=` |
| Linux 提示 `Permission denied` | `chmod +x scripts/*.sh` |
