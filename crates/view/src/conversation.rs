//! Draws a PR's comment threads as cards in the detail pane.
//!
//! Each thread hangs from a gutter colored by its status, so open threads stand out and resolved
//! ones recede. Authors keep one color across every thread, you are marked, and an open thread
//! ends by saying whose turn it is.

use chrono::{DateTime, Utc};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::markdown;
use crate::text::{fit, rule};
use crate::theme::{ACCENT, DIM, MUTED};
use forge::model::{Comment, ThreadStatus, age};

/// Mid-tone colors that read on dark and light backgrounds and stay clear of the status colors.
const AUTHOR_COLORS: [u8; 6] = [33, 135, 168, 172, 105, 131];
const GUTTER: &str = "│ ";

/// Who is reading, so the view can mark your comments and the threads that wait on you.
#[derive(Debug, Clone, Copy)]
pub struct Viewer<'a> {
    /// Your login on the item's host, once known.
    pub me: Option<&'a str>,
    /// The login of the item's author.
    pub author: &'a str,
}

impl Viewer<'_> {
    fn is_me(&self, login: &str) -> bool {
        self.me
            .is_some_and(|me| !login.is_empty() && login.eq_ignore_ascii_case(me))
    }
}

/// Which threads to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    All,
    Unresolved,
}

struct Thread<'a> {
    comments: Vec<&'a Comment>,
}

impl Thread<'_> {
    fn status(&self) -> ThreadStatus {
        self.comments
            .first()
            .map_or(ThreadStatus::Plain, |c| c.status)
    }
}

/// Every comment after the first in a thread is a reply, so a comment that is not a reply starts
/// the next thread.
fn threads(comments: &[Comment]) -> Vec<Thread<'_>> {
    let mut threads: Vec<Thread> = Vec::new();
    for comment in comments {
        match threads.last_mut() {
            Some(thread) if comment.is_reply => thread.comments.push(comment),
            _ => threads.push(Thread {
                comments: vec![comment],
            }),
        }
    }
    threads
}

pub fn lines(
    comments: &[Comment],
    viewer: Viewer,
    show: Show,
    width: usize,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let all = threads(comments);
    let open = all
        .iter()
        .filter(|t| !t.status().is_resolved() && t.status() != ThreadStatus::Plain)
        .count();
    let resolved = all.iter().filter(|t| t.status().is_resolved()).count();
    let mut lines = vec![heading(all.len(), open, resolved, show, width)];
    let shown: Vec<&Thread> = all
        .iter()
        .filter(|t| show == Show::All || !t.status().is_resolved())
        .collect();
    if shown.is_empty() {
        lines.push(Line::styled(
            match show {
                Show::Unresolved if !all.is_empty() => "No unresolved comments.",
                _ => "No comments.",
            },
            DIM,
        ));
    }
    for thread in shown {
        lines.push(Line::raw(""));
        lines.extend(thread_lines(thread, viewer, width, now));
    }
    lines
}

fn heading(
    threads: usize,
    open: usize,
    resolved: usize,
    show: Show,
    width: usize,
) -> Line<'static> {
    let noun = if threads == 1 { "thread" } else { "threads" };
    let mut parts = vec![format!("{threads} {noun}")];
    if open > 0 {
        parts.push(format!("{open} open"));
    }
    if resolved > 0 {
        parts.push(match show {
            Show::All => format!("{resolved} resolved"),
            Show::Unresolved => format!("{resolved} resolved hidden"),
        });
    }
    rule(&format!("Comments · {}", parts.join(" · ")), width)
}

fn status_style(status: ThreadStatus) -> (&'static str, Color) {
    match status {
        ThreadStatus::Open => ("●", Color::Yellow),
        ThreadStatus::Pending => ("◐", Color::Magenta),
        ThreadStatus::Resolved | ThreadStatus::Closed | ThreadStatus::ByDesign => {
            ("✓", Color::Green)
        }
        ThreadStatus::WontFix => ("✕", MUTED),
        ThreadStatus::Plain => ("○", MUTED),
    }
}

