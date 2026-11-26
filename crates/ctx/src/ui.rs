use chrono::{DateTime, Utc};
use forge::model::age;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use view::conversation::{self, Show, Viewer};
use view::markdown;
use view::text::{fit, rule};
use view::theme::{ACCENT, DIM, MUTED};

use crate::app::{
    App, AskKind, AssignPicker, CaptureForm, Confirm, FreezeClaudeForm, FreezeForm, Group, HomeRow,
    Input, Job, Modal, Pick, ResponseView, Screen, SessionState, WaitPicker,
};
use ctx_core::item::{Ci, ItemRef, ItemState, Tone};
use ctx_core::live::Status;
use ctx_core::ops;
use ctx_core::respond::{Assessment, Evaluation, Response};
use ctx_core::search::{MATCH_END, MATCH_START};
use ctx_core::store::{self, Project, State};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SELECTED: Style = Style::new().bg(Color::Indexed(237));
const NAME_WIDTH: usize = 20;

fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Good => Color::Green,
        Tone::Bad => Color::Red,
        Tone::Busy => Color::Cyan,
        Tone::Warn => Color::Yellow,
        Tone::Neutral => Color::Blue,
        Tone::Muted => MUTED,
    }
}

fn tone_glyph(tone: Tone) -> &'static str {
    match tone {
        Tone::Good => "✓",
        Tone::Bad => "✗",
        Tone::Busy => "◐",
        Tone::Warn => "◷",
        Tone::Neutral => "●",
        Tone::Muted => "○",
    }
}

fn ago(time: Option<DateTime<Utc>>) -> String {
    match time {
        Some(t) => age(t, Utc::now()),
        None => "never".into(),
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, tick: usize) {
    let [header, body, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(
        Paragraph::new(header_line(app, tick, header.width as usize)),
        header,
    );
    match app.screen.clone() {
        Screen::Home => draw_home(frame, app, body),
        Screen::Project(slug) => draw_project(frame, app, &slug, body),
        Screen::Search => draw_search(frame, app, body),
    }
    frame.render_widget(Paragraph::new(hint_line(app)), hints);
    if let Some(modal) = app.modal.clone() {
        draw_modal(frame, app, &modal);
    }
}

fn header_line(app: &App, tick: usize, width: usize) -> Line<'static> {
    let mut left = vec![
        Span::styled(" ctx ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw(" "),
    ];
    let groups = app.groups();
    let count = |g: Group| {
        groups
            .iter()
            .find(|(x, _)| *x == g)
            .map(|(_, p)| p.len())
            .unwrap_or(0)
    };
    let live = count(Group::Live);
    let live_style = if live > app.soft_limit {
        Style::new().fg(Color::Yellow).bold()
    } else {
        Style::new()
    };
    left.push(Span::styled(format!("{live} live"), live_style));
    for (g, word) in [(Group::Ready, "ready"), (Group::Waiting, "waiting")] {
        left.push(Span::styled(" · ", DIM));
        left.push(Span::raw(format!("{} {word}", count(g))));
    }
    let unfiled = app.unfiled().len();
    if unfiled > 0 {
        left.push(Span::styled(" · ", DIM));
        left.push(Span::styled(
            format!("{unfiled} unfiled"),
            Style::new().fg(Color::Magenta),
        ));
    }
    if live > app.soft_limit {
        left.push(Span::styled(
            format!("  more than {} live; freeze some", app.soft_limit),
            Style::new().fg(Color::Yellow),
        ));
    }
    let right = if let Some(job) = app.responding.values().min_by_key(|j| j.started) {
        let more = match app.responding.len() - 1 {
            0 => String::new(),
            n => format!(" (+{n})"),
        };
        let text = format!(
            "{} {}: {} · {}{more}",
            SPINNER[tick % SPINNER.len()],
            job.label,
            job.step,
            elapsed(job)
        );
        format!("{} ", fit(&text, width / 2).trim_end())
    } else if app.discovering && app.fetching.is_empty() {
        format!("{} looking for your items ", SPINNER[tick % SPINNER.len()])
    } else if !app.fetching.is_empty() {
        format!(
            "{} refreshing {} ",
            SPINNER[tick % SPINNER.len()],
            app.fetching.len()
        )
    } else {
        format!("↻ {} ago ", ago(app.last_refresh()))
    };
    let used: usize = left.iter().map(|s| s.content.width()).sum();
    let pad = width.saturating_sub(used + right.width());
    left.push(Span::raw(" ".repeat(pad)));
    left.push(Span::styled(right, DIM));
    Line::from(left)
}

fn hint_line(app: &App) -> Line<'static> {
    if let Some(flash) = &app.flash {
        if flash.at.elapsed().as_secs() < 6 {
            let style = if flash.error {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(ACCENT)
            };
            return Line::styled(format!(" {}", flash.text), style);
        }
    }
    if let (None, Some(view)) = (&app.modal, &app.responses) {
        return responses_hint(view);
    }
    let text = match (&app.modal, &app.screen) {
        (Some(Modal::Help), _) => " any key closes",
        (Some(Modal::Confirm(_)), _) => " y confirms  any other key cancels",
        (Some(Modal::Wait(p)), _) if p.asking.is_none() => " j k move  ⏎ choose  esc cancel",
        (Some(Modal::Assign(_)), _) => " type to filter  ↑↓ move  ⏎ add  esc cancel",
        (Some(Modal::FreezeClaude(f)), _) if f.confirm => {
            " y interrupts Claude and freezes  any other key cancels"
        }
        (Some(Modal::Freeze(_) | Modal::Capture(_)), _) => {
            " tab next field  space picks or toggles  ⏎ save  esc cancel"
        }
        (Some(_), _) => " ⏎ save  esc cancel",
        (None, Screen::Home) if matches!(app.selected_row(), Some(HomeRow::Item(_))) => {
            " ⏎ open  a add to a project  n new project from this item  R refresh  / search  ? help"
        }
        (None, Screen::Home) => {
            " ⏎ switch  o open  f freeze  t thaw  w wait  n new  / search  c capture  d done  esc back  ? help"
        }
        (None, Screen::Project(_)) if app.reading => {
            " j k scroll  space b page  gg G top bottom  u resolved threads  esc close"
        }
        (None, Screen::Project(_)) => {
            " ⏎ open  v read  r respond  J K scroll  a add  x remove  N note  c capture  e next  w wait  f freeze  t thaw  s switch  esc home  ? help"
        }
        (None, Screen::Search) => {
            " type to search  ↑↓ move  ⏎ open  tab this project/all  esc back"
        }
    };
    Line::styled(text.to_string(), DIM)
}

/// The glyph, the status text and its color, for a project's home screen row.
fn home_status(app: &App, project: &Project) -> (&'static str, Color, String, Style) {
    let changes = app.changes(&project.slug);
    let since = app.since.get(&project.slug).and_then(|s| s.time);
    let wait = project.wait.as_ref();
    if let Some(cleared) = wait.and_then(|w| w.cleared.as_ref()) {
        let text = format!("{} · {}", cleared.reason, age(cleared.at, Utc::now()));
        return ("●", Color::Green, text, Style::new().fg(Color::Green));
    }
    if let Some(w) = wait.filter(|w| w.is_waiting()) {
        let text = format!(
            "waiting: {} · {}",
            w.condition.describe(),
            age(w.set, Utc::now())
        );
        return ("◌", Color::Yellow, text, Style::new().fg(Color::Yellow));
    }
    let summary = |prefix: Option<&str>| {
        let mut parts: Vec<String> = prefix.map(str::to_string).into_iter().collect();
        match changes {
            [] => {}
            [one] => parts.push(format!("{} {}", one.label, one.text)),
            [first, rest @ ..] => parts.push(format!(
                "{} {} · +{} more",
                first.label,
                first.text,
                rest.len()
            )),
        }
        parts
    };
    match project.state {
        State::Live if app.current.as_deref() == Some(project.slug.as_str()) => {
            let mut text = "(current)".to_string();
            if !changes.is_empty() {
                text.push_str(&format!(" · {} new", changes.len()));
            }
            ("◆", ACCENT, text, Style::new().fg(ACCENT))
        }
        State::Live => {
            let mut parts = summary(None);
            parts.push(if changes.is_empty() {
                format!("quiet · {}", ago(since))
            } else {
                ago(since)
            });
            let style = if changes.is_empty() {
                DIM
            } else {
                Style::new()
            };
            ("◆", ACCENT, parts.join(" · "), style)
        }
        State::Frozen => {
            let frozen = project.handoff.as_ref().map(|h| h.time);
            let mut parts = summary(None);
            parts.push(format!("frozen {}", ago(frozen)));
            ("●", Color::Green, parts.join(" · "), Style::new())
        }
        State::Done => ("✓", MUTED, "done".into(), DIM),
    }
}

fn draw_home(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let selected = app.selected_row();
    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line = 0;
    let sections = app.home_sections();
    if app.groups().is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  No projects yet. Press n to start one here, or run ctx new <name> in a repository.",
            DIM,
        ));
    }
    for (group, rows) in sections {
        let mut heading = vec![Span::styled(
            format!(" {}", group.label()),
            Style::new().bold(),
        )];
        if group == Group::Unfiled {
            heading.push(Span::styled(
                " · your PRs, MRs, Bugs and PBIs in no project",
                DIM,
            ));
            if let Some(e) = app.inbox.errors.first() {
                heading.push(Span::styled(
                    format!(" · {}", fit(e, width.saturating_sub(60))),
                    Style::new().fg(Color::Red),
                ));
            }
        }
        lines.push(Line::from(heading));
        for row in rows {
            let is_selected = selected.as_ref() == Some(&row);
            if is_selected {
                selected_line = lines.len();
            }
            match &row {
                HomeRow::Item(item) => lines.push(item_row(app, item, width, is_selected, "")),
                HomeRow::Project(slug) => {
                    if let Some(project) = app.project(slug) {
                        lines.push(project_row(app, project, width, is_selected));
                    }
                }
            }
        }
    }
    let height = area.height as usize;
    let offset = selected_line.saturating_sub(height.saturating_sub(2));
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
}

