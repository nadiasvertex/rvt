use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::item::{ItemRef, ItemState};
use crate::wait::{Event, Wait};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Live,
    Frozen,
    Done,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Frozen => "frozen",
            State::Done => "done",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    pub time: DateTime<Utc>,
    #[serde(default)]
    pub doing: String,
    #[serde(default)]
    pub next: String,
    #[serde(default)]
    pub waiting: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub added: DateTime<Utc>,
    pub item: ItemRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub name: String,
    /// tmux names the window after its program, so a restore leaves the name to tmux.
    #[serde(default)]
    pub auto_name: bool,
    /// tmux's layout string, which `select-layout` takes back.
    pub layout: String,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeSession {
    pub id: String,
    pub started: DateTime<Utc>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub pane: Option<String>,
    /// `CLAUDE_CONFIG_DIR` of the session, so a resume runs under the same account.
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    /// The name Claude showed for the session when ctx last saw it running.
    #[serde(default)]
    pub name: Option<String>,
    /// Set while the session is set aside to resume later.
    #[serde(default)]
    pub frozen: Option<FrozenSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenSession {
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub note: String,
}

/// Sessions kept per project, not counting frozen ones.
pub const KEEP_SESSIONS: usize = 5;

impl ClaudeSession {
    /// The session's name when known, and otherwise the start of its ID.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.id.chars().take(8).collect())
    }

    /// Whether Claude kept a transcript of the session. `claude --resume` needs one, and Claude
    /// writes none for a session that ends before the first message, or after it is deleted.
    pub fn resumable(&self) -> bool {
        let config = self
            .config_dir
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
            .unwrap_or_default();
        let projects = config.join("projects");
        let file = format!("{}.jsonl", self.id);
        if let Some(cwd) = &self.cwd {
            if projects
                .join(claude_project_name(cwd))
                .join(&file)
                .is_file()
            {
                return true;
            }
        }
        fs::read_dir(&projects)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join(&file).is_file())
    }
}

/// The directory name Claude gives a working directory under `projects/`.
fn claude_project_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(skip)]
    pub slug: String,
    pub state: State,
    pub created: DateTime<Utc>,
    /// The tmux session name.
    pub session: String,
    /// Where new tmux windows start.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// The next step, kept between handoffs.
    #[serde(default)]
    pub next: String,
    #[serde(default)]
    pub handoff: Option<Handoff>,
    #[serde(default)]
    pub wait: Option<Wait>,
    #[serde(default)]
    pub claude_sessions: Vec<ClaudeSession>,
    /// Suggestion keys the user turned down.
    #[serde(default)]
    pub dismissed: Vec<String>,
    #[serde(default)]
    pub layout: Vec<Window>,
    #[serde(default)]
    pub items: Vec<Entry>,
}

impl Project {
    pub fn refs(&self) -> impl Iterator<Item = &ItemRef> {
        self.items.iter().map(|e| &e.item)
    }

    pub fn has(&self, item: &ItemRef) -> bool {
        self.refs().any(|r| r.key() == item.key())
    }

    pub fn waiting(&self) -> bool {
        self.wait.as_ref().is_some_and(Wait::is_waiting)
    }

    pub fn cleared(&self) -> Option<&str> {
        self.wait
            .as_ref()
            .and_then(|w| w.cleared.as_ref())
            .map(|c| c.reason.as_str())
    }

    /// Records a session that started, replacing an earlier record of the same session, which
    /// thaws it. Drops sessions that can no longer be resumed and the oldest ended sessions
    /// past the limit; frozen sessions do not count toward it. Returns the frozen sessions
    /// dropped because their transcript is gone, so the caller can say so.
    pub fn add_claude_session(
        &mut self,
        session: ClaudeSession,
        resumable: impl Fn(&ClaudeSession) -> bool,
    ) -> Vec<ClaudeSession> {
        let (keep, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut self.claude_sessions)
            .into_iter()
            .filter(|s| s.id != session.id)
            .partition(|s| resumable(s));
        self.claude_sessions = keep;
        self.claude_sessions.push(session);
        let unfrozen = self
            .claude_sessions
            .iter()
            .filter(|s| s.frozen.is_none())
            .count();
        let mut excess = unfrozen.saturating_sub(KEEP_SESSIONS);
        self.claude_sessions.retain(|s| {
            let drop = excess > 0 && s.frozen.is_none();
            excess -= usize::from(drop);
            !drop
        });
        dropped.into_iter().filter(|s| s.frozen.is_some()).collect()
    }
}