/// A stable color for each author, so a voice can be followed through a long thread.
fn author_color(comment: &Comment) -> Color {
    let key = if comment.login.is_empty() {
        &comment.author
    } else {
        &comment.login
    };
    let hash = key.to_lowercase().bytes().fold(2_166_136_261_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    let index = usize::try_from(hash).unwrap_or(0) % AUTHOR_COLORS.len();
    Color::Indexed(AUTHOR_COLORS[index])
}

/// Whose move it is on an open thread, or `None` when the view cannot tell or nobody is waiting.
enum Turn {
    Yours,
    Theirs,
}

fn turn(thread: &Thread, viewer: Viewer) -> Option<Turn> {
    if thread.status().is_resolved() {
        return None;
    }
    let last = thread.comments.last()?;
    if viewer.is_me(&last.login) {
        return Some(Turn::Theirs);
    }
    let involved =
        viewer.is_me(viewer.author) || thread.comments.iter().any(|c| viewer.is_me(&c.login));
    (involved && !last.login.is_empty()).then_some(Turn::Yours)
}

fn thread_lines(
    thread: &Thread,
    viewer: Viewer,
    width: usize,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let status = thread.status();
    let resolved = status.is_resolved();
    let (glyph, color) = status_style(status);
    let bar = Style::new().fg(if resolved { MUTED } else { color });
    let body_width = width.saturating_sub(GUTTER.width()).max(1);

    let mut head = vec![
        Span::styled("╭─ ", bar),
        Span::styled(
            format!("{glyph} {}", status.label()),
            Style::new().fg(color).bold(),
        ),
    ];
    if let Some(location) = thread.comments.first().and_then(|c| c.location.as_ref()) {
        let used: usize = head.iter().map(|s| s.content.width()).sum();
        let room = width.saturating_sub(used + 2);
        head.push(Span::raw("  "));
        head.push(Span::styled(
            fit(location, room).trim_end().to_string(),
            Style::new().fg(if resolved { MUTED } else { ACCENT }),
        ));
    }
    let mut lines = vec![Line::from(head)];

    for (n, comment) in thread.comments.iter().enumerate() {
        if n > 0 {
            lines.push(Line::styled(GUTTER.trim_end(), bar));
        }
        lines.push(author_line(comment, viewer, bar, resolved, now));
        for line in markdown::render(&comment.body, body_width) {
            let line = if resolved { mute(line) } else { line };
            let gutter = if line.spans.is_empty() {
                GUTTER.trim_end()
            } else {
                GUTTER
            };
            let mut spans = vec![Span::styled(gutter, bar)];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }

    let foot = match turn(thread, viewer) {
        Some(Turn::Yours) => vec![
            Span::styled("╰─ ", bar),
            Span::styled("↩ Your turn", Style::new().fg(Color::Yellow).bold()),
        ],
        Some(Turn::Theirs) => vec![
            Span::styled("╰─ ", bar),
            Span::styled("Waiting for a reply", DIM),
        ],
        None => vec![Span::styled("╰─", bar)],
    };
    lines.push(Line::from(foot));
    lines
}

fn author_line(
    comment: &Comment,
    viewer: Viewer,
    bar: Style,
    resolved: bool,
    now: DateTime<Utc>,
) -> Line<'static> {
    let me = viewer.is_me(&comment.login);
    let name_style = match (resolved, me) {
        (true, _) => Style::new().fg(MUTED).bold(),
        (false, true) => Style::new().fg(ACCENT).bold(),
        (false, false) => Style::new().fg(author_color(comment)).bold(),
    };
    let mut spans = vec![Span::styled(GUTTER, bar)];
    if comment.is_reply {
        spans.push(Span::styled("↳ ", DIM));
    }
    spans.push(Span::styled(comment.author.clone(), name_style));
    if me {
        spans.push(Span::styled(" (you)", DIM));
    }
    spans.push(Span::styled(format!(" · {}", age(comment.date, now)), DIM));
    Line::from(spans)
}

/// Grays out a line while keeping its weight and emphasis.
fn mute(line: Line<'static>) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| {
                let style = s.style.fg(MUTED);
                Span::styled(s.content, style)
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn comment(login: &str, body: &str, is_reply: bool, status: ThreadStatus) -> Comment {
        Comment {
            login: login.into(),
            author: login.to_uppercase(),
            date: Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
            location: (!is_reply).then(|| "src/a.rs:4".to_string()),
            body: body.into(),
            is_reply,
            status,
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap()
    }

    const VIEWER: Viewer = Viewer {
        me: Some("me"),
        author: "ann",
    };

    #[test]
    fn a_thread_is_a_card_with_its_status_replies_and_turn() {
        let comments = [
            comment("ann", "Why **this**?", false, ThreadStatus::Open),
            comment("me", "Because.", true, ThreadStatus::Open),
            comment("ann", "Fair.", true, ThreadStatus::Open),
        ];
        let lines = lines(&comments, VIEWER, Show::All, 40, now());
        assert_eq!(
            text(&lines),
            [
                "── Comments · 1 thread · 1 open ────────",
                "",
                "╭─ ● Open  src/a.rs:4",
                "│ ANN · 2d",
                "│ Why this?",
                "│",
                "│ ↳ ME (you) · 2d",
                "│ Because.",
                "│",
                "│ ↳ ANN · 2d",
                "│ Fair.",
                "╰─ ↩ Your turn",
            ]
        );
        assert_eq!(lines[2].spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(lines[6].spans[2].style.fg, Some(ACCENT));
    }

    #[test]
    fn a_thread_you_answered_last_waits_for_a_reply() {
        let comments = [
            comment("bob", "Hm?", false, ThreadStatus::Open),
            comment("me", "Done.", true, ThreadStatus::Open),
        ];
        let lines = lines(&comments, VIEWER, Show::All, 40, now());
        assert_eq!(text(&lines).last().unwrap(), "╰─ Waiting for a reply");
    }

    #[test]
    fn any_open_thread_on_your_own_pr_waits_on_you() {
        let comments = [comment("bob", "Hm?", false, ThreadStatus::Open)];
        let viewer = Viewer {
            me: Some("me"),
            author: "me",
        };
        let lines = lines(&comments, viewer, Show::All, 40, now());
        assert_eq!(text(&lines).last().unwrap(), "╰─ ↩ Your turn");
    }

    #[test]
    fn a_thread_you_are_not_part_of_has_no_turn() {
        let comments = [comment("bob", "Hm?", false, ThreadStatus::Open)];
        let viewer = Viewer {
            me: Some("me"),
            author: "cy",
        };
        let lines = lines(&comments, viewer, Show::All, 40, now());
        assert_eq!(text(&lines).last().unwrap(), "╰─");
    }

    #[test]
    fn resolved_threads_are_muted_or_hidden() {
        let comments = [
            comment("bob", "old", false, ThreadStatus::Resolved),
            comment("me", "**ok**", true, ThreadStatus::Resolved),
            comment("cy", "new", false, ThreadStatus::Open),
        ];
        let all = lines(&comments, VIEWER, Show::All, 40, now());
        assert!(text(&all)[0].starts_with("── Comments · 2 threads · 1 open · 1 resolved "));
        let ok = all
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "ok")
            .unwrap();
        assert_eq!(ok.style.fg, Some(MUTED));

        let open = lines(&comments, VIEWER, Show::Unresolved, 40, now());
        let open = text(&open);
        assert!(open[0].contains("1 resolved hidden"), "{open:?}");
        assert!(!open.iter().any(|l| l.contains("old")), "{open:?}");
        assert!(open.iter().any(|l| l.contains("new")), "{open:?}");
    }

    #[test]
    fn nothing_to_show() {
        let none = lines(&[], VIEWER, Show::All, 40, now());
        assert_eq!(text(&none)[1], "No comments.");
        let comments = [comment("bob", "old", false, ThreadStatus::Resolved)];
        let hidden = lines(&comments, VIEWER, Show::Unresolved, 40, now());
        assert_eq!(text(&hidden)[1], "No unresolved comments.");
    }

    #[test]
    fn authors_keep_one_color() {
        let a = comment("bob", "", false, ThreadStatus::Open);
        let b = comment("Bob", "", true, ThreadStatus::Open);
        assert_eq!(author_color(&a), author_color(&b));
    }
}
