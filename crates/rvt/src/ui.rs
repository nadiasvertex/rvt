use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Approval, DetailState, Job, Mode, Pane, Row, View};
use crate::model::{
    Category, Cause, Diagnosis, Item, Review, Severity, Source, Status, age, short_title,
};
use view::conversation::{self, Viewer};
use view::markdown;
use view::text::{fit, rule};
use view::theme::{ACCENT, DIM, MUTED, PANEL};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const AUTHOR_WIDTH: usize = 10;
const STATUS_WIDTH: usize = 10;

/// The two panes overlap by one column and share it as their divider.
const BOARD_BORDER: border::Set = border::Set {
    top_right: "┬",
    bottom_right: "┴",
    ..border::ROUNDED
};
const DETAIL_BORDER: border::Set = border::Set {
    top_left: "┬",
    bottom_left: "┴",
    ..border::ROUNDED
};

fn status_color(status: Status) -> Color {
    match status {
        Status::Draft => MUTED,
        Status::Active => Color::Blue,
        Status::Approved | Status::Ready => Color::Green,
        Status::Waiting => Color::Yellow,
        Status::Rejected | Status::CiFailed => Color::Red,
        Status::CiRunning => Color::Cyan,
        Status::Conflicts => Color::Magenta,
    }
}

fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::High => Color::Red,
        Severity::Medium => Color::Yellow,
        Severity::Low => Color::Blue,
    }
}

/// Two columns at the end of a row: a spinner while a review runs, `R` once one exists, and `P`
/// once any of its findings are posted.
fn review_badge(app: &App, item: &Item, tick: usize) -> Span<'static> {
    let key = item.key();
    match (app.jobs.get(&key), app.reviews.get(&key)) {
        (Some(Job::Queued), _) => Span::styled(" ·", DIM),
        (Some(Job::Running(_)), _) => Span::styled(
            format!(" {}", SPINNER[tick % SPINNER.len()]),
            Style::new().fg(ACCENT),
        ),
        (Some(Job::Failed(_)), _) => Span::styled(" !", Style::new().fg(Color::Red)),
        (None, Some(review)) if review.is_stale(item) => Span::styled(review_letter(review), DIM),
        (None, Some(review)) => Span::styled(
            review_letter(review),
            match review.worst() {
                Some(s) => Style::new().fg(severity_color(s)).bold(),
                None => Style::new().fg(Color::Green),
            },
        ),
        (None, None) => Span::raw("  "),
    }
}

fn review_letter(review: &Review) -> &'static str {
    if review.any_posted() { " P" } else { " R" }
}

/// `↩N` when N threads on the item wait for your reply, otherwise nothing.
fn waiting_marker(app: &App, item: &Item) -> Option<Span<'static>> {
    let count = app.waiting(item).len();
    (count > 0).then(|| Span::styled(format!(" ↩{count}"), Style::new().fg(Color::Yellow).bold()))
}

/// The first line of a comment, with runs of whitespace collapsed.
fn snippet(body: &str) -> String {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn short_source(source: Source) -> &'static str {
    match source {
        Source::Ado => "ADO",
        Source::GitLab => "GitLab",
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, tick: usize) {
    let [header, body, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [board, detail] = Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)])
        .spacing(-1)
        .areas(body);

    draw_header(frame, app, header, tick);
    // The focused pane draws last so its color wins on the shared divider.
    if app.focus == Pane::Board {
        draw_detail(frame, app, detail, tick);
        draw_board(frame, app, board, tick);
    } else {
        draw_board(frame, app, board, tick);
        draw_detail(frame, app, detail, tick);
    }
    draw_hints(frame, app, hints, tick);
    if let Some(item) = app
        .confirm_approve
        .as_ref()
        .and_then(|key| app.items.iter().find(|i| &i.key() == key))
    {
        draw_approve_dialog(frame, item);
    }
    if app.show_help {
        draw_help(frame);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, tick: usize) {
    let mut left = vec![
        Span::styled(" rvt ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  "),
    ];
    for (i, source) in app.sources.iter().enumerate() {
        if i > 0 {
            left.push(Span::styled(" · ", DIM));
        }
        left.push(Span::raw(format!("{} ", short_source(*source))));
        if app.loading.contains(source) {
            left.push(Span::styled(
                SPINNER[tick % SPINNER.len()],
                Style::new().fg(ACCENT),
            ));
        } else if app.errors.contains_key(source) {
            left.push(Span::styled("error", Style::new().fg(Color::Red)));
        } else {
            left.push(Span::raw(app.count(*source).to_string()).bold());
        }
    }
    left.push(Span::styled(" · ", DIM));
    let mine = Span::raw(format!("mine {}", app.mine_count()));
    left.push(if app.mine_only {
        mine.fg(ACCENT).bold()
    } else {
        mine
    });
    left.push(Span::styled(" · ", DIM));
    let review = Span::raw(format!("to review {}", app.review_count()));
    left.push(if app.review_only {
        review.fg(ACCENT).bold()
    } else {
        review
    });

    let mut right = Vec::new();
    match app.mode {
        Mode::Filter => {
            right.push(Span::styled("/", Style::new().fg(ACCENT)));
            right.push(Span::raw(format!("{}▏", app.filter)));
        }
        Mode::Normal if !app.filter.is_empty() => {
            right.push(Span::styled("filter ", DIM));
            right.push(Span::raw(app.filter.clone()).fg(ACCENT));
        }
        Mode::Normal => {}
    }
    right.push(Span::raw("   "));
    right.push(Span::styled(
        match app.refreshed {
            Some(at) => format!("updated {} ago ", age(at, Utc::now())),
            None => "cached ".to_string(),
        },
        DIM,
    ));

    frame.render_widget(Paragraph::new(Line::from(left)), area);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );
}

fn pane_block(title: Line<'static>, set: border::Set<'static>, focused: bool) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_set(set)
        .border_style(if focused {
            Style::new().fg(ACCENT)
        } else {
            DIM
        })
        .title(title)
}