fn project_row(app: &App, project: &Project, width: usize, is_selected: bool) -> Line<'static> {
    let (glyph, glyph_color, status, status_style) = home_status(app, project);
    let next = if project.next.is_empty() {
        String::new()
    } else {
        format!("next: {}", project.next)
    };
    let fixed = 5 + NAME_WIDTH + 1;
    let rest = width.saturating_sub(fixed + 1);
    let (status_width, next_width) = if next.is_empty() {
        (rest, 0)
    } else {
        let status_width = (rest * 11 / 20).max(rest.saturating_sub(next.width() + 2));
        (
            status_width.min(rest),
            rest.saturating_sub(status_width.min(rest)),
        )
    };
    let row = vec![
        Span::raw(if is_selected { " > " } else { "   " }),
        Span::styled(glyph, Style::new().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(fit(&project.name, NAME_WIDTH), Style::new().bold()),
        Span::raw(" "),
        Span::styled(fit(&status, status_width), status_style),
        Span::styled(fit(&next, next_width), DIM),
    ];
    let mut line = Line::from(row);
    if is_selected {
        line = line.style(SELECTED);
    }
    line
}

fn section(title: &str, extra: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(format!(" {title}"), Style::new().bold())];
    if !extra.is_empty() {
        spans.push(Span::styled(format!(" · {extra}"), DIM));
    }
    Line::from(spans)
}

const FLAGS_WIDTH: usize = 7;

/// Conflict, build and reply markers for a PR or MR, `FLAGS_WIDTH` columns wide.
fn flag_spans(state: Option<&ItemState>) -> Vec<Span<'static>> {
    let Some(s) = state else {
        return vec![Span::raw(" ".repeat(FLAGS_WIDTH))];
    };
    let conflict = if s.conflicts {
        Span::styled("⚠", Style::new().fg(Color::Yellow))
    } else {
        Span::raw(" ")
    };
    let build = match s.ci {
        Some(Ci::Passed) => Span::styled("✓", Style::new().fg(Color::Green)),
        Some(Ci::Failed) => Span::styled("✗", Style::new().fg(Color::Red)),
        Some(Ci::Running) => Span::styled("◐", Style::new().fg(Color::Cyan)),
        None => Span::raw(" "),
    };
    let replies = match s.replies_due {
        0 => Span::raw("   "),
        n => Span::styled(fit(&format!("↩{n}"), 3), Style::new().fg(Color::Magenta)),
    };
    vec![conflict, Span::raw(" "), build, Span::raw(" "), replies]
}

/// The conflict, build and reply state in words, for the detail pane.
fn flag_words(s: &ItemState) -> Vec<Span<'static>> {
    let mut words = Vec::new();
    if s.conflicts {
        words.push(Span::styled(
            "conflicts with its target",
            Style::new().fg(Color::Yellow),
        ));
    }
    match s.ci {
        Some(Ci::Passed) => words.push(Span::styled("build passed", Style::new().fg(Color::Green))),
        Some(Ci::Failed) => words.push(Span::styled("build failed", Style::new().fg(Color::Red))),
        Some(Ci::Running) => {
            words.push(Span::styled("build running", Style::new().fg(Color::Cyan)))
        }
        None => {}
    }
    match s.replies_due {
        0 => {}
        1 => words.push(Span::styled(
            "1 thread waits for your reply",
            Style::new().fg(Color::Magenta),
        )),
        n => words.push(Span::styled(
            format!("{n} threads wait for your reply"),
            Style::new().fg(Color::Magenta),
        )),
    }
    let mut spans = Vec::new();
    for word in words {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", DIM));
        }
        spans.push(word);
    }
    spans
}

fn item_row(
    app: &App,
    item: &ItemRef,
    width: usize,
    selected: bool,
    prefix: &str,
) -> Line<'static> {
    let state = app.state(item);
    let error = app.cache.errors.get(&item.key());
    let (glyph, color, status) = match (state, error) {
        (Some(s), _) => (tone_glyph(s.tone), tone_color(s.tone), s.status.clone()),
        (None, Some(_)) => ("!", Color::Red, "fetch failed".to_string()),
        (None, None) => (
            "·",
            MUTED,
            if item.is_remote() || item.is_local() {
                "not fetched".to_string()
            } else {
                String::new()
            },
        ),
    };
    let title = state
        .map(|s| s.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| item.kind_label().to_string());
    let label_width = 18;
    let status_width = 16;
    let rest = width.saturating_sub(label_width + status_width + FLAGS_WIDTH + 9);
    let mut spans = vec![
        Span::raw(if selected { " > " } else { "   " }),
        Span::raw(prefix.to_string()),
        Span::styled(glyph, Style::new().fg(color)),
        Span::raw(" "),
        Span::styled(fit(&item.label(), label_width), Style::new().bold()),
        Span::raw(" "),
        Span::styled(fit(&status, status_width), Style::new().fg(color)),
        Span::raw(" "),
    ];
    spans.extend(flag_spans(state));
    spans.push(Span::raw(" "));
    let rest = match respond_tag(app, item) {
        Some(tag) => {
            let used = tag.content.width();
            spans.push(tag);
            rest.saturating_sub(used)
        }
        None => rest,
    };
    spans.push(Span::raw(fit(&title, rest)));
    let mut line = Line::from(spans);
    if selected {
        line = line.style(SELECTED);
    }
    line
}

/// How long Claude has worked on a job, such as "45s" or "2m 05s".
fn elapsed(job: &Job) -> String {
    let secs = job.started.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// A short tag for an item row while Claude works on its threads, after it failed, or while
/// its drafted replies wait to be posted.
fn respond_tag(app: &App, item: &ItemRef) -> Option<Span<'static>> {
    let key = item.key();
    if app.responding.contains_key(&key) {
        return Some(Span::styled(
            "Claude working · ",
            Style::new().fg(Color::Cyan),
        ));
    }
    if app.respond_failures.contains_key(&key) {
        return Some(Span::styled(
            "Claude failed · ",
            Style::new().fg(Color::Red),
        ));
    }
    match app.drafts.get(&key).copied().unwrap_or(0) {
        0 => None,
        1 => Some(Span::styled("1 draft · ", Style::new().fg(Color::Green))),
        n => Some(Span::styled(
            format!("{n} drafts · "),
            Style::new().fg(Color::Green),
        )),
    }
}