/// Item states at a point in time: the last visit or the last freeze.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub items: BTreeMap<String, ItemState>,
}

#[derive(Debug, Clone, Copy)]
pub enum MarkKind {
    Visit,
    Snapshot,
}

impl MarkKind {
    fn file(self) -> &'static str {
        match self {
            MarkKind::Visit => "visit.json",
            MarkKind::Snapshot => "snapshot.json",
        }
    }
}

pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CTX_HOME") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("ctx")
}

pub fn projects_dir() -> PathBuf {
    home().join("projects")
}

pub fn project_dir(slug: &str) -> PathBuf {
    projects_dir().join(slug)
}

pub fn notes_dir(slug: &str) -> PathBuf {
    project_dir(slug).join("notes")
}

/// Writes through a temporary file so readers never see a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("could not write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))
}

pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "project".into()
    } else {
        slug
    }
}

pub fn load(slug: &str) -> Result<Project> {
    let path = project_dir(slug).join("project.toml");
    let text =
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
    let mut project: Project =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    project.slug = slug.to_string();
    Ok(project)
}

pub fn save(project: &Project) -> Result<()> {
    let text = toml::to_string(project)?;
    write_atomic(
        &project_dir(&project.slug).join("project.toml"),
        text.as_bytes(),
    )
}

/// Loads, changes and saves a project, so that two ctx processes do not undo each other's edits
/// to different fields.
pub fn update(slug: &str, change: impl FnOnce(&mut Project)) -> Result<Project> {
    let mut project = load(slug)?;
    change(&mut project);
    save(&project)?;
    Ok(project)
}

/// All projects, sorted by name. Projects that fail to load are skipped.
pub fn list() -> Vec<Project> {
    let Ok(entries) = fs::read_dir(projects_dir()) else {
        return Vec::new();
    };
    let mut projects: Vec<Project> = entries
        .flatten()
        .filter(|e| e.path().join("project.toml").exists())
        .filter_map(|e| load(&e.file_name().to_string_lossy()).ok())
        .collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    projects
}

pub fn create(name: &str, dir: Option<PathBuf>) -> Result<Project> {
    let base = slugify(name);
    let mut slug = base.clone();
    let mut n = 2;
    while project_dir(&slug).exists() {
        slug = format!("{base}-{n}");
        n += 1;
    }
    let project = Project {
        name: name.trim().to_string(),
        slug: slug.clone(),
        state: State::Live,
        created: Utc::now(),
        session: slug.clone(),
        dir,
        next: String::new(),
        handoff: None,
        wait: None,
        claude_sessions: Vec::new(),
        dismissed: Vec::new(),
        layout: Vec::new(),
        items: Vec::new(),
    };
    fs::create_dir_all(notes_dir(&slug))?;
    save(&project)?;
    append_journal(&slug, "created", "")?;
    Ok(project)
}

