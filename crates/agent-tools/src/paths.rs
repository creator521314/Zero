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
    Ok(normalized)
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
}
