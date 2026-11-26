use std::collections::HashMap;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Deserialize;

use crate::cache;
use crate::config::Config;
use crate::ops;
use crate::refresh;
use crate::store::{self, ClaudeSession, State};
use crate::terminals::Terminals;
use crate::wait::{Condition, Event, EventKind};

/// How often wait conditions are checked between refreshes. Dates and events written while the
/// watcher sleeps are picked up within this time.
const CHECK_EVERY: Duration = Duration::from_secs(5);
/// An item whose state has not changed for this many refreshes is refreshed less often.
const QUIET_ROUNDS: u32 = 10;
const QUIET_EVERY: u32 = 4;

fn pid_path() -> PathBuf {
    store::home().join("watch.pid")
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The pid of a running watcher, if any.
pub fn running() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (pid != std::process::id() && alive(pid)).then_some(pid)
}

/// Refreshes items every few minutes and checks wait conditions every few seconds. With
/// `daemon`, a second watcher exits at once and the watcher stops when the front end does.
pub fn run(cfg: &Config, terminals: &dyn Terminals, daemon: bool, once: bool) -> Result<()> {
    if let Some(pid) = running() {
        if daemon {
            return Ok(());
        }
        bail!("ctx watch is already running as pid {pid}");
    }
    store::write_atomic(&pid_path(), std::process::id().to_string().as_bytes())?;
    let interval = Duration::from_secs(cfg.watch.interval_minutes.max(1) * 60);
    let mut quiet: HashMap<String, u32> = HashMap::new();
    let mut round: u32 = 0;
    let mut next_refresh = Instant::now();
    loop {
        if Instant::now() >= next_refresh {
            refresh_round(cfg, round, &mut quiet);
            round = round.wrapping_add(1);
            next_refresh = Instant::now() + interval;
        }
        check_all(terminals);
        if once {
            break;
        }
        if daemon && !terminals.running() {
            break;
        }
        thread::sleep(CHECK_EVERY);
    }
    if running().is_none() {
        let _ = std::fs::remove_file(pid_path());
    }
    Ok(())
}

fn refresh_round(cfg: &Config, round: u32, quiet: &mut HashMap<String, u32>) {
    let interval = (cfg.watch.interval_minutes.max(1) * 60) as i64;
    crate::inbox::refresh(cfg, interval);
    let projects = store::list();
    let before = cache::load();
    let due: Vec<_> = refresh::wanted(&projects, None)
        .into_iter()
        .filter(|r| {
            quiet.get(&r.key()).copied().unwrap_or(0) < QUIET_ROUNDS || round % QUIET_EVERY == 0
        })
        .collect();
    refresh::fetch(cfg, &due, &|_| {});
    let after = cache::load();
    for item in &due {
        let key = item.key();
        let same = match (before.items.get(&key), after.items.get(&key)) {
            (Some(a), Some(b)) => {
                a.status == b.status
                    && a.comments == b.comments
                    && a.head == b.head
                    && a.ci == b.ci
                    && a.conflicts == b.conflicts
            }
            _ => false,
        };
        let count = quiet.entry(key).or_insert(0);
        *count = if same { count.saturating_add(1) } else { 0 };
    }
}

fn check_all(terminals: &dyn Terminals) {
    for project in store::list() {
        if project.state != State::Done && project.waiting() {
            ops::evaluate(terminals, &project.slug);
        }
    }
}

/// The fields of a Claude Code hook payload that ctx reads. All are optional so that a change
/// in the payload cannot make a hook fail.
#[derive(Debug, Default, Deserialize)]
struct HookPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Notification text, such as a permission request.
    #[serde(default)]
    message: Option<String>,
    /// Claude's last reply, sent with Stop.
    #[serde(default)]
    last_assistant_message: Option<String>,
}

