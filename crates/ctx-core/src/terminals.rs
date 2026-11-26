//! The front end that holds each project's terminals. ctx's own logic reaches terminals only
//! through this trait, so the same logic can run under tmux or another front end.
//!
//! A project's workspace is named by `Project::session`. A terminal is named by an opaque ID,
//! such as a tmux pane ID, and the same IDs are stored in events and Claude session records.

use anyhow::Result;

use crate::store::{Project, Window};

pub trait Terminals {
    /// The name of the workspace this process runs in, if any.
    fn current_workspace(&self) -> Option<String>;
    /// The terminal this process runs in, if any.
    fn current_terminal(&self) -> Option<String>;
    /// Whether the project's workspace is open.
    fn is_open(&self, project: &Project) -> bool;
    /// Opens the project's workspace, restoring its saved layout, unless it is open already.
    fn open(&self, project: &Project) -> Result<()>;
    /// Moves the user to the project's workspace, which must be open, and shows `note` when the
    /// front end can. A front end may take over this process and not return.
    fn show(&self, project: &Project, note: Option<&str>) -> Result<()>;
    /// Moves the user to the home workspace, opening it first when needed.
    fn show_home(&self) -> Result<()>;
    /// Closes the project's workspace and ends what runs in it.
    fn close(&self, project: &Project) -> Result<()>;
    /// The layout of the project's open workspace, to restore when it opens next.
    fn layout(&self, project: &Project) -> Option<Vec<Window>>;
    /// Shows a short message to the user.
    fn message(&self, text: &str);
    /// Types a line into the terminal and presses Enter.
    fn type_line(&self, terminal: &str, text: &str) -> Result<()>;
    /// The name of the workspace the terminal belongs to, if it still exists.
    fn workspace_of(&self, terminal: &str) -> Option<String>;
    /// Whether the front end still runs. The watch daemon stops when it does not.
    fn running(&self) -> bool;
}
