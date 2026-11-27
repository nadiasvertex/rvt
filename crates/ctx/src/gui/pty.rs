//! One terminal: a program on a PTY and alacritty's model of its screen. This module has no GUI
//! code, so tests can run a terminal without a window.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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

/// The program in the terminal's foreground and its working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    pub program: String,
    pub dir: Option<PathBuf>,
}

pub struct Pty {
    pub term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    size: GridSize,
    cell: (u16, u16),
    /// A copy of the PTY's controlling side, to ask which process group is in the foreground.
    controller: Option<OwnedFd>,
    child: i32,
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
        let child = pty.child().id() as i32;
        // SAFETY: dup returns a new descriptor that nothing else owns, or -1.
        let controller = match unsafe { libc::dup(pty.file().as_raw_fd()) } {
            -1 => None,
            fd => Some(unsafe { OwnedFd::from_raw_fd(fd) }),
        };
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
                controller,
                child,
            },
            rx,
        ))
    }

    /// Sends bytes to the program, as if typed.
    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    /// A handle that sends input to the program from any thread.
    pub fn writer(&self) -> alacritty_terminal::event_loop::EventLoopSender {
        self.notifier.0.clone()
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

    /// The foreground program and its directory, or the started program's when the PTY has no
    /// foreground process group.
    pub fn foreground(&self) -> Option<Foreground> {
        let group = self
            .controller
            .as_ref()
            // SAFETY: the descriptor stays open while `self` lives.
            .map(|fd| unsafe { libc::tcgetpgrp(fd.as_raw_fd()) })
            .filter(|&g| g > 0)
            .unwrap_or(self.child);
        let program = process_name(group).or_else(|| process_name(self.child))?;
        Some(Foreground {
            program,
            dir: process_dir(group).or_else(|| process_dir(self.child)),
        })
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

fn process_name(pid: i32) -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: proc_name writes at most `buffer.len()` bytes and returns how many it wrote.
    let n = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buffer[..n as usize]).into_owned())
}

fn process_dir(pid: i32) -> Option<PathBuf> {
    // SAFETY: the struct is plain data, and proc_pidinfo fills at most its size.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // SAFETY: the path is a NUL-terminated C string inside the fixed-size buffer.
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = path.to_str().ok()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
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
    fn the_foreground_program_and_its_directory_are_known() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let launch = Launch {
            dir: Some(dir.clone()),
            ..sh("echo ready; exec sleep 5")
        };
        let (pty, rx) = Pty::spawn(&launch, SIZE, (8, 16)).unwrap();
        wait_for(&rx, &pty, |s| s[0] == "ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let fg = pty.foreground();
            if fg.as_ref().is_some_and(|f| f.program == "sleep") {
                assert_eq!(fg.unwrap().dir.as_deref(), Some(dir.as_path()));
                return;
            }
            assert!(Instant::now() < deadline, "foreground: {fg:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sends a key as the window does, through the key encoder.
    fn press(pty: &Pty, key: &str, typed: Option<&str>) {
        use super::super::keys::{Mods, encode};
        use alacritty_terminal::term::TermMode;
        let app_cursor = pty.term.lock().mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = encode(key, typed, Mods::default(), app_cursor) {
            pty.write(bytes);
        }
    }

    fn type_text(pty: &Pty, text: &str) {
        for c in text.chars() {
            let key = match c {
                '\r' => "enter".to_string(),
                ' ' => "space".to_string(),
                c => c.to_lowercase().to_string(),
            };
            press(pty, &key, Some(&c.to_string()));
        }
    }

    fn alt_screen(pty: &Pty) -> bool {
        use alacritty_terminal::term::TermMode;
        pty.term.lock().mode().contains(TermMode::ALT_SCREEN)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ctx-pty-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn vim_edits_and_saves_a_file() {
        let dir = scratch("vim");
        let file = dir.join("note.txt");
        let script = format!(
            "vim -u NONE -N -n {}; echo vim-done; sleep 5",
            file.display()
        );
        let (pty, rx) = Pty::spawn(&sh(&script), SIZE, (8, 16)).unwrap();
        // vim -u NONE draws `~` on the empty lines.
        wait_for(&rx, &pty, |s| s[1] == "~");
        assert!(alt_screen(&pty));
        type_text(&pty, "ihello from ctx");
        wait_for(&rx, &pty, |s| s[0] == "hello from ctx");
        press(&pty, "escape", None);
        type_text(&pty, ":wq\r");
        wait_for(&rx, &pty, |s| s.iter().any(|l| l == "vim-done"));
        assert!(!alt_screen(&pty));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello from ctx\n");
    }

    #[test]
    fn vim_redraws_after_a_resize() {
        let (mut pty, rx) = Pty::spawn(&sh("vim -u NONE -N -n; sleep 5"), SIZE, (8, 16)).unwrap();
        wait_for(&rx, &pty, |s| s[1] == "~");
        pty.resize(
            GridSize {
                columns: 50,
                lines: 14,
            },
            (8, 16),
        );
        // The empty-line markers reach the new bottom line, above the command line.
        wait_for(&rx, &pty, |s| s.len() == 14 && s[12] == "~");
        type_text(&pty, ":q\r");
    }

    #[test]
    fn less_scrolls_and_gives_the_screen_back() {
        let dir = scratch("less");
        let file = dir.join("lines.txt");
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&file, text).unwrap();
        let script = format!(
            "echo before; less {}; echo less-done; sleep 5",
            file.display()
        );
        let (pty, rx) = Pty::spawn(&sh(&script), SIZE, (8, 16)).unwrap();
        wait_for(&rx, &pty, |s| s[0] == "line 1");
        assert!(alt_screen(&pty));
        press(&pty, "down", None);
        press(&pty, "down", None);
        wait_for(&rx, &pty, |s| s[0] == "line 3");
        press(&pty, "space", Some(" "));
        wait_for(&rx, &pty, |s| s[0] != "line 3" && s[0].starts_with("line "));
        type_text(&pty, "q");
        wait_for(&rx, &pty, |s| s.iter().any(|l| l == "less-done"));
        assert!(!alt_screen(&pty));
        assert_eq!(screen_text(&pty.term.lock())[0], "before");
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