fn draw_board(frame: &mut Frame, app: &mut App, area: Rect, tick: usize) {
    let focused = app.focus == Pane::Board;
    let block = pane_block(Line::from(" Board ").bold(), BOARD_BORDER, focused);
    let inner = block.inner(area);
    app.page = usize::from(inner.height);
    let width = usize::from(inner.width);

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| ListItem::new(row_line(app, row, width, tick)))
        .collect();
    let highlight = if focused {
        Style::new().bg(Color::Indexed(237))
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    let mut state = ListState::default()
        .with_offset(app.board_offset)
        .with_selected(Some(app.cursor));
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight)
        .scroll_padding(2);
    frame.render_stateful_widget(list, area, &mut state);
    app.board_offset = state.offset();
}

fn row_line(app: &App, row: &Row, width: usize, tick: usize) -> Line<'static> {
    match row {
        Row::Spacer => Line::raw(""),
        Row::Section(source) => {
            let mut spans = vec![Span::styled(source.label().to_uppercase(), DIM.bold())];
            if let Some(err) = app.errors.get(source) {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    fit(err, width.saturating_sub(source.label().len() + 2)),
                    Style::new().fg(Color::Red),
                ));
            } else if app.loading.contains(source) && app.count(*source) == 0 {
                spans.push(Span::styled(
                    format!("  {} loading", SPINNER[tick % SPINNER.len()]),
                    DIM,
                ));
            }
            Line::from(spans)
        }
        Row::Group {
            name,
            count,
            folded,
            ..
        } => {
            let marker = if *folded { "▸ " } else { "▾ " };
            let count = count.to_string();
            let name_width = width.saturating_sub(2 + count.len() + 1);
            Line::from(vec![
                Span::styled(marker, DIM),
                Span::raw(fit(name, name_width)).bold(),
                Span::raw(" "),
                Span::styled(count, DIM),
            ])
        }
        Row::Item(i) => {
            let item = &app.items[*i];
            item_line(
                item,
                width,
                waiting_marker(app, item),
                review_badge(app, item, tick),
            )
        }
        Row::Category {
            category,
            count,
            folded,
        } => {
            let color = match category {
                Category::AwaitingApproval => Color::Green,
                Category::AwaitingResponse => Color::Magenta,
            };
            heading_line(&category.label().to_uppercase(), color, *count, *folded)
        }
        Row::Needs { count, folded } => heading_line("NEEDS ME", Color::Yellow, *count, *folded),
        Row::Waiting { item, discussion } => waiting_line(app, *item, discussion, width),
    }
}

fn heading_line(label: &str, color: Color, count: usize, folded: bool) -> Line<'static> {
    let marker = if folded { "▸ " } else { "▾ " };
    Line::from(vec![
        Span::styled(marker, DIM),
        Span::styled(label.to_string(), Style::new().fg(color).bold()),
        Span::raw(" "),
        Span::styled(count.to_string(), DIM),
    ])
}

const REF_WIDTH: usize = 9;
const AGE_WIDTH: usize = 4;

/// One thread in the "Needs me" section: which change, who answered, what they said, and when.
fn waiting_line(app: &App, item: usize, discussion: &str, width: usize) -> Line<'static> {
    let (Some(it), Some(d)) = (app.items.get(item), app.discussion(item, discussion)) else {
        return Line::raw("");
    };
    let Some(last) = d.last() else {
        return Line::raw("");
    };
    let text = match &d.location {
        Some(loc) => format!("{}  · {loc}", snippet(&last.body)),
        None => snippet(&last.body),
    };
    let text_width = width.saturating_sub(4 + REF_WIDTH + 1 + AUTHOR_WIDTH + 1 + 1 + AGE_WIDTH);
    Line::from(vec![
        Span::styled("  ↩ ", Style::new().fg(Color::Yellow)),
        Span::raw(fit(&it.reference(), REF_WIDTH)),
        Span::raw(" "),
        Span::styled(fit(&last.author, AUTHOR_WIDTH), Style::new().bold()),
        Span::raw(" "),
        Span::raw(fit(&text, text_width)),
        Span::raw(" "),
        Span::styled(format!("{:>AGE_WIDTH$}", age(last.date, Utc::now())), DIM),
    ])
}

fn item_line(
    item: &Item,
    width: usize,
    marker: Option<Span<'static>>,
    badge: Span<'static>,
) -> Line<'static> {
    let color = status_color(item.status);
    let marker_width = marker.as_ref().map_or(0, |m| m.content.width());
    let title_width =
        width.saturating_sub(4 + 1 + AUTHOR_WIDTH + 1 + STATUS_WIDTH + marker_width + 2);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(item.status.glyph(), Style::new().fg(color)),
        Span::raw(" "),
        Span::raw(fit(short_title(&item.title), title_width)),
        Span::raw(" "),
        Span::styled(fit(&item.author, AUTHOR_WIDTH), DIM),
        Span::raw(" "),
        Span::styled(
            fit(item.status.label(), STATUS_WIDTH),
            Style::new().fg(color),
        ),
    ];
    spans.extend(marker);
    spans.push(badge);
    Line::from(spans)
}

/// Wraps each line of `text` at word boundaries to fit `width`, prefixing every output line with
/// `indent`. Continuation lines keep the source line's own indent, plus room for a list marker.
pub fn wrap_text(text: &str, width: usize, indent: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let lead_len = line.len() - line.trim_start().len();
        let lead = &line[..lead_len];
        let body = &line[lead_len..];
        let marker = ["- ", "* ", "+ "]
            .iter()
            .find(|m| body.starts_with(**m))
            .map_or(0, |m| m.len());
        let hanging = format!("{indent}{lead}{}", " ".repeat(marker));
        let hanging_width = hanging.width();
        let mut current = format!("{indent}{lead}");
        let mut current_width = current.width();
        let mut has_word = false;
        for word in body.split(' ') {
            let word_width = word.width();
            if has_word && current_width + 1 + word_width > width {
                out.push(std::mem::replace(&mut current, hanging.clone()));
                current_width = hanging_width;
                has_word = false;
            }
            if has_word {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
            has_word = true;
            while current_width > width && width > hanging_width + 1 {
                let split = split_at_width(&current, width);
                let rest = current.split_off(split);
                current_width = hanging_width + rest.width();
                out.push(std::mem::replace(&mut current, format!("{hanging}{rest}")));
            }
        }
        out.push(current);
    }
    out
}

