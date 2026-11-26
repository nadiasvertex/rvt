//! The ctx app: one window with the project sidebar, the home view, a terminal and the project
//! panel.

mod keys;
mod palette;
mod pty;
mod root;
mod snapshot;
mod text;
mod theme;
mod view;

use anyhow::Result;
use gpui_kit::*;

use pty::{GridSize, Launch, Pty};
use root::{Root, TogglePanel};
use view::TerminalView;

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
    let launch = Launch {
        dir: std::env::current_dir().ok(),
        ..Launch::default()
    };
    // The first layout resizes the grid to the window.
    let (pty, events) = Pty::spawn(
        &launch,
        GridSize {
            columns: 100,
            lines: 30,
        },
        (8, 17),
    )?;
    let mut pty = Some((pty, events));
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-i", TogglePanel, None),
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
        let (pty, events) = pty.take().expect("the app starts once");
        let config = config.take().expect("the app starts once");
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            let terminal = cx.new(|cx| TerminalView::new(pty, events, window, cx));
            cx.new(|cx| Root::new(config, terminal, window, cx))
        });
        if let Err(e) = opened {
            eprintln!("ctx: could not open a window: {e:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(0)
}