/// The same state in a sentence for the detail pane, with what to do next.
fn respond_line(app: &App, item: &ItemRef) -> Option<Line<'static>> {
    let key = item.key();
    if let Some(job) = app.responding.get(&key) {
        return Some(Line::styled(
            format!(
                "Claude: {} · {}. The row says when it is done.",
                job.step,
                elapsed(job)
            ),
            Style::new().fg(Color::Cyan),
        ));
    }
    if let Some(e) = app.respond_failures.get(&key) {
        return Some(Line::styled(
            format!("Claude could not answer the threads: {e}. r tries again."),
            Style::new().fg(Color::Red),
        ));
    }
    match app.drafts.get(&key).copied().unwrap_or(0) {
        0 => None,
        n => Some(Line::styled(
            format!(
                "Claude drafted {} that {} not posted yet. r opens {}.",
                if n == 1 {
                    "1 reply".to_string()
                } else {
                    format!("{n} replies")
                },
                if n == 1 { "is" } else { "are" },
                if n == 1 { "it" } else { "them" },
            ),
            Style::new().fg(Color::Green),
        )),
    }
}

fn draw_project(frame: &mut Frame, app: &App, slug: &str, area: Rect) {
    let Some(project) = app.project(slug).cloned() else {
        return;
    };
    if let Some(view) = &app.responses {
        draw_responses(frame, app, view, area);
        return;
    }
    if app.reading {
        let picks = app.picks(slug);
        let selected = picks.get(app.project_selected).cloned();
        draw_detail(frame, app, &project, selected.as_ref(), area);
        return;
    }
    let split = area.width >= 110;
    let (left, right) = if split {
        let [l, r] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Fill(1)]).areas(area);
        (l, Some(r))
    } else {
        (area, None)
    };
    let width = left.width as usize;
    let picks = app.picks(slug);
    let selected = picks.get(app.project_selected).cloned();
    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line = 0;
    let mut pick_index = 0;
    let mut mark = |lines: &Vec<Line>, pick_index: &mut usize| {
        let is = *pick_index == app.project_selected;
        if is {
            selected_line = lines.len();
        }
        *pick_index += 1;
        is
    };

    let session = if app.sessions.contains(&project.session) {
        "session running"
    } else {
        "no session"
    };
    let mut title = vec![
        Span::styled(format!(" {}", project.name), Style::new().bold().fg(ACCENT)),
        Span::styled(format!(" · {} · {session}", project.state.label()), DIM),
    ];
    if let Some(dir) = &project.dir {
        title.push(Span::styled(format!(" · {}", tilde(dir)), DIM));
    }
    lines.push(Line::from(title));
    if !project.next.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" next: ", DIM),
            Span::raw(project.next.clone()),
        ]));
    }
    if let Some(w) = &project.wait {
        let line = match &w.cleared {
            Some(c) => Line::styled(
                format!(" ready: {} · {}", c.reason, age(c.at, Utc::now())),
                Style::new().fg(Color::Green),
            ),
            None => Line::styled(
                format!(
                    " waiting: {} · {}",
                    w.condition.describe(),
                    age(w.set, Utc::now())
                ),
                Style::new().fg(Color::Yellow),
            ),
        };
        lines.push(line);
    }
    lines.push(Line::raw(""));

    if let (State::Frozen, Some(h)) = (project.state, &project.handoff) {
        lines.push(section("Handoff", &ago(Some(h.time))));
        for (label, text) in [
            ("Doing", &h.doing),
            ("Next", &h.next),
            ("Waiting on", &h.waiting),
        ] {
            if !text.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(format!("   {label:<11}"), DIM),
                    Span::raw(text.clone()),
                ]));
            }
        }
        lines.push(Line::raw(""));
    }

    let since = app.since.get(slug);
    if let Some(brief) = app
        .briefs
        .get(slug)
        .filter(|b| since.and_then(|s| s.time).is_none_or(|t| b.time > t))
    {
        lines.push(section("Brief from Claude", &ago(Some(brief.time))));
        lines.extend(indented(
            markdown::render(&brief.text, width.saturating_sub(4)),
            "   ",
        ));
        lines.push(Line::raw(""));
    }
    let since_time = since.and_then(|s| s.time);
    lines.push(section(
        "Since you left",
        &if since_time.is_some() {
            format!("{} ago", ago(since_time))
        } else {
            "first visit".into()
        },
    ));
    let changes = app.changes(slug);
    if changes.is_empty() {
        lines.push(Line::styled("   nothing changed", DIM));
    }
    for c in changes {
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(tone_glyph(c.tone), Style::new().fg(tone_color(c.tone))),
            Span::raw(" "),
            Span::styled(fit(&c.label, 18), Style::new().bold()),
            Span::raw(" "),
            Span::raw(fit(&c.text, width.saturating_sub(26))),
        ]));
    }
    lines.push(Line::raw(""));

    lines.push(section("Items", &project.items.len().to_string()));
    if project.items.is_empty() {
        lines.push(Line::styled(
            "   none yet; a adds a PR, MR, work item, branch, file or URL",
            DIM,
        ));
    }
    for item in project.refs() {
        let is = mark(&lines, &mut pick_index);
        lines.push(item_row(app, item, width, is, ""));
    }
    let suggestions = ops::suggestions(&project, &app.cache.items);
    if !suggestions.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section("Suggested", "⏎ add  x hide"));
        for (item, _) in &suggestions {
            let is = mark(&lines, &mut pick_index);
            lines.push(item_row(app, item, width, is, "+ "));
        }
    }
    let notes = app.notes.get(slug).cloned().unwrap_or_default();
    lines.push(Line::raw(""));
    lines.push(section("Notes", &notes.len().to_string()));
    if notes.is_empty() {
        lines.push(Line::styled(
            "   none yet; c saves the clipboard, N opens an editor",
            DIM,
        ));
    }
    for note in &notes {
        let is = mark(&lines, &mut pick_index);
        let local = note.captured.with_timezone(&chrono::Local);
        let mut line = Line::from(vec![
            Span::raw(if is { " > " } else { "   " }),
            Span::styled(local.format("%m-%d ").to_string(), DIM),
            Span::styled(fit(&note.source, 6), Style::new().fg(Color::Magenta)),
            Span::raw(" "),
            Span::raw(fit(&note.title, width.saturating_sub(17))),
        ]);
        if is {
            line = line.style(SELECTED);
        }
        lines.push(line);
    }
    if !project.claude_sessions.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section(
            "Claude sessions",
            "⏎ go or resume  f freeze  t thaw",
        ));
        for s in app.claude_rows(&project) {
            let is = mark(&lines, &mut pick_index);
            let dir = s.cwd.as_deref().map(tilde).unwrap_or_default();
            let (glyph, color, state, label) = match app.session_state(s) {
                SessionState::Running(live) => {
                    let (word, color) = match live.status {
                        Status::Busy | Status::Other => ("busy", Color::Cyan),
                        Status::Idle => ("idle", Color::Green),
                    };
                    let label = live.name.clone().unwrap_or_else(|| s.label());
                    ("●", color, word.to_string(), label)
                }
                SessionState::Frozen(f) => (
                    "❄",
                    Color::Blue,
                    format!("frozen {} ago", age(f.at, Utc::now())),
                    s.label(),
                ),
                SessionState::Ended => (
                    "▸",
                    MUTED,
                    format!("{} ago", ago(Some(s.started))),
                    s.label(),
                ),
            };
            let mut line = Line::from(vec![
                Span::raw(if is { " > " } else { "   " }),
                Span::styled(format!("{glyph} "), Style::new().fg(color)),
                Span::raw(format!("{}  ", fit(&label, 24))),
                Span::styled(format!("{state}  {dir}"), DIM),
            ]);
            if is {
                line = line.style(SELECTED);
            }
            lines.push(line);
        }
    }
    let entries = store::journal_entries(&app.journal);
    if !entries.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section("Journal", ""));
        for (heading, body) in entries.iter().rev().take(6) {
            lines.push(Line::styled(format!("   {heading}"), DIM));
            let body = markdown::render(body, width.saturating_sub(6));
            lines.extend(indented(body.into_iter().take(4).collect(), "     "));
        }
    }

    let height = left.height as usize;
    let offset = (selected_line + 3).saturating_sub(height);
    let block = if split {
        Block::new().borders(Borders::RIGHT).border_style(DIM)
    } else {
        Block::new()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((offset as u16, 0)),
        left,
    );
    if let Some(right) = right {
        draw_detail(frame, app, &project, selected.as_ref(), right);
    }
}

fn tilde(path: &std::path::Path) -> String {
    let home = dirs::home_dir().unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn indented(lines: Vec<Line<'static>>, indent: &'static str) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(indent)];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn is_markdown(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx"))
}

