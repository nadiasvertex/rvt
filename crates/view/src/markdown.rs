//! Renders Markdown as styled terminal lines that fit a given width.
//!
//! Terminals have one font size, so headings are set apart by color, weight, and the space around
//! them. Code blocks sit on a panel, quotes and list items hang from a gutter, and tables line up
//! their columns. Every returned line is at most `width` columns wide.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::{
    ACCENT, CODE_ADDED, CODE_HUNK, CODE_LABEL, CODE_REMOVED, DIM, INLINE_CODE, LINK, MUTED, PANEL,
};

const BULLETS: [&str; 3] = ["•", "◦", "▪"];
const QUOTE_BAR: &str = "▎ ";
const COLUMN_GAP: &str = " │ ";
const MIN_COLUMN: usize = 3;
const TAB: &str = "    ";
/// A hard line break inside a paragraph travels through the inline spans as this text.
const BREAK: &str = "\n";

pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut renderer = Renderer::new(width.max(1));
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

enum Container {
    Quote,
    Item {
        marker: String,
        style: Style,
        shown: bool,
    },
}

struct CodeBlock {
    lang: String,
    text: String,
}

struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    header_rows: usize,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// The next number of each open list, or `None` for a bulleted one.
    lists: Vec<Option<u64>>,
    /// A blank line is owed before the next block.
    gap: bool,
    code: Option<CodeBlock>,
    table: Option<Table>,
    /// Each open link's target and where its text starts in `inline`.
    links: Vec<(String, usize)>,
    image_starts: Vec<usize>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Renderer {
            width,
            out: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            containers: Vec::new(),
            lists: Vec::new(),
            gap: false,
            code: None,
            table: None,
            links: Vec::new(),
            image_starts: Vec::new(),
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        self.out
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::new(), |acc, style| acc.patch(*style))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        self.inline.push(Span::styled(text.to_string(), style));
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some(code) => code.text.push_str(&text),
                None => self.push_text(&text, self.style()),
            },
            Event::Code(text) => self.push_text(&text, self.style().patch(INLINE_CODE)),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_text(&text, self.style().italic())
            }
            Event::InlineHtml(html) => {
                if is_line_break(&html) {
                    self.push_text(BREAK, Style::new());
                }
            }
            Event::Html(html) => {
                let text = strip_tags(&html);
                if !text.trim().is_empty() {
                    self.push_text(&format!("{text} "), self.style().patch(DIM));
                }
            }
            Event::FootnoteReference(name) => {
                self.push_text(&format!("[{name}]"), self.style().patch(DIM))
            }
            Event::SoftBreak => self.push_text(" ", self.style()),
            Event::HardBreak => self.push_text(BREAK, Style::new()),
            Event::Rule => {
                self.open_block();
                let width = self.content_width();
                let prefix = self.next_prefix();
                self.emit(prefix, vec![Span::styled("─".repeat(width), DIM)]);
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                if let Some(Container::Item { marker, style, .. }) = self.containers.last_mut() {
                    let (glyph, color) = if done {
                        ("☑ ", Style::new().fg(Color::Green))
                    } else {
                        ("☐ ", DIM)
                    };
                    *marker = glyph.to_string();
                    *style = color;
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.open_block(),
            Tag::Heading { level, .. } => {
                self.open_block();
                self.styles.push(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.open_block();
                self.containers.push(Container::Quote);
                self.styles.push(Style::new().fg(MUTED).italic());
            }
            Tag::CodeBlock(kind) => {
                self.open_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock {
                    lang,
                    text: String::new(),
                });
            }
            Tag::List(first) => {
                if self.lists.is_empty() {
                    self.open_block();
                } else {
                    self.flush();
                }
                self.lists.push(first);
            }
            Tag::Item => {
                self.open_block();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => {
                        let depth = self.lists.len().saturating_sub(1);
                        format!("{} ", BULLETS[depth % BULLETS.len()])
                    }
                };
                self.containers.push(Container::Item {
                    marker,
                    style: Style::new().fg(ACCENT),
                    shown: false,
                });
            }
            Tag::Table(alignments) => {
                self.open_block();
                self.table = Some(Table {
                    alignments,
                    rows: Vec::new(),
                    row: Vec::new(),
                    header_rows: 0,
                });
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.styles.push(Style::new().italic()),
            Tag::Strong => self.styles.push(Style::new().bold()),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().fg(MUTED).add_modifier(Modifier::CROSSED_OUT)),
            Tag::Superscript | Tag::Subscript => self.styles.push(Style::new()),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.to_string(), self.inline.len()));
                self.styles.push(LINK);
            }
            Tag::Image { .. } => {
                self.image_starts.push(self.inline.len());
                self.styles.push(Style::new().fg(MUTED).italic());
                self.push_text("▣ ", self.style());
            }
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => self.open_block(),
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush();
                self.gap = true;
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.styles.pop();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.code_block(&code);
                }
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.gap = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table(&table);
                }
                self.gap = true;
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    if tag == TagEnd::TableHead {
                        table.header_rows = table.rows.len();
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let text: String = self.inline[start.min(self.inline.len())..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect();
                    if let Some(host) = host_of(&url).filter(|_| text.trim() != url) {
                        self.push_text(&format!(" ({host})"), self.style().patch(DIM));
                    }
                }
            }
            TagEnd::Image => {
                let start = self.image_starts.pop().unwrap_or(0);
                if self.inline.len() <= start + 1 {
                    self.push_text("image", self.style());
                }
                self.styles.pop();
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {
                self.flush();
                self.gap = true;
            }
        }
    }

    /// Writes out any pending text and the blank line owed before a new block, using the
    /// containers as they stand before the block opens.
    fn open_block(&mut self) {
        self.flush();
        if self.gap && !self.out.is_empty() {
            let prefix = self.gap_prefix();
            self.emit(prefix, Vec::new());
        }
        self.gap = false;
    }

    fn prefix_width(&self) -> usize {
        self.containers
            .iter()
            .map(|c| match c {
                Container::Quote => QUOTE_BAR.width(),
                Container::Item { marker, .. } => marker.width(),
            })
            .sum()
    }

    fn content_width(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(1)
    }

    /// The gutter for the next line of content. An item shows its marker on its first line only.
    fn next_prefix(&mut self) -> Vec<Span<'static>> {
        self.containers
            .iter_mut()
            .map(|c| match c {
                Container::Quote => Span::styled(QUOTE_BAR, DIM),
                Container::Item {
                    marker,
                    style,
                    shown,
                } if !*shown => {
                    *shown = true;
                    Span::styled(marker.clone(), *style)
                }
                Container::Item { marker, .. } => Span::raw(" ".repeat(marker.width())),
            })
            .collect()
    }

    fn gap_prefix(&self) -> Vec<Span<'static>> {
        let mut prefix: Vec<Span<'static>> = self
            .containers
            .iter()
            .map(|c| match c {
                Container::Quote => Span::styled(QUOTE_BAR.trim_end(), DIM),
                Container::Item { marker, .. } => Span::raw(" ".repeat(marker.width())),
            })
            .collect();
        while prefix.last().is_some_and(|s| s.content.trim().is_empty()) {
            prefix.pop();
        }
        prefix
    }

    fn emit(&mut self, mut prefix: Vec<Span<'static>>, content: Vec<Span<'static>>) {
        prefix.extend(content);
        self.out.push(Line::from(prefix));
    }

    fn flush(&mut self) {
        if self.table.is_some() || self.code.is_some() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        if spans.iter().all(|s| s.content.trim().is_empty()) {
            return;
        }
        for line in wrap(&spans, self.content_width()) {
            let prefix = self.next_prefix();
            self.emit(prefix, line);
        }
    }

    fn code_block(&mut self, code: &CodeBlock) {
        let width = self.content_width();
        let inner = width.saturating_sub(2).max(1);
        let lang = code.lang.to_ascii_lowercase();
        let suggestion = lang.starts_with("suggestion");
        let label = if suggestion {
            "suggested change".to_string()
        } else {
            lang.clone()
        };
        let body = code.text.strip_suffix('\n').unwrap_or(&code.text);
        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
        if !label.is_empty() {
            rows.push(vec![Span::styled(format!("{label:>width$}"), CODE_LABEL)]);
        }
        for source in body.split('\n') {
            let source = source.replace('\t', TAB);
            let style = if suggestion {
                CODE_ADDED
            } else if lang == "diff" || lang == "patch" {
                diff_style(&source)
            } else {
                PANEL
            };
            for piece in split_to_width(&source, inner) {
                let pad = inner.saturating_sub(piece.width());
                rows.push(vec![
                    Span::styled(" ", PANEL),
                    Span::styled(piece, style),
                    Span::styled(" ".repeat(pad + 1), PANEL),
                ]);
            }
        }
        for row in rows {
            let prefix = self.next_prefix();
            self.emit(prefix, row);
        }
    }

    fn table(&mut self, table: &Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(spans_width(cell));
            }
        }
        let available = self
            .content_width()
            .saturating_sub(COLUMN_GAP.width() * (columns - 1));
        shrink_to_fit(&mut widths, available);

        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
        for (r, row) in table.rows.iter().enumerate() {
            let header = r < table.header_rows;
            let mut line = Vec::new();
            for (i, width) in widths.iter().enumerate() {
                if i > 0 {
                    line.push(Span::styled(COLUMN_GAP, DIM));
                }
                let empty = Vec::new();
                let cell = row.get(i).unwrap_or(&empty);
                let alignment = table.alignments.get(i).copied().unwrap_or(Alignment::None);
                let mut cell = fit_spans(cell, *width, alignment);
                if header {
                    cell = cell.into_iter().map(|s| s.bold()).collect();
                }
                line.extend(cell);
            }
            rows.push(line);
            if r + 1 == table.header_rows {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                rows.push(vec![Span::styled(rule.join("─┼─"), DIM)]);
            }
        }
        for row in rows {
            let prefix = self.next_prefix();
            self.emit(prefix, row);
        }
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::new()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::new().fg(ACCENT).bold(),
        HeadingLevel::H3 => Style::new().bold(),
        _ => Style::new().bold().italic(),
    }
}

