//! The dialogs for actions that need input: setting a wait, making a project and freezing one.
//! Each runs its ctx operation on a background thread and closes when it succeeds.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use ctx_core::brief::{self, Draft, Stage};
use ctx_core::config::Config;
use ctx_core::ops;
use ctx_core::store::{self, Handoff, Project};
use ctx_core::wait::{self, Condition};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::bridge::Bridge;
use super::text;
use super::theme::*;

/// What a dialog tells the window.
pub enum DialogEvent {
    Close,
    /// A project was made; the window selects it.
    Created(String),
}

/// The wait condition the user typed, in the form `ctx wait` takes.
pub fn parse_wait(project: &Project, text: &str) -> Result<Option<Condition>, String> {
    let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    if words.is_empty() {
        return Ok(None);
    }
    wait::parse_shell(&words, |arg| ops::find_item(project, arg)).map(Some)
}

/// Runs a ctx operation on a background thread.
fn run(
    cx: &mut App,
    op: impl FnOnce() -> Result<Option<String>> + Send + 'static,
) -> Task<Result<Option<String>>> {
    cx.background_spawn(async move { op() })
}

/// A dialog's frame: a title, the body, an error line and the buttons.
fn frame(
    title: &str,
    body: impl IntoElement,
    error: Option<&str>,
    buttons: impl IntoElement,
    width: f32,
) -> Div {
    div()
        .w(px(width))
        .p(px(20.0))
        .rounded(px(12.0))
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(CONTROL_BORDER))
        .shadow_lg()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_string()),
        )
        .child(body)
        .children(error.map(|e| div().text_color(rgb(BAD)).child(e.to_string())))
        .child(div().flex().justify_end().gap(px(8.0)).child(buttons))
}

fn label(text: &str) -> Div {
    div()
        .text_size(px(12.0))
        .text_color(rgb(SECONDARY))
        .child(text.to_string())
}

/// A filled button for the main action. GPUI allows one hover style, so it does not build on
/// `button`.
pub fn primary(id: impl Into<ElementId>, text: &'static str) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .h(px(26.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(0x3d6bb0))
        .bg(rgb(0x2b4f86))
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x335d9c)))
        .child(text)
}

/// Sets or removes the project's wait condition.
pub struct WaitDialog {
    project: Project,
    bridge: Bridge,
    input: Entity<InputState>,
    error: Option<String>,
    busy: bool,
    _events: Subscription,
}

impl EventEmitter<DialogEvent> for WaitDialog {}

impl WaitDialog {
    pub fn new(project: Project, bridge: Bridge, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("review PR 4711, closed #123, ci !78, until 2h, claude, or any text")
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        let events = cx.subscribe_in(&input, window, |d, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                d.save(cx);
            }
        });
        WaitDialog {
            project,
            bridge,
            input,
            error: None,
            busy: false,
            _events: events,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        match parse_wait(&self.project, &text) {
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
            Ok(None) => {
                self.error = Some("Say what the project waits on.".into());
                cx.notify();
            }
            Ok(Some(condition)) => self.set(Some(condition), cx),
        }
    }

    fn set(&mut self, condition: Option<Condition>, cx: &mut Context<Self>) {
        self.busy = true;
        let bridge = self.bridge.clone();
        let slug = self.project.slug.clone();
        let task = run(cx, move || {
            ops::set_wait(&bridge, &slug, condition)?;
            Ok(None)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |d, cx| d.finish(result, cx));
        })
        .detach();
        cx.notify();
    }

    fn finish(&mut self, result: Result<Option<String>>, cx: &mut Context<Self>) {
        self.busy = false;
        match result {
            Ok(_) => cx.emit(DialogEvent::Close),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        cx.notify();
    }
}

impl Render for WaitDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let waiting = self.project.waiting();
        let items: Vec<String> = self.project.refs().map(|r| r.label()).collect();
        let body = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(
                self.project
                    .wait
                    .as_ref()
                    .filter(|w| w.is_waiting())
                    .map(|w| label(&format!("Now waiting on {}.", w.condition.describe()))),
            )
            .child(Input::new(&self.input))
            .when(!items.is_empty(), |d| {
                d.child(label(&format!("Items: {}", items.join(", "))))
            });
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .when(waiting, |d| {
                d.child(button("wait-remove", "Remove wait").on_click(
                    cx.listener(|d, _, _, cx| d.set(None, cx)),
                ))
            })
            .child(
                button("wait-cancel", "Cancel")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))),
            )
            .child(
                primary("wait-save", if self.busy { "Saving…" } else { "Wait" })
                    .on_click(cx.listener(|d, _, _, cx| d.save(cx))),
            );
        frame(
            &format!("Wait in {}", self.project.name),
            body,
            self.error.as_deref(),
            buttons,
            520.0,
        )
    }
}

