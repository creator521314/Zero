//! 共享的路径安全校验：确保任何读写都发生在工作区内。

use agent_core::tool::ToolError;
use std::path::{Component, Path, PathBuf};

/// 把用户/模型给出的相对或绝对路径解析到工作区内。
///
/// 规则：
/// - 相对路径基于 `workspace`；
/// - 结果必须位于 `workspace` 之下（规范化后比较），否则拒绝；
/// - 不要求路径已存在（支持创建新文件），但父目录会被规范化。
pub fn resolve_in_workspace(workspace: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::InvalidArgs("path is empty".into()));
    }
    let raw_path = Path::new(raw);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        workspace.join(raw_path)
    };

    let normalized = normalize(&candidate);
    let ws = normalize(workspace);

    if !normalized.starts_with(&ws) {
        return Err(ToolError::PathEscape(format!(
            "path escapes workspace: {} (workspace: {})",
            normalized.display(),
            ws.display()
        )));
    }
    Ok(normalized)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escape() {
        let ws = Path::new("/home/u/agent/workspace/t1");
        assert!(resolve_in_workspace(ws, "../../../etc/passwd").is_err());
        assert!(resolve_in_workspace(ws, "/etc/passwd").is_err());
    }

    #[test]
    fn accepts_inside() {
        let ws = Path::new("/home/u/agent/workspace/t1");
        let r = resolve_in_workspace(ws, "scripts/a.sh").unwrap();
        assert!(r.starts_with(ws));
        let r2 = resolve_in_workspace(ws, "sub/../b.txt").unwrap();
        assert!(r2.starts_with(ws));
        assert!(r2.ends_with("b.txt"));
    }
}