fn split_at_width(text: &str, width: usize) -> usize {
    let mut used = 0;
    for (i, c) in text.char_indices() {
        used += c.width().unwrap_or(0);
        if used > width {
            return i;
        }
    }
    text.len()
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect, tick: usize) {
    let focused = app.focus == Pane::Detail;
    let Some(item) = app.open_item().cloned() else {
        let block = pane_block(Line::from(" Details "), DETAIL_BORDER, focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let hint = Paragraph::new(Line::styled(
            "Press Enter on an item to see its details.",
            DIM,
        ))
        .alignment(Alignment::Center);
        let middle = Rect {
            y: inner.y + inner.height / 2,
            height: 1,
            ..inner
        };
        frame.render_widget(hint, middle);
        return;
    };

    if app.view == View::Findings {
        draw_findings(frame, app, &item, area, tick);
        return;
    }

    let title = Line::from(vec![
        Span::raw(" "),
        Span::raw(item.reference()).bold(),
        Span::styled(format!(" · {} ", item.repo), DIM),
    ]);
    let block = pane_block(title, DETAIL_BORDER, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inputs = DetailInputs::of(app, &item, inner.width, Utc::now());
    if app.detail_cache.inputs.as_ref() != Some(&inputs) {
        app.detail_cache = DetailCache::build(app, inputs);
    }
    let cache = &app.detail_cache;
    let total = u16::try_from(cache.total_rows()).unwrap_or(u16::MAX);
    app.detail_scroll = app.detail_scroll.min(total.saturating_sub(inner.height));
    let (visible, skip) = cache.visible(usize::from(app.detail_scroll), usize::from(inner.height));
    frame.render_widget(
        Paragraph::new(visible.to_vec())
            .wrap(Wrap { trim: false })
            .scroll((skip, 0)),
        inner,
    );
}

/// Everything the detail pane's lines are built from. Comment ages count in minutes, so the
/// minute is an input too.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailInputs {
    item: Item,
    state: Option<DetailState>,
    review: Option<Review>,
    diagnosis: Option<Diagnosis>,
    diagnosing: Option<Job>,
    unresolved_only: bool,
    /// Your login on the item's host, which marks your comments and the threads that wait on you.
    me: Option<String>,
    width: u16,
    minute: i64,
}

impl DetailInputs {
    fn of(app: &App, item: &Item, width: u16, now: chrono::DateTime<Utc>) -> Self {
        let key = item.key();
        DetailInputs {
            item: item.clone(),
            state: app.details.get(&key).cloned(),
            review: app.reviews.get(&key).cloned(),
            diagnosis: app.diagnoses.get(&key).cloned(),
            diagnosing: app.diagnosing.get(&key).cloned(),
            unresolved_only: app.unresolved_only,
            me: app.me.get(&item.source).cloned(),
            width,
            minute: now.timestamp().div_euclid(60),
        }
    }
}

/// The detail pane's lines, kept between frames because wrapping a long thread is slow.
#[derive(Debug, Default)]
pub struct DetailCache {
    inputs: Option<DetailInputs>,
    lines: Vec<Line<'static>>,
    /// The first screen row of each line once wrapped to the pane, then the total row count.
    starts: Vec<usize>,
}

impl DetailCache {
    fn build(app: &App, inputs: DetailInputs) -> Self {
        let item = &inputs.item;
        let width = usize::from(inputs.width);
        let viewer = Viewer {
            me: inputs.me.as_deref(),
            author: &item.author,
        };
        let show = if inputs.unresolved_only {
            conversation::Show::Unresolved
        } else {
            conversation::Show::All
        };
        let mut lines = detail_lines(item, inputs.state.as_ref(), viewer, show, width);
        // After the title, author and status lines, and the blank line that follows them.
        lines.splice(4..4, ci_lines(app, item, width));
        if let Some(review) = &inputs.review {
            let stale = if review.is_stale(item) { ", stale" } else { "" };
            lines[2].spans.push(Span::styled(
                format!("   review {}{stale}", review.summary()),
                DIM,
            ));
        }
        let starts = std::iter::once(0)
            .chain(lines.iter().scan(0, |row, line| {
                *row += wrapped_rows(line, inputs.width);
                Some(*row)
            }))
            .collect();
        DetailCache {
            inputs: Some(inputs),
            lines,
            starts,
        }
    }

    fn total_rows(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    /// The lines that cover `height` rows from row `scroll`, and how many rows of the first one
    /// lie above the view.
    fn visible(&self, scroll: usize, height: usize) -> (&[Line<'static>], u16) {
        let first = self
            .starts
            .partition_point(|&s| s <= scroll)
            .saturating_sub(1);
        let end = self.starts.partition_point(|&s| s < scroll + height);
        let first = first.min(self.lines.len());
        let end = end.clamp(first, self.lines.len());
        let skip = scroll.saturating_sub(self.starts.get(first).copied().unwrap_or(0));
        (
            &self.lines[first..end],
            u16::try_from(skip).unwrap_or(u16::MAX),
        )
    }
}

/// How many screen rows `line` takes when a paragraph wraps it to `width`.
fn wrapped_rows(line: &Line<'static>, width: u16) -> usize {
    if line.width() <= usize::from(width) {
        1
    } else {
        Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
    }
}

/// The CI block above the description: the diagnosis of a failed run, or a prompt to ask for one.
fn ci_lines(app: &App, item: &Item, width: usize) -> Vec<Line<'static>> {
    let key = item.key();
    let job = app.diagnosing.get(&key);
    let diagnosis = app.diagnoses.get(&key);
    if item.status != Status::CiFailed && job.is_none() && diagnosis.is_none() {
        return Vec::new();
    }
    let mut lines = vec![rule("CI", width)];
    match job {
        Some(Job::Running(stage)) => {
            lines.push(Line::styled(format!("Diagnosing: {stage}"), DIM));
        }
        Some(Job::Failed(err)) => {
            lines.push(Line::styled(
                format!("Diagnosis failed: {err}"),
                Style::new().fg(Color::Red),
            ));
        }
        _ => {}
    }
    match diagnosis {
        Some(diagnosis) => {
            let stale = if diagnosis.is_stale(item) {
                " · from an older commit"
            } else {
                ""
            };
            let (kind, body) = match &diagnosis.cause {
                Cause::Trivial { summary } => ("Trivial failure", vec![("", summary)]),
                Cause::Test { cause, fix } => {
                    ("Test failure", vec![("Cause", cause), ("Fix", fix)])
                }
            };
            lines.push(Line::styled(
                format!("{kind} · {}{stale}", diagnosis.jobs.join(", ")),
                DIM,
            ));
            for (label, text) in body {
                if !label.is_empty() {
                    lines.push(Line::from(label).bold());
                }
                lines.extend(markdown::render(text, width));
            }
            if diagnosis.is_stale(item) {
                lines.push(Line::styled("Press d to diagnose again.", DIM));
            }
        }
        None if job.is_none() => lines.push(Line::styled("Press d to diagnose this failure.", DIM)),
        None => {}
    }
    lines.push(Line::raw(""));
    lines
}

fn detail_lines(
    item: &Item,
    state: Option<&DetailState>,
    viewer: Viewer,
    show: conversation::Show,
    width: usize,
) -> Vec<Line<'static>> {
    let color = status_color(item.status);
    let mut lines = vec![
        Line::from(item.title.clone()).bold(),
        Line::from(vec![
            Span::raw(item.author_name.clone()),
            Span::styled(
                format!("  {} → {}", item.source_branch, item.target_branch),
                DIM,
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{} {}", item.status.glyph(), item.status.label()),
                Style::new().fg(color),
            ),
            Span::styled(
                if item.reviewers.is_empty() {
                    "   no reviewers".to_string()
                } else {
                    format!("   reviewers {}", item.reviewers.join(", "))
                },
                DIM,
            ),
        ]),
        Line::raw(""),
        rule("Description", width),
    ];

    let (description, comments) = match state {
        Some(DetailState::Loaded(d)) => (d.description.as_str(), Some(&d.comments)),
        _ => (item.description.as_str(), None),
    };
    if description.trim().is_empty() {
        lines.push(Line::styled("No description.", DIM));
    } else {
        lines.extend(markdown::render(description, width));
    }
    lines.push(Line::raw(""));

    match (state, comments) {
        (Some(DetailState::Failed(err)), _) => {
            lines.push(rule("Comments", width));
            lines.push(Line::styled(err.clone(), Style::new().fg(Color::Red)));
            lines.push(Line::styled("Press Enter to try again.", DIM));
        }
        (_, Some(comments)) => {
            lines.extend(conversation::lines(
                comments,
                viewer,
                show,
                width,
                Utc::now(),
            ));
        }
        _ => {
            lines.push(rule("Comments", width));
            lines.push(Line::styled("Loading…", DIM));
        }
    }
    lines
}

const LOCATION_WIDTH: usize = 26;

fn draw_findings(frame: &mut Frame, app: &mut App, item: &Item, area: Rect, tick: usize) {
    let focused = app.focus == Pane::Detail;
    let key = item.key();
    let review = app.reviews.get(&key);
    let job = app.jobs.get(&key);
    let posting = app.posting.get(&key);

    let mut title = vec![
        Span::raw(" Findings · "),
        Span::raw(item.reference()).bold(),
    ];
    if let Some(r) = review.filter(|r| !r.findings.is_empty()) {
        let selected = r.findings.iter().filter(|f| f.selected).count();
        title.push(Span::styled(
            format!(" · {selected} of {} selected", r.findings.len()),
            DIM,
        ));
        let posted = r.findings.iter().filter(|f| f.posted).count();
        if posted > 0 {
            title.push(Span::styled(format!(" · {posted} posted"), DIM));
        }
        if r.is_stale(item) {
            title.push(Span::styled(" · stale", Style::new().fg(Color::Yellow)));
        }
    }
    if let Some(Job::Running(stage)) = posting {
        title.push(Span::styled(
            format!(" · {} {stage}", SPINNER[tick % SPINNER.len()]),
            Style::new().fg(ACCENT),
        ));
    }
    if let Some(Job::Running(stage)) = job {
        title.push(Span::styled(
            format!(" · {} {stage}", SPINNER[tick % SPINNER.len()]),
            Style::new().fg(ACCENT),
        ));
    }
    if let Some(Job::Queued) = job {
        title.push(Span::styled(" · queued", DIM));
    }
    title.push(Span::raw(" "));
    let block = pane_block(Line::from(title), DETAIL_BORDER, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);

    let mut notes: Vec<Line> = Vec::new();
    if let Some(Job::Failed(err)) = job {
        notes.push(Line::styled(err.clone(), Style::new().fg(Color::Red)));
        notes.push(Line::styled("Press r to try again.", DIM));
    }
    if let Some(Job::Failed(err)) = posting {
        notes.push(Line::styled(err.clone(), Style::new().fg(Color::Red)));
    }
    let Some(review) = review.filter(|r| !r.findings.is_empty()) else {
        if notes.is_empty() {
            notes.push(Line::styled(
                match (job, review) {
                    (Some(_), _) => "Claude is reviewing this change.",
                    (None, Some(_)) => "The review found nothing to raise.",
                    (None, None) => "No review yet. Press r to run one.",
                },
                DIM,
            ));
        }
        frame.render_widget(Paragraph::new(notes).wrap(Wrap { trim: false }), inner);
        return;
    };

    let cursor = app.finding_cursor.min(review.findings.len() - 1);
    let note_height = notes.len() as u16;
    let list_height = (review.findings.len() as u16)
        .min((inner.height.saturating_sub(note_height)) / 2)
        .max(1);
    let [notes_area, list_area, rule_area, body_area] = Layout::vertical([
        Constraint::Length(note_height),
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(notes), notes_area);

    let rows: Vec<ListItem> = review
        .findings
        .iter()
        .map(|f| {
            let location = f.location().unwrap_or_else(|| "—".into());
            let title_width = width.saturating_sub(4 + 5 + LOCATION_WIDTH + 1);
            ListItem::new(Line::from(vec![
                match (f.posted, f.selected) {
                    (true, _) => Span::styled("[✓] ", Style::new().fg(Color::Green)),
                    (false, true) => Span::raw("[x] "),
                    (false, false) => Span::raw("[ ] "),
                },
                Span::styled(
                    fit(f.severity.label(), 5),
                    Style::new().fg(severity_color(f.severity)).bold(),
                ),
                Span::styled(
                    fit(&location, LOCATION_WIDTH),
                    if f.inline {
                        Style::new().fg(ACCENT)
                    } else {
                        DIM
                    },
                ),
                Span::raw(" "),
                Span::raw(fit(&f.title, title_width)),
            ]))
        })
        .collect();
    let highlight = if focused {
        Style::new().bg(Color::Indexed(237))
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    let mut state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(
        List::new(rows).highlight_style(highlight),
        list_area,
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("─".repeat(width), DIM)),
        rule_area,
    );

    let lines = finding_lines(review, cursor, width);
    let paragraph = Paragraph::new(lines);
    let total = paragraph.line_count(body_area.width) as u16;
    app.detail_scroll = app
        .detail_scroll
        .min(total.saturating_sub(body_area.height));
    frame.render_widget(paragraph.scroll((app.detail_scroll, 0)), body_area);
}

fn finding_lines(review: &Review, cursor: usize, width: usize) -> Vec<Line<'static>> {
    let f = &review.findings[cursor];
    let mut lines = vec![Line::from(f.title.clone()).bold()];
    let verb = if f.posted { "Posted" } else { "Posts" };
    let placement = match (&f.file, f.inline) {
        (_, true) => format!("{verb} on {}.", f.location().unwrap_or_default()),
        (Some(_), false) => format!(
            "{verb} as a PR comment: {} is outside the diff.",
            f.location().unwrap_or_default()
        ),
        (None, false) => format!("{verb} as a PR comment."),
    };
    lines.extend(
        wrap_text(&placement, width, "")
            .into_iter()
            .map(|l| Line::styled(l, DIM)),
    );
    lines.push(Line::raw(""));
    lines.extend(markdown::render(&f.body, width));
    lines
}

fn hint_spans(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in pairs {
        spans.push(Span::raw(key.to_string()).bold());
        spans.push(Span::styled(format!(" {label}   "), DIM));
    }
    Line::from(spans)
}

fn confirm_line(app: &App, key: &str) -> Line<'static> {
    let count = app.post_picks(key).len();
    let reference = app
        .items
        .iter()
        .find(|i| i.key() == key)
        .map(Item::reference)
        .unwrap_or_default();
    let noun = if count == 1 { "finding" } else { "findings" };
    Line::from(vec![
        Span::styled(
            format!(" Post {count} {noun} to {reference}? "),
            Style::new().fg(Color::Yellow).bold(),
        ),
        Span::raw("y").bold(),
        Span::styled(" posts, any other key cancels", DIM),
    ])
}

