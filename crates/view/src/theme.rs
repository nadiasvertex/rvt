//! Colors shared by every pane.
//!
//! Named colors such as DarkGray take the terminal theme's palette, which can make them unreadable
//! on a light background. Indexed colors keep their value in every theme.

use ratatui::style::{Color, Modifier, Style};

pub const MUTED: Color = Color::Indexed(244);
pub const DIM: Style = Style::new().fg(MUTED);
pub const ACCENT: Color = Color::Cyan;
pub const PANEL: Style = Style::new().fg(Color::Indexed(252)).bg(Color::Indexed(236));

pub const LINK: Style = Style::new()
    .fg(Color::Indexed(33))
    .add_modifier(Modifier::UNDERLINED);
pub const INLINE_CODE: Style = Style::new().fg(Color::Indexed(173));

/// Code blocks sit on the dark panel in every theme, so their colors are chosen against it.
pub const CODE_ADDED: Style = PANEL.fg(Color::Indexed(114));
pub const CODE_REMOVED: Style = PANEL.fg(Color::Indexed(210));
pub const CODE_HUNK: Style = PANEL.fg(Color::Indexed(117));
pub const CODE_LABEL: Style = PANEL.fg(Color::Indexed(245));
