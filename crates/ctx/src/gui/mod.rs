//! The ctx app. So far it opens one window with one terminal running the user's shell.

mod keys;
mod palette;
mod pty;
mod view;

use anyhow::Result;
use gpui_kit::*;

use pty::{GridSize, Launch, Pty};
use view::TerminalView;

actions!(ctx, [Quit]);

pub fn run() -> Result<i32> {
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
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
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
                size(px(960.0), px(640.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("ctx".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (pty, events) = pty.take().expect("the app starts once");
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| TerminalView::new(pty, events, window, cx))
        });
        if let Err(e) = opened {
            eprintln!("ctx: could not open a window: {e:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(0)
}
