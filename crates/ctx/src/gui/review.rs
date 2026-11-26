//! A PR's or MR's review threads in a tab: Claude's assessment of each, its plan and a draft
//! reply, which the user edits, checks, posts and resolves. Nothing reaches the PR until the user
//! posts it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use ctx_core::config::Config;
use ctx_core::item::{ItemRef, Tone};
use ctx_core::ops;
use ctx_core::respond::{self, Assessment, Evaluation};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::dialogs::primary;
use super::text;
use super::theme::*;

/// What a review tab asks of its workspace.
pub enum ReviewEvent {
    /// Start Claude in `dir` with `prompt`, to work on the handed plans.
    HandToClaude { dir: PathBuf, prompt: String },
}

pub struct ReviewView {
    slug: String,
    item: ItemRef,
    url: String,
    config: Arc<Option<Config>>,
    evaluation: Option<Evaluation>,
    replies: Vec<Entity<TextareaState>>,
    /// Set while the dialog fills the replies, so their change events are not the user's.
    filling: bool,
    /// What runs now, such as "posting reply 1 of 2".
    busy: Option<String>,
    message: Option<(String, bool)>,
    _events: Vec<Subscription>,
}

impl EventEmitter<ReviewEvent> for ReviewView {}

impl ReviewView {
    pub fn new(
        slug: String,
        item: ItemRef,
        url: String,
        config: Arc<Option<Config>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = ReviewView {
            evaluation: respond::load(&slug, &item),
            slug,
            item,
            url,
            config,
            replies: Vec::new(),
            filling: false,
            busy: None,
            message: None,
            _events: Vec::new(),
        };
        view.make_replies(window, cx);
        view
    }

    pub fn title(&self) -> String {
        format!("Review · {}", self.item.label())
    }