fn approval_line(app: &App, key: &str, approval: &Approval, tick: usize) -> Line<'static> {
    let reference = app
        .items
        .iter()
        .find(|i| i.key() == key)
        .map(Item::reference)
        .unwrap_or_default();
    match approval {
        Approval::Running => Line::styled(
            format!(" {} Approving {reference}", SPINNER[tick % SPINNER.len()]),
            Style::new().fg(ACCENT),
        ),
        Approval::Done => Line::styled(
            format!(" Approved {reference}."),
            Style::new().fg(Color::Green),
        ),
        Approval::Failed(err) => Line::styled(
            format!(" Could not approve {reference}: {err}"),
            Style::new().fg(Color::Red),
        ),
    }
}

fn draw_hints(frame: &mut Frame, app: &App, area: Rect, tick: usize) {
    if let Some(key) = &app.confirm_post {
        frame.render_widget(Paragraph::new(confirm_line(app, key)), area);
        return;
    }
    if let Some((key, approval)) = &app.approval {
        frame.render_widget(
            Paragraph::new(approval_line(app, key, approval, tick)),
            area,
        );
        return;
    }
    let line = match (app.mode, app.focus) {
        (Mode::Filter, _) => hint_spans(&[("type", "to filter"), ("⏎", "keep"), ("esc", "clear")]),
        (_, Pane::Detail) if app.view == View::Findings => hint_spans(&[
            ("j/k", "move"),
            ("space", "toggle"),
            ("a", "all"),
            ("1/2/3", "by severity"),
            ("p", "post"),
            ("J/K", "scroll"),
            ("f", "details"),
            ("r", "rerun"),
            ("h", "board"),
        ]),
        (_, Pane::Detail) => hint_spans(&[
            ("j/k", "scroll"),
            ("f", "findings"),
            ("u", "resolved"),
            ("h", "board"),
            ("o", "browser"),
            ("A", "approve"),
            ("?", "help"),
        ]),
        (_, Pane::Board) => hint_spans(&[
            ("j/k", "move"),
            ("⏎", "open"),
            ("za", "fold"),
            ("/", "filter"),
            ("m", "mine"),
            ("w", "to review"),
            ("r", "review"),
            ("f", "findings"),
            ("A", "approve"),
            ("R", "refresh"),
            ("?", "help"),
        ]),
    };
    frame.render_widget(Paragraph::new(line), area);
}