fn diff_style(line: &str) -> Style {
    if line.starts_with("@@") {
        CODE_HUNK
    } else if line.starts_with('+') {
        CODE_ADDED
    } else if line.starts_with('-') {
        CODE_REMOVED
    } else {
        PANEL
    }
}

fn is_line_break(html: &str) -> bool {
    let tag: String = html
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '/')
        .collect::<String>()
        .to_ascii_lowercase();
    tag == "<br>"
}

/// The text of an HTML block with its tags removed.
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn host_of(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then_some(host)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Narrows the widest columns until the row fits. Columns keep a few cells while they can, and
/// go down to one only when the pane is too narrow for that.
fn shrink_to_fit(widths: &mut [usize], available: usize) {
    for floor in [MIN_COLUMN, 1] {
        while widths.iter().sum::<usize>() > available {
            let Some(widest) = widths
                .iter_mut()
                .filter(|w| **w > floor)
                .max_by_key(|w| **w)
            else {
                break;
            };
            *widest -= 1;
        }
    }
}

/// Pads or cuts a table cell to exactly `width` columns. A cut cell ends with an ellipsis.
fn fit_spans(spans: &[Span<'static>], width: usize, alignment: Alignment) -> Vec<Span<'static>> {
    let flat: Vec<Span<'static>> = spans
        .iter()
        .map(|s| Span::styled(s.content.replace(BREAK, " "), s.style))
        .collect();
    let mut out = Vec::new();
    let total = spans_width(&flat);
    if total <= width {
        let pad = width - total;
        let (left, right) = match alignment {
            Alignment::Right => (pad, 0),
            Alignment::Center => (pad / 2, pad - pad / 2),
            Alignment::Left | Alignment::None => (0, pad),
        };
        out.push(Span::raw(" ".repeat(left)));
        out.extend(flat);
        out.push(Span::raw(" ".repeat(right)));
        return out;
    }
    let mut used = 0;
    let mut last_style = Style::new();
    'spans: for span in flat {
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w + 1 > width {
                out.push(Span::styled(text, span.style));
                last_style = span.style;
                break 'spans;
            }
            text.push(c);
            used += w;
        }
        last_style = span.style;
        out.push(Span::styled(text, span.style));
    }
    out.push(Span::styled("…", last_style));
    used += 1;
    out.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    out
}

/// Cuts `text` into pieces no wider than `width`, for code that must not be reflowed.
fn split_to_width(text: &str, width: usize) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            pieces.push(String::new());
            used = 0;
        }
        if let Some(last) = pieces.last_mut() {
            last.push(c);
        }
        used += w;
    }
    pieces
}

