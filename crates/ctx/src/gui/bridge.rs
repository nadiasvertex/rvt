//! ctx-core's `Terminals` for the app. ctx's operations and the watcher run on background
//! threads, so this implementation reads a copy of the window's state that the window keeps
//! current, and sends anything that changes the window back to it as a command.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use anyhow::{Result, bail};
use ctx_core::store::{Project, Window};
use ctx_core::terminals::Terminals;

/// What the operations ask the window to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Open(String),
    Show { slug: String, note: Option<String> },
    ShowHome,
    Close(String),
    Message(String),
}

/// An open workspace as the operations see it.
#[derive(Default, Clone)]
pub struct OpenWorkspace {
    /// `Project::workspace`, which events and Claude records use.
    pub name: String,
    /// The layout to save, or `None` while the workspace is as it was restored, so looking at
    /// a project does not replace a layout ctx 0.3.0 saved.
    pub layout: Option<Vec<Window>>,
    pub terminals: Vec<String>,
}

/// The window's state that the operations read.
#[derive(Default)]
pub struct State {
    pub selected: Option<String>,
    pub focused: Option<String>,
    /// Open workspaces by project slug.
    pub open: HashMap<String, OpenWorkspace>,
    /// Each terminal's input, by terminal ID.
    pub writers: HashMap<String, EventLoopSender>,
}

#[derive(Clone)]
pub struct Bridge {
    state: Arc<Mutex<State>>,
    commands: async_channel::Sender<Command>,
    running: Arc<AtomicBool>,
}

impl Bridge {
    pub fn new() -> (Bridge, async_channel::Receiver<Command>) {
        let (tx, rx) = async_channel::unbounded();
        (
            Bridge {
                state: Arc::default(),
                commands: tx,
                running: Arc::new(AtomicBool::new(true)),
            },
            rx,
        )
    }

    /// Replaces the copy of the window's state.
    pub fn set(&self, state: State) {
        *self.state.lock().unwrap() = state;
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }

    fn with<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.state.lock().unwrap())
    }
}

/// The bytes `type_line` writes: clear the line, the text, then Enter.
fn typed(text: &str) -> Vec<u8> {
    let mut bytes = vec![0x15];
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(b'\r');
    bytes
}

impl Terminals for Bridge {
    fn current_workspace(&self) -> Option<String> {
        self.with(|s| {
            s.selected
                .as_ref()
                .and_then(|slug| s.open.get(slug))
                .map(|w| w.name.clone())
        })
    }

    fn current_terminal(&self) -> Option<String> {
        self.with(|s| s.focused.clone())
    }

    fn is_open(&self, project: &Project) -> bool {
        self.with(|s| s.open.contains_key(&project.slug))
    }

    fn open(&self, project: &Project) -> Result<()> {
        // Counted as open at once, so a `show` or `close` right after it sees the workspace.
        self.state
            .lock()
            .unwrap()
            .open
            .entry(project.slug.clone())
            .or_insert_with(|| OpenWorkspace {
                name: project.workspace.clone(),
                ..OpenWorkspace::default()
            });
        self.send(Command::Open(project.slug.clone()));
        Ok(())
    }

    fn show(&self, project: &Project, note: Option<&str>) -> Result<()> {
        self.state.lock().unwrap().selected = Some(project.slug.clone());
        self.send(Command::Show {
            slug: project.slug.clone(),
            note: note.map(str::to_string),
        });
        Ok(())
    }

    fn show_home(&self) -> Result<()> {
        self.state.lock().unwrap().selected = None;
        self.send(Command::ShowHome);
        Ok(())
    }

