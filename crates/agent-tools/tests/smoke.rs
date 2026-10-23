//! 端到端冒烟：工具必须**真的能用**，而不只是「能构造、单测能过」。
//!
//! 存在的理由（不要删）：`web_search` 曾 100% 失败——它唯一的后端是自建
//! SearXNG 的 JSON API，而默认配置、配置向导、`config.example.toml` 都不写
//! `[search]` 段，于是每次调用只返回「未配置搜索后端」。当时**257 个单测全绿**，
//! 因为单测都在各自模块里直接 `SearchBackend`-less 地构造工具、或注入替身参数，
//! 谁也没有：
//! - 从 `default_tools()` 这条**生产装配路径**取工具（工具被漏掉注册也会全绿）；
//! - 比对「模型看到的 JSON Schema」与「`execute` 真正读的键」（只改一侧会导致
//!   模型永远收到 `InvalidArgs`，正是 web_search 那类 100% 失败）；
//! - 联网**真跑一遍**并对输出内容下断言。
//!
//! 本文件的用例只做单元测试做不到的事；其余行为仍由各模块的单测覆盖。
//!
//! 运行：
//! - `cargo test -p agent-tools --test smoke`
//! - `cargo run -p xtask -- smoke`（带进度输出）
//!
//! 联网用例在**网络不可达或被限流时跳过而非失败**——那时无法判定工具好坏；
//! 但只要能连上，副产物不成立（解析出零结果、内容不对）就算失败。

use agent_core::tool::{ToolCtx, ToolError, ToolOutput};
use agent_net::HttpClientConfig;
use agent_tools::{default_tools, SearchBackend, ToolRegistry};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 一期的全部工具名。改名/删除工具时这里会红，用来逼人同步文档与注册处。
const EXPECTED_TOOLS: [&str; 8] = [
    "file_edit",
    "file_read",
    "file_write",
    "finish",
    "list_dir",
    "shell_exec",
    "web_fetch",
    "web_search",
];

// ---------------------------------------------------------------------------
// 装配 + 沙箱
// ---------------------------------------------------------------------------

/// 每个用例一个目录，`Drop` 时清理，避免并行跑互相踩。
struct Sandbox {
    root: PathBuf,
}

static SEQ: AtomicU32 = AtomicU32::new(0);

impl Sandbox {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agent-tools-smoke-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        for sub in ["project", "workspace"] {
            std::fs::create_dir_all(root.join(sub)).expect("应能建沙箱目录");
        }
        Self { root }
    }

    /// 项目根（工具的读写沙箱，相对路径以此为基准）。
    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    /// 每任务私有工作区（快照、ctx 外置落这里）。
    fn private(&self) -> PathBuf {
        self.root.join("workspace")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 造出**生产同款**工具集与上下文。
///
/// 搜索后端特意用 `resolve(None, None)`（即未配置 `[search]`）——这是默认配置、
/// 模板、配置向导都会落到的状态，也正是 `web_search` 曾经 100% 失败的场景。
fn harness(tag: &str) -> (ToolRegistry, ToolCtx, Sandbox) {
    let client = agent_net::build_client(&HttpClientConfig::default()).expect("客户端应能构建");
    let reg = default_tools(client, SearchBackend::resolve(None, None));
    let sb = Sandbox::new(tag);
    let ctx = ToolCtx {
        workspace: sb.private(),
        project_root: Some(sb.project()),
        task_id: format!("smoke-{tag}"),
        timeout: Duration::from_secs(90),
        cancel: CancellationToken::new(),
    };
    (reg, ctx, sb)
}

/// 取工具并执行。工具不存在直接 panic——那本身就是「装配漏了」的失败信号。
async fn call(
    reg: &ToolRegistry,
    name: &str,
    args: Value,
    ctx: &ToolCtx,
) -> Result<ToolOutput, ToolError> {
    let tool = reg
        .get(name)
        .unwrap_or_else(|| panic!("工具 {name} 没注册"));
    tool.execute(args, ctx).await
}

async fn must_call(reg: &ToolRegistry, name: &str, args: Value, ctx: &ToolCtx) -> ToolOutput {
    let sent = args.to_string();
    let out = call(reg, name, args, ctx)
        .await
        .unwrap_or_else(|e| panic!("{name} 返回 Err：{e}\n参数：{sent}"));
    assert!(
        !out.is_error,
        "{name} 自报失败：{}\n参数：{sent}",
        out.content
    );
    out
}

// ---------------------------------------------------------------------------
// 装配与契约（纯本地，永远要跑）
// ---------------------------------------------------------------------------

#[test]
fn every_tool_is_registered() {
    let (reg, _ctx, _sb) = harness("registry");
    let mut names = reg.names();
    names.dedup();
    assert_eq!(names, EXPECTED_TOOLS, "注册表内容变了");
}

#[test]
fn llm_sees_a_usable_definition_for_every_tool() {
    // 提供者把 defs() 原样发给模型：description 为空模型不会用，parameters 缺
    // type 会被多数后端拒收——两种情况单测都不报错，但工具对模型等于不存在。
    let (reg, _ctx, _sb) = harness("defs");
    let defs = reg.defs();
    assert_eq!(defs.len(), EXPECTED_TOOLS.len());
    for d in defs {
        assert!(!d.name.is_empty() && !d.description.is_empty(), "{d:?}");
        assert_eq!(d.parameters["type"], json!("object"), "{d:?}");
        let props = d.parameters["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{} 没有 properties：{:?}", d.name, d.parameters));
        assert!(!props.is_empty(), "{} 没有可用参数：{d:?}", d.name);
    }
}

