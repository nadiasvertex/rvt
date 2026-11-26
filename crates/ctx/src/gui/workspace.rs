//! A project's workspace: its tabs, each a terminal or a note, and the tab strip above them.
//! Each project keeps its own workspace while the app runs, so switching projects leaves the
//! other projects' programs running.

use std::path::{Path, PathBuf};

use ctx_core::config::Config;
use ctx_core::store::{Project, Window as SavedWindow};
use gpui_kit::prelude::*;
use gpui_kit::*;

use alacritty_terminal::event_loop::EventLoopSender;

use super::bridge::OpenWorkspace;
use super::note::{NoteEvent, NoteView};
use super::pty::{GridSize, Launch, Pty};
use super::tabs::{self, Restored, Saved};
use super::theme::*;
use super::view::{TerminalEvent, TerminalView};

actions!(
    ctx,
    [NewTerminal, NewClaude, CloseTab, NextTab, PreviousTab]
);

/// What the workspace asks of the window around it.
pub enum WorkspaceEvent {
    TogglePanel,
}

/// How a Claude tab starts Claude, from the config.
#[derive(Clone)]
pub struct ClaudeLaunch {
    pub command: String,
    pub config_dir: Option<PathBuf>,
}

impl ClaudeLaunch {
    pub fn from_config(config: Option<&Config>) -> Self {
        ClaudeLaunch {
            command: config
                .map(|c| c.claude.command.trim().to_string())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "claude".into()),
            config_dir: config.and_then(|c| c.claude.config_path()),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Shell,
    Claude,
}

enum Content {
    Terminal(Kind, Entity<TerminalView>),
    Note(Entity<NoteView>),
}

struct Tab {
    /// `gui:<n>` for a terminal, `note:<path>` for a note.
    id: String,
    content: Content,
    /// The name the user set, if any.
    name: Option<String>,
    _events: Subscription,
}

pub struct Workspace {
    slug: String,
    dir: PathBuf,
    claude: ClaudeLaunch,
    tabs: Vec<Tab>,
    active: usize,
    /// The tab being renamed and the text typed so far.
    renaming: Option<(String, String)>,
    rename_focus: FocusHandle,
    focus: FocusHandle,
    pub panel_open: bool,
    error: Option<String>,
    /// Whether the user opened, closed or renamed a tab since the workspace was restored.
    changed: bool,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    /// Opens the project's workspace from its saved layout, or with one shell.
    pub fn new(
        project: &Project,
        claude: ClaudeLaunch,
        panel_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dir = tabs::start_dir(project);
        let mut workspace = Workspace {
            slug: project.slug.clone(),
            dir: dir.clone(),
            claude,
            tabs: Vec::new(),
            active: 0,
            renaming: None,
            rename_focus: cx.focus_handle(),
            focus: cx.focus_handle(),
            panel_open,
            error: None,
            changed: false,
        };
        for Restored { dir, name } in tabs::restore(&project.layout, &dir) {
            workspace.open_terminal(Kind::Shell, Launch::default(), dir, name, window, cx);
        }
        workspace.active = 0;
        workspace.changed = false;
        workspace
    }