/// The body of an item below its status lines: the description and threads of a PR, MR or work
/// item, a Markdown file rendered, or the plain text of anything else.
fn item_body(item: &ItemRef, s: &ItemState, show: Show, width: usize) -> Vec<Line<'static>> {
    let reviewable = matches!(
        item,
        ItemRef::AdoPr { .. } | ItemRef::GitlabMr { .. } | ItemRef::WorkItem { .. }
    );
    // States cached before ctx kept threads carry only their text until the next refresh.
    let structured = !s.description.is_empty() || !s.conversation.is_empty() || s.text.is_empty();
    match item {
        _ if reviewable && structured => {
            let mut lines = vec![rule("Description", width)];
            if s.description.trim().is_empty() {
                lines.push(Line::styled("No description.", DIM));
            } else {
                lines.extend(markdown::render(&s.description, width));
            }
            lines.push(Line::raw(""));
            let viewer = Viewer {
                me: s.viewer.as_deref(),
                author: &s.author,
            };
            lines.extend(conversation::lines(
                &s.conversation,
                viewer,
                show,
                width,
                Utc::now(),
            ));
            lines
        }
        ItemRef::File { path } if is_markdown(path) => markdown::render(&s.text, width),
        _ => s
            .text
            .lines()
            .take(400)
            .map(|l| Line::raw(l.to_string()))
            .collect(),
    }
}

fn draw_detail(frame: &mut Frame, app: &App, project: &Project, pick: Option<&Pick>, area: Rect) {
    let area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let lines = detail_lines(app, project, pick, usize::from(area.width));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(area.width)).unwrap_or(u16::MAX);
    app.detail_size.set((total, area.height));
    let scroll = app.detail_scroll.min(total.saturating_sub(area.height));
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn detail_lines(
    app: &App,
    project: &Project,
    pick: Option<&Pick>,
    width: usize,
) -> Vec<Line<'static>> {
    let show = if app.show_resolved {
        Show::All
    } else {
        Show::Unresolved
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    match pick {
        Some(Pick::Item(item)) | Some(Pick::Suggestion(item)) => {
            if matches!(pick, Some(Pick::Suggestion(_))) {
                let from = ops::suggestions(project, &app.cache.items)
                    .into_iter()
                    .find(|(r, _)| r == item)
                    .map(|(_, from)| from)
                    .unwrap_or_default();
                lines.push(Line::styled(
                    format!("Suggested because {from} links to it. ⏎ adds it, x hides it."),
                    Style::new().fg(ACCENT),
                ));
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(item.describe(), DIM));
            match app.state(item) {
                Some(s) => {
                    lines.push(Line::styled(s.title.clone(), Style::new().bold()));
                    lines.push(Line::from(vec![
                        Span::styled(s.status.clone(), Style::new().fg(tone_color(s.tone))),
                        Span::styled(
                            format!(
                                " · {} comments · fetched {} ago",
                                s.comments,
                                ago(Some(s.fetched))
                            ),
                            DIM,
                        ),
                    ]));
                    let flags = flag_words(s);
                    if !flags.is_empty() {
                        lines.push(Line::from(flags));
                    }
                    lines.extend(respond_line(app, item));
                    if let Some(url) = &s.url {
                        lines.push(Line::styled(url.clone(), DIM));
                    }
                    if let Some(e) = app.cache.errors.get(&item.key()) {
                        lines.push(Line::styled(
                            format!("last refresh failed: {e}"),
                            Style::new().fg(Color::Red),
                        ));
                    }
                    lines.push(Line::raw(""));
                    lines.extend(item_body(item, s, show, width));
                }
                None => match app.cache.errors.get(&item.key()) {
                    Some(e) => lines.push(Line::styled(e.clone(), Style::new().fg(Color::Red))),
                    None => lines.push(Line::styled("not fetched yet; R refreshes", DIM)),
                },
            }
        }
        Some(Pick::Note(path)) => {
            if let Some(note) = app
                .notes
                .get(&project.slug)
                .and_then(|ns| ns.iter().find(|n| &n.path == path))
            {
                lines.push(Line::styled(note.title.clone(), Style::new().bold()));
                let mut meta = format!(
                    "{} · {}",
                    note.source,
                    note.captured
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                );
                if !note.link.is_empty() {
                    meta.push_str(&format!(" · {}", note.link));
                }
                lines.push(Line::styled(meta, DIM));
                lines.push(Line::raw(""));
                lines.extend(markdown::render(&note.body, width));
            }
        }
        Some(Pick::Claude(id)) => {
            if let Some(s) = project.claude_sessions.iter().find(|s| &s.id == id) {
                lines.push(Line::styled(
                    format!("Claude Code session {}", s.label()),
                    Style::new().bold(),
                ));
                lines.push(Line::styled(s.id.clone(), DIM));
                lines.push(Line::raw(""));
                match app.session_state(s) {
                    SessionState::Running(live) => {
                        let status = match live.status {
                            Status::Busy => "working",
                            Status::Idle => "idle, waiting for you",
                            Status::Other => "running",
                        };
                        lines.push(Line::raw(format!(
                            "Running, {status}. Process {}.",
                            live.pid
                        )));
                        lines.push(Line::raw(
                            "⏎ goes to its window. f freezes it: ctx ends Claude and keeps the session to resume later.",
                        ));
                    }
                    SessionState::Frozen(f) => {
                        let when = f.at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M");
                        lines.push(Line::raw(format!("Frozen {when}.")));
                        if !f.note.is_empty() {
                            lines.push(Line::raw(""));
                            lines.extend(markdown::render(&f.note, width));
                            lines.push(Line::raw(""));
                        }
                        lines.push(Line::raw("t or ⏎ resumes it in a new window."));
                    }
                    SessionState::Ended => {
                        lines.push(Line::raw(
                            "Ended. ⏎ resumes it in a new window, and f keeps it here as frozen.",
                        ));
                    }
                }
                if let Some(cwd) = &s.cwd {
                    lines.push(Line::styled(format!("in {}", tilde(cwd)), DIM));
                }
            }
        }
        None => {
            lines.push(Line::styled("Select an item or note to see it here.", DIM));
        }
    }
    lines
}

fn highlighted(snippet: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut matched = false;
    let mut current = String::new();
    for c in snippet.chars() {
        if c == MATCH_START || c == MATCH_END {
            if !current.is_empty() {
                let text = std::mem::take(&mut current);
                spans.push(if matched {
                    Span::styled(text, Style::new().fg(Color::Yellow).bold())
                } else {
                    Span::styled(text, DIM)
                });
            }
            matched = c == MATCH_START;
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        spans.push(Span::styled(current, DIM));
    }
    spans
}

fn responses_hint(view: &ResponseView) -> Line<'static> {
    if view.confirm {
        return Line::from(vec![
            Span::styled(
                format!(" {} ", confirm_question(&view.evaluation, view.cursor)),
                Style::new().fg(Color::Yellow).bold(),
            ),
            Span::styled("y", Style::new().bold()),
            Span::styled(" goes ahead, any other key cancels", DIM),
        ]);
    }
    Line::styled(
        " p post  r resolve on/off  e edit reply  space check  a all  C fix with Claude  j k move  J K scroll  R ask again  o browser  esc back",
        DIM,
    )
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What `y` does after `p`, such as "Post 2 replies and resolve 1 thread on PR 7?".
fn confirm_question(e: &Evaluation, cursor: usize) -> String {
    let (replies, resolves) = e.post_counts(&e.to_post(cursor));
    let reply = count(replies, "reply", "replies");
    let resolve = count(resolves, "thread", "threads");
    let what = match (replies, resolves) {
        (_, 0) => format!("Post {reply}"),
        (0, _) => format!("Resolve {resolve}"),
        _ => format!("Post {reply} and resolve {resolve}"),
    };
    format!("{what} on {}?", e.item.label())
}

/// The line between the list and the detail: what `p` posts, or what posting is doing.
fn post_line(e: &Evaluation, cursor: usize, job: Option<&Job>) -> String {
    if let Some(job) = job {
        return format!("{} · {}", job.step, elapsed(job));
    }
    let checked = e.responses.iter().filter(|r| r.selected).count();
    if checked > 0 {
        let (replies, resolves) = e.post_counts(&e.to_post(cursor));
        return match (replies, resolves) {
            (0, 0) => format!("{checked} checked, all posted"),
            _ => format!("{checked} checked · p posts them"),
        };
    }
    match e.responses.get(cursor) {
        Some(r) if !r.posted => "p posts this reply · space checks several".into(),
        Some(r) if r.needs_post() => "p resolves this thread".into(),
        Some(_) => "this reply is posted".into(),
        None => String::new(),
    }
}

/// What posting does to the thread, and how to change it.
fn resolve_sentence(r: &Response) -> &'static str {
    match (r.posted, r.resolve, r.resolved) {
        (_, _, true) => "Posted, and the thread is resolved.",
        (false, false, _) => "Posting leaves the thread open. r resolves it as well.",
        (false, true, _) => "Posting also resolves the thread. r leaves it open.",
        (true, true, _) => "Posted. The thread is still open; p resolves it.",
        (true, false, _) => "Posted. The thread is open; r and then p resolves it.",
    }
}

