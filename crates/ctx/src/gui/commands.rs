//! The command palette: it finds projects, notes, items and journal entries, and runs the app's
//! actions. Pressing Shift twice opens it. Typing `>` limits it to actions.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ctx_core::search;
use ctx_core::store::State;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::theme::*;

/// The most time between the two Shift presses, and for each press.
const DOUBLE_SHIFT: Duration = Duration::from_millis(300);

/// Tells when Shift was pressed and released twice in quick succession with no other key.
#[derive(Default)]
pub struct DoubleShift {
    down_at: Option<Instant>,
    /// When the last clean Shift tap ended.
    tapped_at: Option<Instant>,
}

impl DoubleShift {
    /// Hears a change of the modifier keys. Returns true when it completes a double tap.
    pub fn modifiers(&mut self, shift_alone: bool, nothing: bool, now: Instant) -> bool {
        if shift_alone {
            self.down_at = Some(now);
            return false;
        }
        let Some(down) = self.down_at.take() else {
            self.tapped_at = None;
            return false;
        };
        if !nothing || now.duration_since(down) > DOUBLE_SHIFT {
            self.tapped_at = None;
            return false;
        }
        match self.tapped_at.take() {
            Some(t) if now.duration_since(t) <= DOUBLE_SHIFT => true,
            _ => {
                self.tapped_at = Some(now);
                false
            }
        }
    }

    /// Hears any other key, which breaks a double tap.
    pub fn key(&mut self) {
        self.down_at = None;
        self.tapped_at = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Home,
    NewProject,
    Freeze,
    Thaw,
    Wait,
    Done,
    NewTerminal,
    NewClaude,
    RenameTab,
    CloseTab,
    TogglePanel,
    ShowJournal,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Home => "Go home",
            Action::NewProject => "New project",
            Action::Freeze => "Freeze the project",
            Action::Thaw => "Thaw the project",
            Action::Wait => "Wait on something",
            Action::Done => "Mark the project done",
            Action::NewTerminal => "New terminal",
            Action::NewClaude => "New Claude tab",
            Action::RenameTab => "Rename the tab",
            Action::CloseTab => "Close the tab",
            Action::TogglePanel => "Show or hide the panel",
            Action::ShowJournal => "Show the journal",
        }
    }

    pub fn keys(self) -> &'static str {
        match self {
            Action::NewProject => "⌘N",
            Action::NewTerminal => "⌘T",
            Action::NewClaude => "⌘⇧T",
            Action::CloseTab => "⌘W",
            Action::TogglePanel => "⌘I",
            _ => "",
        }
    }
}

/// The actions that apply with the selected project in `state`, whose label has `query`.
pub fn actions(query: &str, state: Option<State>) -> Vec<Action> {
    let mut all = vec![Action::NewProject];
    if let Some(state) = state {
        all.push(Action::Home);
        match state {
            State::Frozen => all.push(Action::Thaw),
            _ => all.push(Action::Freeze),
        }
        all.extend([
            Action::Wait,
            Action::Done,
            Action::NewTerminal,
            Action::NewClaude,
            Action::RenameTab,
            Action::CloseTab,
            Action::TogglePanel,
            Action::ShowJournal,
        ]);
    }
    let query = query.trim().to_lowercase();
    all.into_iter()
        .filter(|a| a.label().to_lowercase().contains(&query))
        .collect()
}

/// What the user picked in the palette.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Project(String),
    Note { slug: String, path: PathBuf },
    Item { slug: String, key: String },
    Journal { slug: String },
    Action(Action),
}

struct Entry {
    title: String,
    detail: String,
    keys: &'static str,
    choice: Choice,
}

pub enum PaletteEvent {
    Run(Choice),
    Close,
}

/// A project as the palette lists it.
pub struct Listed {
    pub slug: String,
    pub name: String,
    pub detail: String,
}

