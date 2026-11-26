use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use forge::exec;

use crate::store::{Pane, Project, Window};

/// The session that shows the home screen, used when the current session closes.
pub const HOME_SESSION: &str = "ctx";
/// The name of window 0 in each project session, which runs ctx on the project screen.
pub const CTX_WINDOW: &str = "ctx";

fn tmux(args: &[&str]) -> Result<String> {
    let out = exec::run_with(Command::new("tmux").args(args), None)?;
    Ok(out.trim_end().to_string())
}

fn quiet(args: &[&str]) -> bool {
    Command::new("tmux")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn inside() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

pub fn available() -> bool {
    quiet(&["-V"])
}

pub fn server_running() -> bool {
    quiet(&["list-sessions"])
}

/// The session the client is in, when ctx runs inside tmux.
pub fn current_session() -> Option<String> {
    if !inside() {
        return None;
    }
    tmux(&["display-message", "-p", "#{session_name}"]).ok()
}

/// The active pane of the current session. A popup is not a pane, so from a popup this is the
/// pane the popup covers.
pub fn active_pane() -> Option<String> {
    if !inside() {
        return None;
    }
    tmux(&["display-message", "-p", "#{pane_id}"]).ok()
}

/// The working directory of the active pane, when ctx runs inside tmux.
pub fn active_path() -> Option<PathBuf> {
    if !inside() {
        return None;
    }
    tmux(&["display-message", "-p", "#{pane_current_path}"])
        .ok()
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| p.is_dir())
}

/// The session of a pane that still runs a program rather than an idle shell. A pane that ctx
/// started runs its command under `$SHELL -c`, so tmux names the shell and the program only
/// shows up as the shell's child.
pub fn busy_pane_session(pane: &str) -> Option<String> {
    let out = tmux(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{session_name}\t#{pane_current_command}\t#{pane_pid}",
    ])
    .ok()?;
    let mut fields = out.split('\t');
    let (session, command, pid) = (fields.next()?, fields.next()?, fields.next()?);
    let busy = !is_shell(command) || !descendants(&process_table(), pid).is_empty();
    busy.then(|| session.to_string())
}

/// Every process as `pid ppid command` lines.
fn process_table() -> String {
    Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// The command names of all processes below `root` in a `ps` table.
fn descendants(table: &str, root: &str) -> Vec<String> {
    let rows: Vec<(&str, &str, String)> = table
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?;
            let ppid = parts.next()?;
            let command = parts.collect::<Vec<_>>().join(" ");
            let name = command.rsplit('/').next().unwrap_or_default().to_string();
            Some((pid, ppid, name))
        })
        .collect();
    let mut found = Vec::new();
    let mut parents = vec![root.trim().to_string()];
    while let Some(parent) = parents.pop() {
        for (pid, ppid, name) in &rows {
            if *ppid == parent {
                found.push(name.clone());
                parents.push(pid.to_string());
            }
        }
    }
    found
}

fn is_shell(command: &str) -> bool {
    matches!(
        command.trim().trim_start_matches('-'),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "login"
    )
}

/// The session a pane belongs to, while the pane exists.
pub fn pane_session(pane: &str) -> Option<String> {
    tmux(&["display-message", "-p", "-t", pane, "#{session_name}"]).ok()
}

/// Types the text into the pane after clearing its input line, then presses Enter.
pub fn type_line(pane: &str, text: &str) -> Result<()> {
    tmux(&["send-keys", "-t", pane, "C-u"])?;
    tmux(&["send-keys", "-t", pane, "-l", text])?;
    tmux(&["send-keys", "-t", pane, "Enter"])?;
    Ok(())
}

/// Makes the pane the active pane of its window, and the window the current one of its session.
pub fn select_pane(pane: &str) -> Result<()> {
    tmux(&["select-window", "-t", pane])?;
    tmux(&["select-pane", "-t", pane])?;
    Ok(())
}

pub fn has_session(name: &str) -> bool {
    quiet(&["has-session", "-t", &format!("={name}")])
}

