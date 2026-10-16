//! 事件落盘 —— 「什么该进真相源」的唯一裁决处。
//!
//! 执行侧只管往总线广播，落盘任务与 TUI 各自订阅，互不阻塞。
//! 落盘规则集中在本模块，避免各处各写一份导致**同一条事件落两遍**
//! （历史上 `ToolCallEnd` 曾同时被 runner 直写 store 与总线订阅者写入）。
//!
//! 副作用：headless（agent-server）与 TUI（agent-cli）共用同一套口径，
//! 因此两条入口的审计内容完全一致。

use std::sync::Arc;

use agent_core::event::{Event, EventBus};
use agent_core::ids::{SessionId, TaskId};
use agent_core::store::{Durability, LogConfig};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::JsonlStore;

/// 判定一条事件是否落盘，以及它的数据分层。
///
/// 返回 `None` 表示**不落盘**——这类事件只服务于实时 UI（如流式增量），
/// 内容已被按轮次的快照覆盖，落盘只会撑爆文件。
pub fn classify(ev: &Event, cfg: &LogConfig) -> Option<(TaskId, Durability)> {
    match ev {
        // 任务生命周期与错误：参与二期同步，长期保留。
        Event::TaskStarted { id, .. }
        | Event::TaskFinished { id, .. }
        | Event::Error { id, .. } => Some((id.clone(), Durability::Persistent)),

        // 过程信息：量大、可重建，超期清理。
        Event::ToolCallStart { id, .. }
        | Event::ToolCallEnd { id, .. }
        | Event::AwaitingApproval { id, .. }
        | Event::ContextSnapshot { id, .. }
        | Event::AssistantTurn { id, .. } => Some((id.clone(), Durability::Transient)),

        // 逐字增量：默认丢弃（内容已由 AssistantTurn 完整留存）。
        Event::TextDelta { id, .. } | Event::ReasoningDelta { id, .. } => {
            if cfg.save_stream_deltas {
                Some((id.clone(), Durability::Transient))
            } else {
                None
            }
        }
    }
}

/// 订阅总线：按规则落盘，并在 `ui` 存在时转发一份给前端。
///
/// 落盘失败仅记日志，不打断任务；总线滞后（`Lagged`）时跳过而非退出，
/// 否则一次突发流量就会让审计流提前断尾。
pub fn spawn_event_sink(
    bus: EventBus,
    store: Arc<JsonlStore>,
    cfg: LogConfig,
    ui: Option<mpsc::Sender<Event>>,
) {
    // 订阅必须在**函数返回前**完成：否则调用方紧接着发出的第一批事件
    // （如 `TaskStarted`）会被漏掉。
    let mut sub = bus.subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match sub.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "event sink lagged, skip missing events");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            if let Some(ui) = &ui {
                let _ = ui.send(ev.clone()).await;
            }
            if let Some((tid, durable)) = classify(&ev, &cfg) {
                let sid = SessionId(tid.0.clone());
                let _ = store.record(&sid, &tid, durable, ev).await;
            }
        }
        tracing::debug!("event sink stopped");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::store::Store;

    fn cfg_deltas(on: bool) -> LogConfig {
        LogConfig {
            save_context: true,
            save_stream_deltas: on,
        }
    }

    #[test]
    fn task_lifecycle_is_persistent() {
        let id = TaskId::new();
        let ev = Event::TaskStarted {
            id: id.clone(),
            title: "t".into(),
        };
        let (got, d) = classify(&ev, &LogConfig::default()).expect("应落盘");
        assert_eq!(got, id);
        assert_eq!(d, Durability::Persistent);
    }

    #[test]
    fn context_and_turn_are_transient() {
        let ev = Event::ContextSnapshot {
            id: TaskId::new(),
            iteration: 0,
            model: "m".into(),
            messages: vec![],
            tools: vec![],
        };
        assert_eq!(
            classify(&ev, &LogConfig::default()).map(|(_, d)| d),
            Some(Durability::Transient)
        );

        let ev = Event::AssistantTurn {
            id: TaskId::new(),
            iteration: 0,
            text: "a".into(),
            calls: vec![],
            usage: agent_core::provider::Usage::default(),
        };
        assert_eq!(
            classify(&ev, &LogConfig::default()).map(|(_, d)| d),
            Some(Durability::Transient)
        );
    }

    #[test]
    fn stream_deltas_are_skipped_unless_enabled() {
        let ev = Event::TextDelta {
            id: TaskId::new(),
            text: "你".into(),
        };
        assert!(
            classify(&ev, &cfg_deltas(false)).is_none(),
            "默认不落逐字增量"
        );
        assert!(
            classify(&ev, &cfg_deltas(true)).is_some(),
            "显式开启后应落盘"
        );

        let ev = Event::ReasoningDelta {
            id: TaskId::new(),
            text: "想想".into(),
        };
        assert!(classify(&ev, &cfg_deltas(false)).is_none());
        assert!(classify(&ev, &cfg_deltas(true)).is_some());
    }

    #[tokio::test]
    async fn sink_persists_each_event_exactly_once() {
        let tmp = std::env::temp_dir().join(format!("agent-store-rec-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(JsonlStore::new(&tmp));
        store.init().await.unwrap();

        let bus = agent_core::event::event_bus();
        spawn_event_sink(bus.clone(), store.clone(), LogConfig::default(), None);

        let tid = TaskId::new();
        let sid = SessionId(tid.0.clone());
        let mut rx = bus.subscribe();
        // 先让落盘任务完成订阅，否则它会错过下面的广播。
        tokio::task::yield_now().await;
        let mut expected = 0usize;
        for ev in [
            Event::TaskStarted {
                id: tid.clone(),
                title: "t".into(),
            },
            Event::ContextSnapshot {
                id: tid.clone(),
                iteration: 0,
                model: "m".into(),
                messages: vec![],
                tools: vec![],
            },
            Event::TextDelta {
                id: tid.clone(),
                text: "逐字增量".into(),
            },
            Event::ToolCallStart {
                id: tid.clone(),
                call_id: agent_core::ids::ToolCallId::new(),
                name: "list_dir".into(),
                args: serde_json::json!({}),
            },
            Event::ToolCallEnd {
                id: tid.clone(),
                call_id: agent_core::ids::ToolCallId::new(),
                outcome: agent_core::event::ToolOutcome::Ok,
                ms: 1,
                detail: None,
                output: Some("空目录".into()),
            },
        ] {
            let _ = bus.send(ev);
            expected += 1;
        }
        // 逐条取走广播（每次 await 都给落盘任务让出执行机会）
        for _ in 0..expected {
            let _ = rx.recv().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let session = store.load_session(&sid).await.unwrap();
        // 逐字增量被丢弃：5 条广播 → 4 条落盘
        assert_eq!(session.events.len(), expected - 1);
        let kinds: Vec<&str> = session
            .events
            .iter()
            .map(|e| match &e.payload {
                Event::TaskStarted { .. } => "task_started",
                Event::ContextSnapshot { .. } => "context_snapshot",
                Event::ToolCallStart { .. } => "tool_call_start",
                Event::ToolCallEnd { .. } => "tool_call_end",
                Event::TextDelta { .. } => "text_delta",
                other => unreachable!("意外的事件: {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "task_started",
                "context_snapshot",
                "tool_call_start",
                "tool_call_end"
            ]
        );
        // 每条 call 只落一次（回归：曾经 ToolCallEnd 落两遍）
        assert_eq!(
            kinds.iter().filter(|k| **k == "tool_call_end").count(),
            1,
            "ToolCallEnd 只能落一条"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