/// Makes a new project.
pub struct NewProjectDialog {
    name: Entity<InputState>,
    dir: Entity<InputState>,
    error: Option<String>,
    busy: bool,
    _events: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for NewProjectDialog {}

impl NewProjectDialog {
    pub fn new(dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Project name"));
        let dir = cx.new(|cx| {
            InputState::new(window, cx).default_value(dir.to_string_lossy().into_owned())
        });
        name.update(cx, |i, cx| i.focus(window, cx));
        let mut events = Vec::new();
        for input in [&name, &dir] {
            events.push(cx.subscribe_in(input, window, |d, _, event, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    d.create(cx);
                }
            }));
        }
        NewProjectDialog {
            name,
            dir,
            error: None,
            busy: false,
            _events: events,
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let dir = PathBuf::from(self.dir.read(cx).value().trim());
        if name.is_empty() {
            self.error = Some("Give the project a name.".into());
            cx.notify();
            return;
        }
        if !dir.is_dir() {
            self.error = Some(format!("{} is not a directory.", dir.display()));
            cx.notify();
            return;
        }
        self.busy = true;
        let task = run(cx, move || ops::create(&name, &dir).map(|p| Some(p.slug)));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |d, cx| {
                d.busy = false;
                match result {
                    Ok(slug) => cx.emit(DialogEvent::Created(slug.unwrap_or_default())),
                    Err(e) => d.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for NewProjectDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(label("Name"))
            .child(Input::new(&self.name))
            .child(label(
                "Directory. In a git repository, the current branch becomes the first item.",
            ))
            .child(Input::new(&self.dir));
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .child(
                button("new-cancel", "Cancel")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))),
            )
            .child(
                primary("new-create", if self.busy { "Creating…" } else { "Create" })
                    .on_click(cx.listener(|d, _, _, cx| d.create(cx))),
            );
        frame("New project", body, self.error.as_deref(), buttons, 520.0)
    }
}

/// Where the freeze dialog's draft stands.
#[derive(Clone, PartialEq)]
enum DraftState {
    Running(Stage),
    Arrived,
    /// The user chose to write the handoff, so a later draft is dropped.
    Skipped,
    Failed(String),
}

/// What the draft thread reports.
enum DraftMessage {
    Stage(Stage),
    Done(Result<Draft>),
}

/// Freezes a project with a handoff that Claude drafts and the user checks.
pub struct FreezeDialog {
    project: Project,
    config: Arc<Option<Config>>,
    bridge: Bridge,
    doing: Entity<TextareaState>,
    next: Entity<TextareaState>,
    wait: Entity<InputState>,
    /// The fields the user typed in, which a late draft leaves alone.
    touched: [bool; 3],
    /// Set while the dialog fills the fields, so their change events are not the user's.
    filling: bool,
    draft: DraftState,
    progress: f32,
    close_tabs: bool,
    error: Option<String>,
    busy: bool,
    _events: Vec<Subscription>,
    _draft: Option<Task<()>>,
}

impl EventEmitter<DialogEvent> for FreezeDialog {}