/// Names of the running sessions.
pub fn sessions() -> Vec<String> {
    tmux(&["list-sessions", "-F", "#{session_name}"])
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Opens a window in the session that runs `command`, then a shell when it ends.
pub fn new_window(session: &str, dir: &Path, name: &str, command: &str) -> Result<()> {
    let dir = dir.to_string_lossy();
    let full = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");
    tmux(&[
        "new-window",
        "-t",
        &format!("={session}:"),
        "-n",
        name,
        "-c",
        &dir,
        &full,
    ])?;
    Ok(())
}

/// Splits the session's current window side by side and runs the command in the new pane,
/// which becomes the active one.
pub fn split_window(session: &str, dir: &Path, command: &str) -> Result<()> {
    let dir = dir.to_string_lossy();
    let full = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");
    tmux(&[
        "split-window",
        "-h",
        "-t",
        &format!("={session}:"),
        "-c",
        &dir,
        &full,
    ])?;
    Ok(())
}

/// Whether any window of the session is a Claude window or has a pane running Claude.
pub fn has_claude(session: &str) -> bool {
    tmux(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={session}:"),
        "-F",
        "#{window_name}\t#{pane_current_command}\t#{pane_pid}",
    ])
    .is_ok_and(|listing| lists_claude(&listing, &process_table()))
}

/// `listing` has `window name, command, pane pid` lines, `table` is the `ps` table.
fn lists_claude(listing: &str, table: &str) -> bool {
    listing.lines().any(|line| {
        let mut fields = line.split('\t');
        let window = fields.next().unwrap_or_default();
        let command = fields.next().unwrap_or_default().trim();
        let pid = fields.next().unwrap_or_default();
        window == "claude"
            || command == "claude"
            || descendants(table, pid).iter().any(|name| name == "claude")
    })
}

pub fn quote(s: &str) -> String {
    shell_quote(s)
}

fn ctx_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ctx".into())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs ctx and then a shell, so quitting ctx leaves a usable window.
fn ctx_window_command(args: &str) -> String {
    format!(
        "{} {args}; exec \"${{SHELL:-/bin/sh}}\"",
        shell_quote(&ctx_bin())
    )
}

fn start_dir(project: &Project) -> PathBuf {
    project
        .dir
        .clone()
        .filter(|d| d.is_dir())
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn existing_dir(path: &Path, fallback: &Path) -> String {
    let dir = if path.is_dir() { path } else { fallback };
    dir.to_string_lossy().into_owned()
}

/// Creates the project's session if it is missing: window 0 runs ctx on the project screen and
/// the other windows come back from the saved layout, with their working directories. Programs
/// are not restarted. Returns true when the session was created.
pub fn ensure_session(project: &Project) -> Result<bool> {
    if has_session(&project.session) {
        return Ok(false);
    }
    let dir = start_dir(project);
    let dir_arg = dir.to_string_lossy().into_owned();
    let env = format!("CTX_PROJECT={}", project.slug);
    let command = ctx_window_command(&format!("open {}", shell_quote(&project.slug)));
    let session_id = tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{session_id}",
        "-s",
        &project.session,
        "-n",
        CTX_WINDOW,
        "-c",
        &dir_arg,
        "-x",
        "240",
        "-y",
        "64",
        "-e",
        &env,
        &command,
    ])
    .with_context(|| format!("could not create tmux session {}", project.session))?;
    let _ = tmux(&[
        "set-environment",
        "-t",
        &session_id,
        "CTX_PROJECT",
        &project.slug,
    ]);
    let windows: Vec<&Window> = project
        .layout
        .iter()
        .filter(|w| w.name != CTX_WINDOW)
        .collect();
    if windows.is_empty() {
        let _ = tmux(&[
            "new-window",
            "-d",
            "-t",
            &format!("{session_id}:"),
            "-c",
            &dir_arg,
        ]);
    }
    for window in windows {
        restore_window(&session_id, window, &dir);
    }
    Ok(true)
}

