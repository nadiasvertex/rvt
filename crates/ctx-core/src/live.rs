//! The interactive Claude Code sessions that are running now, from the registry Claude keeps
//! under `<config dir>/sessions/<pid>.json`. The format is Claude's own and undocumented, so
//! every field is optional and a file that does not parse is skipped.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Busy,
    Idle,
    /// A status this version of ctx does not know. Treated as busy, which is the safe reading.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSession {
    pub id: String,
    pub pid: u32,
    pub status: Status,
    /// The terminal the session runs in: the tmux pane ID, such as `%5`, from Claude's registry.
    pub pane: Option<String>,
    pub name: Option<String>,
    /// The registry file, which Claude removes when it exits normally.
    pub file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    pid: Option<u32>,
    session_id: Option<String>,
    proc_start: Option<String>,
    /// Milliseconds since the epoch.
    started_at: Option<i64>,
    kind: Option<String>,
    tmux: Option<String>,
    name: Option<String>,
    status: Option<String>,
}

/// When a running process started, as `ps` reports it.
#[derive(Debug, Clone, PartialEq)]
struct Started {
    /// Local time in the form `ps -o lstart` prints and Claude records as `procStart`.
    text: String,
    /// Seconds since the epoch, from the process's elapsed time, which no time zone affects.
    epoch: i64,
}

/// How far Claude's own `startedAt` may trail the process start.
const START_SLACK_SECS: i64 = 120;

/// Running sessions by session ID, across the given Claude config directories.
pub fn sessions(config_dirs: &[PathBuf]) -> HashMap<String, LiveSession> {
    let started = process_starts();
    let mut dirs: Vec<&PathBuf> = config_dirs.iter().collect();
    dirs.sort();
    dirs.dedup();
    dirs.into_iter()
        .flat_map(|dir| read_registry(&dir.join("sessions")))
        .filter_map(|(file, text)| parse(&file, &text, &started))
        .map(|s| (s.id.clone(), s))
        .collect()
}

/// Whether the session's registry file is gone or its process no longer runs.
pub fn ended(session: &LiveSession) -> bool {
    !session.file.is_file() || !process_starts().contains_key(&session.pid)
}

fn read_registry(dir: &Path) -> Vec<(PathBuf, String)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| fs::read_to_string(&p).ok().map(|t| (p, t)))
        .collect()
}

/// A live interactive session from one registry file. A file left by a killed process, or by
/// a process whose pid was reused, fails the start time check. Either recorded start time may
/// match: `procStart` is local time, which differs when ctx and Claude run in different time
/// zones, and `startedAt` is taken a little after the process starts.
fn parse(file: &Path, text: &str, started: &HashMap<u32, Started>) -> Option<LiveSession> {
    let record: Record = serde_json::from_str(text).ok()?;
    if record.kind.as_deref() != Some("interactive") {
        return None;
    }
    let pid = record.pid?;
    let process = started.get(&pid)?;
    let same_text = record
        .proc_start
        .as_deref()
        .is_some_and(|t| t.trim() == process.text);
    let same_epoch = record
        .started_at
        .is_some_and(|ms| (0..=START_SLACK_SECS).contains(&(ms / 1000 - process.epoch)));
    if !same_text && !same_epoch {
        return None;
    }
    let status = match record.status.as_deref() {
        Some("busy") => Status::Busy,
        Some("idle") => Status::Idle,
        _ => Status::Other,
    };
    Some(LiveSession {
        id: record.session_id?,
        pid,
        status,
        pane: record.tmux.as_deref().and_then(pane_of),
        name: record.name.filter(|n| !n.is_empty()),
        file: file.to_path_buf(),
    })
}

/// The pane in Claude's `session:@window.%pane` form.
fn pane_of(tmux: &str) -> Option<String> {
    let (_, pane) = tmux.rsplit_once('.')?;
    pane.starts_with('%').then(|| pane.to_string())
}

/// Each running process's start time.
fn process_starts() -> HashMap<u32, Started> {
    let now = chrono::Utc::now().timestamp();
    Command::new("ps")
        .args(["-A", "-o", "pid=,etime=,lstart="])
        .output()
        .map(|o| parse_starts(&String::from_utf8_lossy(&o.stdout), now))
        .unwrap_or_default()
}

/// Parses `ps -o pid=,etime=,lstart=` lines, given the time `ps` ran.
fn parse_starts(table: &str, now: i64) -> HashMap<u32, Started> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let elapsed = elapsed_secs(fields.next()?)?;
            let text = fields.collect::<Vec<_>>().join(" ");
            Some((
                pid,
                Started {
                    text,
                    epoch: now - elapsed,
                },
            ))
        })
        .collect()
}