/// 各工具的「真实调用样例」，同时用于契约校验（不落盘）。
fn samples() -> Vec<(&'static str, Value)> {
    const NOTES: &str = "notes.md";
    vec![
        ("file_read", json!({ "path": NOTES })),
        (
            "file_write",
            json!({ "path": NOTES, "content": "第一行\n第二行\n" }),
        ),
        (
            "file_edit",
            json!({ "path": NOTES, "old_str": "第二行", "new_str": "改过的第二行" }),
        ),
        ("list_dir", json!({ "path": "." })),
        ("shell_exec", json!({ "cmd": "echo smoke-ok" })),
        ("web_fetch", json!({ "url": "https://example.com/" })),
        (
            "web_search",
            json!({ "query": "rust programming language", "count": 3 }),
        ),
        (
            "finish",
            json!({ "summary": "冒烟完成", "artifacts": [NOTES] }),
        ),
    ]
}

#[test]
fn tools_accept_exactly_the_args_they_advertise() {
    // 核心漂移检测：`parameters()` 是给模型看的合同，`execute()` 读的才是真的。
    // 一边改名另一边没跟上时，模型拿到的永远是 InvalidArgs——100% 失败且单测全绿。
    let (reg, _ctx, _sb) = harness("contract");
    for (name, args) in samples() {
        let tool = reg.get(name).unwrap_or_else(|| panic!("{name} 没注册"));
        let params = tool.parameters();
        let props = params["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} 的 schema 没有 properties"));

        for k in args.as_object().expect("样例必须是对象").keys() {
            assert!(
                props.contains_key(k),
                "{name} 的样例用了未声明的参数 `{k}`：看起来 execute 读的键与 parameters 给模型的合同不一致（properties: {:?}）",
                props.keys().collect::<Vec<_>>()
            );
        }
        if let Some(required) = params["required"].as_array() {
            for r in required {
                let key = r.as_str().expect("required 必须是字符串数组");
                assert!(
                    args.get(key).is_some(),
                    "{name} 声明 `{key}` 必填，样例却没给：样例不具代表性，失去覆盖意义"
                );
            }
        }
    }
}