impl FreezeDialog {
    pub fn new(
        project: Project,
        config: Arc<Option<Config>>,
        bridge: Bridge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let doing = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder("What you were doing")
        });
        let next = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder("The next step")
        });
        let wait = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Nothing, or review PR 4711, until 2h, …")
        });
        let events = vec![
            cx.subscribe_in(&doing, window, |d, _, e, _, _| d.touch(0, e)),
            cx.subscribe_in(&next, window, |d, _, e, _, _| d.touch(1, e)),
            cx.subscribe_in(&wait, window, |d, _, e, _, _| d.touch(2, e)),
        ];
        let close_tabs = config
            .as_ref()
            .as_ref()
            .is_some_and(|c| c.workspace.close_on_freeze);
        let mut dialog = FreezeDialog {
            project,
            config,
            bridge,
            doing,
            next,
            wait,
            touched: [false; 3],
            filling: false,
            draft: DraftState::Running(Stage::ReadingClaude),
            progress: Stage::ReadingClaude.progress(),
            close_tabs,
            error: None,
            busy: false,
            _events: events,
            _draft: None,
        };
        dialog.start_draft(window, cx);
        dialog
    }

    fn touch(&mut self, field: usize, event: &InputEvent) {
        if matches!(event, InputEvent::Change) && !self.filling {
            self.touched[field] = true;
        }
    }

    fn start_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config.as_ref().clone() else {
            self.draft = DraftState::Failed("ctx has no config, so Claude cannot draft.".into());
            return;
        };
        let (tx, rx) = async_channel::unbounded();
        let slug = self.project.slug.clone();
        cx.background_spawn(async move {
            let stage_tx = tx.clone();
            let result = brief::draft_handoff(&config, &slug, &move |s| {
                let _ = stage_tx.try_send(DraftMessage::Stage(s));
            });
            let _ = tx.try_send(DraftMessage::Done(result));
        })
        .detach();
        // Moves the bar while Claude writes, which reports nothing until it is done.
        let tick = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let going = this
                    .update(cx, |d, cx| {
                        if d.draft == DraftState::Running(Stage::Writing) {
                            d.progress += (0.95 - d.progress) * 0.02;
                            cx.notify();
                        }
                        matches!(d.draft, DraftState::Running(_))
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        });
        tick.detach();
        self._draft = Some(cx.spawn_in(window, async move |this, cx| {
            while let Ok(message) = rx.recv().await {
                let done = matches!(message, DraftMessage::Done(_));
                let _ = this.update_in(cx, |d, window, cx| d.on_draft(message, window, cx));
                if done {
                    break;
                }
            }
        }));
    }

    fn on_draft(&mut self, message: DraftMessage, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.draft, DraftState::Running(_)) {
            return;
        }
        match message {
            DraftMessage::Stage(stage) => {
                self.draft = DraftState::Running(stage);
                self.progress = self.progress.max(stage.progress());
            }
            DraftMessage::Done(Ok(draft)) => {
                self.progress = 1.0;
                self.draft = DraftState::Arrived;
                self.filling = true;
                if !self.touched[0] {
                    self.doing
                        .update(cx, |i, cx| i.set_value(draft.doing.clone(), window, cx));
                }
                if !self.touched[1] {
                    self.next
                        .update(cx, |i, cx| i.set_value(draft.next.clone(), window, cx));
                }
                if !self.touched[2] {
                    let wait = draft.wait.clone().unwrap_or_default();
                    self.wait.update(cx, |i, cx| i.set_value(wait, window, cx));
                }
                self.filling = false;
            }
            DraftMessage::Done(Err(e)) => {
                self.draft = DraftState::Failed(format!("Claude could not draft the handoff: {e:#}"));
            }
        }
        cx.notify();
    }

    fn write_myself(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft = DraftState::Skipped;
        self.doing.update(cx, |i, cx| i.focus(window, cx));
        cx.notify();
    }

    fn freeze(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.config.as_ref().clone() else {
            self.error = Some("ctx has no config, so it cannot freeze. Run ctx once in a terminal to write one.".into());
            cx.notify();
            return;
        };
        let waiting = self.wait.read(cx).value().trim().to_string();
        let condition = match parse_wait(&self.project, &waiting) {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(format!("Wait: {e}"));
                cx.notify();
                return;
            }
        };
        let handoff = Handoff {
            time: Utc::now(),
            doing: self.doing.read(cx).value().trim().to_string(),
            next: self.next.read(cx).value().trim().to_string(),
            waiting,
        };
        self.busy = true;
        self.error = None;
        let bridge = self.bridge.clone();
        let slug = self.project.slug.clone();
        let close = self.close_tabs;
        let task = run(cx, move || {
            ops::freeze(&config, &bridge, &slug, handoff, condition, close)?;
            Ok(None)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |d, cx| {
                d.busy = false;
                match result {
                    Ok(_) => cx.emit(DialogEvent::Close),
                    Err(e) => d.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn progress_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let DraftState::Running(stage) = self.draft else {
            return match &self.draft {
                DraftState::Failed(e) => Some(label(e).text_color(rgb(WAIT)).into_any_element()),
                _ => None,
            };
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(12.0))
                .rounded(px(8.0))
                .bg(rgb(RAISED))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(div().child(format!("Claude is drafting · {}", stage.label())))
                        .child(
                            div()
                                .id("draft-skip")
                                .text_size(px(12.0))
                                .text_color(rgb(ACCENT))
                                .cursor_pointer()
                                .on_click(cx.listener(|d, _, window, cx| d.write_myself(window, cx)))
                                .child("Write it myself"),
                        ),
                )
                .child(
                    div()
                        .h(px(4.0))
                        .rounded(px(2.0))
                        .bg(rgb(CONTROL_BORDER))
                        .child(
                            div()
                                .h_full()
                                .rounded(px(2.0))
                                .bg(rgb(ACCENT))
                                .w(relative(self.progress.clamp(0.0, 1.0))),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for FreezeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let last = self.project.handoff.as_ref().map(|h| {
            let mut lines = Vec::new();
            if !h.doing.trim().is_empty() {
                lines.push(format!("Doing: {}", text::plain(&h.doing)));
            }
            if !h.next.trim().is_empty() {
                lines.push(format!("Next: {}", text::plain(&h.next)));
            }
            if !h.waiting.trim().is_empty() {
                lines.push(format!("Waiting: {}", h.waiting.trim()));
            }
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .p(px(12.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(rgb(BORDER))
                .child(label(&format!(
                    "Last handoff, {}",
                    text::ago(h.time, Utc::now())
                )))
                .children(lines.into_iter().map(|l| div().text_color(rgb(SECONDARY)).child(l)))
        });
        let close_tabs = self.close_tabs;
        let body = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(self.progress_view(cx))
            .child(label("Doing"))
            .child(Textarea::new(&self.doing))
            .child(label("Next"))
            .child(Textarea::new(&self.next))
            .child(label("Waits on"))
            .child(Input::new(&self.wait))
            .child(
                div()
                    .id("close-tabs")
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .mt(px(4.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|d, _, _, cx| {
                        d.close_tabs = !d.close_tabs;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(14.0))
                            .rounded(px(3.0))
                            .border_1()
                            .border_color(rgb(if close_tabs { ACCENT } else { CONTROL_BORDER }))
                            .when(close_tabs, |d| d.bg(rgb(ACCENT))),
                    )
                    .child("Close the project's tabs and end what runs in them"),
            )
            .children(last);
        let freezing = self.busy;
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .child(
                button("freeze-cancel", "Cancel")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DialogEvent::Close))),
            )
            .when(matches!(self.draft, DraftState::Running(_)), |d| {
                d.child(
                    button("freeze-now", "Freeze without a draft")
                        .on_click(cx.listener(|d, _, _, cx| d.freeze(cx))),
                )
            })
            .child(
                primary("freeze", if freezing { "Freezing…" } else { "Freeze" })
                    .on_click(cx.listener(|d, _, _, cx| d.freeze(cx))),
            );
        frame(
            &format!("Freeze {}", self.project.name),
            body,
            self.error.as_deref(),
            buttons,
            580.0,
        )
    }
}

/// Loads a project for an action, from the store so the action sees its latest state.
pub fn load(slug: &str) -> Result<Project> {
    store::load(slug).map_err(|e| anyhow!("could not read {slug}: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::parse_wait;
    use ctx_core::item::ItemRef;
    use ctx_core::store::{Entry, Project, State};
    use ctx_core::wait::Condition;

    #[test]
    fn a_typed_wait_names_an_item_of_the_project_or_is_free_text() {
        let project = Project {
            name: "p".into(),
            slug: "p".into(),
            state: State::Live,
            created: chrono::Utc::now(),
            workspace: "p".into(),
            dir: None,
            next: String::new(),
            handoff: None,
            wait: None,
            claude_sessions: Vec::new(),
            dismissed: Vec::new(),
            layout: Vec::new(),
            items: vec![Entry {
                added: chrono::Utc::now(),
                item: ItemRef::AdoPr { id: 4711 },
            }],
        };
        assert_eq!(parse_wait(&project, "  "), Ok(None));
        assert!(matches!(
            parse_wait(&project, "closed 4711"),
            Ok(Some(Condition::Closed { .. }))
        ));
        assert!(parse_wait(&project, "closed 99").is_err());
        assert_eq!(
            parse_wait(&project, "answer from jdoe"),
            Ok(Some(Condition::Manual {
                text: "answer from jdoe".into()
            }))
        );
    }
}
