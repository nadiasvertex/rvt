//! A note shown in a tab as rendered Markdown. The tab reads the file again when it changes.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, Utc};
use ctx_core::store::{self, Note};
use gpui_kit::component::text::TextView;
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::text;
use super::theme::*;

/// How often the tab checks whether the file changed.
const CHECK_EVERY: Duration = Duration::from_secs(1);

/// What a note tab asks of the tab strip.
pub enum NoteEvent {
    /// Open the note in the user's editor.
    Edit(PathBuf),
}

pub struct NoteView {
    path: PathBuf,
    note: Option<Note>,
    stamp: Option<(u64, Option<SystemTime>)>,
    _watch: Task<()>,
}

impl EventEmitter<NoteEvent> for NoteView {}

impl NoteView {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let watch = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                if this.update(cx, |view, cx| view.reload(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        });
        NoteView {
            path,
            note: None,
            stamp: None,
            _watch: watch,
        }
    }

    /// The note's title, or its file name before it is read.
    pub fn title(&self) -> String {
        self.note
            .as_ref()
            .map(|n| n.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                self.path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let stamp = fs::metadata(&self.path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        if stamp == self.stamp && self.note.is_some() {
            return;
        }
        self.stamp = stamp;
        self.note = fs::read_to_string(&self.path)
            .ok()
            .map(|t| store::parse_note(&self.path, &t));
        cx.notify();
    }
}

impl Render for NoteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.path.clone();
        let toolbar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(20.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(12.0))
                    .text_color(rgb(SECONDARY))
                    .child(self.path.to_string_lossy().into_owned()),
            )
            .child(button("note-edit", "Open in editor").on_click(cx.listener(
                move |_, _, _, cx| {
                    cx.emit(NoteEvent::Edit(path.clone()));
                },
            )));
        let body = match &self.note {
            None => div()
                .p(px(24.0))
                .text_color(rgb(SECONDARY))
                .child("This note could not be read. It may have been moved or deleted."),
            Some(note) => {
                let mut meta = vec![note.source.clone()];
                meta.push(format!("captured {}", captured(note.captured)));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .px(px(28.0))
                    .py(px(22.0))
                    .max_w(px(820.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(note.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(SECONDARY))
                                    .child(meta.join(" · ")),
                            )
                            .when(!note.link.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(ACCENT))
                                        .truncate()
                                        .child(note.link.clone()),
                                )
                            }),
                    )
                    .child(
                        TextView::markdown(
                            SharedString::from(format!("note-{}", self.path.display())),
                            note.body.clone(),
                        )
                        .selectable(true),
                    )
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BACKGROUND))
            .child(toolbar)
            .child(
                div()
                    .id("note-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

fn captured(at: DateTime<Utc>) -> String {
    let local = at.with_timezone(&Local);
    text::day(local.date_naive(), Local::now().date_naive()) + &local.format(" %H:%M").to_string()
}