#[tokio::test]
async fn missing_required_arg_is_rejected_not_ignored() {
    // 反向：缺必填参数必须是明确的 InvalidArgs，不能静默当空值继续跑。
    let (reg, ctx, _sb) = harness("required");
    let cases: Vec<(&str, Value)> = vec![
        ("file_read", json!({})),
        ("file_write", json!({ "path": "a.md" })),
        ("shell_exec", json!({})),
        ("web_fetch", json!({})),
        ("web_search", json!({})),
    ];
    for (name, args) in cases {
        let err = call(&reg, name, args, &ctx).await;
        assert!(
            matches!(err, Err(ToolError::InvalidArgs(_))),
            "{name} 缺必填参数时应报 InvalidArgs，实际 {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 文件类工具：真实读写
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_then_read_round_trips_content() {
    let (reg, ctx, sb) = harness("write-read");
    let body = "# 冒烟\n\n第一行：中文也要原样回来 😀\n";
    must_call(
        &reg,
        "file_write",
        json!({ "path": "notes.md", "content": body }),
        &ctx,
    )
    .await;

    let disk = sb.project().join("notes.md");
    assert!(disk.exists(), "file_write 说成功但磁盘上没文件");

    let out = must_call(&reg, "file_read", json!({ "path": "notes.md" }), &ctx).await;
    assert!(
        out.content.contains("中文也要原样回来"),
        "实际: {}",
        out.content
    );
    assert!(out.content.contains('😀'), "emoji 丢了: {}", out.content);
    assert!(out.content.contains("# 冒烟"), "首行丢了: {}", out.content);
}

#[tokio::test]
async fn edit_actually_rewrites_the_file() {
    let (reg, ctx, sb) = harness("edit");
    must_call(
        &reg,
        "file_write",
        json!({ "path": "notes.md", "content": "alpha\nbeta\n" }),
        &ctx,
    )
    .await;
    must_call(
        &reg,
        "file_edit",
        json!({ "path": "notes.md", "old_str": "beta", "new_str": "BETA" }),
        &ctx,
    )
    .await;

    let after = tokio::fs::read_to_string(sb.project().join("notes.md"))
        .await
        .expect("应能读到");
    assert!(after.contains("BETA"), "编辑没落盘: {after:?}");
    assert!(!after.contains("beta"), "旧内容仍在: {after:?}");

    // 旧内容被子串包含是最容易出的错：`beta` 若在其它词里，替换不唯一应报错。
    let ambiguous = call(
        &reg,
        "file_edit",
        json!({ "path": "notes.md", "old_str": "不存在的片段", "new_str": "x" }),
        &ctx,
    )
    .await;
    assert!(
        ambiguous.is_err() || ambiguous.unwrap().is_error,
        "找不到 old_str 时必须明确报错，不能静默成功"
    );
}

#[tokio::test]
async fn list_dir_shows_what_was_written() {
    let (reg, ctx, _sb) = harness("list");
    must_call(
        &reg,
        "file_write",
        json!({ "path": "a.txt", "content": "hi" }),
        &ctx,
    )
    .await;
    let out = must_call(&reg, "list_dir", json!({ "path": "." }), &ctx).await;
    assert!(out.content.contains("a.txt"), "实际: {}", out.content);
}

#[tokio::test]
async fn escape_attempt_is_rejected_through_the_registry() {
    // 走 default_tools 这条路再验一次逃逸防护：装配件若换了实现也得继续保持。
    let (reg, ctx, _sb) = harness("escape");
    let err = call(&reg, "file_read", json!({ "path": "../outside.md" }), &ctx).await;
    assert!(
        matches!(err, Err(ToolError::PathEscape(_))),
        "路径逃逸必须被拒，实际 {err:?}"
    );
}

#[tokio::test]
async fn finish_reports_summary_and_real_artifacts() {
    let (reg, ctx, _sb) = harness("finish");
    must_call(
        &reg,
        "file_write",
        json!({ "path": "out.md", "content": "# 结果\n" }),
        &ctx,
    )
    .await;

    let tool = reg.get("finish").expect("finish 应注册");
    assert!(tool.is_terminating(), "finish 必须是终止信号");

    let out = must_call(
        &reg,
        "finish",
        json!({ "summary": "冒烟完成", "artifacts": ["out.md"] }),
        &ctx,
    )
    .await;
    assert_eq!(out.content, "冒烟完成");
    assert_eq!(out.side_effects.len(), 1, "交付物应登记: {out:?}");
    assert!(
        out.side_effects[0].path.ends_with("out.md"),
        "实际: {:?}",
        out.side_effects[0]
    );
    assert!(out.side_effects[0].bytes > 0, "字节数应为真实大小");
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_exec_runs_a_real_command() {
    let (reg, ctx, _sb) = harness("shell");
    let out = must_call(&reg, "shell_exec", json!({ "cmd": "echo smoke-ok" }), &ctx).await;
    assert!(out.content.contains("smoke-ok"), "实际: {}", out.content);
    assert!(!out.content.contains('?'), "输出疑似乱码: {}", out.content);
}

// ---------------------------------------------------------------------------
// 联网工具
// ---------------------------------------------------------------------------

#[tokio::test]
async fn web_fetch_retrieves_a_real_page() {
    let (reg, ctx, _sb) = harness("fetch");
    let Some(out) = conclusive(
        call(
            &reg,
            "web_fetch",
            json!({ "url": "https://example.com/" }),
            &ctx,
        )
        .await,
    ) else {
        return;
    };
    // example.com 的正文是固定的：断言它，而不是只断言「没报错」。
    assert!(
        out.content.contains("Example Domain"),
        "没抓到预期正文: {}",
        &out.content[..out.content.len().min(400)]
    );
    assert!(
        !out.content.contains("<html") && !out.content.contains("</div>"),
        "HTML 标签没剥离: {}",
        &out.content[..out.content.len().min(200)]
    );
}

#[tokio::test]
async fn web_search_returns_real_hits_without_configuration() {
    // 关键回归：零配置（无 [search] 段）时必须能搜。这条挂了就是又一次
    // 「web_search 100% 失败」。
    let (reg, ctx, _sb) = harness("search");
    let Some(out) = conclusive(
        call(
            &reg,
            "web_search",
            json!({ "query": "rust programming language", "count": 3 }),
            &ctx,
        )
        .await,
    ) else {
        return;
    };
    let hits = out
        .content
        .lines()
        .filter(|l| l.contains("https://"))
        .count();
    assert!(hits >= 1, "一条候选都没有: {}", out.content);
    assert!(
        out.content
            .lines()
            .any(|l| l.trim_start().starts_with("1. ")),
        "结果应按编号列出，模型才能引用: {}",
        out.content
    );
}

#[tokio::test]
async fn web_search_rejects_blank_query() {
    let (reg, ctx, _sb) = harness("search-bad");
    let err = call(&reg, "web_search", json!({ "query": "   " }), &ctx).await;
    assert!(
        matches!(err, Err(ToolError::InvalidArgs(_))),
        "空 query 应被拒，实际 {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 联网用例的判定helper
// ---------------------------------------------------------------------------

/// 联网工具的结果里，哪些「失败」其实说明不了工具好坏（网络不通 / 被限流）。
///
/// 判据必须是**传输层**信号（reqwest 连不出去才会 `error sending request`；
/// HTTP 状态码报错走的是另一条路，不属于这里）。这样「工具坏了」依旧会红：
/// 例如搜索后端退化成「无结果」时，必须判失败而不是跳过。
fn is_undetermined(text: &str) -> bool {
    const MARKERS: [&str; 11] = [
        "error sending request", // reqwest 传输失败的统一措辞
        "dns",
        "connect",
        "timed out",
        "timeout",
        "tls",
        "certificate",
        "proxy",
        "429",
        "too many requests",
        "offline",
    ];
    let lower = text.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

#[test]
fn transport_failures_are_skipped_but_real_breakage_is_not() {
    // 守这条边界：放宽判定会让整个冒烟变走过场，收紧又会让离网 CI 误红。
    let skip = [
        "execution failed: fetch failed: error sending request for url (https://example.com/)",
        "- bing：请求失败：error sending request for url (https://www.bing.com/search?q=x)",
        "fetch failed: dns error: failed to lookup address information",
        "请求失败：connection refused (os error 111)",
        "- bing：HTTP 429：太多请求",
    ];
    for s in skip {
        assert!(is_undetermined(s), "应判为「无法判定」从而跳过：{s}");
    }

    // 这些必须让测试红——工具或后端真出问题了。
    let fail = [
        "web_search 失败（已依次尝试 bing）：\n- bing：无结果",
        "web_search 失败（已依次尝试 bing）：\n- bing：HTTP 403：Forbidden",
        "web_search 已被关闭（config.toml: [search] backend = \"off\"）",
        "响应是 HTML 而非 JSON（该实例可能未启用 JSON 输出）",
    ];
    for s in fail {
        assert!(!is_undetermined(s), "不该被跳过：{s}");
    }
}

/// 归一化联网结果：`Some` = 结论成立（成功或确属工具坏了），`None` = 无法判定。
///
/// 返回 `None` 时**跳过**而不是通过——离网 CI 不该红，但也不该假装验过了。
fn conclusive(result: Result<ToolOutput, ToolError>) -> Option<ToolOutput> {
    match result {
        Ok(o) if !o.is_error => Some(o),
        Ok(o) if is_undetermined(&o.content) => {
            eprintln!("跳过： {}", first_line(&o.content));
            None
        }
        Ok(o) => Some(o), // 工具说了人话，交给调用方的 assert 让它红
        Err(e) if is_undetermined(&e.to_string()) => {
            eprintln!("跳过： {e}");
            None
        }
        Err(e) => Some(ToolOutput::err(e.to_string())),
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(160)
        .collect()
}
