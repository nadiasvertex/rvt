//! The app's window: a header, the project sidebar, the home view or the selected project's
//! workspace, and the project panel.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use ctx_core::config::Config;
use ctx_core::item::{ItemRef, Tone};
use ctx_core::live::Status;
use ctx_core::order::{self, Group};
use ctx_core::ops;
use ctx_core::store::{Project, State};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::bridge::{self, Bridge, Command};
use super::commands::{Action, Choice, DoubleShift, Listed, Palette, PaletteEvent};
use super::dialogs::{self, DialogEvent, FreezeDialog, NewProjectDialog, WaitDialog};
use super::snapshot::{JournalEntry, Snapshot, fingerprint};
use super::text;
use super::theme::*;
use super::workspace::{ClaudeLaunch, Workspace, WorkspaceEvent};

actions!(ctx, [TogglePanel, NewProject]);

/// How long a message stays on screen.
const TOAST_FOR: Duration = Duration::from_secs(6);

/// How often the app reads Claude's session registry when nothing in the store changed.
const CLAUDE_EVERY: Duration = Duration::from_secs(5);

/// A project being dragged in the sidebar.
#[derive(Clone)]
struct Dragged {
    slug: String,
    name: String,
    group: Group,
}

impl Render for Dragged {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(rgb(SELECTED))
            .border_1()
            .border_color(rgb(CONTROL_BORDER))
            .font_family(UI_FONT)
            .text_size(px(13.0))
            .text_color(rgb(TEXT))
            .child(self.name.clone())
    }
}

/// Where a dragged project would land: just above or below `target`.
#[derive(Clone, PartialEq)]
struct DropAt {
    target: String,
    above: bool,
}

pub struct Root {
    snapshot: Snapshot,
    /// The selected project, or `None` for the home view.
    selected: Option<String>,
    panel_open: bool,
    journal_open: bool,
    drop_at: Option<DropAt>,
    /// Each project's workspace, opened the first time the project is shown.
    workspaces: HashMap<String, Entity<Workspace>>,
    claude: ClaudeLaunch,
    config: Arc<Option<Config>>,
    bridge: Bridge,
    /// Messages shown at the bottom right, newest last, with their numbers.
    toasts: Vec<(u64, String)>,
    next_toast: u64,
    dialog: Option<(AnyView, Subscription)>,
    /// Whether the dialog sits near the top, as the palette does, rather than in the middle.
    dialog_top: bool,
    double_shift: DoubleShift,
    focus: FocusHandle,
    shown_title: String,
    _reload: Task<()>,
    _commands: Task<()>,
    _workspace_events: Vec<Subscription>,
}