const HELP: &[(&str, &str)] = &[
    ("j / k", "Move down / up"),
    ("gg / G", "First / last"),
    ("Ctrl-d / Ctrl-u", "Half page down / up"),
    ("za  Space", "Fold or open a group or category"),
    ("zM / zR", "Fold all / open all"),
    ("Enter", "Open details"),
    ("Tab  h / l", "Switch pane"),
    ("J / K", "Scroll details from the board"),
    ("u", "Show or hide resolved comments"),
    ("/", "Filter by title, author, repo, or id"),
    ("Esc", "Clear filter, or leave details"),
    ("m", "Only items I wrote or review"),
    ("w", "Only items waiting for review"),
    ("r", "Run a Claude review"),
    ("d", "Diagnose a failed CI run"),
    ("f", "Findings, or back to details"),
    ("Space  a  1/2/3", "Toggle one, all, or a severity"),
    ("p", "Post checked findings to the PR"),
    ("A", "Approve, after y confirms"),
    ("o", "Open in browser"),
    ("R", "Refresh"),
    ("q", "Quit"),
];

const KEYS_WIDTH: u16 = 56;
const LEGEND_WIDTH: u16 = 38;

fn key_lines() -> Vec<Line<'static>> {
    HELP.iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::raw(format!("  {}", fit(k, 18))).bold(),
                Span::styled(v.to_string(), DIM),
            ])
        })
        .collect()
}

