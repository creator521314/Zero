//! 主题：语义色板 + 视觉符号 + 预混合表面色。
//!
//! 色板参考 iCode（Dracula 系）。ratatui 不做运行时 alpha 混合，
//! 故这里把「前景色按百分比叠加在背景上」的取值预先算成常量，
//! 见 [`surface`] 模块。详见 `docs/ui-design.md` 第二、三章。

use ratatui::style::Color;

/// 主背景（最深）。
pub const BG: Color = Color::Rgb(18, 18, 18);

/// 语义色。
pub mod ink {
    use ratatui::style::Color;

    /// 品牌色 / 焦点 / 选中（紫）。
    pub const PRIMARY: Color = Color::Rgb(175, 135, 255);
    /// 次级（深紫）。
    pub const SECONDARY: Color = Color::Rgb(95, 95, 175);
    /// 强调 / 用户消息（粉）。
    pub const ACCENT: Color = Color::Rgb(255, 135, 215);
    /// 成功 / 工具完成（绿）。
    pub const SUCCESS: Color = Color::Rgb(95, 255, 135);
    /// 进行中 / 警告 / 审批 AUTO（橙）。
    pub const WARNING: Color = Color::Rgb(255, 175, 95);
    /// 失败 / 审批 BYPASS（红）。
    pub const ERROR: Color = Color::Rgb(255, 95, 95);
    /// 正文（近白）。
    pub const FG: Color = Color::Rgb(238, 238, 238);
    /// 辅助文字 / 思考链 / 提示（灰）。
    pub const FG_MUTED: Color = Color::Rgb(88, 88, 88);
    /// 卡片 / 输入框表面。
    pub const SURFACE: Color = Color::Rgb(48, 48, 48);
    /// 弹窗表面。
    pub const OVERLAY: Color = Color::Rgb(58, 58, 58);
}

/// 预混合表面色（等价于 Textual 的 `color N%` 叠加在 [`BG`] 上）。
pub mod surface {
    use ratatui::style::Color;

    /// `$foreground 8%`：用户消息底、hover。
    pub const FAINT: Color = Color::Rgb(36, 36, 36);
    /// `$foreground 15%`：表头、次级控件底。
    pub const DIM: Color = Color::Rgb(51, 51, 51);
    /// `$primary 18%`：选中轮次高亮。
    pub const SELECTED: Color = Color::Rgb(46, 38, 56);
    /// `$foreground 25%`：分隔线、弱边框。
    pub const LINE: Color = Color::Rgb(73, 73, 73);
}

/// 视觉符号（见 `docs/ui-design.md` 附录 A）。
pub mod glyph {
    /// 用户消息左竖线。
    pub const USER_BAR: &str = "▌";
    /// 工具运行中。
    pub const RUNNING: &str = "⏺";
    /// 工具成功。
    pub const OK: &str = "✔";
    /// 工具失败。
    pub const FAIL: &str = "✖";
    /// 工具被拒。
    pub const REJECTED: &str = "⊘";
    /// 思考链。
    pub const REASONING: &str = "⋯";
    /// 系统提示。
    pub const SYSTEM: &str = "·";
}

/// 权限档位 → header 徽章的底色（三态语义与 iCode approval badge 一致）。
pub fn tier_badge_color(tier: &str) -> Color {
    match tier.to_ascii_uppercase().as_str() {
        "L1" => ink::ERROR,   // BYPASS：高危放行
        "L3" => ink::PRIMARY, // MANUAL：逐个确认
        _ => ink::WARNING,    // L2 / 其他：AI 审视
    }
}

/// 权限档位 → 中文说明。
pub fn tier_label(tier: &str) -> &'static str {
    match tier.to_ascii_uppercase().as_str() {
        "L1" => "BYPASS",
        "L3" => "MANUAL",
        _ => "AUTO",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_l1_maps_to_error() {
        assert_eq!(tier_badge_color("L1"), ink::ERROR);
        assert_eq!(tier_label("L1"), "BYPASS");
    }

    #[test]
    fn tier_l3_maps_to_primary() {
        assert_eq!(tier_badge_color("L3"), ink::PRIMARY);
        assert_eq!(tier_label("L3"), "MANUAL");
    }

    #[test]
    fn tier_l2_and_unknown_map_to_warning() {
        assert_eq!(tier_badge_color("L2"), ink::WARNING);
        assert_eq!(tier_badge_color(""), ink::WARNING);
        assert_eq!(tier_label("l2"), "AUTO");
    }

    #[test]
    fn tier_matching_is_case_insensitive() {
        assert_eq!(tier_badge_color("l1"), ink::ERROR);
        assert_eq!(tier_label("l3"), "MANUAL");
    }
}
