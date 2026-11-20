//! Fitting text into fixed-width terminal columns.

use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::DIM;

/// Truncates to `width` columns with an ellipsis, then pads to exactly `width`.
pub fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return format!("{text}{}", " ".repeat(width - text.width()));
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    used += 1;
    out + &" ".repeat(width.saturating_sub(used))
}

/// A section heading drawn as a labelled rule across `width` columns.
pub fn rule(label: &str, width: usize) -> Line<'static> {
    let head = format!("── {label} ");
    let fill = "─".repeat(width.saturating_sub(head.width()));
    Line::styled(format!("{head}{fill}"), DIM)
}