    fn close(&self, project: &Project) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(w) = state.open.remove(&project.slug) {
                for t in &w.terminals {
                    state.writers.remove(t);
                }
            }
        }
        self.send(Command::Close(project.slug.clone()));
        Ok(())
    }

    fn layout(&self, project: &Project) -> Option<Vec<Window>> {
        self.with(|s| s.open.get(&project.slug).and_then(|w| w.layout.clone()))
    }

    fn message(&self, text: &str) {
        self.send(Command::Message(text.to_string()));
    }

    fn type_line(&self, terminal: &str, text: &str) -> Result<()> {
        let Some(writer) = self.with(|s| s.writers.get(terminal).cloned()) else {
            bail!("terminal {terminal} is not open in the ctx app");
        };
        if writer.send(Msg::Input(typed(text).into())).is_err() {
            bail!("terminal {terminal} has ended");
        }
        Ok(())
    }

    fn workspace_of(&self, terminal: &str) -> Option<String> {
        self.with(|s| {
            s.open
                .values()
                .find(|w| w.terminals.iter().any(|t| t == terminal))
                .map(|w| w.name.clone())
        })
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctx_core::store::State as ProjectState;

    fn project(slug: &str) -> Project {
        Project {
            name: slug.into(),
            slug: slug.into(),
            state: ProjectState::Live,
            created: chrono::Utc::now(),
            workspace: format!("ws-{slug}"),
            dir: None,
            next: String::new(),
            handoff: None,
            wait: None,
            claude_sessions: Vec::new(),
            dismissed: Vec::new(),
            layout: Vec::new(),
            items: Vec::new(),
        }
    }

    #[test]
    fn operations_see_the_window_and_send_it_commands() {
        let (bridge, rx) = Bridge::new();
        let a = project("a");
        assert!(!bridge.is_open(&a));
        bridge.open(&a).unwrap();
        bridge.show(&a, Some("hello")).unwrap();
        assert!(bridge.is_open(&a));
        assert_eq!(bridge.current_workspace().as_deref(), Some("ws-a"));
        assert_eq!(rx.try_recv().unwrap(), Command::Open("a".into()));
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::Show {
                slug: "a".into(),
                note: Some("hello".into())
            }
        );

        let mut state = State {
            selected: Some("a".into()),
            focused: Some("gui:2".into()),
            ..State::default()
        };
        state.open.insert(
            "a".into(),
            OpenWorkspace {
                name: "ws-a".into(),
                layout: None,
                terminals: vec!["gui:1".into(), "gui:2".into()],
            },
        );
        bridge.set(state);
        assert_eq!(bridge.current_terminal().as_deref(), Some("gui:2"));
        assert_eq!(bridge.workspace_of("gui:1").as_deref(), Some("ws-a"));
        assert_eq!(bridge.workspace_of("gui:9"), None);
        assert!(bridge.type_line("gui:1", "/exit").is_err());

        bridge.show_home().unwrap();
        bridge.close(&a).unwrap();
        assert!(!bridge.is_open(&a));
        assert_eq!(bridge.current_workspace(), None);
        assert_eq!(rx.try_recv().unwrap(), Command::ShowHome);
        assert_eq!(rx.try_recv().unwrap(), Command::Close("a".into()));
        assert!(bridge.running());
        bridge.stop();
        assert!(!bridge.running());
    }

    #[test]
    fn type_line_reaches_the_program_in_the_terminal() {
        use crate::gui::pty::{GridSize, Launch, Pty, screen_text};
        use std::time::{Duration, Instant};
        let launch = Launch {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; echo \"got $line\"; sleep 5".into(),
            ],
            ..Launch::default()
        };
        let size = GridSize {
            columns: 40,
            lines: 10,
        };
        let (pty, _events) = Pty::spawn(&launch, size, (8, 16)).unwrap();
        let (bridge, _rx) = Bridge::new();
        let mut state = State::default();
        state.writers.insert("gui:1".into(), pty.writer());
        bridge.set(state);
        bridge.type_line("gui:1", "/exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = screen_text(&pty.term.lock());
            if screen.iter().any(|l| l == "got /exit") {
                break;
            }
            assert!(Instant::now() < deadline, "screen: {screen:#?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_typed_line_clears_the_line_first_and_ends_with_enter() {
        assert_eq!(typed("/exit"), b"\x15/exit\r");
    }
}
