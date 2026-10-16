//! agent-tui —— 终端交互层（ratatui + crossterm）。

pub mod app;
pub mod markdown;
pub mod render;
pub mod theme;

pub use app::{App, UiAction};
pub use render::run_tui;