fn legend_entry(mark: &str, style: Style, meaning: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(fit(mark, 4), style),
        Span::styled(meaning.to_string(), DIM),
    ])
}

/// Explains the markers that `waiting_marker` and `review_badge` draw at the end of a board row.
fn legend_lines() -> Vec<Line<'static>> {
    let severity = |s: Severity| Style::new().fg(severity_color(s)).bold();
    vec![
        Line::from("  Board row markers").bold(),
        legend_entry(
            "↩N",
            Style::new().fg(Color::Yellow).bold(),
            "Threads waiting for your reply",
        ),
        legend_entry("·", DIM, "Review queued"),
        legend_entry(SPINNER[0], Style::new().fg(ACCENT), "Review running"),
        legend_entry("!", Style::new().fg(Color::Red), "Review failed"),
        legend_entry("R", Style::new(), "Review done, nothing posted"),
        legend_entry("P", Style::new(), "Review done, findings posted"),
        Line::raw(""),
        Line::from("  R and P colors").bold(),
        legend_entry("R", severity(Severity::High), "Worst finding is high"),
        legend_entry("R", severity(Severity::Medium), "Worst finding is medium"),
        legend_entry("R", severity(Severity::Low), "Worst finding is low"),
        legend_entry("R", Style::new().fg(Color::Green), "No findings"),
        legend_entry("R", DIM, "New commits since the review"),
    ]
}

const APPROVE_WIDTH: u16 = 60;