impl Root {
    pub fn new(config: Option<Config>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let claude = ClaudeLaunch::from_config(config.as_ref());
        let config = Arc::new(config);
        let (bridge, commands) = Bridge::new();
        // The watcher refreshes items and clears wait conditions while the app runs, unless
        // ctx 0.3.0's watcher already does.
        if let Some(cfg) = config.as_ref().clone() {
            let terminals = bridge.clone();
            std::thread::spawn(move || {
                if let Err(e) = ctx_core::watch::run(&cfg, &terminals, true, false) {
                    eprintln!("ctx: the watcher stopped: {e:#}");
                }
            });
        }
        let commands = cx.spawn_in(window, async move |this, cx| {
            while let Ok(command) = commands.recv().await {
                if this
                    .update_in(cx, |root, window, cx| root.apply(command, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let reload_config = config.clone();
        let reload = cx.spawn_in(window, async move |this, cx| {
            let config = reload_config;
            let mut last = None;
            let mut loaded = Instant::now();
            loop {
                let now = cx.background_spawn(async { fingerprint() }).await;
                if last != Some(now) || loaded.elapsed() >= CLAUDE_EVERY {
                    let config = config.clone();
                    let snapshot = cx
                        .background_spawn(async move { Snapshot::load(config.as_ref().as_ref()) })
                        .await;
                    last = Some(now);
                    loaded = Instant::now();
                    let updated = this.update_in(cx, |root, window, cx| {
                        root.snapshot = snapshot;
                        root.sync(cx);
                        // A project selected before the store was read gets its workspace now.
                        if root.workspace_entity().is_none() {
                            root.open_workspace(window, cx);
                            // Debug builds can also open the project's newest note.
                            if cfg!(debug_assertions)
                                && std::env::var_os("CTX_GUI_NOTE").is_some()
                            {
                                let note = root
                                    .selected
                                    .as_ref()
                                    .and_then(|s| root.snapshot.notes.get(s))
                                    .and_then(|n| n.first())
                                    .map(|n| n.path.clone());
                                if let Some(path) = note {
                                    root.open_note(path, window, cx);
                                }
                            }
                            // Debug builds can also open a dialog, to check how it looks.
                            if cfg!(debug_assertions) {
                                let slug = root.selected.clone().unwrap_or_default();
                                match std::env::var("CTX_GUI_DIALOG").as_deref() {
                                    Ok("freeze") => root.freeze(&slug, window, cx),
                                    Ok("wait") => root.wait(&slug, window, cx),
                                    Ok("new") => root.new_project(window, cx),
                                    Ok("palette") => root.open_palette(window, cx),
                                    _ => {}
                                }
                                // And open an item's review tab, by item key.
                                if let Ok(key) = std::env::var("CTX_GUI_REVIEW") {
                                    let item = root
                                        .selected_project()
                                        .and_then(|p| p.refs().find(|r| r.key() == key).cloned());
                                    if let Some(item) = item {
                                        root.open_review(item, window, cx);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
                // Keeps the operations' copy of the window current, such as tab directories.
                if this.update(cx, |root, cx| root.sync(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // Debug builds can start on a project, or on its journal, to check the views.
        let (selected, journal_open) = if cfg!(debug_assertions) {
            (
                std::env::var("CTX_GUI_SELECT").ok(),
                std::env::var_os("CTX_GUI_JOURNAL").is_some(),
            )
        } else {
            (None, false)
        };
        Root {
            snapshot: Snapshot::default(),
            selected,
            panel_open: true,
            journal_open,
            drop_at: None,
            workspaces: HashMap::new(),
            claude,
            config,
            bridge,
            toasts: Vec::new(),
            next_toast: 0,
            dialog: None,
            dialog_top: false,
            double_shift: DoubleShift::default(),
            focus,
            shown_title: String::new(),
            _reload: reload,
            _commands: commands,
            _workspace_events: Vec::new(),
        }
    }

    fn select(&mut self, slug: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.journal_open = false;
        // Leaving a project records the visit, so "since you left" starts from now.
        if let Some(from) = self.selected.clone().filter(|f| Some(f) != slug.as_ref()) {
            self.sync(cx);
            let bridge = self.bridge.clone();
            cx.background_spawn(async move { ops::leave(&bridge, &from) })
                .detach();
        }
        self.selected = slug;
        self.open_workspace(window, cx);
        match self.workspace_entity() {
            Some(ws) => ws.read(cx).focus_target(cx).focus(window, cx),
            None => self.focus.focus(window, cx),
        }
        self.sync(cx);
        cx.notify();
    }

    /// Gives ctx's operations a fresh copy of the window's state.
    fn sync(&self, cx: &App) {
        let mut state = bridge::State {
            selected: self.selected.clone(),
            focused: self
                .workspace_entity()
                .and_then(|ws| ws.read(cx).active_terminal()),
            ..bridge::State::default()
        };
        for (slug, ws) in &self.workspaces {
            let name = self
                .snapshot
                .project(slug)
                .map(|p| p.workspace.clone())
                .unwrap_or_else(|| slug.clone());
            let (open, writers) = ws.read(cx).describe(&name, cx);
            state.writers.extend(writers);
            state.open.insert(slug.clone(), open);
        }
        self.bridge.set(state);
    }

    /// Carries out what an operation asked of the window.
    fn apply(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::Open(slug) => self.open_workspace_of(&slug, window, cx),
            Command::Show { slug, note } => {
                self.select(Some(slug), window, cx);
                if let Some(note) = note {
                    self.toast(note, cx);
                }
            }
            Command::ShowHome => self.select(None, window, cx),
            Command::Close(slug) => {
                if self.selected.as_deref() == Some(slug.as_str()) {
                    self.select(None, window, cx);
                }
                // Dropping the workspace ends its terminals.
                self.workspaces.remove(&slug);
                self.sync(cx);
            }
            Command::Message(text) => self.toast(text, cx),
        }
        cx.notify();
    }

    fn toast(&mut self, text: String, cx: &mut Context<Self>) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push((id, text));
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(TOAST_FOR).await;
            let _ = this.update(cx, |root, cx| {
                root.toasts.retain(|(i, _)| *i != id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Runs a ctx operation on a background thread and shows its error, if any.
    fn run_op(
        &mut self,
        what: &'static str,
        cx: &mut Context<Self>,
        op: impl FnOnce(&Bridge) -> anyhow::Result<()> + Send + 'static,
    ) {
        let bridge = self.bridge.clone();
        let task = cx.background_spawn(async move { op(&bridge) });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Err(e) = task.await {
                let _ = this.update(cx, |root, cx| {
                    root.toast(format!("Could not {what}: {e:#}"), cx)
                });
            }
        })
        .detach();
    }

    fn show_dialog<V: Render + EventEmitter<DialogEvent>>(
        &mut self,
        dialog: Entity<V>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let events = cx.subscribe_in(&dialog, window, |root, _, event, window, cx| match event {
            DialogEvent::Close => root.close_dialog(window, cx),
            DialogEvent::Created(slug) => {
                let slug = slug.clone();
                root.close_dialog(window, cx);
                root.select(Some(slug), window, cx);
            }
        });
        self.dialog = Some((dialog.into(), events));
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Utc::now();
        let mut projects = Vec::new();
        for (group, members) in self.snapshot.groups() {
            for p in members {
                projects.push(Listed {
                    slug: p.slug.clone(),
                    name: p.name.clone(),
                    detail: format!("{} · {}", group.label(), text::status(&self.snapshot, p, now)),
                });
            }
        }
        let state = self.selected_project().map(|p| p.state);
        let palette = cx.new(|cx| Palette::new(projects, state, window, cx));
        let events = cx.subscribe_in(&palette, window, |root, _, event, window, cx| match event {
            PaletteEvent::Close => root.close_dialog(window, cx),
            PaletteEvent::Run(choice) => {
                let choice = choice.clone();
                root.close_dialog(window, cx);
                root.run_choice(choice, window, cx);
            }
        });
        self.dialog = Some((palette.into(), events));
        self.dialog_top = true;
        cx.notify();
    }

    fn run_choice(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        match choice {
            Choice::Project(slug) => self.select(Some(slug), window, cx),
            Choice::Note { slug, path } => {
                self.select(Some(slug), window, cx);
                self.open_note(path, window, cx);
            }
            Choice::Item { slug, key } => {
                self.select(Some(slug.clone()), window, cx);
                let item = self
                    .snapshot
                    .project(&slug)
                    .and_then(|p| p.refs().find(|r| r.key() == key).cloned());
                if let Some(item) = item.filter(reviewable) {
                    self.open_review(item, window, cx);
                }
            }
            Choice::Journal { slug } => {
                self.select(Some(slug), window, cx);
                self.journal_open = true;
                self.panel_open = false;
                self.toggle_panel(cx);
            }
            Choice::Action(action) => self.run_action(action, window, cx),
        }
    }

    fn run_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let slug = self.selected.clone();
        let ws = self.workspace_entity().cloned();
        match (action, slug) {
            (Action::Home, _) => self.select(None, window, cx),
            (Action::NewProject, _) => self.new_project(window, cx),
            (Action::TogglePanel, _) => self.toggle_panel(cx),
            (Action::Freeze, Some(s)) => self.freeze(&s, window, cx),
            (Action::Thaw, Some(s)) => self.thaw(&s, window, cx),
            (Action::Wait, Some(s)) => self.wait(&s, window, cx),
            (Action::Done, Some(s)) => self.done(&s, window, cx),
            (Action::ShowJournal, Some(_)) => {
                self.journal_open = true;
                if !self.panel_open {
                    self.toggle_panel(cx);
                }
            }
            (Action::NewTerminal | Action::NewClaude | Action::RenameTab | Action::CloseTab, _) => {
                if let Some(ws) = ws {
                    ws.update(cx, |ws, cx| match action {
                        Action::NewTerminal => ws.new_shell(window, cx),
                        Action::NewClaude => ws.new_claude(window, cx),
                        Action::RenameTab => ws.rename_active(window, cx),
                        _ => window.dispatch_action(Box::new(super::workspace::CloseTab), cx),
                    });
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Shows the item's review replies in a tab of the selected project.
    fn open_review(&mut self, item: ctx_core::item::ItemRef, window: &mut Window, cx: &mut Context<Self>) {
        let url = self
            .snapshot
            .state(&item)
            .and_then(|s| s.url.clone())
            .unwrap_or_default();
        let config = self.config.clone();
        if let Some(ws) = self.workspace_entity().cloned() {
            ws.update(cx, |ws, cx| ws.open_review(item, url, config, window, cx));
        }
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        self.dialog_top = false;
        match self.workspace_entity() {
            Some(ws) => ws.read(cx).focus_target(cx).focus(window, cx),
            None => self.focus.focus(window, cx),
        }
        cx.notify();
    }

    fn load(&mut self, slug: &str, cx: &mut Context<Self>) -> Option<Project> {
        match dialogs::load(slug) {
            Ok(p) => Some(p),
            Err(e) => {
                self.toast(format!("{e:#}"), cx);
                None
            }
        }
    }

    fn freeze(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.load(slug, cx) else {
            return;
        };
        self.sync(cx);
        let (config, bridge) = (self.config.clone(), self.bridge.clone());
        let dialog = cx.new(|cx| FreezeDialog::new(project, config, bridge, window, cx));
        self.show_dialog(dialog, window, cx);
    }

    fn wait(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.load(slug, cx) else {
            return;
        };
        let bridge = self.bridge.clone();
        let dialog = cx.new(|cx| WaitDialog::new(project, bridge, window, cx));
        self.show_dialog(dialog, window, cx);
    }

    fn new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let dialog = cx.new(|cx| NewProjectDialog::new(dir, window, cx));
        self.show_dialog(dialog, window, cx);
    }

    /// Makes a frozen project live and shows its workspace.
    fn thaw(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        let s = slug.to_string();
        self.run_op("thaw the project", cx, move |_| ops::thaw(&s));
        self.select(Some(slug.to_string()), window, cx);
    }

    /// Marks the project done, closes its workspace and goes home.
    fn done(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        let s = slug.to_string();
        self.run_op("mark the project done", cx, move |bridge| {
            ops::done(&s)?;
            if let Ok(project) = ctx_core::store::load(&s) {
                ctx_core::terminals::Terminals::close(bridge, &project)?;
            }
            Ok(())
        });
        if self.selected.as_deref() == Some(slug) {
            self.select(None, window, cx);
        }
    }

    /// The selected project's workspace, if it is open.
    fn workspace_entity(&self) -> Option<&Entity<Workspace>> {
        self.selected.as_ref().and_then(|s| self.workspaces.get(s))
    }

    /// Opens the selected project's workspace unless it is open.
    fn open_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slug) = self.selected.clone() {
            self.open_workspace_of(&slug, window, cx);
        }
    }

    /// Opens the project's workspace unless it is open. A project the snapshot does not have
    /// yet, such as one just made, is read from the store.
    fn open_workspace_of(&mut self, slug: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspaces.contains_key(slug) {
            return;
        }
        let project = match self.snapshot.project(slug) {
            Some(p) => p.clone(),
            None => match ctx_core::store::load(slug) {
                Ok(p) => p,
                Err(_) => return,
            },
        };
        let claude = self.claude.clone();
        let panel_open = self.panel_open;
        let ws = cx.new(|cx| Workspace::new(&project, claude, panel_open, window, cx));
        let events = cx.subscribe(&ws, |root, _, event, cx| match event {
            WorkspaceEvent::TogglePanel => root.toggle_panel(cx),
        });
        self._workspace_events.push(events);
        if self.selected.as_deref() == Some(slug) {
            ws.read(cx).focus_target(cx).focus(window, cx);
        }
        self.workspaces.insert(project.slug, ws);
    }

    fn open_note(&mut self, path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace_entity().cloned() {
            ws.update(cx, |ws, cx| ws.open_note(path, window, cx));
        }
    }

    fn selected_project(&self) -> Option<&Project> {
        self.selected
            .as_deref()
            .and_then(|s| self.snapshot.project(s))
    }

    /// The panel's buttons for the project's state.
    fn actions(&self, p: &Project, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let frozen = p.state == State::Frozen;
        let (a, b, c) = (p.slug.clone(), p.slug.clone(), p.slug.clone());
        div()
            .flex()
            .gap(px(6.0))
            .px(px(18.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(rgb(BORDER))
            .text_size(px(12.0))
            .child(if frozen {
                dialogs::primary("act-thaw", "Thaw")
                    .on_click(cx.listener(move |root, _, window, cx| root.thaw(&a, window, cx)))
            } else {
                dialogs::primary("act-freeze", "Freeze…")
                    .on_click(cx.listener(move |root, _, window, cx| root.freeze(&a, window, cx)))
            })
            .child(
                button("act-wait", "Wait…")
                    .on_click(cx.listener(move |root, _, window, cx| root.wait(&b, window, cx))),
            )
            .child(
                button("act-done", "Done")
                    .on_click(cx.listener(move |root, _, window, cx| root.done(&c, window, cx))),
            )
    }

    fn toasts(&self) -> impl IntoElement + use<> {
        div()
            .absolute()
            .right(px(16.0))
            .bottom(px(16.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(self.toasts.iter().map(|(_, text)| {
                div()
                    .max_w(px(420.0))
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(8.0))
                    .bg(rgb(RAISED))
                    .border_1()
                    .border_color(rgb(CONTROL_BORDER))
                    .shadow_lg()
                    .child(text.clone())
            }))
    }

    fn drop_project(&mut self, dragged: &Dragged, cx: &mut Context<Self>) {
        let Some(at) = self.drop_at.take() else {
            return;
        };
        if let Err(e) = order::place(&self.snapshot.projects, &dragged.slug, &at.target, at.above) {
            eprintln!("ctx: could not save the project order: {e:#}");
        }
        self.snapshot.order = order::load();
        cx.notify();
    }

    fn header(&self, title: &str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("header")
            .relative()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |event, window, _| {
                if event.click_count == 2 {
                    window.titlebar_double_click();
                } else {
                    window.start_window_move();
                }
            })
            .child(
                div()
                    .flex_1()
                    .flex()
                    .justify_center()
                    .text_color(rgb(SECONDARY))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title.to_string()),
            )
            .child(
                div()
                    .id("palette-button")
                    .absolute()
                    .right(px(12.0))
                    .px(px(10.0))
                    .py(px(3.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(rgb(CONTROL_BORDER))
                    .text_size(px(12.0))
                    .text_color(rgb(SECONDARY))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(RAISED)).text_color(rgb(TEXT)))
                    // A mouse down here must not start moving the window.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|root, _, window, cx| {
                            cx.stop_propagation();
                            root.open_palette(window, cx);
                        }),
                    )
                    .child("Search  ⇧⇧"),
            )
    }

    fn sidebar(&self, now: DateTime<Utc>, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.selected.is_none();
        let mut list = div()
            .id("projects")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(8.0))
            .py(px(12.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .id("home-row")
                    .px(px(8.0))
                    .py(px(7.0))
                    .rounded(px(6.0))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(home, |d| d.bg(rgb(SELECTED)))
                    .when(!home, |d| d.hover(|s| s.bg(rgb(RAISED))))
                    .on_click(cx.listener(|root, _, window, cx| root.select(None, window, cx)))
                    .child("Home"),
            )
            .child(
                div()
                    .id("new-project")
                    .px(px(8.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .text_color(rgb(SECONDARY))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(RAISED)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(|root, _, window, cx| root.new_project(window, cx)))
                    .child("+ New project  ⌘N"),
            );
        for (group, projects) in self.snapshot.groups() {
            let mut section = div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(group_heading(group.label(), projects.len()));
            for project in projects {
                section = section.child(self.project_row(project, group, now, cx));
            }
            list = list.child(section);
        }
        if !self.snapshot.unfiled.is_empty() {
            let mut section = div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(group_heading("Unfiled", self.snapshot.unfiled.len()));
            for (i, item) in self.snapshot.unfiled.iter().enumerate() {
                section = section.child(self.unfiled_row(i, item, cx));
            }
            list = list.child(section);
        }
        div()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(SURFACE))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(list)
    }

    fn project_row(
        &self,
        project: &Project,
        group: Group,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selected.as_deref() == Some(project.slug.as_str());
        let marker = self
            .drop_at
            .as_ref()
            .filter(|d| cx.has_active_drag() && d.target == project.slug)
            .map(|d| d.above);
        let color = match group {
            Group::Live => LIVE,
            Group::Ready => ACCENT,
            Group::Waiting => WAIT,
            Group::Done => MUTED,
        };
        let slug = project.slug.clone();
        let target = project.slug.clone();
        let dragged = Dragged {
            slug: project.slug.clone(),
            name: project.name.clone(),
            group,
        };
        div()
            .id(SharedString::from(format!("project-{}", project.slug)))
            .relative()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(8.0))
            .py(px(7.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(SELECTED)))
            .when(!selected, |d| d.hover(|s| s.bg(rgb(RAISED))))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(dot(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(project.name.clone()),
                    ),
            )
            .child(
                div()
                    .pl(px(16.0))
                    .text_size(px(12.0))
                    .text_color(rgb(SECONDARY))
                    .truncate()
                    .child(text::status(&self.snapshot, project, now)),
            )
            .children(marker.map(|above| {
                div()
                    .absolute()
                    .left(px(4.0))
                    .right(px(4.0))
                    .h(px(2.0))
                    .rounded(px(1.0))
                    .bg(rgb(ACCENT))
                    .map(|d| {
                        if above {
                            d.top(px(-2.0))
                        } else {
                            d.bottom(px(-2.0))
                        }
                    })
            }))
            .on_click(
                cx.listener(move |root, _, window, cx| root.select(Some(slug.clone()), window, cx)),
            )
            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
            .on_drag_move(
                cx.listener(move |root, event: &DragMoveEvent<Dragged>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let (from, slug) = {
                        let dragged = event.drag(cx);
                        (dragged.group, dragged.slug.clone())
                    };
                    // A project moves only within its group; its group follows its state.
                    let at = (from == group && slug != target).then(|| DropAt {
                        target: target.clone(),
                        above: event.event.position.y < event.bounds.center().y,
                    });
                    if root.drop_at != at {
                        root.drop_at = at;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|root, dragged: &Dragged, _, cx| root.drop_project(dragged, cx)))
    }

    fn unfiled_row(&self, i: usize, item: &ItemRef, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.snapshot.state(item);
        let title = match state.map(|s| s.title.as_str()).filter(|t| !t.is_empty()) {
            Some(t) => format!("{} · {t}", item.label()),
            None => item.label(),
        };
        div()
            .id(("unfiled", i))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(8.0))
            .py(px(7.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(RAISED)))
            .on_click(cx.listener(|root, _, window, cx| root.select(None, window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(dot(MUTED))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    ),
            )
            .child(
                div()
                    .pl(px(16.0))
                    .text_size(px(12.0))
                    .text_color(rgb(SECONDARY))
                    .truncate()
                    .child(state.map(|s| s.status.clone()).unwrap_or_default()),
            )
    }

    fn home(&self, now: DateTime<Utc>, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = self.snapshot.groups();
        let members = |g: Group| -> Vec<&Project> {
            groups
                .iter()
                .find(|(group, _)| *group == g)
                .map(|(_, ps)| ps.clone())
                .unwrap_or_default()
        };
        let ready = members(Group::Ready);
        let waiting = members(Group::Waiting);
        let unfiled = &self.snapshot.unfiled;
        let mut page = div()
            .id("home")
            .flex_1()
            .min_w(px(0.0))
            .overflow_y_scroll()
            .px(px(40.0))
            .py(px(32.0))
            .flex()
            .flex_col()
            .gap(px(28.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(text::greeting(Local::now())),
                    )
                    .child(div().text_color(rgb(SECONDARY)).child(text::home_summary(
                        ready.len(),
                        waiting.len(),
                        unfiled.len(),
                    ))),
            );
        if !ready.is_empty() {
            let mut grid = div().flex().flex_col().gap(px(12.0));
            for pair in ready.chunks(2) {
                let mut row = div().flex().gap(px(12.0));
                for p in pair {
                    row = row.child(self.ready_card(p, now, cx));
                }
                if pair.len() == 1 {
                    row = row.child(div().flex_1());
                }
                grid = grid.child(row);
            }
            page = page.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(heading("Ready for you"))
                    .child(grid),
            );
        }
        if !waiting.is_empty() {
            let mut rows = div()
                .flex()
                .flex_col()
                .rounded(px(10.0))
                .border_1()
                .border_color(rgb(BORDER))
                .overflow_hidden();
            for (i, p) in waiting.iter().enumerate() {
                let slug = p.slug.clone();
                let wait = p.wait.as_ref();
                rows = rows.child(
                    div()
                        .id(SharedString::from(format!("waiting-{}", p.slug)))
                        .flex()
                        .items_center()
                        .gap(px(14.0))
                        .px(px(16.0))
                        .py(px(12.0))
                        .bg(rgb(CARD))
                        .when(i > 0, |d| d.border_t_1().border_color(rgb(BORDER)))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(RAISED)))
                        .on_click(cx.listener(move |root, _, window, cx| {
                            root.select(Some(slug.clone()), window, cx)
                        }))
                        .child(
                            div()
                                .w(px(180.0))
                                .flex_none()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(p.name.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_color(rgb(WAIT))
                                .child(wait.map(|w| w.condition.describe()).unwrap_or_default()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(12.0))
                                .text_color(rgb(SECONDARY))
                                .child(
                                    wait.map(|w| format!("set {}", text::ago(w.set, now)))
                                        .unwrap_or_default(),
                                ),
                        ),
                );
            }
            page = page.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(heading("Waiting"))
                    .child(rows),
            );
        }
        if !unfiled.is_empty() {
            let mut rows = div().flex().flex_col().gap(px(8.0));
            for item in unfiled {
                let state = self.snapshot.state(item);
                let title = match state.map(|s| s.title.as_str()).filter(|t| !t.is_empty()) {
                    Some(t) => format!("{} · {t}", item.label()),
                    None => item.label(),
                };
                let detail = match state {
                    Some(s) => format!("{} · updated {}", s.status, text::ago(s.fetched, now)),
                    None => "not fetched yet".into(),
                };
                rows = rows.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .px(px(16.0))
                        .py(px(12.0))
                        .rounded(px(10.0))
                        .bg(rgb(CARD))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(div().font_weight(FontWeight::MEDIUM).child(title))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(SECONDARY))
                                .child(detail),
                        ),
                );
            }
            page = page.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(heading("Unfiled"))
                    .child(rows),
            );
        }
        page
    }

    fn ready_card(
        &self,
        p: &Project,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let reason = p.cleared().unwrap_or("ready").to_string();
        let handoff = p.handoff.as_ref();
        let next = if p.next.trim().is_empty() {
            handoff.map(|h| h.next.clone()).unwrap_or_default()
        } else {
            p.next.clone()
        };
        let changes = self
            .snapshot
            .since
            .get(&p.slug)
            .map(|s| s.changes.len())
            .unwrap_or(0);
        let mut since = Vec::new();
        if changes > 0 {
            since.push(match changes {
                1 => "1 change since you left".to_string(),
                n => format!("{n} changes since you left"),
            });
        }
        if let Some(h) = handoff {
            since.push(format!("frozen {}", text::ago(h.time, now)));
        }
        let slug = p.slug.clone();
        let thaw = p.slug.clone();
        div()
            .flex_1()
            .min_w(px(0.0))
            .p(px(16.0))
            .rounded(px(10.0))
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(p.name.clone()),
                    )
                    .child(pill(reason, chip(Tone::Busy))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .children(
                        handoff
                            .map(|h| h.doing.clone())
                            .filter(|d| !d.trim().is_empty())
                            .map(|d| field("Doing", &d, 52.0)),
                    )
                    .when(!next.trim().is_empty(), |d| {
                        d.child(field("Next", &next, 52.0))
                    }),
            )
            .when(!since.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(SECONDARY))
                        .child(since.join(" · ")),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        dialogs::primary(SharedString::from(format!("thaw-{}", p.slug)), "Thaw")
                            .on_click(cx.listener(move |root, _, window, cx| {
                                root.thaw(&thaw, window, cx)
                            })),
                    )
                    .child(
                        button(SharedString::from(format!("look-{}", p.slug)), "Look first")
                            .on_click(cx.listener(move |root, _, window, cx| {
                                root.select(Some(slug.clone()), window, cx)
                            })),
                    ),
            )
    }

    fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        self.panel_open = !self.panel_open;
        let open = self.panel_open;
        for ws in self.workspaces.values() {
            ws.update(cx, |ws, cx| {
                ws.panel_open = open;
                cx.notify();
            });
        }
        cx.notify();
    }

    fn panel(&self, p: &Project, now: DateTime<Utc>, cx: &mut Context<Self>) -> impl IntoElement {
        let group = Group::of(p);
        let colors = match group {
            Group::Live => chip(Tone::Good),
            Group::Ready => chip(Tone::Busy),
            Group::Waiting => chip(Tone::Warn),
            Group::Done => chip(Tone::Muted),
        };
        let header = div()
            .px(px(18.0))
            .pt(px(16.0))
            .pb(px(12.0))
            .border_b_1()
            .border_color(rgb(BORDER))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(17.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(p.name.clone()),
            )
            .child(pill(group.label(), colors));
        let body = if self.journal_open {
            self.journal(p, cx).into_any_element()
        } else {
            self.panel_body(p, now, cx).into_any_element()
        };
        div()
            .w(px(PANEL_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(SURFACE))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(header)
            .child(self.actions(p, cx))
            .child(body)
    }

    fn panel_body(
        &self,
        p: &Project,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut body = div()
            .id("panel")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(18.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(20.0));

        // Handoff.
        let handoff = p.handoff.as_ref();
        let next = if p.next.trim().is_empty() {
            handoff.map(|h| h.next.clone()).unwrap_or_default()
        } else {
            p.next.clone()
        };
        let mut fields = div().flex().flex_col().gap(px(6.0));
        let doing = handoff.map(|h| h.doing.clone()).unwrap_or_default();
        if !doing.trim().is_empty() {
            fields = fields.child(field("Doing", &doing, 72.0));
        }
        if !next.trim().is_empty() {
            fields = fields.child(field("Next", &next, 72.0));
        }
        match &p.wait {
            Some(w) if w.is_waiting() => {
                fields = fields.child(colored_field("Waiting", &w.condition.describe(), WAIT))
            }
            Some(w) => {
                let reason = w
                    .cleared
                    .as_ref()
                    .map(|c| c.reason.clone())
                    .unwrap_or_default();
                fields = fields.child(colored_field("Cleared", &reason, LIVE))
            }
            None => {}
        }
        if doing.trim().is_empty() && next.trim().is_empty() && p.wait.is_none() {
            fields = fields.child(div().text_color(rgb(SECONDARY)).child("No handoff yet."));
        }
        body = body.child(section("Handoff").child(fields));

        // Since you left.
        if let Some(since) = self
            .snapshot
            .since
            .get(&p.slug)
            .filter(|s| !s.changes.is_empty())
        {
            let mut card = div()
                .px(px(12.0))
                .py(px(10.0))
                .rounded(px(8.0))
                .bg(rgb(RAISED))
                .flex()
                .flex_col()
                .gap(px(6.0));
            for c in &since.changes {
                card = card.child(div().child(text::change(c)));
            }
            body = body.child(section("Since you left").child(card));
        }

        // Items.
        if !p.items.is_empty() {
            let mut items = section("Items").gap(px(2.0));
            for entry in &p.items {
                let item = &entry.item;
                let state = self.snapshot.state(item);
                let title = match state.map(|s| s.title.as_str()).filter(|t| !t.is_empty()) {
                    Some(t) => format!("{} · {t}", item.label()),
                    None => item.describe(),
                };
                let (status, tone) = match (state, self.snapshot.cache.errors.get(&item.key())) {
                    (Some(s), _) => (s.status.clone(), s.tone),
                    (None, Some(_)) => ("fetch failed".to_string(), Tone::Bad),
                    (None, None) if item.is_remote() => ("not fetched".to_string(), Tone::Muted),
                    (None, None) => (item.kind_label().to_string(), Tone::Neutral),
                };
                let mut meta = vec![format!("added {}", text::ago(entry.added, now))];
                if let Some(s) = state {
                    if s.replies_due > 0 {
                        meta.insert(0, format!("{} to reply to", s.replies_due));
                    }
                    if s.conflicts {
                        meta.insert(0, "conflicts".into());
                    }
                }
                let review = reviewable(item).then(|| item.clone());
                items = items.child(
                    div()
                        .id(SharedString::from(format!("item-{}", item.key())))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .px(px(10.0))
                        .py(px(8.0))
                        .rounded(px(6.0))
                        .when_some(review, |d, item| {
                            d.cursor_pointer()
                                .hover(|s| s.bg(rgb(RAISED)))
                                .on_click(cx.listener(move |root, _, window, cx| {
                                    root.open_review(item.clone(), window, cx)
                                }))
                        })
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .justify_between()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(title),
                                )
                                .child(pill(status, chip(tone))),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(SECONDARY))
                                .child(meta.join(" · ")),
                        ),
                );
            }
            body = body.child(items);
        }

        // Claude sessions: those running now, then those set aside to resume.
        let running = self
            .snapshot
            .claude
            .get(&p.slug)
            .cloned()
            .unwrap_or_default();
        let frozen: Vec<_> = p
            .claude_sessions
            .iter()
            .filter(|s| s.frozen.is_some() && !running.iter().any(|(r, _)| r.id == s.id))
            .collect();
        if !running.is_empty() || !frozen.is_empty() {
            let mut sessions = section("Claude sessions").gap(px(4.0));
            for (session, live) in &running {
                let (state, color) = match live.status {
                    Status::Idle => ("waiting for you", SECONDARY),
                    _ => ("working", LIVE),
                };
                let started = text::ago(session.started, now);
                sessions = sessions.child(session_row(
                    live.name.clone().unwrap_or_else(|| session.label()),
                    format!("{state} · started {started}"),
                    color,
                    true,
                ));
            }
            for session in frozen {
                let at = session
                    .frozen
                    .as_ref()
                    .map(|f| f.at)
                    .unwrap_or(session.started);
                sessions = sessions.child(session_row(
                    session.label(),
                    format!("frozen {}", text::ago(at, now)),
                    SECONDARY,
                    false,
                ));
            }
            body = body.child(sessions);
        }

        // Notes, which open in a tab.
        if let Some(notes) = self.snapshot.notes.get(&p.slug).filter(|n| !n.is_empty()) {
            let mut list = section("Notes").gap(px(2.0));
            for (i, note) in notes.iter().enumerate() {
                let path = note.path.clone();
                list = list.child(
                    div()
                        .id(("note", i))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .px(px(10.0))
                        .py(px(8.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(RAISED)))
                        .on_click(cx.listener(move |root, _, window, cx| {
                            root.open_note(path.clone(), window, cx)
                        }))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .child(note.title.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(SECONDARY))
                                .child(format!(
                                    "{} · {}",
                                    note.source,
                                    text::ago(note.captured, now)
                                )),
                        ),
                );
            }
            body = body.child(list);
        }

        // The latest journal entries.
        let journal = self.snapshot.journals.get(&p.slug);
        if let Some(entries) = journal.filter(|e| !e.is_empty()) {
            let today = Local::now().date_naive();
            let mut latest = div().flex().flex_col().gap(px(10.0));
            for entry in entries.iter().rev().take(3) {
                let (meta, text) = entry_text(entry);
                let when = entry
                    .time
                    .map(|t| format!("{} {}", text::day(t.date(), today), meta))
                    .unwrap_or(meta);
                latest = latest.child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(rgb(SECONDARY))
                                .child(when),
                        )
                        .child(div().line_clamp(3).child(text)),
                );
            }
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(heading("Journal"))
                            .child(
                                div()
                                    .id("show-journal")
                                    .text_size(px(12.0))
                                    .text_color(rgb(ACCENT))
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(rgb(0xa9cbff)))
                                    .on_click(cx.listener(|root, _, _, cx| {
                                        root.journal_open = true;
                                        cx.notify();
                                    }))
                                    .child("Show all"),
                            ),
                    )
                    .child(latest),
            );
        }
        body
    }

    fn journal(&self, p: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = div()
            .id("journal")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(18.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(button("journal-back", "← Back").on_click(cx.listener(
                        |root, _, _, cx| {
                            root.journal_open = false;
                            cx.notify();
                        },
                    )))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Journal"),
                    ),
            );
        let today = Local::now().date_naive();
        let entries = self
            .snapshot
            .journals
            .get(&p.slug)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut day: Option<(String, Div)> = None;
        for entry in entries.iter().rev() {
            let label = entry
                .time
                .map(|t| text::day(t.date(), today))
                .unwrap_or_else(|| "Undated".into());
            if day.as_ref().is_none_or(|(l, _)| *l != label) {
                if let Some((_, section)) = day.take() {
                    page = page.child(section);
                }
                day = Some((
                    label.clone(),
                    div().flex().flex_col().gap(px(8.0)).child(heading(label)),
                ));
            }
            let (meta, text) = entry_text(entry);
            if let Some((_, section)) = day.as_mut() {
                let row = div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .pl(px(10.0))
                    .border_l_2()
                    .border_color(rgb(CONTROL_BORDER))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(SECONDARY))
                            .child(meta),
                    )
                    .child(div().child(text));
                *section = std::mem::replace(section, div()).child(row);
            }
        }
        if let Some((_, section)) = day {
            page = page.child(section);
        }
        page
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Utc::now();
        let title = self
            .selected_project()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "ctx".into());
        if self.shown_title != title {
            window.set_window_title(&title);
            self.shown_title = title.clone();
        }
        let main = match (self.selected_project(), self.workspace_entity()) {
            (Some(_), Some(ws)) => div()
                .flex_1()
                .min_w(px(0.0))
                .child(ws.clone())
                .into_any_element(),
            (Some(_), None) => div().flex_1().into_any_element(),
            (None, _) => self.home(now, cx).into_any_element(),
        };
        let panel = self
            .selected_project()
            .filter(|_| self.panel_open)
            .map(|p| self.panel(p, now, cx).into_any_element());
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(UI_FONT)
            .text_size(px(13.0))
            .line_height(relative(1.45))
            .text_color(rgb(TEXT))
            .bg(rgb(BACKGROUND))
            .track_focus(&self.focus)
            .relative()
            .on_action(cx.listener(|root, _: &TogglePanel, _, cx| root.toggle_panel(cx)))
            .on_action(cx.listener(|root, _: &NewProject, window, cx| root.new_project(window, cx)))
            .on_modifiers_changed(cx.listener(|root, event: &ModifiersChangedEvent, window, cx| {
                let m = event.modifiers;
                let others = m.control || m.alt || m.platform || m.function;
                let shift_alone = m.shift && !others;
                let nothing = !m.shift && !others;
                if root.double_shift.modifiers(shift_alone, nothing, Instant::now())
                    && root.dialog.is_none()
                {
                    root.open_palette(window, cx);
                }
            }))
            .capture_key_down(cx.listener(|root, _: &KeyDownEvent, _, _| root.double_shift.key()))
            .on_drop(cx.listener(|root, _: &Dragged, _, cx| {
                // A drop outside the project rows leaves the order as it was.
                root.drop_at = None;
                cx.notify();
            }))
            .child(self.header(&title, cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .child(self.sidebar(now, cx))
                    .child(main)
                    .children(panel),
            )
            .child(self.toasts())
            .children(self.dialog.as_ref().map(|(view, _)| {
                div()
                    .id("dialog-layer")
                    .absolute()
                    .inset_0()
                    .flex()
                    .when(self.dialog_top, |d| d.items_start())
                    .when(!self.dialog_top, |d| d.items_center())
                    .justify_center()
                    .bg(hsla(0.0, 0.0, 0.0, 0.5))
                    .occlude()
                    .on_key_down(cx.listener(|root, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            root.close_dialog(window, cx);
                        }
                    }))
                    .child(view.clone())
            }))
    }
}