    /// What takes the keyboard in the workspace: the active terminal, or the workspace itself.
    pub fn focus_target(&self, cx: &App) -> FocusHandle {
        match self.tabs.get(self.active).map(|t| &t.content) {
            Some(Content::Terminal(_, view)) => view.read(cx).focus_handle(),
            _ => self.focus.clone(),
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_target(cx).focus(window, cx);
    }

    fn new_id(&mut self) -> Option<String> {
        match tabs::next_id(&tabs::counter_path()) {
            Ok(id) => Some(id),
            Err(e) => {
                self.error = Some(format!("Could not number the terminal: {e:#}"));
                None
            }
        }
    }

    fn open_terminal(
        &mut self,
        kind: Kind,
        mut launch: Launch,
        dir: PathBuf,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.new_id() else {
            return;
        };
        launch.dir = Some(dir);
        launch.env.insert("CTX_PROJECT".into(), self.slug.clone());
        launch.env.insert("CTX_TERMINAL".into(), id.clone());
        // The first layout resizes the grid to the tab.
        let size = GridSize {
            columns: 100,
            lines: 30,
        };
        let (pty, events) = match Pty::spawn(&launch, size, (8, 17)) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("Could not start the terminal: {e:#}"));
                cx.notify();
                return;
            }
        };
        self.error = None;
        let view = cx.new(|cx| TerminalView::new(pty, events, window, cx));
        let tab_id = id.clone();
        let events = cx.subscribe_in(&view, window, move |ws, _, event, window, cx| match event {
            TerminalEvent::Exited => ws.close(&tab_id, window, cx),
            TerminalEvent::Foreground => cx.notify(),
        });
        self.changed = true;
        self.tabs.push(Tab {
            id,
            content: Content::Terminal(kind, view),
            name,
            _events: events,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    pub fn new_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active_dir(cx).unwrap_or_else(|| self.dir.clone());
        self.open_terminal(Kind::Shell, Launch::default(), dir, None, window, cx);
    }

    /// Opens a tab that runs Claude in the project directory. The tab closes when Claude exits.
    pub fn new_claude(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut launch = Launch {
            command: shell_command(&format!("exec {}", self.claude.command), &[]),
            ..Launch::default()
        };
        if let Some(dir) = &self.claude.config_dir {
            launch.env.insert(
                "CLAUDE_CONFIG_DIR".into(),
                dir.to_string_lossy().into_owned(),
            );
            launch.env.insert(
                "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD".into(),
                "1".into(),
            );
        }
        let dir = self.dir.clone();
        self.open_terminal(Kind::Claude, launch, dir, None, window, cx);
    }

    /// Shows the note in a tab, reusing the note's tab when it is open.
    pub fn open_note(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let id = format!("note:{}", path.display());
        if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
            self.activate(i, window, cx);
            return;
        }
        let view = cx.new(|cx| NoteView::new(path, cx));
        let events = cx.subscribe_in(&view, window, |ws, _, event, window, cx| match event {
            NoteEvent::Edit(path) => ws.edit(path, window, cx),
        });
        self.tabs.push(Tab {
            id,
            content: Content::Note(view),
            name: None,
            _events: events,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    /// Opens a file in `$VISUAL` or `$EDITOR` in a new tab, or in the default text editor.
    fn edit(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let has_editor = ["VISUAL", "EDITOR"]
            .iter()
            .any(|v| std::env::var(v).is_ok_and(|e| !e.trim().is_empty()));
        if !has_editor {
            if let Err(e) = std::process::Command::new("open")
                .arg("-t")
                .arg(path)
                .spawn()
            {
                self.error = Some(format!("Could not open the note: {e}"));
                cx.notify();
            }
            return;
        }
        let launch = Launch {
            command: shell_command(
                r#"eval "exec ${VISUAL:-$EDITOR} \"\$1\"""#,
                &[path.to_string_lossy().into_owned()],
            ),
            ..Launch::default()
        };
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.dir.clone());
        self.open_terminal(Kind::Shell, launch, dir, None, window, cx);
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.active = index;
        }
        self.renaming = None;
        self.focus(window, cx);
        cx.notify();
    }

    fn close(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.tabs.remove(index);
        self.changed = true;
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if self.renaming.as_ref().is_some_and(|(r, _)| r == id) {
            self.renaming = None;
        }
        self.focus(window, cx);
        cx.notify();
    }

    fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.tabs.get(self.active).map(|t| t.id.clone()) {
            self.close(&id, window, cx);
        }
    }

    fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        if n > 1 {
            let next = if forward {
                (self.active + 1) % n
            } else {
                (self.active + n - 1) % n
            };
            self.activate(next, window, cx);
        }
    }

    /// The working directory of the active terminal, for a new terminal to start in.
    fn active_dir(&self, cx: &App) -> Option<PathBuf> {
        match self.tabs.get(self.active).map(|t| &t.content) {
            Some(Content::Terminal(_, view)) => view
                .read(cx)
                .foreground()
                .and_then(|f| f.dir.clone())
                .filter(|d| d.is_dir()),
            _ => None,
        }
    }

    /// The tab's automatic name: its program and directory, or the note's title.
    fn auto_name(&self, tab: &Tab, cx: &App) -> String {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match &tab.content {
            Content::Terminal(kind, view) => {
                let fg = view.read(cx).foreground();
                // Claude names its process after its version, so a Claude tab says `claude`
                // while Claude runs.
                let program = match (kind, fg) {
                    (Kind::Claude, _) => "claude".to_string(),
                    (_, Some(f)) => f.program.clone(),
                    (_, None) => "terminal".to_string(),
                };
                let dir = fg.and_then(|f| f.dir.as_deref());
                tabs::auto_name(&program, dir, home.as_deref())
            }
            Content::Note(view) => view.read(cx).title(),
        }
    }

    fn shown_name(&self, tab: &Tab, cx: &App) -> String {
        tab.name.clone().unwrap_or_else(|| self.auto_name(tab, cx))
    }

    /// The workspace as ctx's operations see it, and each terminal's input.
    pub fn describe(&self, name: &str, cx: &App) -> (OpenWorkspace, Vec<(String, EventLoopSender)>) {
        let writers: Vec<(String, EventLoopSender)> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.content {
                Content::Terminal(_, view) => Some((t.id.clone(), view.read(cx).writer())),
                Content::Note(_) => None,
            })
            .collect();
        let open = OpenWorkspace {
            name: name.to_string(),
            layout: self.changed.then(|| self.layout(cx)),
            terminals: writers.iter().map(|(id, _)| id.clone()).collect(),
        };
        (open, writers)
    }

    /// The ID of the active tab's terminal, if the active tab is a terminal.
    pub fn active_terminal(&self) -> Option<String> {
        self.tabs
            .get(self.active)
            .filter(|t| matches!(t.content, Content::Terminal(..)))
            .map(|t| t.id.clone())
    }

    /// The layout to save for the workspace: its shell tabs, with the names the user set.
    pub fn layout(&self, cx: &App) -> Vec<SavedWindow> {
        let saved: Vec<Saved> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.content {
                Content::Terminal(Kind::Shell, view) => Some(Saved {
                    shown: self.shown_name(t, cx),
                    set: t.name.clone(),
                    dir: view
                        .read(cx)
                        .foreground()
                        .and_then(|f| f.dir.clone())
                        .unwrap_or_else(|| self.dir.clone()),
                }),
                _ => None,
            })
            .collect();
        tabs::save(&saved)
    }

    fn start_rename(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        self.active = index;
        self.renaming = Some((tab.id.clone(), self.shown_name(tab, cx)));
        self.rename_focus.focus(window, cx);
        cx.notify();
    }

    fn rename_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, text)) = self.renaming.as_mut() else {
            return;
        };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return;
        }
        cx.stop_propagation();
        match keystroke.key.as_str() {
            "enter" => {
                let name = tabs::typed_name(text);
                let id = id.clone();
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tab.name = name;
                }
                self.changed = true;
                self.renaming = None;
                self.focus(window, cx);
            }
            "escape" => {
                self.renaming = None;
                self.focus(window, cx);
            }
            "backspace" => {
                text.pop();
            }
            _ => {
                if let Some(c) = keystroke.key_char.as_deref() {
                    text.push_str(c);
                }
            }
        }
        cx.notify();
    }

    fn tab(&self, index: usize, tab: &Tab, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let active = index == self.active;
        let renaming = self
            .renaming
            .as_ref()
            .filter(|(id, _)| *id == tab.id)
            .map(|(_, text)| text.clone());
        let color = match &tab.content {
            Content::Terminal(Kind::Claude, _) => LIVE,
            Content::Terminal(Kind::Shell, _) => FAINT,
            Content::Note(_) => ACCENT,
        };
        let id = tab.id.clone();
        let label = match renaming {
            Some(text) => div()
                .track_focus(&self.rename_focus)
                .on_key_down(cx.listener(Self::rename_key))
                .px(px(4.0))
                .rounded(px(4.0))
                .bg(rgb(RAISED))
                .border_1()
                .border_color(rgb(ACCENT))
                .text_color(rgb(TEXT))
                .child(format!("{text}▏")),
            None => div()
                .max_w(px(220.0))
                .truncate()
                .child(self.shown_name(tab, cx)),
        };
        div()
            .id(SharedString::from(format!("tab-{}", tab.id)))
            .group("tab")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(14.0))
            .pr(px(8.0))
            .border_r_1()
            .border_color(rgb(BORDER))
            .text_size(px(12.5))
            .cursor_pointer()
            .when(active, |d| {
                d.bg(rgb(BACKGROUND))
                    .text_color(rgb(TEXT))
                    .border_b_2()
                    .border_color(rgb(ACCENT))
            })
            .when(!active, |d| {
                d.text_color(rgb(SECONDARY)).hover(|s| s.bg(rgb(RAISED)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, event: &MouseDownEvent, window, cx| {
                    if event.click_count == 2 {
                        ws.start_rename(index, window, cx);
                    } else if ws.renaming.as_ref().is_none_or(|(r, _)| *r != ws.tabs[index].id)
                    {
                        ws.activate(index, window, cx);
                    }
                }),
            )
            .child(dot(color))
            .child(label)
            .child(
                div()
                    .id(SharedString::from(format!("close-{}", tab.id)))
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .text_color(rgb(MUTED))
                    .when(!active, |d| d.invisible().group_hover("tab", |s| s.visible()))
                    .hover(|s| s.bg(rgb(SELECTED)).text_color(rgb(TEXT)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |ws, _, window, cx| {
                            cx.stop_propagation();
                            ws.close(&id, window, cx);
                        }),
                    )
                    .child("×"),
            )
    }

    fn strip(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let panel_open = self.panel_open;
        let mut tabs = div()
            .id("tabs")
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_stretch()
            .overflow_x_scroll();
        for (i, tab) in self.tabs.iter().enumerate() {
            tabs = tabs.child(self.tab(i, tab, cx));
        }
        tabs = tabs
            .child(
                small_button("new-terminal", "+ Terminal")
                    .ml(px(6.0))
                    .on_click(cx.listener(|ws, _, window, cx| ws.new_shell(window, cx))),
            )
            .child(
                small_button("new-claude", "+ Claude")
                    .ml(px(6.0))
                    .on_click(cx.listener(|ws, _, window, cx| ws.new_claude(window, cx))),
            );
        div()
            .h(px(TABS_HEIGHT))
            .flex_none()
            .flex()
            .items_stretch()
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(tabs)
            .child(
                small_button(
                    "panel-toggle",
                    if panel_open {
                        "Hide panel ⌘I"
                    } else {
                        "Show panel ⌘I"
                    },
                )
                .mx(px(8.0))
                .when(panel_open, |d| d.bg(rgb(SELECTED)).text_color(rgb(TEXT)))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(WorkspaceEvent::TogglePanel))),
            )
    }

    fn empty(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .text_color(rgb(SECONDARY))
            .child("No tabs are open.")
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        button("empty-terminal", "New terminal ⌘T")
                            .on_click(cx.listener(|ws, _, window, cx| ws.new_shell(window, cx))),
                    )
                    .child(
                        button("empty-claude", "New Claude tab")
                            .on_click(cx.listener(|ws, _, window, cx| ws.new_claude(window, cx))),
                    ),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.tabs.get(self.active).map(|t| &t.content) {
            Some(Content::Terminal(_, view)) => view.clone().into_any_element(),
            Some(Content::Note(view)) => view.clone().into_any_element(),
            None => self.empty(cx).into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_action(cx.listener(|ws, _: &NewTerminal, window, cx| ws.new_shell(window, cx)))
            .on_action(cx.listener(|ws, _: &NewClaude, window, cx| ws.new_claude(window, cx)))
            .on_action(cx.listener(|ws, _: &CloseTab, window, cx| ws.close_active(window, cx)))
            .on_action(cx.listener(|ws, _: &NextTab, window, cx| ws.step(true, window, cx)))
            .on_action(cx.listener(|ws, _: &PreviousTab, window, cx| ws.step(false, window, cx)))
            .child(self.strip(cx))
            .children(self.error.clone().map(|e| {
                div()
                    .flex_none()
                    .px(px(14.0))
                    .py(px(6.0))
                    .bg(rgb(chip(ctx_core::item::Tone::Bad).0))
                    .text_color(rgb(BAD))
                    .child(e)
            }))
            .child(div().flex_1().min_h(px(0.0)).child(content))
    }
}

/// The user's shell running `script` with `args` as `$1` and on, as a login shell so it has
/// the user's `PATH` when the app starts outside a terminal.
fn shell_command(script: &str, args: &[String]) -> Vec<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into());
    let mut command = vec![shell, "-l".into(), "-c".into(), script.into(), "ctx".into()];
    command.extend(args.iter().cloned());
    command
}

fn small_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .my(px(6.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(CONTROL_BORDER))
        .text_size(px(12.0))
        .text_color(rgb(SECONDARY))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(RAISED)).text_color(rgb(TEXT)))
        .child(label)
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob brings its own `test` attribute.
    use super::shell_command;

    #[test]
    fn a_shell_command_passes_its_arguments_as_positional_parameters() {
        let command = shell_command("echo \"$1\"", &["a b".into()]);
        assert_eq!(&command[1..], ["-l", "-c", "echo \"$1\"", "ctx", "a b"]);
        let out = std::process::Command::new(&command[0])
            .args(&command[1..])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).ends_with("a b\n"));
    }
}