/// Finds a project by slug, name, or a unique prefix of either.
pub fn find(projects: &[Project], query: &str) -> Result<String> {
    let q = query.trim().to_lowercase();
    if let Some(p) = projects
        .iter()
        .find(|p| p.slug == q || p.name.to_lowercase() == q)
    {
        return Ok(p.slug.clone());
    }
    let matches: Vec<&Project> = projects
        .iter()
        .filter(|p| p.slug.starts_with(&q) || p.name.to_lowercase().starts_with(&q))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.slug.clone()),
        [] => bail!("no project matches {query}"),
        many => bail!(
            "{query} matches {}",
            many.iter()
                .map(|p| p.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn journal_path(slug: &str) -> PathBuf {
    project_dir(slug).join("journal.md")
}

pub fn append_journal(slug: &str, heading: &str, body: &str) -> Result<()> {
    let path = journal_path(slug);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let when = Local::now().format("%Y-%m-%d %H:%M");
    let mut entry = format!("## {when} · {heading}\n");
    if !body.trim().is_empty() {
        entry.push('\n');
        entry.push_str(body.trim_end());
        entry.push('\n');
    }
    entry.push('\n');
    file.write_all(entry.as_bytes())?;
    Ok(())
}

pub fn journal(slug: &str) -> String {
    fs::read_to_string(journal_path(slug)).unwrap_or_default()
}

/// Journal entries, newest last. Each is a heading line and a body.
pub fn journal_entries(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for chunk in text.split("\n## ").map(|c| c.trim_start_matches("## ")) {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let (heading, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
        entries.push((heading.trim().to_string(), body.trim().to_string()));
    }
    entries
}

pub fn handoff_body(h: &Handoff) -> String {
    let mut body = String::new();
    for (label, text) in [
        ("Doing", &h.doing),
        ("Next", &h.next),
        ("Waiting on", &h.waiting),
    ] {
        if !text.trim().is_empty() {
            body.push_str(&format!("**{label}:** {}\n", text.trim()));
        }
    }
    body
}

pub fn load_mark(slug: &str, kind: MarkKind) -> Mark {
    fs::read(project_dir(slug).join(kind.file()))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_mark(slug: &str, kind: MarkKind, mark: &Mark) -> Result<()> {
    write_atomic(
        &project_dir(slug).join(kind.file()),
        &serde_json::to_vec(mark)?,
    )
}

/// The cached states of the project's items, for a visit mark or a snapshot.
pub fn mark_now(project: &Project, cache: &BTreeMap<String, ItemState>) -> Mark {
    Mark {
        time: Some(Utc::now()),
        items: project
            .refs()
            .filter_map(|r| cache.get(&r.key()).map(|s| (r.key(), s.clone())))
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub path: PathBuf,
    pub title: String,
    pub source: String,
    pub link: String,
    pub captured: DateTime<Utc>,
    pub body: String,
}

pub const SOURCES: [&str; 5] = ["teams", "email", "web", "ai", "local"];

pub fn note_text(note: &Note) -> String {
    let mut text = format!(
        "---\ntitle: {}\nsource: {}\n",
        note.title.replace('\n', " "),
        note.source
    );
    if !note.link.is_empty() {
        text.push_str(&format!("link: {}\n", note.link));
    }
    text.push_str(&format!(
        "captured: {}\n---\n\n",
        note.captured.to_rfc3339()
    ));
    text.push_str(note.body.trim_end());
    text.push('\n');
    text
}

pub fn parse_note(path: &Path, text: &str) -> Note {
    let mut note = Note {
        path: path.to_path_buf(),
        title: String::new(),
        source: "local".into(),
        link: String::new(),
        captured: fs::metadata(path)
            .and_then(|m| m.modified())
            .map(DateTime::from)
            .unwrap_or_else(|_| Utc::now()),
        body: text.to_string(),
    };
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some((front, body)) = rest.split_once("\n---") {
            for line in front.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim().to_string();
                match key.trim() {
                    "title" => note.title = value,
                    "source" => note.source = value,
                    "link" => note.link = value,
                    "captured" => {
                        if let Ok(d) = DateTime::parse_from_rfc3339(&value) {
                            note.captured = d.with_timezone(&Utc);
                        }
                    }
                    _ => {}
                }
            }
            note.body = body.trim_start_matches('\n').trim_start().to_string();
        }
    }
    if note.title.is_empty() {
        note.title = note
            .body
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_start_matches('#').trim().to_string())
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
    }
    note
}

/// The project's notes, newest first.
pub fn notes(slug: &str) -> Vec<Note> {
    let Ok(entries) = fs::read_dir(notes_dir(slug)) else {
        return Vec::new();
    };
    let mut notes: Vec<Note> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .filter_map(|p| fs::read_to_string(&p).ok().map(|t| parse_note(&p, &t)))
        .collect();
    notes.sort_by(|a, b| b.captured.cmp(&a.captured));
    notes
}

pub fn new_note_path(slug: &str, title: &str) -> PathBuf {
    let date = Local::now().format("%Y-%m-%d");
    let base = slugify(title);
    let base: String = base.chars().take(48).collect();
    let dir = notes_dir(slug);
    let mut path = dir.join(format!("{date}-{base}.md"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{date}-{base}-{n}.md"));
        n += 1;
    }
    path
}

pub fn write_note(
    slug: &str,
    title: &str,
    source: &str,
    link: &str,
    body: &str,
) -> Result<PathBuf> {
    let path = new_note_path(slug, title);
    let note = Note {
        path: path.clone(),
        title: title.to_string(),
        source: source.to_string(),
        link: link.to_string(),
        captured: Utc::now(),
        body: body.to_string(),
    };
    write_atomic(&path, note_text(&note).as_bytes())?;
    Ok(path)
}

pub fn events_path(slug: &str) -> PathBuf {
    project_dir(slug).join("events.jsonl")
}

pub fn record_event(slug: &str, event: &Event) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path(slug))?;
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Recent events, oldest first. The file is trimmed when it grows past a few hundred lines.
pub fn events(slug: &str) -> Vec<Event> {
    let path = events_path(slug);
    let text = fs::read_to_string(&path).unwrap_or_default();
    let events: Vec<Event> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    const KEEP: usize = 200;
    if events.len() > KEEP * 2 {
        let kept = &events[events.len() - KEEP..];
        let text: String = kept
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .map(|l| l + "\n")
            .collect();
        let _ = write_atomic(&path, text.as_bytes());
        return kept.to_vec();
    }
    events
}

/// Commits everything under `projects/`. History is a convenience, so failures are ignored.
pub fn commit(message: &str) {
    let dir = projects_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if !dir.join(".git").exists() {
        if !git(&["init", "-q"]) {
            return;
        }
        let _ = fs::write(dir.join(".gitignore"), "*.tmp-*\nevents.jsonl\n");
    }
    if git(&["add", "-A"]) {
        let _ = git(&[
            "-c",
            "user.name=ctx",
            "-c",
            "user.email=ctx@localhost",
            "commit",
            "-q",
            "-m",
            message,
        ]);
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Points `CTX_HOME` at a fresh directory for the length of a test.
    pub struct TempHome {
        pub dir: std::path::PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl TempHome {
        pub fn new(name: &str) -> Self {
            let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("ctx-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: tests that touch CTX_HOME hold LOCK, so no other thread reads it meanwhile.
            unsafe { std::env::set_var("CTX_HOME", &dir) };
            TempHome { dir, _lock: lock }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempHome;
    use super::*;
    use crate::wait::{Condition, Wait};

    #[test]
    fn a_claude_session_is_resumable_only_while_its_transcript_exists() {
        let home = TempHome::new("resumable");
        let config = home.dir.join("claude");
        let session = |id: &str, cwd: &str| ClaudeSession {
            id: id.into(),
            started: Utc::now(),
            cwd: Some(PathBuf::from(cwd)),
            pane: None,
            config_dir: Some(config.clone()),
            name: None,
            frozen: None,
        };
        let here = config.join("projects/-src-core--claude-wt");
        fs::create_dir_all(&here).unwrap();
        fs::write(here.join("kept.jsonl"), "{}").unwrap();
        let moved = config.join("projects/elsewhere");
        fs::create_dir_all(&moved).unwrap();
        fs::write(moved.join("moved.jsonl"), "{}").unwrap();

        assert!(session("kept", "/src/core/.claude/wt").resumable());
        assert!(session("moved", "/src/core").resumable());
        assert!(!session("empty", "/src/core/.claude/wt").resumable());
    }

    fn numbered(n: usize, frozen: bool) -> ClaudeSession {
        ClaudeSession {
            id: format!("s{n}"),
            started: Utc::now(),
            cwd: None,
            pane: None,
            config_dir: None,
            name: None,
            frozen: frozen.then(|| FrozenSession {
                at: Utc::now(),
                note: String::new(),
            }),
        }
    }

    fn ids(project: &Project) -> Vec<&str> {
        project
            .claude_sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect()
    }

    #[test]
    fn frozen_sessions_do_not_count_toward_the_session_limit() {
        let _home = TempHome::new("session-limit");
        let mut project = create("Limit", None).unwrap();
        project.add_claude_session(numbered(0, true), |_| true);
        for n in 1..=6 {
            project.add_claude_session(numbered(n, false), |_| true);
        }
        assert_eq!(ids(&project), ["s0", "s2", "s3", "s4", "s5", "s6"]);
    }

    #[test]
    fn a_restart_replaces_the_frozen_record_and_lost_frozen_sessions_are_returned() {
        let _home = TempHome::new("session-thaw");
        let mut project = create("Thaw", None).unwrap();
        project.add_claude_session(numbered(1, true), |_| true);
        project.add_claude_session(numbered(2, true), |_| true);
        project.add_claude_session(numbered(3, false), |_| true);
        let dropped = project.add_claude_session(numbered(1, false), |s| s.id == "s1");
        assert_eq!(ids(&project), ["s1"]);
        assert!(project.claude_sessions[0].frozen.is_none());
        assert_eq!(
            dropped.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s2"]
        );
    }

    #[test]
    fn projects_round_trip_through_toml() {
        let _home = TempHome::new("round-trip");
        let mut project = create("Markdown serial", Some(PathBuf::from("/src/core"))).unwrap();
        assert_eq!(project.slug, "markdown-serial");
        project.items.push(Entry {
            added: Utc::now(),
            item: ItemRef::GitlabMr {
                project: "group/parser".into(),
                iid: 1431,
            },
        });
        project.items.push(Entry {
            added: Utc::now(),
            item: ItemRef::Branch {
                repo: "/src/core".into(),
                branch: "Feature/markdown".into(),
            },
        });
        project.wait = Some(Wait::new(Condition::Claude {
            pane: Some("%3".into()),
        }));
        project.handoff = Some(Handoff {
            time: Utc::now(),
            doing: "table tests".into(),
            next: "write round-trip test".into(),
            waiting: String::new(),
        });
        project.layout = vec![Window {
            name: "edit".into(),
            auto_name: false,
            layout: "b25d,200x50,0,0,1".into(),
            panes: vec![Pane {
                path: "/src/core".into(),
            }],
        }];
        save(&project).unwrap();
        let loaded = load("markdown-serial").unwrap();
        assert_eq!(loaded, project);

        let second = create("Markdown serial", None).unwrap();
        assert_eq!(second.slug, "markdown-serial-2");
        assert_eq!(list().len(), 2);
        assert!(journal("markdown-serial").contains("· created"));
    }

    #[test]
    fn find_takes_names_slugs_and_unique_prefixes() {
        let p = |name: &str| Project {
            name: name.into(),
            slug: slugify(name),
            state: State::Live,
            created: Utc::now(),
            session: slugify(name),
            dir: None,
            next: String::new(),
            handoff: None,
            wait: None,
            claude_sessions: Vec::new(),
            dismissed: Vec::new(),
            layout: Vec::new(),
            items: Vec::new(),
        };
        let all = [p("Thread safety"), p("tcu zip"), p("wasm target")];
        assert_eq!(find(&all, "thread-safety").unwrap(), "thread-safety");
        assert_eq!(find(&all, "Thread Safety").unwrap(), "thread-safety");
        assert_eq!(find(&all, "w").unwrap(), "wasm-target");
        assert!(find(&all, "t").is_err(), "ambiguous");
        assert!(find(&all, "x").is_err());
    }

    #[test]
    fn notes_keep_their_front_matter() {
        let _home = TempHome::new("notes");
        create("Notes", None).unwrap();
        let path = write_note(
            "notes",
            "Teams: zlib flags",
            "teams",
            "https://teams/x",
            "body text",
        )
        .unwrap();
        let notes = notes("notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].path, path);
        assert_eq!(notes[0].title, "Teams: zlib flags");
        assert_eq!(notes[0].source, "teams");
        assert_eq!(notes[0].link, "https://teams/x");
        assert_eq!(notes[0].body, "body text\n");

        let bare = parse_note(
            Path::new("/x/2026-01-01-plain.md"),
            "# Plain heading\n\ntext",
        );
        assert_eq!(bare.title, "Plain heading");
        assert_eq!(bare.source, "local");
    }

    #[test]
    fn journal_entries_split_on_headings() {
        let text =
            "## 2026-11-16 10:00 · created\n\n## 2026-11-16 11:00 · frozen\n\n**Next:** x\n\n";
        let entries = journal_entries(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].0, "2026-11-16 11:00 · frozen");
        assert_eq!(entries[1].1, "**Next:** x");
    }

    #[test]
    fn slugs_are_lowercase_words_joined_by_dashes() {
        assert_eq!(slugify("  Find across anchors! "), "find-across-anchors");
        assert_eq!(slugify("C++ / wire v2"), "c-wire-v2");
        assert_eq!(slugify("!!!"), "project");
    }
}