/// Whether the item has review threads the review tab can show: a PR or an MR.
fn reviewable(item: &ctx_core::item::ItemRef) -> bool {
    matches!(
        item,
        ctx_core::item::ItemRef::AdoPr { .. } | ctx_core::item::ItemRef::GitlabMr { .. }
    )
}

fn group_heading(label: &str, count: usize) -> impl IntoElement + use<> {
    div()
        .flex()
        .justify_between()
        .px(px(8.0))
        .pb(px(4.0))
        .child(heading(label.to_string()))
        .child(heading(count.to_string()))
}

fn section(title: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(heading(title.to_string()))
}

fn field(label: &str, value: &str, width: f32) -> impl IntoElement + use<> {
    div()
        .flex()
        .gap(px(10.0))
        .child(
            div()
                .w(px(width))
                .flex_none()
                .text_color(rgb(SECONDARY))
                .child(label.to_string()),
        )
        .child(div().flex_1().min_w(px(0.0)).child(text::plain(value)))
}

fn colored_field(label: &str, value: &str, color: u32) -> impl IntoElement + use<> {
    div()
        .flex()
        .gap(px(10.0))
        .child(
            div()
                .w(px(72.0))
                .flex_none()
                .text_color(rgb(SECONDARY))
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(rgb(color))
                .child(value.to_string()),
        )
}

fn session_row(name: String, detail: String, color: u32, running: bool) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .when(running, |d| d.bg(rgb(RAISED)))
        .child(div().font_weight(FontWeight::MEDIUM).child(name))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(color))
                .child(detail),
        )
}

/// A journal entry's meta line and text. An entry with a body shows its heading in the meta
/// line, such as `16:05 · frozen`, and an entry without one shows its heading as the text.
fn entry_text(entry: &JournalEntry) -> (String, String) {
    let time = entry.time.map(|t| t.format("%H:%M").to_string());
    let body = text::plain(&entry.body);
    if body.is_empty() {
        (time.unwrap_or_default(), entry.heading.clone())
    } else {
        let meta = match time {
            Some(t) => format!("{t} · {}", entry.heading),
            None => entry.heading.clone(),
        };
        (meta, body)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        // The watcher stops when the window does.
        self.bridge.stop();
    }
}
