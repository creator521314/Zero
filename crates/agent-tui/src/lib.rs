//! agent-tui —— 终端交互层（ratatui + crossterm）。

pub mod app;
pub mod render;

pub use app::{App, UiAction};
pub use render::run_tui;
