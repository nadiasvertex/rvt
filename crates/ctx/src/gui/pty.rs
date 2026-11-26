//! One terminal: a program on a PTY and alacritty's model of its screen. This module has no GUI
//! code, so tests can run a terminal without a window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Shell};
use anyhow::{Context, Result};

/// Sends the terminal's events to the GUI, which reads them on its own thread.
#[derive(Clone)]
pub struct Listener(async_channel::Sender<Event>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.try_send(event);
    }
}

/// A grid size in cells, for alacritty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub columns: usize,
    pub lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// What to run in a new terminal.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// The program and its arguments, or the user's login shell when empty.
    pub command: Vec<String>,
    pub dir: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

pub struct Pty {
    pub term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    size: GridSize,
    cell: (u16, u16),
}

impl Pty {
    /// Starts the program. The terminal's events arrive on the returned receiver.
    pub fn spawn(
        launch: &Launch,
        size: GridSize,
        cell: (u16, u16),
    ) -> Result<(Pty, async_channel::Receiver<Event>)> {
        let (tx, rx) = async_channel::unbounded();
        let listener = Listener(tx);
        let mut env = launch.env.clone();
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into())
            .or_insert_with(|| "truecolor".into());
        env.insert("TERM_PROGRAM".into(), "ctx".into());
        let options = tty::Options {
            shell: launch
                .command
                .split_first()
                .map(|(program, args)| Shell::new(program.clone(), args.to_vec())),
            working_directory: launch.dir.clone(),
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(&options, window_size(size, cell), 0).context("could not open a PTY")?;
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            listener.clone(),
        )));
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)
            .context("could not start the terminal")?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();
        Ok((
            Pty {
                term,
                notifier,
                size,
                cell,
            },
            rx,
        ))
    }

    /// Sends bytes to the program, as if typed.
    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Resizes the grid and tells the program, when the size changed.
    pub fn resize(&mut self, size: GridSize, cell: (u16, u16)) {
        if size == self.size && cell == self.cell {
            return;
        }
        self.size = size;
        self.cell = cell;
        self.term.lock().resize(size);
        self.notifier.on_resize(window_size(size, cell));
    }

    /// Scrolls the view back to the program's screen, as typing does.
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// The window size alacritty reports to programs that ask for it.
    pub fn window_size(&self) -> WindowSize {
        window_size(self.size, self.cell)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

fn window_size(size: GridSize, cell: (u16, u16)) -> WindowSize {
    WindowSize {
        num_lines: size.lines as u16,
        num_cols: size.columns as u16,
        cell_width: cell.0,
        cell_height: cell.1,
    }
}

/// The text of the visible screen, one string per line with trailing spaces removed.
#[cfg(test)]
pub fn screen_text(term: &Term<Listener>) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line, Point};
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|line| {
            let text: String = (0..grid.columns())
                .map(|col| grid[Point::new(Line(line as i32), Column(col))].c)
                .collect();
            text.trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(rx: &async_channel::Receiver<Event>, pty: &Pty, check: impl Fn(&[String]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = screen_text(&pty.term.lock());
            if check(&screen) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out; screen: {screen:#?}");
            // Answer the program's queries, as the GUI does.
            while let Ok(event) = rx.try_recv() {
                if let Event::PtyWrite(text) = event {
                    pty.write(text.into_bytes());
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn sh(script: &str) -> Launch {
        Launch {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            ..Launch::default()
        }
    }

    const SIZE: GridSize = GridSize {
        columns: 40,
        lines: 10,
    };

    #[test]
    fn a_program_draws_on_the_screen() {
        let (pty, rx) =
            Pty::spawn(&sh("printf 'hello\\r\\nworld'; sleep 5"), SIZE, (8, 16)).unwrap();
        wait_for(&rx, &pty, |s| s[0] == "hello" && s[1] == "world");
    }

    #[test]
    fn typed_input_reaches_the_program() {
        let (pty, rx) =
            Pty::spawn(&sh("read line; echo \"got $line\"; sleep 5"), SIZE, (8, 16)).unwrap();
        pty.write(b"abc\r".as_slice());
        wait_for(&rx, &pty, |s| s.iter().any(|l| l == "got abc"));
    }

    #[test]
    fn the_program_sees_the_terminal_size_and_environment() {
        let launch = Launch {
            env: HashMap::from([("CTX_TERMINAL".into(), "gui:7".into())]),
            ..sh("stty size; echo \"$TERM $CTX_TERMINAL\"; sleep 5")
        };
        let (pty, rx) = Pty::spawn(&launch, SIZE, (8, 16)).unwrap();
        wait_for(&rx, &pty, |s| {
            s[0] == "10 40" && s[1] == "xterm-256color gui:7"
        });
    }

    /// Runs the user's own login shell, so it depends on their shell setup. Run it by hand with
    /// `cargo test -p ctx -- --ignored --nocapture login_shell`.
    #[test]
    #[ignore]
    fn the_login_shell_shows_a_prompt() {
        let (pty, rx) = Pty::spawn(&Launch::default(), SIZE, (8, 16)).unwrap();
        let started = Instant::now();
        wait_for(&rx, &pty, |s| {
            s.iter()
                .any(|l| l.ends_with('%') || l.ends_with('$') || l.contains('❯'))
        });
        println!(
            "after {:?}:\n{}",
            started.elapsed(),
            screen_text(&pty.term.lock()).join("\n")
        );
    }

    #[test]
    fn a_resize_reaches_the_program() {
        let (mut pty, rx) = Pty::spawn(
            &sh("trap 'stty size' WINCH; echo ready; while :; do sleep 0.1; done"),
            SIZE,
            (8, 16),
        )
        .unwrap();
        wait_for(&rx, &pty, |s| s[0] == "ready");
        pty.resize(
            GridSize {
                columns: 50,
                lines: 12,
            },
            (8, 16),
        );
        wait_for(&rx, &pty, |s| s.iter().any(|l| l == "12 50"));
    }
}