fn assessment_color(a: Assessment) -> Color {
    match a {
        Assessment::Agree => Color::Green,
        Assessment::Partly => Color::Yellow,
        Assessment::Disagree => Color::Red,
        Assessment::Answer => Color::Cyan,
    }
}

const LOCATION_WIDTH: usize = 24;

/// The notes above the list: what Claude read, and whether the threads moved on since.
fn evaluation_notes(e: &Evaluation, state: Option<&ItemState>, width: usize) -> Vec<Line<'static>> {
    let title = state.map(|s| s.title.clone()).unwrap_or_default();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(" Responses to {}", e.item.label()),
            Style::new().bold().fg(ACCENT),
        ),
        Span::styled(fit(&format!(" · {title}"), width.saturating_sub(20)), DIM),
    ])];
    lines.push(Line::styled(
        fit(
            &format!(
                " Claude read {} at {} · {} ago",
                app_tilde(&e.dir),
                e.local_head.get(..10).unwrap_or(&e.local_head),
                ago(Some(e.created))
            ),
            width,
        ),
        DIM,
    ));
    let behind = e
        .head
        .as_deref()
        .is_some_and(|h| !h.eq_ignore_ascii_case(&e.local_head));
    if behind {
        lines.push(Line::styled(
            " The checkout was not at the PR's head commit, so check the plans against the latest code.",
            Style::new().fg(Color::Yellow),
        ));
    }
    let newer = state
        .and_then(|s| s.last_comment.as_ref())
        .is_some_and(|c| c.date > e.created);
    if newer {
        lines.push(Line::styled(
            " There are new comments since Claude read the threads. R asks again.",
            Style::new().fg(Color::Yellow),
        ));
    }
    lines
}

fn app_tilde(path: &std::path::Path) -> String {
    ctx_core::paths::tilde(path)
}

fn response_row(e: &Evaluation, i: usize, cursor: usize, width: usize) -> Line<'static> {
    let r = &e.responses[i];
    let check = match (r.posted, r.selected) {
        (true, _) => Span::styled("[✓] ", Style::new().fg(Color::Green)),
        (false, true) => Span::raw("[x] "),
        (false, false) => Span::raw("[ ] "),
    };
    let marker = if i == cursor { " > " } else { "   " };
    let resolve = match (r.resolve, r.resolved) {
        (_, true) => " · resolved",
        (true, false) => " · resolves",
        (false, false) => "",
    };
    let handed = format!("{resolve}{}", if r.handed { " → Claude" } else { "" });
    let rest = width.saturating_sub(3 + 4 + 9 + LOCATION_WIDTH + 1 + handed.width());
    let mut line = Line::from(vec![
        Span::raw(marker),
        check,
        Span::styled(
            fit(r.assessment.label(), 9),
            Style::new().fg(assessment_color(r.assessment)).bold(),
        ),
        Span::styled(
            fit(r.location.as_deref().unwrap_or("on the PR"), LOCATION_WIDTH),
            DIM,
        ),
        Span::raw(" "),
        Span::raw(fit(&r.summary, rest)),
        Span::styled(handed, DIM),
    ]);
    if i == cursor {
        line = line.style(SELECTED);
    }
    line
}

/// The thread, Claude's judgment, its plan and the draft reply, for the response at `cursor`.
fn response_detail(
    e: &Evaluation,
    cursor: usize,
    viewer: Viewer,
    width: usize,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let Some(r) = e.responses.get(cursor) else {
        return Vec::new();
    };
    let mut lines = conversation::lines(&r.thread, viewer, Show::All, width, now);
    lines.push(Line::raw(""));
    lines.push(rule("Assessment", width));
    lines.push(Line::from(vec![
        Span::styled(
            r.assessment.label().to_string(),
            Style::new().fg(assessment_color(r.assessment)).bold(),
        ),
        Span::styled(" · ", DIM),
        Span::raw(r.summary.clone()),
    ]));
    lines.push(Line::raw(""));
    lines.push(rule("Plan", width));
    if r.plan.trim().is_empty() {
        lines.push(Line::styled("No code change.", DIM));
    } else {
        lines.extend(markdown::render(&r.plan, width));
        if r.handed {
            lines.push(Line::styled(
                "Handed to Claude in the project's session.",
                DIM,
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(rule(
        if r.posted {
            "Reply, posted"
        } else {
            "Draft reply"
        },
        width,
    ));
    lines.extend(markdown::render(&r.reply, width));
    lines.push(Line::raw(""));
    lines.push(Line::styled(resolve_sentence(r), Style::new().fg(ACCENT)));
    lines
}

fn draw_responses(frame: &mut Frame, app: &App, view: &ResponseView, area: Rect) {
    let e = &view.evaluation;
    let width = usize::from(area.width);
    let state = app.state(&e.item);
    let notes = evaluation_notes(e, state, width);
    let list_height = u16::try_from(e.responses.len())
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(notes.len() as u16 + 2) / 2)
        .max(1);
    let [notes_area, list_area, rule_area, body_area] = Layout::vertical([
        Constraint::Length(notes.len() as u16 + 1),
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(notes), notes_area);
    let first = view
        .cursor
        .saturating_sub(usize::from(list_height).saturating_sub(1));
    let rows: Vec<Line> = (first..e.responses.len())
        .take(usize::from(list_height))
        .map(|i| response_row(e, i, view.cursor, width))
        .collect();
    frame.render_widget(Paragraph::new(rows), list_area);
    let job = app.responding.get(&e.item.key());
    frame.render_widget(
        Paragraph::new(rule(&post_line(e, view.cursor, job), width)),
        rule_area,
    );
    let body_area = Rect {
        x: body_area.x + 1,
        width: body_area.width.saturating_sub(2),
        ..body_area
    };
    let viewer = Viewer {
        me: state.and_then(|s| s.viewer.as_deref()),
        author: state.map_or("", |s| s.author.as_str()),
    };
    let lines = response_detail(
        e,
        view.cursor,
        viewer,
        usize::from(body_area.width),
        Utc::now(),
    );
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(body_area.width)).unwrap_or(u16::MAX);
    app.detail_size.set((total, body_area.height));
    let scroll = app
        .detail_scroll
        .min(total.saturating_sub(body_area.height));
    frame.render_widget(paragraph.scroll((scroll, 0)), body_area);
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let scope = if app.search_all || app.own_search.is_none() {
        "all projects".to_string()
    } else {
        app.own_search.clone().unwrap_or_default()
    };
    let mut prompt = vec![
        Span::styled(" Search ", Style::new().bold()),
        Span::styled(format!("{scope}: "), DIM),
    ];
    let prompt_width = " Search ".len() + scope.width() + 2;
    prompt.extend(input_spans(
        &app.search_input,
        true,
        width.saturating_sub(prompt_width + 1),
    ));
    let mut lines = vec![Line::from(prompt), Line::raw("")];
    if let Some(e) = &app.search_error {
        lines.push(Line::styled(format!("   {e}"), Style::new().fg(Color::Red)));
    } else if app.hits.is_empty() && !app.search_input.text.trim().is_empty() {
        lines.push(Line::styled("   no matches", DIM));
    }
    let mut selected_line = 0;
    for (i, hit) in app.hits.iter().enumerate() {
        let is = i == app.search_selected;
        if is {
            selected_line = lines.len();
        }
        let mut head = Line::from(vec![
            Span::raw(if is { " > " } else { "   " }),
            Span::styled(fit(&hit.project, 18), Style::new().fg(ACCENT)),
            Span::raw(" "),
            Span::styled(fit(&hit.kind, 8), DIM),
            Span::styled(
                fit(&hit.title, width.saturating_sub(31)),
                Style::new().bold(),
            ),
        ]);
        if is {
            head = head.style(SELECTED);
        }
        lines.push(head);
        let mut snippet = vec![Span::raw("     ")];
        snippet.extend(highlighted(&hit.snippet));
        lines.push(Line::from(snippet));
    }
    let height = area.height as usize;
    let offset = (selected_line + 3).saturating_sub(height);
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 3,
        width,
        height,
    }
}

fn field(label: &str, input: &Input, focused: bool, width: usize) -> Line<'static> {
    let label_style = if focused {
        Style::new().fg(ACCENT).bold()
    } else {
        DIM
    };
    let mut spans = vec![Span::styled(format!(" {label:<12}"), label_style)];
    spans.extend(input_spans(input, focused, width.saturating_sub(16)));
    Line::from(spans)
}

/// The text of a field, with the cursor drawn as a reversed cell while the field has focus.
fn input_spans(input: &Input, focused: bool, width: usize) -> Vec<Span<'static>> {
    let view = input_view(&input.text, input.cursor, width);
    if !focused {
        return vec![Span::raw(format!(
            "{}{}{}",
            view.before, view.at, view.after
        ))];
    }
    let at = if view.at.is_empty() {
        " ".to_string()
    } else {
        view.at
    };
    vec![
        Span::raw(view.before),
        Span::styled(at, Style::new().add_modifier(Modifier::REVERSED)),
        Span::raw(view.after),
    ]
}