/// Seconds in a `ps` elapsed time, `[[dd-]hh:]mm:ss`.
fn elapsed_secs(etime: &str) -> Option<i64> {
    let (days, clock) = match etime.split_once('-') {
        Some((d, rest)) => (d.parse::<i64>().ok()?, rest),
        None => (0, etime),
    };
    let parts: Vec<i64> = clock
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, s) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(((days * 24 + h) * 60 + m) * 60 + s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "Mon Nov 23 11:08:40 2026";

    /// When `ps` ran: an hour and a minute after process 29054 started.
    const NOW: i64 = 1_795_432_120 + 3660;
    /// Claude's `startedAt` for process 29054, a little after the process started.
    const STARTED_MS: i64 = 1_795_432_120_760;

    fn record_at(kind: &str, start: &str, started_ms: i64, status: &str) -> String {
        format!(
            r#"{{"pid":29054,"sessionId":"e7c8","cwd":"/w","procStart":"{start}",
            "startedAt":{started_ms},"kind":"{kind}","tmux":"lib:@3.%5","name":"rvt-d6",
            "status":"{status}","futureField":1}}"#
        )
    }

    fn record(kind: &str, start: &str, status: &str) -> String {
        record_at(kind, start, STARTED_MS, status)
    }

    fn started() -> HashMap<u32, Started> {
        parse_starts(
            &format!("  101 2-01:00:00 Sun Nov 22 09:00:00 2026\n29054    01:01:00 {START}    \n"),
            NOW,
        )
    }

    #[test]
    fn a_session_is_live_when_either_start_time_matches() {
        let other_zone = record("interactive", "Mon Nov 23 16:08:40 2026", "idle");
        assert!(parse(Path::new("/s/29054.json"), &other_zone, &started()).is_some());
        let no_epoch = record_at("interactive", START, 0, "idle");
        assert!(parse(Path::new("/s/29054.json"), &no_epoch, &started()).is_some());
    }

    #[test]
    fn elapsed_times_count_days_hours_minutes_and_seconds() {
        assert_eq!(elapsed_secs("05"), None);
        assert_eq!(elapsed_secs("01:05"), Some(65));
        assert_eq!(elapsed_secs("02:01:05"), Some(7265));
        assert_eq!(elapsed_secs("3-02:01:05"), Some(3 * 86400 + 7265));
        assert_eq!(elapsed_secs("x:05"), None);
    }

    #[test]
    fn a_running_interactive_session_is_live_with_its_pane_and_status() {
        let live = parse(
            Path::new("/s/29054.json"),
            &record("interactive", START, "busy"),
            &started(),
        )
        .expect("the session runs");
        assert_eq!(live.id, "e7c8");
        assert_eq!(live.status, Status::Busy);
        assert_eq!(live.pane.as_deref(), Some("%5"));
        assert_eq!(live.name.as_deref(), Some("rvt-d6"));
    }

    #[test]
    fn a_leftover_file_whose_pid_started_at_another_time_is_not_live() {
        let reused = record_at(
            "interactive",
            "Mon Nov 23 08:00:00 2026",
            STARTED_MS - 3_600_000,
            "idle",
        );
        assert_eq!(parse(Path::new("/s/29054.json"), &reused, &started()), None);
        let gone = record("interactive", START, "idle");
        assert_eq!(
            parse(Path::new("/s/29054.json"), &gone, &HashMap::new()),
            None
        );
    }

    #[test]
    fn background_jobs_and_unreadable_files_are_skipped() {
        let bg = record("bg", START, "idle");
        assert_eq!(parse(Path::new("/s/29054.json"), &bg, &started()), None);
        assert_eq!(
            parse(Path::new("/s/29054.json"), "{not json", &started()),
            None
        );
        assert_eq!(parse(Path::new("/s/29054.json"), "{}", &started()), None);
    }

    #[test]
    fn an_unknown_status_reads_as_other() {
        let odd = record("interactive", START, "compacting");
        let live = parse(Path::new("/s/29054.json"), &odd, &started()).expect("runs");
        assert_eq!(live.status, Status::Other);
    }

    #[test]
    fn a_pane_comes_only_from_the_pane_part_of_the_tmux_field() {
        assert_eq!(pane_of("a.b:@3.%12").as_deref(), Some("%12"));
        assert_eq!(pane_of("lib:@3"), None);
    }
}
