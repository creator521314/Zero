//! 前端边界：TUI 只是 [`Frontend`] 的一种实现；三期的手机 App 将是另一个实现。

use crate::event::{Command, Event};

/// 前端契约：只发 [`Command`]、只收 [`Event`]，不认识工具与 provider。
pub trait Frontend: Send {
    /// 取命令接收端（Agent 侧消费）。
    fn commands(&self) -> tokio::sync::mpsc::Receiver<Command>;
    /// 处理一条事件（更新 UI 状态，不做昂贵 IO）。
    fn render(&mut self, ev: &Event);
}