struct Word {
    pieces: Vec<Span<'static>>,
    /// The style of the space before this word, if one came before it on the same line.
    space: Option<Style>,
}

enum Token {
    Word(Word),
    Break,
}

fn finish_word(word: &mut Vec<Span<'static>>, space: &mut Option<Style>, tokens: &mut Vec<Token>) {
    if !word.is_empty() {
        tokens.push(Token::Word(Word {
            pieces: std::mem::take(word),
            space: space.take(),
        }));
    }
}

/// Splits styled text into words, keeping each piece's style. Runs of spaces collapse to one.
fn tokens(spans: &[Span<'static>]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word: Vec<Span<'static>> = Vec::new();
    let mut space: Option<Style> = None;
    for span in spans {
        if span.content == BREAK {
            finish_word(&mut word, &mut space, &mut tokens);
            space = None;
            tokens.push(Token::Break);
            continue;
        }
        let mut piece = String::new();
        for c in span.content.chars() {
            if c.is_whitespace() {
                if !piece.is_empty() {
                    word.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                finish_word(&mut word, &mut space, &mut tokens);
                let after_text = tokens.last().is_some_and(|t| matches!(t, Token::Word(_)));
                if after_text && space.is_none() {
                    space = Some(span.style);
                }
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            word.push(Span::styled(piece, span.style));
        }
    }
    finish_word(&mut word, &mut space, &mut tokens);
    tokens
}

fn push_styled(line: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match line.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => line.push(Span::styled(text.to_string(), style)),
    }
}

/// Wraps styled text at spaces to fit `width`. A word wider than the line is cut.
fn wrap(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for token in tokens(spans) {
        let word = match token {
            Token::Break => {
                lines.push(std::mem::take(&mut line));
                used = 0;
                continue;
            }
            Token::Word(word) => word,
        };
        let word_width = spans_width(&word.pieces);
        if used > 0 && used + 1 + word_width > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if used > 0 {
            push_styled(&mut line, " ", word.space.unwrap_or_default());
            used += 1;
        }
        for piece in &word.pieces {
            for c in piece.content.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                push_styled(&mut line, c.encode_utf8(&mut [0; 4]), piece.style);
                used += w;
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn span_with<'a>(lines: &'a [Line<'static>], text: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(text))
            .unwrap_or_else(|| panic!("no span contains {text:?} in {:?}", plain(lines)))
    }

    #[test]
    fn paragraphs_rewrap_to_the_width_and_are_separated_by_a_blank_line() {
        let lines = render("one two\nthree four five\n\nsix", 10);
        assert_eq!(plain(&lines), ["one two", "three four", "five", "", "six"]);
    }

    #[test]
    fn every_line_fits_the_width() {
        let text = "# A heading that is long\n\n- a list item that wraps over lines\n  - nested \
                    item with `code` and a [link](https://example.com/x)\n\n> quoted text that \
                    wraps\n\n```\nlet code = \"a very long line of code that must be cut\";\n```\n\n\
                    | a | b |\n|---|---|\n| long cell text | more |\n\nsupercalifragilisticexpialidocious";
        for width in [8, 13, 20, 40] {
            for line in render(text, width) {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn emphasis_code_and_links_keep_their_styles() {
        let lines = render(
            "a **bold** and *italic* `code` [docs](https://dev.azure.com/x)",
            80,
        );
        assert!(
            span_with(&lines, "bold")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            span_with(&lines, "italic")
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(span_with(&lines, "code").style.fg, INLINE_CODE.fg);
        assert_eq!(span_with(&lines, "docs").style.fg, LINK.fg);
        assert_eq!(
            plain(&lines),
            ["a bold and italic code docs (dev.azure.com)"]
        );
    }

    #[test]
    fn headings_stand_apart_from_the_text_around_them() {
        let lines = render("intro\n# Title\n## Part\nbody", 40);
        assert_eq!(
            plain(&lines),
            ["intro", "", "Title", "", "Part", "", "body"]
        );
        assert_eq!(span_with(&lines, "Title").style.fg, Some(ACCENT));
        assert!(
            span_with(&lines, "Title")
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            span_with(&lines, "Part")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn list_items_hang_under_their_markers() {
        let lines = render(
            "- first item wraps\n- second\n  1. inner\n  2. next\n- [x] done",
            12,
        );
        assert_eq!(
            plain(&lines),
            [
                "• first item",
                "  wraps",
                "• second",
                "  1. inner",
                "  2. next",
                "☑ done",
            ]
        );
    }

    #[test]
    fn quotes_hang_from_a_bar() {
        let lines = render("> said this\n>\n> and that\n\nreply", 40);
        assert_eq!(
            plain(&lines),
            ["▎ said this", "▎", "▎ and that", "", "reply"]
        );
        assert_eq!(span_with(&lines, "said").style.fg, Some(MUTED));
    }

    #[test]
    fn code_blocks_keep_their_lines_on_a_panel() {
        let lines = render("```diff\n+added\n-removed\n  same\n```", 20);
        assert!(lines.iter().all(|l| l.width() == 20), "{:?}", plain(&lines));
        let trimmed: Vec<_> = plain(&lines)
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(
            trimmed,
            ["                diff", " +added", " -removed", "   same"]
        );
        assert_eq!(span_with(&lines, "added").style, CODE_ADDED);
        assert_eq!(span_with(&lines, "removed").style, CODE_REMOVED);
    }

    #[test]
    fn suggestions_are_labelled_and_shown_as_additions() {
        let lines = render("```suggestion:-0+0\nlet x = 1;\n```", 30);
        assert!(plain(&lines)[0].ends_with("suggested change"));
        assert_eq!(span_with(&lines, "let x").style, CODE_ADDED);
    }

    #[test]
    fn tables_line_up_their_columns() {
        let lines = render("| Name | N |\n|---|--:|\n| a | 10 |\n| bbb | 2 |", 40);
        assert_eq!(
            plain(&lines),
            ["Name │  N", "─────┼───", "a    │ 10", "bbb  │  2"]
        );
    }

    #[test]
    fn wide_tables_cut_their_widest_column() {
        let lines = render("| a | b |\n|---|---|\n| a long cell | x |", 10);
        assert_eq!(plain(&lines)[2], "a lon… │ x");
    }

    #[test]
    fn html_line_breaks_and_tags() {
        let lines = render("one<br>two\n\n<details>\nhidden\n</details>", 40);
        assert_eq!(plain(&lines), ["one", "two", "", "hidden"]);
    }
}
