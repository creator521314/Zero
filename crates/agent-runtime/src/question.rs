//! 问答应答通道与端口实现。
//!
//! 与 `runner::ApprovalInbox` 同构但语义独立：审批是「放行 / 拒绝」，问答是
//! 「候选 / 自编 / 取消」。分开的好处是审计事件不会互相污染，前端也能按
//! 类型给不同的弹层。

use std::collections::HashMap;
use std::sync::Arc;

use agent_core::error::AgentError;
use agent_core::event::{Event, EventBus};
use agent_core::ids::QuestionId;
use agent_core::question::{QuestionAnswer, QuestionRequest, UserQuestionPort};
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// 问答消息：(问题 ID, 回答)。
pub type QuestionMsg = (QuestionId, QuestionAnswer);

/// 问答应答收件箱。多任务共用一个通道，错配到达的应答暂存而不是丢弃——
/// 否则用户在 A 弹窗点的回答会被 B 任务吃掉，A 从此永远等下去。
pub struct QuestionInbox {
    rx: Mutex<mpsc::Receiver<QuestionMsg>>,
    pending: Mutex<HashMap<QuestionId, QuestionAnswer>>,
}

impl QuestionInbox {
    pub fn new(rx: mpsc::Receiver<QuestionMsg>) -> Self {
        Self {
            rx: Mutex::new(rx),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub async fn recv_for(
        &self,
        id: &QuestionId,
        cancel: &CancellationToken,
    ) -> Result<QuestionAnswer, AgentError> {
        if let Some(answer) = self.pending.lock().await.remove(id) {
            return Ok(answer);
        }
        let mut rx = self.rx.lock().await;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AgentError::Cancelled),
                msg = rx.recv() => match msg {
                    Some((got, answer)) if got == *id => return Ok(answer),
                    Some((got, answer)) => {
                        let mut pending = self.pending.lock().await;
                        if pending.len() >= 64 {
                            pending.clear();
                        }
                        pending.insert(got, answer);
                    }
                    None => return Err(AgentError::Internal("question channel closed".into())),
                },
            }
        }
    }
}

/// 真正对接事件总线的问答端口：广播 `QuestionAsked`，等前端回灌，再广播结果。
#[derive(Clone)]
pub struct InteractiveQuestionPort {
    bus: EventBus,
    inbox: Arc<QuestionInbox>,
}

impl InteractiveQuestionPort {
    pub fn new(bus: EventBus, inbox: Arc<QuestionInbox>) -> Self {
        Self { bus, inbox }
    }

    /// 供装配层直接使用：建好通道，返回 (端口, 供命令循环发送应答的 tx)。
    pub fn pair(bus: EventBus) -> (Self, mpsc::Sender<QuestionMsg>) {
        let (tx, rx) = mpsc::channel::<QuestionMsg>(16);
        let inbox = Arc::new(QuestionInbox::new(rx));
        (Self::new(bus, inbox), tx)
    }
}

#[async_trait]
impl UserQuestionPort for InteractiveQuestionPort {
    async fn ask(
        &self,
        req: QuestionRequest,
        cancel: CancellationToken,
    ) -> Result<QuestionAnswer, AgentError> {
        let _ = self.bus.send(Event::QuestionAsked {
            id: req.task_id.clone(),
            req: req.clone(),
        });
        // 没有回答后端时通道会关闭，recv_for 立刻报错，工具随即返回失败；
        // 绝不让模型停在一个永远等不到的提问上。
        let answer = match self.inbox.recv_for(&req.id, &cancel).await {
            Ok(a) => a,
            Err(e) => {
                // 取消也要告诉模型，否则它会以为问题还悬着而反复重试。
                if matches!(e, AgentError::Cancelled) {
                    let answer = QuestionAnswer::Cancelled;
                    let _ = self.bus.send(Event::QuestionAnswered {
                        id: req.task_id.clone(),
                        question_id: req.id.clone(),
                        answer: answer.clone(),
                    });
                    return Ok(answer);
                }
                return Err(e);
            }
        };
        let _ = self.bus.send(Event::QuestionAnswered {
            id: req.task_id.clone(),
            question_id: req.id.clone(),
            answer: answer.clone(),
        });
        Ok(answer)
    }
}

/// 前端不具备问答能力时的兜底端口：立刻失败，模型会改用其它策略。
/// 目前 headless 直接不注册工具，这里留给需要显式拒绝的场景。
pub struct UnsupportedQuestionPort;

#[async_trait]
impl UserQuestionPort for UnsupportedQuestionPort {
    async fn ask(
        &self,
        _req: QuestionRequest,
        _cancel: CancellationToken,
    ) -> Result<QuestionAnswer, AgentError> {
        Err(AgentError::Internal(
            "当前前端不支持交互式问答，请自行决策或说明需要用户补充的信息".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::event_bus;
    use agent_core::ids::TaskId;
    use std::time::Duration;

    fn req(id: &str) -> QuestionRequest {
        QuestionRequest {
            id: QuestionId::from_raw(id),
            task_id: TaskId::from_raw("t-1"),
            question: "选哪个".into(),
            header: None,
            options: vec!["A".into()],
            allow_custom: true,
        }
    }

    #[tokio::test]
    async fn mismatched_answer_is_buffered_not_lost() {
        let (tx, rx) = mpsc::channel(8);
        let inbox = QuestionInbox::new(rx);
        tx.send((QuestionId::from_raw("q-b"), QuestionAnswer::Cancelled))
            .await
            .unwrap();
        let got = tokio::time::timeout(
            Duration::from_millis(50),
            inbox.recv_for(&QuestionId::from_raw("q-a"), &CancellationToken::new()),
        )
        .await;
        assert!(got.is_err(), "q-a 不能吃掉 q-b 的应答");
    }

    #[tokio::test]
    async fn buffered_answer_is_taken_when_its_owner_waits() {
        let (tx, rx) = mpsc::channel(8);
        let inbox = QuestionInbox::new(rx);
        let answer = QuestionAnswer::Custom {
            text: "自己写".into(),
        };
        tx.send((QuestionId::from_raw("q-b"), answer.clone()))
            .await
            .unwrap();
        let got = inbox
            .recv_for(&QuestionId::from_raw("q-b"), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(got, answer);
    }

    #[tokio::test]
    async fn cancel_unblocks_wait_immediately() {
        let (_tx, rx) = mpsc::channel(8);
        let inbox = QuestionInbox::new(rx);
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn({
            let cancel = cancel.clone();
            async move { inbox.recv_for(&QuestionId::from_raw("q-a"), &cancel).await }
        });
        cancel.cancel();
        let got = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(got, Err(AgentError::Cancelled)));
    }

    #[tokio::test]
    async fn port_broadcasts_question_and_answer() {
        let bus = event_bus();
        let mut rx = bus.subscribe();
        let (port, tx) = InteractiveQuestionPort::pair(bus.clone());
        let cancel = CancellationToken::new();
        let r = req("q-port");
        let waited = tokio::spawn({
            let port = port.clone();
            let r = r.clone();
            let cancel = cancel.clone();
            async move { port.ask(r, cancel).await }
        });
        let asked = rx.recv().await.unwrap();
        assert!(matches!(asked, Event::QuestionAsked { .. }));
        tx.send((
            r.id.clone(),
            QuestionAnswer::Selected {
                index: 0,
                label: "A".into(),
            },
        ))
        .await
        .unwrap();
        let got = waited.await.unwrap().unwrap();
        assert!(matches!(got, QuestionAnswer::Selected { .. }));
        let answered = rx.recv().await.unwrap();
        assert!(matches!(answered, Event::QuestionAnswered { .. }));
    }
}