fn restore_window(session_id: &str, window: &Window, fallback: &Path) {
    let Some(first) = window.panes.first() else {
        return;
    };
    let target = format!("{session_id}:");
    let first_dir = existing_dir(&first.path, fallback);
    let mut args = vec![
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-t",
        &target,
    ];
    if !window.auto_name {
        args.extend(["-n", &window.name]);
    }
    args.extend(["-c", &first_dir]);
    let Ok(window_id) = tmux(&args) else {
        return;
    };
    for pane in &window.panes[1..] {
        let dir = existing_dir(&pane.path, fallback);
        let _ = tmux(&["split-window", "-d", "-t", &window_id, "-c", &dir]);
        let _ = tmux(&["select-layout", "-t", &window_id, "tiled"]);
    }
    if window.panes.len() > 1 {
        let _ = tmux(&["select-layout", "-t", &window_id, &window.layout]);
    }
}

/// The session's windows, panes and working directories.
pub fn capture_layout(session: &str) -> Result<Vec<Window>> {
    let out = tmux(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={session}"),
        "-F",
        "#{window_index}\t#{window_name}\t#{automatic-rename}\t#{window_layout}\t#{pane_current_path}",
    ])?;
    Ok(parse_layout(&out))
}

pub fn parse_layout(out: &str) -> Vec<Window> {
    let mut windows: Vec<(String, Window)> = Vec::new();
    for line in out.lines() {
        let mut parts = line.splitn(5, '\t');
        let (Some(index), Some(name), Some(auto), Some(layout), Some(path)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        let pane = Pane {
            path: PathBuf::from(path),
        };
        match windows.iter_mut().find(|(i, _)| i == index) {
            Some((_, w)) => w.panes.push(pane),
            None => windows.push((
                index.to_string(),
                Window {
                    name: name.to_string(),
                    auto_name: auto == "1",
                    layout: layout.to_string(),
                    panes: vec![pane],
                },
            )),
        }
    }
    windows.into_iter().map(|(_, w)| w).collect()
}

/// Moves this client to the session. Outside tmux, ctx becomes the tmux client and does not
/// return, so no ctx thread is left reading the terminal that tmux needs.
pub fn switch_to(session: &str) -> Result<()> {
    let target = format!("={session}");
    if inside() {
        tmux(&["switch-client", "-t", &target])?;
        return Ok(());
    }
    let error = Command::new("tmux")
        .args(["attach-session", "-t", &target])
        .exec();
    Err(error).context("could not start tmux")
}

pub fn ensure_home_session() -> Result<()> {
    if has_session(HOME_SESSION) {
        return Ok(());
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    tmux(&[
        "new-session",
        "-d",
        "-s",
        HOME_SESSION,
        "-n",
        CTX_WINDOW,
        "-c",
        &home.to_string_lossy(),
        &ctx_window_command(""),
    ])?;
    Ok(())
}

pub fn kill_session(session: &str) -> Result<()> {
    tmux(&["kill-session", "-t", &format!("={session}")])?;
    Ok(())
}

/// Shows a message on every attached client.
pub fn message(text: &str) {
    let Ok(clients) = tmux(&["list-clients", "-F", "#{client_name}"]) else {
        return;
    };
    for client in clients.lines().filter(|c| !c.is_empty()) {
        let _ = tmux(&["display-message", "-c", client, "-d", "6000", text]);
    }
}

pub fn config_snippet() -> String {
    let ctx = shell_quote(&ctx_bin());
    format!(
        r##"# ctx: project switching. Source this file from ~/.tmux.conf:
#   source-file ~/.config/ctx/tmux.conf

# prefix j opens the project picker. Esc closes it and leaves you where you were.
bind-key j display-popup -E -w 90% -h 80% -T " ctx " "{ctx} pick"

# When a frozen project's session closes, move to another session instead of detaching.
set -g detach-on-destroy off

# How many projects are ready, next to the clock.
set -g status-interval 15
set -g status-right-length 60
set -g status-right "#({ctx} status) │ %H:%M "

# Keep the watcher running. It refreshes items and clears wait conditions.
run-shell -b "{ctx} watch --daemon"
"##
    )
}

/// The tmux front end: each project's workspace is a tmux session and each terminal a pane.
pub struct Tmux;

impl crate::terminals::Terminals for Tmux {
    fn current_workspace(&self) -> Option<String> {
        current_session()
    }

    fn current_terminal(&self) -> Option<String> {
        std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())
    }

    fn is_open(&self, project: &Project) -> bool {
        has_session(&project.session)
    }

    fn open(&self, project: &Project) -> Result<()> {
        ensure_session(project).map(|_| ())
    }

    fn show(&self, project: &Project, note: Option<&str>) -> Result<()> {
        let was_inside = inside();
        switch_to(&project.session)?;
        if let Some(note) = note.filter(|_| was_inside) {
            message(note);
        }
        Ok(())
    }

    fn show_home(&self) -> Result<()> {
        ensure_home_session()?;
        switch_to(HOME_SESSION)
    }

    fn close(&self, project: &Project) -> Result<()> {
        kill_session(&project.session)
    }

    fn layout(&self, project: &Project) -> Option<Vec<Window>> {
        if !has_session(&project.session) {
            return None;
        }
        capture_layout(&project.session)
            .ok()
            .filter(|layout| !layout.is_empty())
    }

    fn message(&self, text: &str) {
        message(text);
    }

    fn type_line(&self, terminal: &str, text: &str) -> Result<()> {
        type_line(terminal, text)
    }

    fn workspace_of(&self, terminal: &str) -> Option<String> {
        pane_session(terminal)
    }

    fn running(&self) -> bool {
        server_running()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn login_shells_and_plain_shells_count_as_shells() {
        assert!(super::is_shell("zsh"));
        assert!(super::is_shell("-zsh"));
        assert!(!super::is_shell("claude"));
        assert!(!super::is_shell("node"));
    }

    const TABLE: &str = "  100     1 /bin/zsh\n  \
                         101   100 /usr/local/bin/claude\n  \
                         102   101 /usr/bin/git status\n  \
                         200     1 -zsh\n  \
                         300     1 zsh\n  \
                         301   300 nvim\n";

    #[test]
    fn descendants_follow_children_of_children() {
        assert_eq!(super::descendants(TABLE, "100"), ["claude", "git status"]);
        assert_eq!(super::descendants(TABLE, "200"), Vec::<String>::new());
    }

    #[test]
    fn a_session_runs_claude_with_a_claude_window_or_a_pane_running_claude() {
        let lists = |listing| super::lists_claude(listing, TABLE);
        assert!(!lists("ctx\tctx\t200\nedit\tzsh\t300"));
        assert!(lists("ctx\tctx\t200\nclaude\tzsh\t200"));
        assert!(lists("ctx\tctx\t200\nedit\tclaude\t200"));
        assert!(lists("ctx\tctx\t200\nctx\tzsh\t100"));
        assert!(!lists(""));
    }

    use super::*;

    #[test]
    fn layouts_group_panes_by_window() {
        let out = "0\tctx\t0\tb25d,240x64,0,0,1\t/src\n\
                   1\tedit\t0\tc3a1,240x64,0,0{120x64,0,0,2,119x64,121,0,3}\t/src/core\n\
                   1\tedit\t0\tc3a1,240x64,0,0{120x64,0,0,2,119x64,121,0,3}\t/src/core/tests\n\
                   2\tzsh\t1\t1234,240x64,0,0,4\t/tmp";
        let windows = parse_layout(out);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[1].name, "edit");
        assert_eq!(windows[1].panes.len(), 2);
        assert_eq!(windows[1].panes[1].path, PathBuf::from("/src/core/tests"));
        assert_eq!(windows[2].layout, "1234,240x64,0,0,4");
    }

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