#[derive(Debug, PartialEq)]
struct InputView {
    before: String,
    at: String,
    after: String,
}

/// The part of the text that fits in `width` columns around the cursor, split at the cursor.
/// An ellipsis stands for text cut off on either side.
fn input_view(text: &str, cursor: usize, width: usize) -> InputView {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let cell = |c: &char| c.width().unwrap_or(0);
    let at_end = (cursor + 1).min(chars.len());
    let at_width = chars.get(cursor).map(cell).unwrap_or(1).max(1);
    let total: usize = chars.iter().map(cell).sum::<usize>() + usize::from(cursor == chars.len());
    if total <= width {
        return InputView {
            before: chars[..cursor].iter().collect(),
            at: chars[cursor..at_end].iter().collect(),
            after: chars[at_end..].iter().collect(),
        };
    }
    let room = width.saturating_sub(at_width + 1 + usize::from(at_end < chars.len()));
    let mut start = cursor;
    let mut used = 0;
    while start > 0 && used + cell(&chars[start - 1]) <= room {
        start -= 1;
        used += cell(&chars[start]);
    }
    let mut rest = room - used + usize::from(start == 0);
    let mut end = at_end;
    while end < chars.len() && cell(&chars[end]) <= rest {
        rest -= cell(&chars[end]);
        end += 1;
    }
    let ellipsis = |cut: bool| if cut { "…" } else { "" };
    InputView {
        before: format!(
            "{}{}",
            ellipsis(start > 0),
            chars[start..cursor].iter().collect::<String>()
        ),
        at: chars[cursor..at_end].iter().collect(),
        after: format!(
            "{}{}",
            chars[at_end..end].iter().collect::<String>(),
            ellipsis(end < chars.len())
        ),
    }
}

fn modal_block(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
}

fn draw_modal(frame: &mut Frame, app: &App, modal: &Modal) {
    let area = frame.area();
    let (title, lines, height): (String, Vec<Line>, u16) = match modal {
        Modal::Freeze(form) => freeze_lines(app, form, area.width.min(90) as usize),
        Modal::FreezeClaude(form) => freeze_claude_lines(form),
        Modal::Wait(picker) => wait_lines(app, picker),
        Modal::NewProject(form) => {
            let mut lines = vec![
                field("Name", &form.name, form.focus == 0, 90),
                field("Directory", &form.dir, form.focus == 1, 90),
                Line::raw(""),
                Line::styled(
                    match &form.item {
                        Some(item) => format!(
                            " The project starts with {}. Tab moves.",
                            item.describe()
                        ),
                        None => " In a git repository, the current branch becomes the first item. Tab moves."
                            .to_string(),
                    },
                    DIM,
                ),
            ];
            if let Some(e) = &form.error {
                lines.push(Line::styled(format!(" {e}"), Style::new().fg(Color::Red)));
            }
            ("New project".into(), lines, 6)
        }
        Modal::Assign(picker) => assign_lines(app, picker),
        Modal::AddItem { input, error, .. } => {
            let mut lines = vec![
                field("Item", input, true, 80),
                Line::raw(""),
                Line::styled(
                    " A URL, a path, pr 123, #456 (work item), parser!78, !78, branch or branch:name",
                    DIM,
                ),
            ];
            if let Some(e) = error {
                lines.push(Line::styled(format!(" {e}"), Style::new().fg(Color::Red)));
            }
            ("Add item".into(), lines, 6)
        }
        Modal::Next { input, .. } => ("Next step".into(), vec![field("Next", input, true, 80)], 3),
        Modal::Import { input, .. } => (
            "Import from Teams or email".into(),
            vec![
                field("Find", input, true, 80),
                Line::raw(""),
                Line::styled(
                    " A Teams or Outlook link, or words to search for. Claude summarizes it into a note.",
                    DIM,
                ),
            ],
            5,
        ),
        Modal::Capture(form) => capture_lines(form),
        Modal::Confirm(confirm) => {
            let text = match confirm {
                Confirm::Done(slug) => {
                    format!(" Mark {slug} done? It leaves the home screen but stays searchable.")
                }
                Confirm::Remove(_, key) => format!(" Remove {key} from the project?"),
            };
            (
                "Confirm".into(),
                vec![
                    Line::raw(text),
                    Line::raw(""),
                    Line::styled(" y confirms, any other key cancels", DIM),
                ],
                5,
            )
        }
        Modal::Help => {
            let lines = help_lines();
            let height = lines.len() as u16;
            ("Keys".into(), lines, height)
        }
    };
    let rect = centered(area, area.width.min(96), height + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(modal_block(&title)), rect);
}

fn freeze_claude_lines(form: &FreezeClaudeForm) -> (String, Vec<Line<'static>>, u16) {
    let mut lines = vec![field("Note", &form.note, !form.confirm, 80), Line::raw("")];
    let warn = Style::new().fg(Color::Yellow);
    if form.confirm {
        lines.push(Line::styled(
            " Claude is working. Freezing ends it now and loses the current turn. y confirms.",
            warn,
        ));
    } else if form.busy {
        lines.push(Line::styled(
            " Claude is working. Freezing interrupts the current turn.",
            warn,
        ));
    } else {
        lines.push(Line::styled(
            " ctx ends Claude and keeps the session here. t resumes it later.",
            DIM,
        ));
    }
    let height = lines.len() as u16 + 2;
    (
        format!("Freeze Claude session {}", form.label),
        lines,
        height,
    )
}

fn freeze_lines(app: &App, form: &FreezeForm, width: usize) -> (String, Vec<Line<'static>>, u16) {
    let name = app
        .project(&form.slug)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let condition = match &form.condition {
        Some(c) => c.describe(),
        None if !form.waiting.text.trim().is_empty() => {
            format!("{} (no check)", form.waiting.text.trim())
        }
        None => "none".into(),
    };
    let close = if form.close {
        "[x] close tmux session"
    } else {
        "[ ] close tmux session"
    };
    let lines = vec![
        field("Doing", &form.doing, form.focus == 0, width),
        field("Next", &form.next, form.focus == 1, width),
        field("Waiting on", &form.waiting, form.focus == 2, width),
        Line::from(vec![
            Span::styled(
                format!(" {:<12}", "Condition"),
                if form.focus == 3 {
                    Style::new().fg(ACCENT).bold()
                } else {
                    DIM
                },
            ),
            Span::raw(condition),
            Span::styled(if form.focus == 3 { "  space picks" } else { "" }, DIM),
        ]),
        Line::from(vec![
            Span::styled(format!(" {:<12}", ""), DIM),
            Span::styled(
                close,
                if form.focus == 4 {
                    Style::new().fg(ACCENT).bold()
                } else {
                    Style::new()
                },
            ),
            Span::styled(
                if form.focus == 4 {
                    "  space toggles"
                } else {
                    ""
                },
                DIM,
            ),
        ]),
        Line::raw(""),
        Line::styled(
            " The layout is saved either way. Enter freezes, Esc cancels.",
            DIM,
        ),
    ];
    (format!("Freeze {name}"), lines, 7)
}