/// Handles a Claude Code hook. Does nothing outside a project session, and never fails, so a
/// hook cannot get in Claude's way.
pub fn event(terminals: &dyn Terminals, kind: &str) -> Result<()> {
    let Some(slug) = std::env::var("CTX_PROJECT").ok().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if store::load(&slug).is_err() {
        return Ok(());
    }
    let kind = match kind {
        "claude-start" | "session-start" => EventKind::ClaudeStart,
        "claude-end" | "session-end" => EventKind::ClaudeEnd,
        "claude-stop" | "stop" => EventKind::ClaudeStop,
        "claude-notification" | "notification" => EventKind::ClaudeNotification,
        _ => return Ok(()),
    };
    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_string(&mut input);
    }
    let payload: HookPayload = serde_json::from_str(&input).unwrap_or_default();
    let pane = terminals.current_terminal();
    let event = Event {
        time: Utc::now(),
        kind,
        pane: pane.clone(),
        session: payload.session_id.clone(),
        detail: payload
            .message
            .or(payload.last_assistant_message)
            .map(|m| crate::item::snippet(&m).chars().take(200).collect())
            .unwrap_or_default(),
    };
    let _ = store::record_event(&slug, &event);
    if matches!(kind, EventKind::ClaudeStart | EventKind::ClaudeStop) {
        if let Some(cwd) = &payload.cwd {
            let _ = ops::track_worktree(&slug, cwd);
        }
    }
    if kind == EventKind::ClaudeStart {
        if let Some(id) = payload.session_id {
            let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
            let mut lost = Vec::new();
            let _ = store::update(&slug, |p| {
                let name = p
                    .claude_sessions
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| s.name.clone());
                lost = p.add_claude_session(
                    ClaudeSession {
                        id,
                        started: Utc::now(),
                        cwd: payload.cwd,
                        pane,
                        config_dir,
                        name,
                        frozen: None,
                    },
                    ClaudeSession::resumable,
                );
            });
            for s in lost {
                let _ = store::append_journal(
                    &slug,
                    &format!("frozen Claude session {} lost", s.label()),
                    "Its transcript is gone, so it can no longer be resumed.\n",
                );
            }
        }
    } else {
        ops::evaluate(terminals, &slug);
    }
    Ok(())
}

/// Runs a command in the foreground and records when it exits. Unless the project already
/// waits on something, it waits on this command.
pub fn run_command(terminals: &dyn Terminals, args: &[String], set_wait: bool) -> Result<i32> {
    let Some((program, rest)) = args.split_first() else {
        bail!("usage: ctx run [--no-wait] -- <command> [args...]");
    };
    let slug = std::env::var("CTX_PROJECT")
        .ok()
        .filter(|s| store::load(s).is_ok());
    let label = args.join(" ");
    if let Some(slug) = &slug {
        let _ = store::record_event(
            slug,
            &Event {
                time: Utc::now(),
                kind: EventKind::CommandStart,
                pane: terminals.current_terminal(),
                session: None,
                detail: label.clone(),
            },
        );
        let waiting = store::load(slug).is_ok_and(|p| p.waiting());
        if set_wait && !waiting {
            ops::set_wait(
                terminals,
                slug,
                Some(Condition::Command {
                    label: label.clone(),
                }),
            )?;
        }
    }
    let status = Command::new(program)
        .args(rest)
        .status()
        .with_context(|| format!("could not start {program}"))?;
    let code = status.code().unwrap_or(1);
    if let Some(slug) = &slug {
        let _ = store::record_event(
            slug,
            &Event {
                time: Utc::now(),
                kind: EventKind::CommandExit,
                pane: terminals.current_terminal(),
                session: None,
                detail: code.to_string(),
            },
        );
        ops::evaluate(terminals, slug);
    }
    Ok(code)
}

/// A short summary for the tmux status line, read from the project files only.
pub fn status_line() -> String {
    let projects = store::list();
    let live = projects.iter().filter(|p| p.state == State::Live).count();
    let ready = projects
        .iter()
        .filter(|p| match p.state {
            State::Live => p.cleared().is_some(),
            State::Frozen => !p.waiting(),
            State::Done => false,
        })
        .count();
    let mut parts = vec![format!("{live} live")];
    if ready > 0 {
        parts.push(format!("{ready} ready"));
    }
    format!("ctx {}", parts.join(" · "))
}

pub fn hooks_snippet() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ctx".into());
    let hook = |kind: &str| serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("{exe} event {kind}") }] }]);
    let value = serde_json::json!({
        "hooks": {
            "SessionStart": hook("claude-start"),
            "SessionEnd": hook("claude-end"),
            "Stop": hook("claude-stop"),
            "Notification": hook("claude-notification"),
        }
    });
    serde_json::to_string_pretty(&value).unwrap_or_default()
}