pub struct Palette {
    input: Entity<InputState>,
    projects: Vec<Listed>,
    state: Option<State>,
    entries: Vec<Entry>,
    selected: usize,
    _events: Subscription,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    pub fn new(
        projects: Vec<Listed>,
        state: Option<State>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Find a project, note, item or journal entry, or type > for actions")
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        let events = cx.subscribe_in(&input, window, |p, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                p.refresh(&text);
                cx.notify();
            }
        });
        let mut palette = Palette {
            input,
            projects,
            state,
            entries: Vec::new(),
            selected: 0,
            _events: events,
        };
        palette.refresh("");
        palette
    }

    fn refresh(&mut self, text: &str) {
        self.selected = 0;
        self.entries.clear();
        let action = |a: Action| Entry {
            title: a.label().into(),
            detail: String::new(),
            keys: a.keys(),
            choice: Choice::Action(a),
        };
        if let Some(rest) = text.strip_prefix('>') {
            self.entries = actions(rest, self.state).into_iter().map(action).collect();
            return;
        }
        let query = text.trim().to_lowercase();
        for p in &self.projects {
            if p.name.to_lowercase().contains(&query) || p.slug.contains(&query) {
                self.entries.push(Entry {
                    title: p.name.clone(),
                    detail: p.detail.clone(),
                    keys: "",
                    choice: Choice::Project(p.slug.clone()),
                });
            }
        }
        if query.chars().count() >= 2 {
            let name = |slug: &str| {
                self.projects
                    .iter()
                    .find(|p| p.slug == slug)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| slug.to_string())
            };
            for hit in search::search(&query, None, 12).unwrap_or_default() {
                let snippet = hit
                    .snippet
                    .replace([search::MATCH_START, search::MATCH_END], "");
                let (kind, choice) = match hit.kind.as_str() {
                    "note" => (
                        "Note",
                        Choice::Note {
                            slug: hit.project.clone(),
                            path: PathBuf::from(&hit.reference),
                        },
                    ),
                    "item" => (
                        "Item",
                        Choice::Item {
                            slug: hit.project.clone(),
                            key: hit.reference.clone(),
                        },
                    ),
                    "journal" => (
                        "Journal",
                        Choice::Journal {
                            slug: hit.project.clone(),
                        },
                    ),
                    _ => continue,
                };
                self.entries.push(Entry {
                    title: format!("{kind} · {}", hit.title),
                    detail: format!("{} · {snippet}", name(&hit.project)),
                    keys: "",
                    choice,
                });
            }
        }
        let found: Vec<Action> = actions(&query, self.state)
            .into_iter()
            .take(if query.is_empty() { 0 } else { 5 })
            .collect();
        self.entries.extend(found.into_iter().map(action));
    }

    fn key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "down" => {
                if self.selected + 1 < self.entries.len() {
                    self.selected += 1;
                }
            }
            "up" => self.selected = self.selected.saturating_sub(1),
            "enter" => {
                if let Some(e) = self.entries.get(self.selected) {
                    cx.emit(PaletteEvent::Run(e.choice.clone()));
                }
            }
            "escape" => cx.emit(PaletteEvent::Close),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("palette-list")
            .max_h(px(420.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .py(px(4.0));
        for (i, e) in self.entries.iter().enumerate() {
            let choice = e.choice.clone();
            list = list.child(
                div()
                    .id(("palette", i))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(14.0))
                    .py(px(7.0))
                    .cursor_pointer()
                    .when(i == self.selected, |d| d.bg(rgb(SELECTED)))
                    .hover(|s| s.bg(rgb(RAISED)))
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(PaletteEvent::Run(choice.clone()))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .child(div().truncate().child(e.title.clone()))
                            .when(!e.detail.is_empty(), |d| {
                                d.child(
                                    div()
                                        .truncate()
                                        .text_size(px(12.0))
                                        .text_color(rgb(SECONDARY))
                                        .child(e.detail.clone()),
                                )
                            }),
                    )
                    .when(!e.keys.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(MUTED))
                                .child(e.keys),
                        )
                    }),
            );
        }
        if self.entries.is_empty() {
            list = list.child(
                div()
                    .px(px(14.0))
                    .py(px(10.0))
                    .text_color(rgb(SECONDARY))
                    .child("Nothing matches."),
            );
        }
        div()
            .w(px(620.0))
            .mt(px(80.0))
            .rounded(px(12.0))
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(CONTROL_BORDER))
            .shadow_lg()
            .overflow_hidden()
            .capture_key_down(cx.listener(Self::key))
            .child(
                div()
                    .p(px(10.0))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(Input::new(&self.input)),
            )
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, DoubleShift, actions};
    use ctx_core::store::State;
    use std::time::{Duration, Instant};

    /// Presses and releases Shift at `at` milliseconds for `held` milliseconds.
    fn tap(d: &mut DoubleShift, start: Instant, at: u64, held: u64) -> bool {
        let down = start + Duration::from_millis(at);
        assert!(!d.modifiers(true, false, down));
        d.modifiers(false, true, down + Duration::from_millis(held))
    }

    #[test]
    fn two_quick_shift_taps_open_the_palette() {
        let start = Instant::now();
        let mut d = DoubleShift::default();
        assert!(!tap(&mut d, start, 0, 80));
        assert!(tap(&mut d, start, 200, 80));
        // A third tap starts over.
        assert!(!tap(&mut d, start, 400, 80));
    }

    #[test]
    fn slow_taps_held_shift_and_other_keys_do_not() {
        let start = Instant::now();
        let mut d = DoubleShift::default();
        assert!(!tap(&mut d, start, 0, 80));
        assert!(!tap(&mut d, start, 800, 80), "too far apart");

        let mut d = DoubleShift::default();
        assert!(!tap(&mut d, start, 0, 500), "held too long");
        assert!(!tap(&mut d, start, 600, 80));

        let mut d = DoubleShift::default();
        assert!(!tap(&mut d, start, 0, 80));
        d.key();
        assert!(!tap(&mut d, start, 200, 80), "a key in between");

        // Shift with another modifier, as in Shift-Command, is not a tap.
        let mut d = DoubleShift::default();
        assert!(!tap(&mut d, start, 0, 80));
        let at = start + Duration::from_millis(200);
        assert!(!d.modifiers(true, false, at));
        assert!(!d.modifiers(false, false, at + Duration::from_millis(50)));
    }

    #[test]
    fn typing_capitals_does_not_open_the_palette() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut d = DoubleShift::default();
        // "AB" typed quickly: Shift goes down and up around each letter.
        for (down, up) in [(0, 60), (120, 180)] {
            assert!(!d.modifiers(true, false, at(down)));
            d.key();
            assert!(!d.modifiers(false, true, at(up)));
        }
    }

    #[test]
    fn actions_follow_the_selected_project() {
        assert_eq!(actions("", None), [Action::NewProject]);
        let live = actions("", Some(State::Live));
        assert!(live.contains(&Action::Freeze) && !live.contains(&Action::Thaw));
        let frozen = actions("", Some(State::Frozen));
        assert!(frozen.contains(&Action::Thaw) && !frozen.contains(&Action::Freeze));
        assert_eq!(actions("FREEZE", Some(State::Live)), [Action::Freeze]);
    }
}
