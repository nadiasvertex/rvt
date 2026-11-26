//! A terminal drawn in a GPUI window. The grid is painted cell by cell from alacritty's screen
//! model, so every glyph sits on the cell grid whatever the font does.

use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::event::Event;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor, Rgb};
use gpui_kit::*;

use super::keys::{self, Mods};
use super::palette;
use super::pty::{Foreground, GridSize, Pty};

/// How often the view asks which program runs in the foreground, for the tab's name.
const FOREGROUND_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// What a terminal tells the tab that holds it.
pub enum TerminalEvent {
    /// The program ended.
    Exited,
    /// The foreground program or its directory changed.
    Foreground,
}

const FONT_FAMILY: &str = "Menlo";
const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = 1.3;
const PADDING: f32 = 6.0;

fn hsla(c: Rgb) -> Hsla {
    Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// Where the grid was last painted, so mouse positions can be turned into cells.
#[derive(Clone, Copy)]
struct Metrics {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
}

pub struct TerminalView {
    pty: Rc<RefCell<Pty>>,
    focus: FocusHandle,
    metrics: Rc<RefCell<Metrics>>,
    foreground: Option<Foreground>,
    exited: bool,
    /// Scrolled distance not yet turned into whole lines.
    scroll_lines: f32,
    selecting: bool,
    _events: Task<()>,
    _foreground: Task<()>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    pub fn new(
        pty: Pty,
        events: async_channel::Receiver<Event>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let font_size = px(FONT_SIZE);
        let text = window.text_system();
        let font_id = text.resolve_font(&font(FONT_FAMILY));
        let cell_width = text
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(FONT_SIZE * 0.6));
        let line_height = px((FONT_SIZE * LINE_HEIGHT).round());
        let focus = cx.focus_handle();
        let task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |view, cx| view.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let foreground = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                if this
                    .update(cx, |view, cx| view.check_foreground(cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(FOREGROUND_EVERY).await;
            }
        });
        let first = pty.foreground();
        TerminalView {
            pty: Rc::new(RefCell::new(pty)),
            focus,
            metrics: Rc::new(RefCell::new(Metrics {
                origin: Point::default(),
                cell_width,
                line_height,
            })),
            foreground: first,
            exited: false,
            scroll_lines: 0.0,
            selecting: false,
            _events: task,
            _foreground: foreground,
        }
    }

    /// The program in the foreground and its directory, as last seen.
    pub fn foreground(&self) -> Option<&Foreground> {
        self.foreground.as_ref()
    }

    fn check_foreground(&mut self, cx: &mut Context<Self>) {
        if self.exited {
            return;
        }
        let now = self.pty.borrow().foreground();
        if now.is_some() && now != self.foreground {
            self.foreground = now;
            cx.emit(TerminalEvent::Foreground);
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        let pty = self.pty.borrow();
        match event {
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => cx.notify(),
            // Tabs are named after their program and directory, not the title a program sets.
            Event::Title(_) | Event::ResetTitle => {}
            Event::PtyWrite(text) => pty.write(text.into_bytes()),
            Event::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            Event::ClipboardLoad(_, reply) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                pty.write(reply(&text).into_bytes());
            }
            Event::ColorRequest(index, reply) => {
                let rgb = {
                    let term = pty.term.lock();
                    let colors = term.colors();
                    match index {
                        256 => palette::named(NamedColor::Foreground, colors),
                        257 => palette::named(NamedColor::Background, colors),
                        258 => palette::named(NamedColor::Cursor, colors),
                        i => colors[i].unwrap_or_else(|| palette::indexed(i as u8)),
                    }
                };
                pty.write(reply(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(reply) => pty.write(reply(pty.window_size()).into_bytes()),
            Event::Exit | Event::ChildExit(_) => {
                if !self.exited {
                    self.exited = true;
                    cx.emit(TerminalEvent::Exited);
                }
            }
            Event::Bell => {}
        }
    }

    fn mode(&self) -> TermMode {
        *self.pty.borrow().term.lock().mode()
    }

    /// The cell under a window position, and which half of it.
    fn cell_at(&self, position: Point<Pixels>) -> (GridPoint, Side) {
        let m = *self.metrics.borrow();
        let pty = self.pty.borrow();
        let size = pty.size();
        let x = ((position.x - m.origin.x) / m.cell_width).max(0.0);
        let y = ((position.y - m.origin.y) / m.line_height).max(0.0);
        let column = (x as usize).min(size.columns - 1);
        let line = (y as usize).min(size.lines - 1);
        let side = if x.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        let offset = pty.term.lock().grid().display_offset() as i32;
        (
            GridPoint::new(Line(line as i32 - offset), Column(column)),
            side,
        )
    }

    /// The visible cell under a window position, for mouse reports.
    fn screen_cell(&self, position: Point<Pixels>) -> (usize, usize) {
        let (point, _) = self.cell_at(position);
        let offset = self.pty.borrow().term.lock().grid().display_offset() as i32;
        (point.column.0, (point.line.0 + offset) as usize)
    }

    fn report_mouse(
        &self,
        button: u8,
        modifiers: Modifiers,
        position: Point<Pixels>,
        pressed: bool,
    ) {
        let (column, line) = self.screen_cell(position);
        let sgr = self.mode().contains(TermMode::SGR_MOUSE);
        if let Some(bytes) = keys::mouse(button, mods(modifiers), column, line, pressed, sgr) {
            self.pty.borrow().write(bytes);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "c" => self.copy(cx),
                "v" => self.paste(cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let app_cursor = self.mode().contains(TermMode::APP_CURSOR);
        let Some(bytes) = keys::encode(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            mods(keystroke.modifiers),
            app_cursor,
        ) else {
            return;
        };
        cx.stop_propagation();
        let pty = self.pty.borrow();
        {
            let mut term = pty.term.lock();
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
        }
        pty.write(bytes);
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.pty.borrow().term.lock().selection_to_string();
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        let pty = self.pty.borrow();
        pty.scroll_to_bottom();
        pty.write(keys::paste(&text, bracketed));
        cx.notify();
    }

    /// Whether the program takes the mouse. Shift gives it back to selection, as in xterm.
    fn program_has_mouse(&self, modifiers: Modifiers) -> bool {
        self.mode().intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if self.program_has_mouse(event.modifiers) {
            self.report_mouse(keys::LEFT, event.modifiers, event.position, true);
            return;
        }
        let (point, side) = self.cell_at(event.position);
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.pty.borrow().term.lock().selection = Some(Selection::new(kind, point, side));
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if self.selecting {
            let (point, side) = self.cell_at(event.position);
            if let Some(selection) = self.pty.borrow().term.lock().selection.as_mut() {
                selection.update(point, side);
            }
            cx.notify();
        } else if self.program_has_mouse(event.modifiers)
            && self
                .mode()
                .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
        {
            self.report_mouse(
                keys::LEFT + keys::MOTION,
                event.modifiers,
                event.position,
                true,
            );
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            let pty = self.pty.borrow();
            let mut term = pty.term.lock();
            if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                term.selection = None;
            }
            cx.notify();
        } else if self.program_has_mouse(event.modifiers) {
            self.report_mouse(keys::LEFT, event.modifiers, event.position, false);
        }
    }

    fn scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self.metrics.borrow().line_height;
        self.scroll_lines += event.delta.pixel_delta(line_height).y / line_height;
        let lines = self.scroll_lines.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_lines -= lines as f32;
        let mode = self.mode();
        if self.program_has_mouse(event.modifiers) {
            let button = if lines > 0 {
                keys::WHEEL_UP
            } else {
                keys::WHEEL_DOWN
            };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(button, event.modifiers, event.position, true);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs such as less get arrow keys, as in other terminals.
            let key = if lines > 0 { "up" } else { "down" };
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
            if let Some(bytes) = keys::encode(key, None, Mods::default(), app_cursor) {
                let bytes = bytes.repeat(lines.unsigned_abs() as usize);
                self.pty.borrow().write(bytes);
            }
        } else {
            self.pty
                .borrow()
                .term
                .lock()
                .scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }
}

fn mods(m: Modifiers) -> Mods {
    Mods {
        shift: m.shift,
        alt: m.alt,
        control: m.control,
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let pty = self.pty.clone();
        let metrics = self.metrics.clone();
        div()
            .size_full()
            .p(px(PADDING))
            .bg(hsla(palette::BACKGROUND))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll(cx.listener(Self::scroll))
            .child(
                canvas(
                    move |bounds, window, _| layout(&pty, &metrics, bounds, focused, window),
                    move |_, frame, window, cx| frame.paint(window, cx),
                )
                .size_full(),
            )
    }
}

/// A run of cells on one line drawn with one style.
#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

struct Segment {
    line: usize,
    column: usize,
    text: String,
    style: Style,
}

/// What one frame paints, computed before painting.
struct Frame {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    backgrounds: Vec<(usize, usize, usize, Rgb)>,
    lines: Vec<(usize, usize, ShapedLine)>,
    cursor: Option<PaintQuad>,
}

impl Frame {
    fn at(&self, line: usize, column: usize) -> Point<Pixels> {
        point(
            self.origin.x + self.cell_width * column as f32,
            self.origin.y + self.line_height * line as f32,
        )
    }

    fn paint(self, window: &mut Window, cx: &mut App) {
        for &(line, column, width, color) in &self.backgrounds {
            let extent = size(self.cell_width * width as f32, self.line_height);
            window.paint_quad(fill(
                Bounds::new(self.at(line, column), extent),
                hsla(color),
            ));
        }
        if let Some(cursor) = self.cursor.clone() {
            window.paint_quad(cursor);
        }
        for (line, column, shaped) in &self.lines {
            let _ = shaped.paint(
                self.at(*line, *column),
                self.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

/// Fits the grid to the bounds, then reads the screen into a frame.
fn layout(
    pty: &Rc<RefCell<Pty>>,
    metrics: &Rc<RefCell<Metrics>>,
    bounds: Bounds<Pixels>,
    focused: bool,
    window: &mut Window,
) -> Frame {
    let Metrics {
        cell_width,
        line_height,
        ..
    } = *metrics.borrow();
    metrics.borrow_mut().origin = bounds.origin;
    let grid = GridSize {
        columns: ((bounds.size.width / cell_width) as usize).max(2),
        lines: ((bounds.size.height / line_height) as usize).max(1),
    };
    let cell = (
        f32::from(cell_width).round() as u16,
        f32::from(line_height) as u16,
    );
    pty.borrow_mut().resize(grid, cell);

    let pty = pty.borrow();
    let term = pty.term.lock();
    let content = term.renderable_content();
    let colors = content.colors;
    let offset = content.display_offset as i32;
    let selection = content.selection;
    let cursor = content.cursor;
    let cursor_line = cursor.point.line.0 + offset;
    let block_cursor = focused && cursor.shape == CursorShape::Block;

    let mut backgrounds: Vec<(usize, usize, usize, Rgb)> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();
    for indexed in content.display_iter {
        let cell = indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let line = (indexed.point.line.0 + offset) as usize;
        let column = indexed.point.column.0;
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let mut fg = palette::resolve(cell.fg, colors);
        let mut bg = palette::resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(Flags::DIM) {
            fg = Rgb {
                r: (fg.r as u16 * 2 / 3) as u8,
                g: (fg.g as u16 * 2 / 3) as u8,
                b: (fg.b as u16 * 2 / 3) as u8,
            };
        }
        if selection.is_some_and(|s| s.contains(indexed.point)) {
            bg = palette::SELECTION;
        }
        if block_cursor && indexed.point == cursor.point {
            bg = palette::named(NamedColor::Cursor, colors);
            fg = palette::named(NamedColor::Background, colors);
        }
        if bg != palette::named(NamedColor::Background, colors) {
            match backgrounds.last_mut() {
                Some((l, c, w, color)) if *l == line && *c + *w == column && *color == bg => {
                    *w += width
                }
                _ => backgrounds.push((line, column, width, bg)),
            }
        }
        let style = Style {
            fg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            strike: cell.flags.contains(Flags::STRIKEOUT),
        };
        let mut text = String::new();
        text.push(if cell.flags.contains(Flags::HIDDEN) {
            ' '
        } else {
            cell.c
        });
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra);
        }
        // A wide character gets a segment of its own, so the cells after it stay on the grid.
        match segments.last_mut() {
            Some(s)
                if width == 1
                    && s.line == line
                    && s.style == style
                    && s.column + s.text.chars().count() == column =>
            {
                s.text.push_str(&text)
            }
            _ => segments.push(Segment {
                line,
                column,
                text,
                style,
            }),
        }
        if width == 2 {
            // Start a fresh segment after the wide character.
            segments.push(Segment {
                line,
                column: column + 2,
                text: String::new(),
                style,
            });
        }
    }

    let font_size = px(FONT_SIZE);
    let text_system = window.text_system().clone();
    let lines = segments
        .into_iter()
        .filter(|s| {
            !s.text.is_empty() && (s.style.underline || s.style.strike || s.text.trim() != "")
        })
        .map(|s| {
            let mut font = font(FONT_FAMILY);
            if s.style.bold {
                font.weight = FontWeight::BOLD;
            }
            if s.style.italic {
                font.style = FontStyle::Italic;
            }
            let color = hsla(s.style.fg);
            let run = TextRun {
                len: s.text.len(),
                font,
                color,
                background_color: None,
                underline: s.style.underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(color),
                    wavy: false,
                }),
                strikethrough: s.style.strike.then_some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(color),
                }),
            };
            let shaped = text_system.shape_line(s.text.into(), font_size, &[run], Some(cell_width));
            (s.line, s.column, shaped)
        })
        .collect();

    let frame_origin = bounds.origin;
    let cursor_quad = (cursor.shape != CursorShape::Hidden
        && !block_cursor
        && cursor_line >= 0
        && (cursor_line as usize) < term.screen_lines())
    .then(|| {
        let origin = point(
            frame_origin.x + cell_width * cursor.point.column.0 as f32,
            frame_origin.y + line_height * cursor_line as f32,
        );
        let color = hsla(palette::named(NamedColor::Cursor, colors));
        match cursor.shape {
            CursorShape::Beam => fill(Bounds::new(origin, size(px(2.0), line_height)), color),
            CursorShape::Underline => fill(
                Bounds::new(
                    point(origin.x, origin.y + line_height - px(2.0)),
                    size(cell_width, px(2.0)),
                ),
                color,
            ),
            _ => outline(
                Bounds::new(origin, size(cell_width, line_height)),
                color,
                BorderStyle::Solid,
            ),
        }
    });

    Frame {
        origin: frame_origin,
        cell_width,
        line_height,
        backgrounds,
        lines,
        cursor: cursor_quad,
    }
}
