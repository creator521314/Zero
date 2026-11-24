//! 共享的路径安全校验：确保任何读写都发生在允许的根之内。
//!
//! 沙箱有两个根：
//! - **项目根**（可选）：用户真实的项目目录，工具的读写基准；
//! - **私有工作区**：外置输出等系统数据，始终可访问。
//!
//! 历史上只有私有工作区（每任务新建的空目录），导致 Agent 看不见用户项目，
//! `file_read` 一律报「文件不存在」、`list_dir` 一律返回空。

use agent_core::tool::{ToolCtx, ToolError};
use std::path::{Component, Path, PathBuf};

/// 把用户/模型给出的路径解析到沙箱内。
///
/// 规则：
/// - 相对路径基于 `base`（项目根优先）；
/// - 结果必须位于 `roots` 中某一个之下（规范化后比较），否则拒绝；
/// - 不要求路径已存在（支持创建新文件）。
pub fn resolve_in(base: &Path, roots: &[PathBuf], raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::InvalidArgs("path is empty".into()));
    }
    let raw_path = Path::new(raw);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base.join(raw_path)
    };

    // 第一道：词法校验（不访问文件系统），快速拒绝 `..` 逃逸。
    let normalized = normalize(&candidate);
    let allowed: Vec<PathBuf> = roots.iter().map(|r| normalize(r)).collect();
    if !allowed.iter().any(|r| is_under(&normalized, r)) {
        return Err(ToolError::PathEscape(format!(
            "path escapes sandbox: {} (allowed: {})",
            normalized.display(),
            allowed
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // 第二道：真实路径校验。词法上「在沙箱内」不代表真的在沙箱内——
    // 项目根下的软链 `link -> /etc/passwd` 词法校验照样通过。
    // 把已存在的最深前缀 canonicalize（跟随后符号链接）后复核；
    // 尚不存在的后缀没有链接可逃逸，词法拼接即可。
    let real = canonicalize_existing(&normalized).map_err(|e| {
        ToolError::PathEscape(format!(
            "path resolution failed for {}: {e}",
            normalized.display()
        ))
    })?;
    let allowed_real: Vec<PathBuf> = roots
        .iter()
        .map(|r| canonicalize_existing(r))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ToolError::PathEscape(format!("sandbox root resolution failed: {e}")))?;
    if !allowed_real.iter().any(|r| is_under(&real, r)) {
        return Err(ToolError::PathEscape(format!(
            "path escapes sandbox (symlink?): {} -> {} (allowed: {})",
            normalized.display(),
            real.display(),
            allowed_real
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // 返回真实路径：后续读写都落在 canonical 位置上，且已验证在沙箱内。
    Ok(real)
}

/// 把路径解析成「真实位置」：对已存在的最深祖先做 `canonicalize`
/// （解析全部符号链接），不存在的尾部按词法拼接。
///
/// 例：`/a/link/x/y.txt`（`link -> /b`，`/b/x/y.txt` 不存在）
///   → canonicalize(`/a/link`) = `/b`，拼上 `x/y.txt` → `/b/x/y.txt`。
fn canonicalize_existing(p: &Path) -> std::io::Result<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Ok(c);
    }
    // 逐级向上找第一个真实存在的祖先。
    let mut anc = p.parent();
    while let Some(a) = anc {
        if let Ok(c) = std::fs::canonicalize(a) {
            let suffix = p.strip_prefix(a).unwrap_or(Path::new(""));
            return Ok(c.join(suffix));
        }
        anc = a.parent();
    }
    // 连根都不存在（测试里的假盘符路径、未初始化的沙箱）——退回词法路径。
    // 与修复前行为一致：后续真实 IO 时自然会因文件不存在而失败。
    Ok(p.to_path_buf())
}

/// 工具入口：按执行上下文解析（相对路径基于项目根）。
pub fn resolve_in_ctx(ctx: &ToolCtx, raw: &str) -> Result<PathBuf, ToolError> {
    resolve_in(&ctx.sandbox_root(), &ctx.allowed_roots(), raw)
}

/// 词法规范化（不访问文件系统），消解 `.`/`..`。
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 判断 `candidate` 是否位于 `root` 之内。
///
/// 比较用统一后的字符串而非 `Path::starts_with`：Windows 上盘符大小写
/// （`d:/x` 与 `D:\x`）与斜杠方向不参与路径语义，直接比组件会误判。
fn is_under(candidate: &Path, root: &Path) -> bool {
    let c = unify(candidate);
    let r = unify(root);
    if c == r {
        return true;
    }
    let r = r.trim_end_matches('/');
    !r.is_empty() && c.starts_with(r) && c.as_bytes().get(r.len()) == Some(&b'/')
}

fn unify(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(ws: &Path) -> Vec<PathBuf> {
        vec![ws.to_path_buf()]
    }

    #[test]
    fn rejects_escape() {
        let ws = Path::new("/home/u/agent/workspace/t1");
        assert!(resolve_in(ws, &roots(ws), "../../../etc/passwd").is_err());
        assert!(resolve_in(ws, &roots(ws), "/etc/passwd").is_err());
    }

    #[test]
    fn accepts_inside() {
        let ws = Path::new("/home/u/agent/workspace/t1");
        let r = resolve_in(ws, &roots(ws), "scripts/a.sh").unwrap();
        assert!(r.starts_with(ws));
        let r2 = resolve_in(ws, &roots(ws), "sub/../b.txt").unwrap();
        assert!(r2.starts_with(ws));
        assert!(r2.ends_with("b.txt"));
    }

    /// 有项目根时：相对路径落到项目根，而不是私有工作区。
    #[test]
    fn relative_paths_resolve_against_project_root() {
        let proj = PathBuf::from("/home/u/proj/zero");
        let ws = PathBuf::from("/home/u/agent/workspace/t1");
        let got = resolve_in(&proj, &[proj.clone(), ws.clone()], "README.md").unwrap();
        assert_eq!(got, proj.join("README.md"));
    }

    /// 私有工作区仍可访问：否则外置到 ctx/ 的内容读不回来。
    #[test]
    fn private_workspace_stays_reachable() {
        let proj = PathBuf::from("/home/u/proj/zero");
        let ws = PathBuf::from("/home/u/agent/workspace/t1");
        let target = ws.join("ctx/a.txt");
        let got = resolve_in(&proj, &[proj.clone(), ws], &target.to_string_lossy()).unwrap();
        // 比较 Path 而非字符串：分隔符方向不参与路径语义。
        assert_eq!(got, target);
    }

    /// 项目根之外一概拒绝（哪怕在私有工作区的兄弟目录里）。
    #[test]
    fn still_rejects_outside_both_roots() {
        let proj = PathBuf::from("/home/u/proj/zero");
        let ws = PathBuf::from("/home/u/agent/workspace/t1");
        assert!(resolve_in(&proj, &[proj.clone(), ws], "/home/u/proj/other/x").is_err());
    }

    /// Windows：盘符大小写与斜杠方向不参与语义。
    #[test]
    fn accepts_windows_drive_case_and_slash_variants() {
        let proj = PathBuf::from("D:/git_rc/Zero");
        let got = resolve_in(
            &proj,
            std::slice::from_ref(&proj),
            "d:/git_rc/Zero/README.md",
        )
        .unwrap();
        assert!(got.to_string_lossy().ends_with("README.md"));
        let got2 = resolve_in(
            &proj,
            std::slice::from_ref(&proj),
            "D:\\git_rc\\Zero\\Cargo.toml",
        )
        .unwrap();
        assert!(got2.to_string_lossy().ends_with("Cargo.toml"));
    }

    /// 符号链接指向沙箱外：词法校验放行、真实路径校验必须拦下。
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let base = std::env::temp_dir().join(format!("agent-paths-{}", std::process::id()));
        let root = base.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.txt"), "hi").unwrap();
        // 沙箱内的链接 → /etc/passwd（词法上完全在沙箱内）。
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", root.join("innocent")).unwrap();

        let r = resolve_in(&root, std::slice::from_ref(&root), "innocent");
        assert!(r.is_err(), "symlink 逃逸必须被拒: {r:?}");

        // 指向沙箱内的链接应放行。
        std::os::unix::fs::symlink("real.txt", root.join("inside")).unwrap();
        let ok = resolve_in(&root, std::slice::from_ref(&root), "inside").unwrap();
        assert!(ok.starts_with(std::fs::canonicalize(&root).unwrap()));

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 不存在的新文件（常规创建路径）：真实路径校验不得误伤。
    #[test]
    fn accepts_new_file_inside() {
        let base = std::env::temp_dir().join(format!("agent-paths-new-{}", std::process::id()));
        let root = base.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let got = resolve_in(&root, std::slice::from_ref(&root), "sub/new.txt").unwrap();
        assert!(got.ends_with("sub/new.txt"), "实际: {got:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