    /// One editable reply per response.
    fn make_replies(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replies.clear();
        self._events.clear();
        let Some(evaluation) = &self.evaluation else {
            return;
        };
        let texts: Vec<String> = evaluation.responses.iter().map(|r| r.reply.clone()).collect();
        self.filling = true;
        for (i, reply) in texts.into_iter().enumerate() {
            let state = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 10)
                    .default_value(reply)
            });
            self._events.push(cx.subscribe_in(&state, window, move |v, s, e, _, cx| {
                if matches!(e, InputEvent::Change) && !v.filling {
                    let text = s.read(cx).value().to_string();
                    v.edit(i, |r| r.reply = text, cx);
                }
            }));
            self.replies.push(state);
        }
        self.filling = false;
    }

    /// Changes one response and saves the responses.
    fn edit(
        &mut self,
        i: usize,
        change: impl FnOnce(&mut respond::Response),
        cx: &mut Context<Self>,
    ) {
        let Some(evaluation) = self.evaluation.as_mut() else {
            return;
        };
        if let Some(r) = evaluation.responses.get_mut(i) {
            change(r);
        }
        if let Err(e) = respond::save(&self.slug, evaluation) {
            self.message = Some((format!("Could not save: {e:#}"), true));
        }
        cx.notify();
    }

    fn config(&mut self, cx: &mut Context<Self>) -> Option<Config> {
        let config = self.config.as_ref().clone();
        if config.is_none() {
            self.message = Some(("ctx has no config.".into(), true));
            cx.notify();
        }
        config
    }

    /// Runs `job` on a background thread, showing the steps it reports.
    fn job<T: Send + 'static>(
        &mut self,
        first: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce(&dyn Fn(String)) -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        self.busy = Some(first.to_string());
        self.message = None;
        let (tx, rx) = async_channel::unbounded::<String>();
        let task = cx.background_spawn(async move {
            let report = move |step: String| {
                let _ = tx.try_send(step);
            };
            job(&report)
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(step) = rx.recv().await {
                let _ = this.update(cx, |v, cx| {
                    v.busy = Some(step);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |v, window, cx| {
                v.busy = None;
                done(v, result, window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.config(cx) else {
            return;
        };
        let (slug, item) = (self.slug.clone(), self.item.clone());
        self.job(
            "starting",
            window,
            cx,
            move |progress| respond::run(&config, &slug, &item, progress),
            |v, result: Result<Evaluation>, window, cx| match result {
                Ok(evaluation) => {
                    v.evaluation = Some(evaluation);
                    v.make_replies(window, cx);
                }
                Err(e) => v.message = Some((format!("{e:#}"), true)),
            },
        );
    }

    fn post(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(evaluation) = self.evaluation.clone() else {
            return;
        };
        let picks: Vec<usize> = evaluation
            .responses
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selected && r.needs_post())
            .map(|(i, _)| i)
            .collect();
        if picks.is_empty() {
            return;
        }
        let Some(config) = self.config(cx) else {
            return;
        };
        self.job(
            "posting",
            window,
            cx,
            move |progress| respond::post(&config, &evaluation, &picks, progress),
            |v, outcome: respond::Outcome, _, _| {
                let Some(evaluation) = v.evaluation.as_mut() else {
                    return;
                };
                let summary = respond::record_posts(evaluation, &outcome);
                let _ = respond::save(&v.slug, evaluation);
                let mut text = format!(
                    "Posted {} {} and resolved {} {}.",
                    summary.replied,
                    if summary.replied == 1 { "reply" } else { "replies" },
                    summary.resolved,
                    if summary.resolved == 1 { "thread" } else { "threads" },
                );
                if let Some(e) = &summary.error {
                    text.push_str(&format!(" {e}"));
                }
                v.message = Some((text, summary.error.is_some()));
            },
        );
    }

    /// Saves the checked plans as a note and asks a new Claude tab to work from it.
    fn hand(&mut self, cx: &mut Context<Self>) {
        let Some(evaluation) = self.evaluation.as_mut() else {
            return;
        };
        let picks = evaluation.plans_to_hand();
        if picks.is_empty() {
            return;
        }
        let note = ops::capture(
            &self.slug,
            &format!("Fix plan for {}", self.item.label()),
            "ai",
            &self.url,
            &respond::fix_note(evaluation, &picks),
        );
        match note {
            Ok(note) => {
                for i in &picks {
                    if let Some(r) = evaluation.responses.get_mut(*i) {
                        r.handed = true;
                    }
                }
                let _ = respond::save(&self.slug, evaluation);
                cx.emit(ReviewEvent::HandToClaude {
                    dir: evaluation.dir.clone(),
                    prompt: respond::fix_prompt(&note),
                });
            }
            Err(e) => self.message = Some((format!("Could not save the plan: {e:#}"), true)),
        }
        cx.notify();
    }

    fn response(&self, i: usize, r: &respond::Response, cx: &mut Context<Self>) -> AnyElement {
        let tone = match r.assessment {
            Assessment::Agree => Tone::Good,
            Assessment::Partly => Tone::Warn,
            Assessment::Disagree => Tone::Bad,
            Assessment::Answer => Tone::Busy,
        };
        let mut status = Vec::new();
        if r.posted {
            status.push("reply posted");
        }
        if r.resolved {
            status.push("resolved");
        }
        if r.handed {
            status.push("plan handed to Claude");
        }
        let thread = r.thread.iter().map(|c| {
            div()
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
                        .child(format!("{} · {}", c.author, text::ago(c.date, chrono::Utc::now()))),
                )
                .child(div().child(text::plain(&c.body)))
        });
        let (selected, resolve) = (r.selected, r.resolve);
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(16.0))
            .rounded(px(10.0))
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(if selected { 0x3d6bb0 } else { BORDER }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        check(("pick", i), selected).on_click(cx.listener(move |v, _, _, cx| {
                            v.edit(i, |r| r.selected = !r.selected, cx)
                        })),
                    )
                    .child(pill(r.assessment.label(), chip(tone)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(r.summary.clone()),
                    )
                    .children(r.location.clone().map(|l| {
                        div()
                            .flex_none()
                            .max_w(px(280.0))
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(rgb(SECONDARY))
                            .child(l)
                    })),
            )
            .child(div().flex().flex_col().gap(px(8.0)).children(thread))
            .when(!r.plan.trim().is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(heading("Plan"))
                        .child(div().text_color(rgb(SECONDARY)).child(text::plain(&r.plan))),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(heading(if r.posted { "Reply (posted)" } else { "Reply" }))
                    .children(self.replies.get(i).map(Textarea::new)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.0))
                    .text_size(px(12.0))
                    .child(
                        div()
                            .id(("resolve", i))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .cursor_pointer()
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.edit(i, |r| r.resolve = !r.resolve, cx)
                            }))
                            .child(check(("resolve-box", i), resolve))
                            .child("Resolve the thread when posting"),
                    )
                    .when(!status.is_empty(), |d| {
                        d.child(div().text_color(rgb(LIVE)).child(status.join(" · ")))
                    }),
            )
            .into_any_element()
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let busy = self.busy.is_some();
        let (to_post, to_hand) = self
            .evaluation
            .as_ref()
            .map(|e| {
                (
                    e.responses
                        .iter()
                        .filter(|r| r.selected && r.needs_post())
                        .count(),
                    e.plans_to_hand().len(),
                )
            })
            .unwrap_or((0, 0));
        let mut toolbar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(20.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(rgb(SECONDARY))
                    .child(match (&self.busy, &self.evaluation) {
                        (Some(step), _) => format!("{}…", capitalize(step)),
                        (None, Some(e)) => format!(
                            "Claude read {} {} {} at {}",
                            e.responses.len(),
                            if e.responses.len() == 1 { "thread" } else { "threads" },
                            text::ago(e.created, chrono::Utc::now()),
                            e.local_head.get(..10).unwrap_or(&e.local_head),
                        ),
                        (None, None) => "Claude has not read the threads yet.".into(),
                    }),
            );
        if !busy {
            toolbar = toolbar.child(
                button(
                    "review-ask",
                    if self.evaluation.is_some() {
                        "Ask Claude again"
                    } else {
                        "Ask Claude"
                    },
                )
                .on_click(cx.listener(|v, _, window, cx| v.ask(window, cx))),
            );
            if to_hand > 0 {
                toolbar = toolbar.child(
                    button("review-hand", "Hand checked plans to Claude")
                        .on_click(cx.listener(|v, _, _, cx| v.hand(cx))),
                );
            }
            if to_post > 0 {
                toolbar = toolbar.child(
                    primary("review-post", "Post checked")
                        .on_click(cx.listener(|v, _, window, cx| v.post(window, cx))),
                );
            }
        }
        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .px(px(24.0))
            .py(px(18.0))
            .max_w(px(900.0))
            .children(self.message.clone().map(|(m, bad)| {
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .bg(rgb(RAISED))
                    .text_color(rgb(if bad { BAD } else { LIVE }))
                    .child(m)
            }));
        if let Some(evaluation) = self.evaluation.clone() {
            for (i, r) in evaluation.responses.iter().enumerate() {
                list = list.child(self.response(i, r, cx));
            }
        } else if !busy {
            list = list.child(
                div()
                    .text_color(rgb(SECONDARY))
                    .child("Ask Claude to read the open threads that wait on you. It judges each comment against the code in a checkout of the branch, plans a fix and drafts a reply. Nothing is posted until you check a reply and post it."),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BACKGROUND))
            .child(toolbar)
            .child(
                div()
                    .id("review-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(list),
            )
    }
}

/// A checkbox.
fn check(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_none()
        .size(px(14.0))
        .rounded(px(3.0))
        .border_1()
        .cursor_pointer()
        .border_color(rgb(if on { ACCENT } else { CONTROL_BORDER }))
        .when(on, |d| d.bg(rgb(ACCENT)))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
