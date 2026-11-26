//! The app's colors and sizes, from the mock-up.

use ctx_core::item::Tone;
use gpui_kit::*;

pub const UI_FONT: &str = ".SystemUIFont";

pub const BACKGROUND: u32 = 0x0e1013;
pub const SURFACE: u32 = 0x121418;
pub const CARD: u32 = 0x15181d;
pub const RAISED: u32 = 0x1a1d22;
pub const SELECTED: u32 = 0x22262d;
pub const BORDER: u32 = 0x23272e;
pub const CONTROL_BORDER: u32 = 0x2a2f37;

pub const TEXT: u32 = 0xe6e8eb;
pub const SECONDARY: u32 = 0x9aa1ab;
pub const MUTED: u32 = 0x858c96;
pub const FAINT: u32 = 0x4a5059;

pub const ACCENT: u32 = 0x79aeff;
pub const LIVE: u32 = 0x7fd6c2;
pub const WAIT: u32 = 0xe3a857;
pub const BAD: u32 = 0xe5767b;

pub const SIDEBAR_WIDTH: f32 = 248.0;
pub const PANEL_WIDTH: f32 = 340.0;
pub const HEADER_HEIGHT: f32 = 40.0;
pub const TABS_HEIGHT: f32 = 36.0;

/// A chip's background and text colors for an item's tone.
pub fn chip(tone: Tone) -> (u32, u32) {
    match tone {
        Tone::Good => (0x183b35, LIVE),
        Tone::Bad => (0x3b1c20, BAD),
        Tone::Busy => (0x1f2b40, 0xa9cbff),
        Tone::Warn => (0x3a2e1a, WAIT),
        Tone::Neutral => (CONTROL_BORDER, 0xc9cdd3),
        Tone::Muted => (RAISED, MUTED),
    }
}

/// A section heading: small, spaced capitals.
pub fn heading(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(text.into().to_uppercase())
}

/// A small rounded label.
pub fn pill(text: impl Into<SharedString>, (bg, fg): (u32, u32)) -> Div {
    div()
        .flex_none()
        .px(px(7.0))
        .py(px(1.0))
        .rounded(px(10.0))
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(text.into())
}

pub fn dot(color: u32) -> Div {
    div()
        .flex_none()
        .size(px(8.0))
        .rounded(px(4.0))
        .bg(rgb(color))
}

/// An outlined button with a label.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .h(px(26.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(CONTROL_BORDER))
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(RAISED)))
        .child(label.into())
}