fn wait_lines(app: &App, picker: &WaitPicker) -> (String, Vec<Line<'static>>, u16) {
    let name = app
        .project(&picker.slug)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let mut lines: Vec<Line> = Vec::new();
    if let Some((kind, input)) = &picker.asking {
        let label = match kind {
            AskKind::Until => "Until",
            AskKind::Manual => "Waiting on",
        };
        lines.push(field(label, input, true, 80));
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            match kind {
                AskKind::Until => " 30m, 2h, 3d, a weekday such as thu, or 2026-11-20",
                AskKind::Manual => " Free text. ctx shows it but cannot check it.",
            },
            DIM,
        ));
    } else {
        for (i, (label, _)) in picker.options.iter().enumerate() {
            let mut line = Line::raw(format!(
                "{}{label}",
                if i == picker.selected { " > " } else { "   " }
            ));
            if i == picker.selected {
                line = line.style(SELECTED);
            }
            lines.push(line);
        }
    }
    if let Some(e) = &picker.error {
        lines.push(Line::styled(format!(" {e}"), Style::new().fg(Color::Red)));
    }
    let height = lines.len().min(24) as u16;
    (format!("Wait in {name}"), lines, height)
}

fn assign_lines(app: &App, picker: &AssignPicker) -> (String, Vec<Line<'static>>, u16) {
    let mut lines = vec![field("Project", &picker.filter, true, 80), Line::raw("")];
    let choices = app.assign_choices(picker);
    if choices.is_empty() {
        lines.push(Line::styled(" No project matches.", DIM));
    }
    for (i, project) in choices.iter().enumerate().take(18) {
        let mut line = Line::from(vec![
            Span::raw(if i == picker.selected { " > " } else { "   " }),
            Span::styled(fit(&project.name, NAME_WIDTH + 10), Style::new().bold()),
            Span::styled(format!(" {}", project.state.label()), DIM),
        ]);
        if i == picker.selected {
            line = line.style(SELECTED);
        }
        lines.push(line);
    }
    let height = lines.len() as u16;
    (
        format!("Add {} to a project", picker.item.describe()),
        lines,
        height,
    )
}

fn capture_lines(form: &CaptureForm) -> (String, Vec<Line<'static>>, u16) {
    let mut sources: Vec<Span> = vec![Span::styled(
        format!(" {:<12}", "Source"),
        if form.focus == 1 {
            Style::new().fg(ACCENT).bold()
        } else {
            DIM
        },
    )];
    for (i, s) in store::SOURCES.iter().enumerate() {
        let style = if i == form.source {
            Style::new().fg(Color::Black).bg(Color::Magenta)
        } else {
            DIM
        };
        sources.push(Span::styled(format!(" {s} "), style));
        sources.push(Span::raw(" "));
    }
    let mut lines = vec![
        field("Title", &form.title, form.focus == 0, 80),
        Line::from(sources),
        field("Link", &form.link, form.focus == 2, 80),
        Line::raw(""),
    ];
    for l in form.body.lines().take(6) {
        lines.push(Line::styled(format!(" │ {}", fit(l, 76)), DIM));
    }
    let more = form.body.lines().count().saturating_sub(6);
    if more > 0 {
        lines.push(Line::styled(format!(" │ … {more} more lines"), DIM));
    }
    let height = lines.len() as u16;
    ("Capture clipboard".into(), lines, height)
}

fn help_lines() -> Vec<Line<'static>> {
    let rows = [
        ("Home", ""),
        ("⏎", "switch to the project's tmux session"),
        ("o, space", "open the project screen here"),
        (
            "f / t / d",
            "freeze, thaw, done (t also reopens a done project)",
        ),
        ("w / W", "set a wait condition / remove it"),
        (
            "n",
            "new project, in the directory of the pane you came from",
        ),
        (
            "c / N / I",
            "capture the clipboard / write a note / import from Teams or email",
        ),
        ("/", "search notes, handoffs and items"),
        ("D", "show done projects"),
        ("R", "refresh every item now, and look for unfiled ones"),
        (
            "a / n",
            "on an unfiled item: add it to a project / start a project with it",
        ),
        (
            "esc",
            "close the popup, or go back to this session's project",
        ),
        ("", ""),
        ("Project", ""),
        (
            "⏎ or o",
            "open the item in the browser, or the note or file in $EDITOR",
        ),
        (
            "a / x",
            "add an item / remove it (on a suggestion: add / hide)",
        ),
        ("v", "read the selected item or note full screen"),
        ("J / K", "scroll the detail pane"),
        ("u", "show or hide resolved comment threads"),
        (
            "r",
            "Claude judges the threads waiting on you, plans fixes and drafts replies",
        ),
        (
            "",
            "in the responses: p posts the reply, r resolves the thread too, space checks several, C hands plans to Claude",
        ),
        ("e", "edit the next step"),
        (
            "f / t",
            "on a Claude session: freeze it (ends Claude, keeps it to resume) / resume it",
        ),
        ("s", "switch to this project's tmux session"),
        (
            "C",
            "start Claude beside the current window, or in a new window if Claude runs",
        ),
        ("", ""),
        ("Glyphs", ""),
        ("◆", "live"),
        ("◌", "waiting on a condition"),
        ("●", "ready: the wait cleared, or frozen with no wait"),
        (
            "✓ ✗ ◐ ◷ ●",
            "item is good, failing, running, needs a look, open",
        ),
        (
            "⚠ ✓✗◐ ↩2",
            "after a PR's status: conflicts, build passed/failed/running, threads awaiting your reply",
        ),
        ("", ""),
        ("j k gg G", "move; Ctrl-d Ctrl-u page"),
        ("q", "quit"),
    ];
    rows.iter()
        .map(|(key, text)| {
            if text.is_empty() {
                Line::styled(format!(" {key}"), Style::new().add_modifier(Modifier::BOLD))
            } else {
                Line::from(vec![
                    Span::styled(format!("   {key:<12}"), Style::new().fg(ACCENT)),
                    Span::raw(text.to_string()),
                ])
            }
        })
        .collect()
}

#[cfg(test)]
mod input_tests {
    use super::*;

    fn view(text: &str, cursor: usize, width: usize) -> (String, String, String) {
        let v = input_view(text, cursor, width);
        (v.before, v.at, v.after)
    }

    fn parts(before: &str, at: &str, after: &str) -> (String, String, String) {
        (before.into(), at.into(), after.into())
    }

    #[test]
    fn the_cursor_splits_the_text_where_it_sits() {
        assert_eq!(view("pr 123", 6, 40), parts("pr 123", "", ""));
        assert_eq!(view("pr 123", 2, 40), parts("pr", " ", "123"));
        assert_eq!(view("pr 123", 0, 40), parts("", "p", "r 123"));
    }

    #[test]
    fn long_text_scrolls_to_keep_the_cursor_in_view() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(view(text, 26, 10), parts("…stuvwxyz", "", ""));
        assert_eq!(view(text, 0, 10), parts("", "a", "bcdefghi…"));
        assert_eq!(view(text, 13, 10), parts("…ghijklm", "n", "…"));
    }
}

#[cfg(test)]
mod flag_tests {
    use super::*;

