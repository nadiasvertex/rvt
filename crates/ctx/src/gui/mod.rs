//! The ctx app: one window with the project sidebar, the home view, each project's workspace of
//! tabs and the project panel.

mod bridge;
mod dialogs;
mod keys;
mod note;
mod palette;
mod pty;
mod root;
mod snapshot;
mod tabs;
mod text;
mod theme;
mod view;
mod workspace;

use anyhow::Result;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;

use root::{NewProject, Root, TogglePanel};
use workspace::{CloseTab, NewClaude, NewTerminal, NextTab, PreviousTab};

actions!(ctx, [Quit]);

pub fn run() -> Result<i32> {
    // Without a config the app still shows the store, and the first run writes one to fill in.
    let config = match ctx_core::config::load() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("ctx: {e:#}");
            None
        }
    };
    let mut config = Some(config);
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // Rendered Markdown in note tabs takes its colors from the component theme.
        Theme::change(ThemeMode::Dark, None, cx);
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-i", TogglePanel, None),
            KeyBinding::new("cmd-n", NewProject, None),
            KeyBinding::new("cmd-t", NewTerminal, None),
            KeyBinding::new("cmd-shift-t", NewClaude, None),
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("cmd-shift-]", NextTab, None),
            KeyBinding::new("cmd-shift-[", PreviousTab, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus([Menu {
            name: "ctx".into(),
            items: vec![MenuItem::action("Quit ctx", Quit)],
            disabled: false,
        }]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1280.0), px(800.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("ctx".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.0), px(13.0))),
            }),
            ..Default::default()
        };
        let config = config.take().expect("the app starts once");
        // The component library's root holds what its text inputs need.
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            let root = cx.new(|cx| Root::new(config, window, cx));
            cx.new(|cx| gpui_kit::component::Root::new(root, window, cx))
        });
        if let Err(e) = opened {
            eprintln!("ctx: could not open a window: {e:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(0)
}