/// Asks before approving, so a stray `A` cannot approve anything.
fn draw_approve_dialog(frame: &mut Frame, item: &Item) {
    let area = frame.area();
    let width = APPROVE_WIDTH.min(area.width);
    let text_width = usize::from(width.saturating_sub(4));
    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw(" Approve "),
            Span::raw(item.reference()).bold(),
            Span::raw(format!(" in {}?", item.repo)),
        ]),
    ];
    lines.extend(
        wrap_text(&item.title, text_width, " ")
            .into_iter()
            .map(|l| Line::styled(l, DIM)),
    );
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw(" y").bold(),
        Span::styled(" approves, any other key cancels", DIM),
    ]));
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = pane_block(Line::from(" Approve ").bold(), border::ROUNDED, true).style(PANEL);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_help(frame: &mut Frame) {
    let area = frame.area();
    let mut keys = key_lines();
    let legend = legend_lines();
    let side_by_side = area.width >= KEYS_WIDTH + LEGEND_WIDTH + 2;
    let (content_width, content_height) = if side_by_side {
        (KEYS_WIDTH + LEGEND_WIDTH, keys.len().max(legend.len()))
    } else {
        (KEYS_WIDTH, keys.len() + 1 + legend.len())
    };
    let height = (content_height as u16 + 4).min(area.height);
    let width = (content_width + 2).min(area.width);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = pane_block(Line::from(" Help ").bold(), border::ROUNDED, true)
        .style(PANEL)
        .title_bottom(Line::styled(" any key closes ", DIM).right_aligned());
    let inner = block.inner(rect);
    let [_, content] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    if side_by_side {
        let [left, right] =
            Layout::horizontal([Constraint::Length(KEYS_WIDTH), Constraint::Fill(1)])
                .areas(content);
        frame.render_widget(Paragraph::new(keys), left);
        frame.render_widget(Paragraph::new(legend), right);
    } else {
        keys.push(Line::raw(""));
        keys.extend(legend);
        frame.render_widget(Paragraph::new(keys), content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Comment, Details, ThreadStatus};
    use view::conversation::Show;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn help_panel_sets_its_own_colors() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(draw_help).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let body = buffer.cell((5, 10)).unwrap();
        assert_eq!(body.bg, Color::Indexed(236));
        let description = (0..60)
            .map(|x| buffer.cell((x, 10)).unwrap())
            .find(|c| c.symbol() != " " && c.fg == MUTED)
            .map(|c| c.symbol().to_string());
        assert!(
            description.is_some(),
            "no description was drawn in muted gray"
        );
    }

    fn rendered_help(width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(draw_help).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_shows_every_key_and_legend_entry_in_both_layouts() {
        for (width, height) in [(100, 28), (60, 42)] {
            let screen = rendered_help(width, height);
            let expected = HELP.iter().map(|(_, meaning)| *meaning).chain([
                "↩N",
                "Threads waiting for your reply",
                "Review queued",
                "Review running",
                "Review failed",
                "Review done, nothing posted",
                "Review done, findings posted",
                "Worst finding is high",
                "Worst finding is medium",
                "Worst finding is low",
                "No findings",
                "New commits since the review",
            ]);
            for text in expected {
                assert!(
                    screen.contains(text),
                    "{width}x{height} is missing {text:?}:\n{screen}"
                );
            }
        }
    }

    #[test]
    fn legend_colors_match_the_badge_colors() {
        let color_of = |meaning: &str| {
            legend_lines()
                .iter()
                .find(|l| l.to_string().contains(meaning))
                .map(|l| l.spans[1].style.fg)
                .unwrap()
        };
        assert_eq!(color_of("is high"), Some(severity_color(Severity::High)));
        assert_eq!(
            color_of("is medium"),
            Some(severity_color(Severity::Medium))
        );
        assert_eq!(color_of("is low"), Some(severity_color(Severity::Low)));
        assert_eq!(color_of("No findings"), Some(Color::Green));
        assert_eq!(color_of("New commits"), DIM.fg);
    }

    #[test]
    fn approve_dialog_names_the_item_and_the_confirming_key() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let item: Item = serde_json::from_value(serde_json::json!({
            "source": "GitLab", "id": 1405, "project_id": 7, "repo": "parser",
            "group": "core", "title": "Fix the parser", "author": "jdoe", "author_name": "J Doe",
            "reviewers": [], "status": "Active", "source_branch": "b", "target_branch": "master",
            "description": "", "web_url": "https://example/1405", "head_sha": null,
            "created": "2026-10-30T00:00:00Z"
        }))
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| draw_approve_dialog(frame, &item))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..20)
            .map(|y| {
                (0..80)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for text in ["Approve !1405 in parser?", "Fix the parser", "y approves"] {
            assert!(screen.contains(text), "missing {text:?}:\n{screen}");
        }
    }

    #[test]
    fn muted_text_is_a_fixed_palette_color() {
        assert_eq!(DIM.fg, Some(Color::Indexed(244)));
    }

    #[test]
    fn ci_block_shows_the_failure_kind_fix_and_staleness() {
        use crate::model::Diagnosis;

        let item: Item = serde_json::from_value(serde_json::json!({
            "source": "GitLab", "id": 14, "project_id": 603, "repo": "r", "group": "g",
            "title": "t", "author": "a", "author_name": "A", "reviewers": [],
            "status": "CiFailed", "source_branch": "b", "target_branch": "master",
            "description": "", "web_url": "", "head_sha": "new",
            "created": "2026-10-01T00:00:00Z"
        }))
        .unwrap();
        let mut app = App::new(vec![item.clone()]);
        let before = text(&ci_lines(&app, &item, 60)).join("\n");
        assert!(
            before.contains("Press d to diagnose this failure."),
            "{before}"
        );

        app.diagnoses.insert(
            item.key(),
            Diagnosis {
                key: item.key(),
                head_sha: "old".into(),
                created: Utc::now(),
                jobs: vec!["unit".into()],
                cause: Cause::Test {
                    cause: "The assertion fails.".into(),
                    fix: "Update the expected value.".into(),
                },
            },
        );
        let shown = text(&ci_lines(&app, &item, 60)).join("\n");
        assert!(
            shown.contains("Test failure · unit · from an older commit"),
            "{shown}"
        );
        assert!(
            shown.contains("Cause") && shown.contains("The assertion fails."),
            "{shown}"
        );
        assert!(
            shown.contains("Fix") && shown.contains("Update the expected value."),
            "{shown}"
        );
        assert!(shown.contains("Press d to diagnose again."), "{shown}");

        let passing = Item {
            status: Status::Active,
            ..item
        };
        let none = App::new(vec![passing.clone()]);
        assert!(ci_lines(&none, &passing, 60).is_empty());
    }

    #[test]
    fn unresolved_only_hides_resolved_threads_and_their_replies() {
        let item: Item = serde_json::from_value(serde_json::json!({
            "source": "Ado", "id": 1, "project_id": null, "repo": "r", "group": "g",
            "title": "t", "author": "a", "author_name": "A", "reviewers": [],
            "status": "Active", "source_branch": "b", "target_branch": "master",
            "description": "", "web_url": "", "head_sha": null,
            "created": "2026-10-01T00:00:00Z"
        }))
        .unwrap();
        let comment = |author: &str, body: &str, is_reply, resolved| Comment {
            login: author.into(),
            author: author.into(),
            date: Utc::now(),
            location: None,
            body: body.into(),
            is_reply,
            status: if resolved {
                ThreadStatus::Resolved
            } else {
                ThreadStatus::Open
            },
        };
        let viewer = Viewer {
            me: Some("a"),
            author: "a",
        };
        let state = DetailState::Loaded(Details {
            description: String::new(),
            comments: vec![
                comment("ann", "fixed thing", false, true),
                comment("bob", "fixed reply", true, true),
                comment("cy", "open thing", false, false),
            ],
        });

        let all = text(&detail_lines(&item, Some(&state), viewer, Show::All, 60)).join("\n");
        assert!(
            all.contains("Comments · 2 threads · 1 open · 1 resolved"),
            "{all}"
        );
        assert!(all.contains("fixed reply"), "{all}");

        let open = text(&detail_lines(
            &item,
            Some(&state),
            viewer,
            Show::Unresolved,
            60,
        ))
        .join("\n");
        assert!(
            open.contains("Comments · 2 threads · 1 open · 1 resolved hidden"),
            "{open}"
        );
        assert!(open.contains("open thing"), "{open}");
        assert!(!open.contains("fixed thing"), "{open}");
        assert!(!open.contains("fixed reply"), "{open}");

        let none = DetailState::Loaded(Details {
            description: String::new(),
            comments: vec![comment("ann", "fixed thing", false, true)],
        });
        let empty = text(&detail_lines(
            &item,
            Some(&none),
            viewer,
            Show::Unresolved,
            60,
        ))
        .join("\n");
        assert!(empty.contains("No unresolved comments."), "{empty}");
    }

    #[test]
    fn review_badge_dims_a_stale_review_and_colors_a_current_one() {
        let mut item: Item = serde_json::from_value(serde_json::json!({
            "source": "Ado", "id": 1, "project_id": null, "repo": "r", "group": "g",
            "title": "t", "author": "a", "author_name": "A", "reviewers": [],
            "status": "Active", "source_branch": "b", "target_branch": "master",
            "description": "", "web_url": "", "head_sha": "h1",
            "created": "2026-10-01T00:00:00Z"
        }))
        .unwrap();
        let mut app = App::new(vec![item.clone()]);
        assert_eq!(review_badge(&app, &item, 0).content, "  ");
        let finding = crate::model::Finding {
            severity: Severity::High,
            file: None,
            line: None,
            title: String::new(),
            body: String::new(),
            inline: false,
            selected: true,
            posted: false,
        };
        app.reviews.insert(
            item.key(),
            Review {
                key: item.key(),
                head_sha: "h1".into(),
                base_sha: "b".into(),
                created: Utc::now(),
                findings: vec![finding],
            },
        );
        assert_eq!(review_badge(&app, &item, 0).style.fg, Some(Color::Red));
        item.head_sha = Some("h2".into());
        assert_eq!(review_badge(&app, &item, 0).style, DIM);
        assert_eq!(review_badge(&app, &item, 0).content, " R");
        app.reviews.get_mut(&item.key()).unwrap().findings[0].posted = true;
        assert_eq!(review_badge(&app, &item, 0).content, " P");
        app.jobs.insert(item.key(), Job::Failed("x".into()));
        assert_eq!(review_badge(&app, &item, 0).content, " !");
    }

    #[test]
    fn wrap_keeps_indent_on_continuation_lines() {
        assert_eq!(
            wrap_text("one two three four", 9, "  "),
            ["  one two", "  three", "  four"]
        );
        assert_eq!(
            wrap_text("- alpha beta gamma", 12, ""),
            ["- alpha beta", "  gamma"]
        );
        assert_eq!(wrap_text("a\n\nb", 10, ""), ["a", "", "b"]);
        assert_eq!(wrap_text("abcdefghij", 4, ""), ["abcd", "efgh", "ij"]);
        for line in wrap_text("x ".repeat(40).as_str(), 7, "    ") {
            assert!(line.width() <= 7, "{line:?}");
        }
    }

    #[test]
    fn fit_pads_and_truncates_to_exact_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 5), "abcd…");
        assert_eq!(fit("abcde", 5), "abcde");
        assert_eq!(fit("日本語テキスト", 7).width(), 7);
        assert_eq!(fit("anything", 0), "");
    }

    fn long_thread() -> (App, Item) {
        let item = Item {
            source: Source::GitLab,
            id: 7,
            project_id: None,
            repo: "repo".into(),
            group: "group".into(),
            title: "A title long enough that the pane has to wrap it onto a second row".into(),
            author: "ann".into(),
            author_name: "Ann".into(),
            reviewers: vec!["bob".into(), "carol".into(), "dave".into()],
            approved_by: Vec::new(),
            status: Status::Active,
            source_branch: "feature/a-branch-with-a-long-name".into(),
            target_branch: "master".into(),
            description: "Some words that wrap. ".repeat(20),
            web_url: String::new(),
            head_sha: None,
            created: Utc::now(),
        };
        let comments = (0..6)
            .map(|n| Comment {
                login: format!("user{n}"),
                author: format!("user{n}"),
                date: Utc::now(),
                location: Some("src/a/rather/deep/path/to/a/file.cpp:120".into()),
                body: format!("Comment {n} says something that runs on. ").repeat(4),
                is_reply: n % 2 == 1,
                status: ThreadStatus::Open,
            })
            .collect();
        let mut app = App::new(vec![item.clone()]);
        app.details.insert(
            item.key(),
            DetailState::Loaded(Details {
                description: item.description.clone(),
                comments,
            }),
        );
        app.open = Some(item.key());
        (app, item)
    }

    fn draw_detail_pane(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_detail(f, app, f.area(), 0)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn detail_pane_shows_the_same_rows_as_one_wrapped_paragraph() {
        use ratatui::widgets::Widget;
        let (mut app, _) = long_thread();
        for width in [32u16, 57] {
            let height = 12;
            draw_detail_pane(&mut app, width, height);
            let inner = Rect::new(1, 1, width - 2, height - 2);
            let full = Paragraph::new(app.detail_cache.lines.clone()).wrap(Wrap { trim: false });
            let total = full.line_count(inner.width) as u16;
            assert_eq!(usize::from(total), app.detail_cache.total_rows());
            for scroll in 0..=total - inner.height {
                app.detail_scroll = scroll;
                let shown = draw_detail_pane(&mut app, width, height);
                let mut expected = ratatui::buffer::Buffer::empty(shown.area);
                full.clone()
                    .scroll((scroll, 0))
                    .render(inner, &mut expected);
                for y in inner.top()..inner.bottom() {
                    let row = |b: &ratatui::buffer::Buffer| {
                        (inner.left()..inner.right())
                            .map(|x| b[(x, y)].symbol().to_string())
                            .collect::<String>()
                    };
                    assert_eq!(
                        row(&shown),
                        row(&expected),
                        "width {width}, scroll {scroll}"
                    );
                }
            }
        }
    }

    #[test]
    fn detail_cache_rebuilds_when_its_inputs_change() {
        let (mut app, item) = long_thread();
        let fresh = |app: &App, width: u16| {
            DetailCache::build(app, DetailInputs::of(app, &item, width, Utc::now())).lines
        };
        draw_detail_pane(&mut app, 40, 20);
        assert_eq!(app.detail_cache.lines, fresh(&app, 38));

        draw_detail_pane(&mut app, 60, 20);
        assert_eq!(app.detail_cache.lines, fresh(&app, 58));
        let description = app.detail_cache.lines[5].width();
        assert!(description > 50 && description <= 58, "{description}");

        if let Some(DetailState::Loaded(d)) = app.details.get_mut(&item.key()) {
            d.comments[0].body = "A brand new reply.".into();
        }
        draw_detail_pane(&mut app, 60, 20);
        assert!(
            text(&app.detail_cache.lines)
                .iter()
                .any(|l| l.contains("A brand new reply."))
        );

        app.unresolved_only = false;
        draw_detail_pane(&mut app, 60, 20);
        assert_eq!(app.detail_cache.lines, fresh(&app, 58));
    }
}