    fn text(state: Option<&ItemState>) -> String {
        flag_spans(state)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn flags_mark_conflicts_builds_and_replies_in_a_fixed_width() {
        let mut s: ItemState = serde_json::from_value(serde_json::json!({
            "title": "t", "status": "Active", "open": true, "fetched": "2026-11-17T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(text(None), "       ");
        assert_eq!(text(Some(&s)), "       ");
        s.conflicts = true;
        s.ci = Some(Ci::Failed);
        s.replies_due = 2;
        assert_eq!(text(Some(&s)), "⚠ ✗ ↩2 ");
        s.replies_due = 12345;
        assert_eq!(text(Some(&s)).width(), FLAGS_WIDTH);
        let words: String = flag_words(&s).iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            words,
            "conflicts with its target · build failed · 12345 threads wait for your reply"
        );
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;
    use forge::model::{Comment, ThreadStatus};

    fn state(text: &str) -> ItemState {
        let mut s: ItemState = serde_json::from_value(serde_json::json!({
            "title": "t", "status": "Active", "open": true, "fetched": "2026-11-17T00:00:00Z"
        }))
        .unwrap();
        s.text = text.into();
        s
    }

    fn comment(login: &str, body: &str, is_reply: bool, status: ThreadStatus) -> Comment {
        Comment {
            login: login.into(),
            author: login.to_uppercase(),
            date: Utc::now(),
            location: None,
            body: body.into(),
            is_reply,
            status,
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn evaluation() -> Evaluation {
        use ctx_core::respond::Response;
        let response = Response {
            thread_id: "41".into(),
            location: Some("src/parse.cpp:12".into()),
            thread: vec![comment("bob", "Use a **set**.", false, ThreadStatus::Open)],
            assessment: Assessment::Partly,
            summary: "A set is right for lookups".into(),
            plan: "- swap the map in `parse()`".into(),
            reply: "I'll switch to a set.".into(),
            selected: true,
            posted: false,
            handed: false,
            resolve: false,
            resolved: false,
        };
        Evaluation {
            item: ItemRef::AdoPr { id: 7 },
            created: Utc::now(),
            dir: std::path::PathBuf::from("/src/lib"),
            local_head: "abc".into(),
            head: Some("abc".into()),
            responses: vec![
                response.clone(),
                Response {
                    plan: String::new(),
                    posted: true,
                    assessment: Assessment::Answer,
                    ..response
                },
            ],
        }
    }

    #[test]
    fn a_response_shows_the_thread_then_the_assessment_plan_and_reply() {
        let viewer = Viewer {
            me: Some("me"),
            author: "me",
        };
        let text = plain(&response_detail(&evaluation(), 0, viewer, 60, Utc::now())).join("\n");
        let order = [
            "BOB",
            "Use a set.",
            "Assessment",
            "partly · A set is right for lookups",
            "Plan",
            "swap the map in parse()",
            "Draft reply",
            "I'll switch to a set.",
        ];
        let mut from = 0;
        for want in order {
            let at = text[from..]
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} after {from} in:\n{text}"));
            from += at + want.len();
        }
        assert!(!text.contains("No code change."), "{text}");

        let answered = plain(&response_detail(&evaluation(), 1, viewer, 60, Utc::now())).join("\n");
        assert!(answered.contains("No code change."), "{answered}");
        assert!(answered.contains("Reply, posted"), "{answered}");
    }

    #[test]
    fn response_rows_show_checks_posted_replies_and_the_assessment() {
        let e = evaluation();
        let rows: Vec<String> = (0..2)
            .map(|i| plain(&[response_row(&e, i, 0, 80)]).remove(0))
            .collect();
        assert!(rows[0].starts_with(" > [x] partly"), "{rows:?}");
        assert!(rows[0].contains("src/parse.cpp:12"), "{rows:?}");
        assert!(rows[1].starts_with("   [✓] answer"), "{rows:?}");
    }

    #[test]
    fn the_responses_say_what_p_posts_and_whether_it_resolves() {
        let mut e = evaluation();
        assert_eq!(confirm_question(&e, 0), "Post 1 reply on PR 7?");
        assert_eq!(post_line(&e, 0, None), "2 checked · p posts them");
        e.responses[0].resolve = true;
        assert_eq!(
            confirm_question(&e, 0),
            "Post 1 reply and resolve 1 thread on PR 7?"
        );
        let row = plain(&[response_row(&e, 0, 0, 80)]).remove(0);
        assert!(row.ends_with(" · resolves"), "{row:?}");
        let viewer = Viewer {
            me: Some("me"),
            author: "me",
        };
        let detail = plain(&response_detail(&e, 0, viewer, 60, Utc::now()));
        assert_eq!(
            detail.last().map(String::as_str),
            Some("Posting also resolves the thread. r leaves it open.")
        );

        for r in &mut e.responses {
            r.selected = false;
        }
        assert_eq!(
            post_line(&e, 0, None),
            "p posts this reply · space checks several"
        );
        assert_eq!(post_line(&e, 1, None), "this reply is posted");
        e.responses[1].resolve = true;
        assert_eq!(confirm_question(&e, 1), "Resolve 1 thread on PR 7?");
        assert_eq!(post_line(&e, 1, None), "p resolves this thread");
        e.responses[1].resolved = true;
        assert_eq!(
            resolve_sentence(&e.responses[1]),
            "Posted, and the thread is resolved."
        );
        e.responses[0].resolve = false;
        assert_eq!(
            resolve_sentence(&e.responses[0]),
            "Posting leaves the thread open. r resolves it as well."
        );
    }

    #[test]
    fn claudes_work_on_the_threads_shows_in_the_row_the_detail_and_the_header() {
        let mut app = App::new(None, false);
        let pr = ItemRef::AdoPr { id: 7 };
        let shown = |app: &App| {
            (
                respond_tag(app, &pr).map(|s| s.content.to_string()),
                respond_line(app, &pr).map(|l| plain(&[l]).remove(0)),
            )
        };
        assert_eq!(shown(&app), (None, None));

        app.drafts.insert(pr.key(), 2);
        let (tag, line) = shown(&app);
        assert_eq!(tag.as_deref(), Some("2 drafts · "));
        assert_eq!(
            line.as_deref(),
            Some("Claude drafted 2 replies that are not posted yet. r opens them.")
        );

        app.respond_failures.insert(pr.key(), "no checkout".into());
        let (tag, line) = shown(&app);
        assert_eq!(tag.as_deref(), Some("Claude failed · "));
        assert_eq!(
            line.as_deref(),
            Some("Claude could not answer the threads: no checkout. r tries again.")
        );

        app.start_job(&pr, "loading the threads");
        let (tag, line) = shown(&app);
        assert_eq!(tag.as_deref(), Some("Claude working · "));
        assert_eq!(
            line.as_deref(),
            Some("Claude: loading the threads · 0s. The row says when it is done.")
        );
        let header = plain(&[header_line(&app, 0, 120)]).remove(0);
        assert!(
            header.ends_with("⠋ PR 7: loading the threads · 0s "),
            "{header:?}"
        );
    }

    #[test]
    fn the_notes_warn_when_the_checkout_or_the_threads_moved_on() {
        let mut e = evaluation();
        let quiet = plain(&evaluation_notes(&e, None, 100));
        assert_eq!(quiet.len(), 2, "{quiet:?}");
        e.head = Some("def".into());
        let mut s = state("");
        s.last_comment = Some(ctx_core::item::LastComment {
            author: "bob".into(),
            date: e.created + chrono::Duration::minutes(5),
            snippet: String::new(),
        });
        let warned = plain(&evaluation_notes(&e, Some(&s), 100)).join("\n");
        assert!(warned.contains("not at the PR's head"), "{warned}");
        assert!(warned.contains("new comments since"), "{warned}");
    }

    #[test]
    fn a_pr_shows_its_description_as_markdown_and_its_threads_as_cards() {
        let mut s = state("flattened text");
        s.description = "## Plan\n\n- one".into();
        s.author = "me".into();
        s.viewer = Some("me".into());
        s.conversation = vec![
            comment("bob", "Why **this**?", false, ThreadStatus::Open),
            comment("me", "Because.", true, ThreadStatus::Open),
            comment("cy", "old news", false, ThreadStatus::Resolved),
        ];
        let pr = ItemRef::AdoPr { id: 1 };
        let open = plain(&item_body(&pr, &s, Show::Unresolved, 50));
        for wanted in [
            "── Description ",
            "Plan",
            "• one",
            "╭─ ● Open",
            "│ ↳ ME (you) · 0m",
        ] {
            assert!(
                open.iter().any(|l| l.starts_with(wanted)),
                "{wanted:?} in {open:#?}"
            );
        }
        assert!(!open.iter().any(|l| l.contains("flattened")), "{open:#?}");
        assert!(!open.iter().any(|l| l.contains("old news")), "{open:#?}");
        let all = plain(&item_body(&pr, &s, Show::All, 50));
        assert!(all.iter().any(|l| l.contains("old news")), "{all:#?}");
    }

    #[test]
    fn a_state_cached_before_threads_shows_its_text() {
        let s = state("raw **text**");
        let lines = item_body(&ItemRef::AdoPr { id: 1 }, &s, Show::All, 50);
        assert_eq!(plain(&lines), ["raw **text**"]);
    }

    #[test]
    fn markdown_files_render_and_other_files_stay_as_written() {
        let s = state("# Title\n\n**bold**");
        let file = |name: &str| ItemRef::File {
            path: std::path::PathBuf::from(name),
        };
        assert_eq!(
            plain(&item_body(&file("plan.md"), &s, Show::All, 50)),
            ["Title", "", "bold"]
        );
        assert_eq!(
            plain(&item_body(&file("main.rs"), &s, Show::All, 50)),
            ["# Title", "", "**bold**"]
        );
    }
}
