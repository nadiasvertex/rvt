//! Actions shared by the TUI and the command line. Each loads what it needs from disk, so any
//! number of ctx processes can run side by side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};

use crate::cache;
use crate::changes::{self, Change};
use crate::config::Config;
use crate::item::{ItemRef, ItemState};
use crate::live::{self, LiveSession};
use crate::search;
use crate::store::{
    self, ClaudeSession, Entry, FrozenSession, Handoff, MarkKind, Note, Project, State,
};
use crate::tmux;
use crate::wait::{self, Cleared, Condition, Wait};

/// The project whose tmux session this process runs in.
pub fn current_project(projects: &[Project]) -> Option<String> {
    if let Some(session) = tmux::current_session() {
        return projects
            .iter()
            .find(|p| p.session == session)
            .map(|p| p.slug.clone());
    }
    std::env::var("CTX_PROJECT")
        .ok()
        .filter(|slug| projects.iter().any(|p| &p.slug == slug))
}

/// The named project, or the current one when no name is given.
pub fn resolve(projects: &[Project], name: Option<&str>) -> Result<String> {
    match name {
        Some(name) => store::find(projects, name),
        None => match current_project(projects) {
            Some(slug) => Ok(slug),
            None => bail!("not in a project's tmux session, so name the project"),
        },
    }
}

/// "Since you left": changes after the later of the last visit and the last freeze.
pub struct Since {
    pub time: Option<DateTime<Utc>>,
    pub changes: Vec<Change>,
}

pub fn since(project: &Project, items: &BTreeMap<String, ItemState>, notes: &[Note]) -> Since {
    let visit = store::load_mark(&project.slug, MarkKind::Visit);
    let snapshot = store::load_mark(&project.slug, MarkKind::Snapshot);
    let base = if snapshot.time > visit.time {
        snapshot
    } else {
        visit
    };
    let refs: Vec<&ItemRef> = project.refs().collect();
    let mut found = changes::diff(&refs, &base.items, items);
    found.extend(changes::new_notes(notes, base.time));
    Since {
        time: base.time,
        changes: found,
    }
}

fn save_layout(project: &Project) {
    if !tmux::has_session(&project.session) {
        return;
    }
    if let Ok(layout) = tmux::capture_layout(&project.session) {
        if !layout.is_empty() {
            let _ = store::update(&project.slug, |p| p.layout = layout);
        }
    }
}

/// Records the visit mark and the session layout of a project you are leaving.
pub fn leave(slug: &str) {
    let Ok(project) = store::load(slug) else {
        return;
    };
    let cache = cache::load();
    let _ = store::save_mark(
        slug,
        MarkKind::Visit,
        &store::mark_now(&project, &cache.items),
    );
    save_layout(&project);
}

/// Leaves `from`, makes sure the target session exists, and moves the client there.
pub fn switch(cfg: &Config, slug: &str, from: Option<&str>) -> Result<()> {
    if let Some(from) = from.filter(|f| *f != slug) {
        leave(from);
    }
    let project = store::load(slug)?;
    tmux::ensure_session(&project)?;
    let inside = tmux::inside();
    tmux::switch_to(&project.session)?;
    if inside && cfg.tmux.switch_message {
        tmux::message(&switch_message(&project));
    }
    Ok(())
}

fn switch_message(project: &Project) -> String {
    let cache = cache::load();
    let notes = store::notes(&project.slug);
    let found = since(project, &cache.items, &notes);
    let mut parts = vec![project.name.clone()];
    if project.state == State::Frozen {
        parts.push("frozen".into());
    }
    if let Some(reason) = project.cleared() {
        parts.push(reason.to_string());
    }
    match found.changes.len() {
        0 => {}
        1 => parts.push("1 change".into()),
        n => parts.push(format!("{n} changes")),
    }
    if !project.next.is_empty() {
        parts.push(format!("next: {}", project.next));
    }
    parts.join(" · ")
}

/// Starts `ctx <args>` in its own process group, so it outlives a closing popup.
pub fn spawn_detached(args: &[&str]) {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

pub fn reindex_project(slug: &str) {
    if let Ok(project) = store::load(slug) {
        let _ = search::index_project(&project, &cache::load().items);
    }
}

pub fn freeze(
    cfg: &Config,
    slug: &str,
    handoff: Handoff,
    condition: Option<Condition>,
    close: bool,
) -> Result<()> {
    let project = store::load(slug)?;
    save_layout(&project);
    let cache = cache::load();
    let mark = store::mark_now(&project, &cache.items);
    store::save_mark(slug, MarkKind::Snapshot, &mark)?;
    store::save_mark(slug, MarkKind::Visit, &mark)?;
    let keep = project
        .wait
        .clone()
        .filter(|w| condition.as_ref() == Some(&w.condition) && w.is_waiting());
    let wait = keep.or_else(|| condition.map(Wait::new));
    let mut body = store::handoff_body(&handoff);
    let manual_repeat = matches!(
        wait.as_ref().map(|w| &w.condition),
        Some(Condition::Manual { text }) if text.trim() == handoff.waiting.trim()
    );
    if let Some(w) = wait.as_ref().filter(|_| !manual_repeat) {
        body.push_str(&format!("**Wait:** {}\n", w.condition.describe()));
    }
    store::append_journal(slug, "frozen", &body)?;
    let project = store::update(slug, |p| {
        p.state = State::Frozen;
        p.next = handoff.next.clone();
        p.handoff = Some(handoff);
        p.wait = wait;
    })?;
    store::commit(&format!("Freeze {slug}"));
    let _ = search::index_project(&project, &cache.items);
    if cfg.claude.summarize_on_freeze {
        spawn_detached(&["summarize", slug]);
    }
    if close && tmux::has_session(&project.session) {
        freeze_claudes_in_session(cfg, &project)?;
        if tmux::current_session().as_deref() == Some(project.session.as_str()) {
            tmux::ensure_home_session()?;
            tmux::switch_to(tmux::HOME_SESSION)?;
        }
        tmux::kill_session(&project.session)?;
    }
    Ok(())
}

/// The Claude config directories whose session registries ctx reads.
pub fn claude_config_dirs(configured: Option<PathBuf>, projects: &[Project]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = configured.into_iter().collect();
    dirs.extend(std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    dirs.extend(dirs::home_dir().map(|h| h.join(".claude")));
    dirs.extend(
        projects
            .iter()
            .flat_map(|p| &p.claude_sessions)
            .filter_map(|s| s.config_dir.clone()),
    );
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The project's Claude sessions that run now, by session ID.
pub fn live_claudes(cfg: &Config, project: &Project) -> Vec<(ClaudeSession, LiveSession)> {
    let running = live::sessions(&claude_config_dirs(
        cfg.claude.config_path(),
        std::slice::from_ref(project),
    ));
    project
        .claude_sessions
        .iter()
        .filter_map(|s| running.get(&s.id).map(|l| (s.clone(), l.clone())))
        .collect()
}

/// Marks a Claude session frozen and journals it. Ending its process is `end_claude`.
pub fn mark_claude_frozen(
    slug: &str,
    id: &str,
    note: &str,
    name: Option<String>,
) -> Result<ClaudeSession> {
    let mut found = None;
    store::update(slug, |p| {
        if let Some(s) = p.claude_sessions.iter_mut().find(|s| s.id == id) {
            s.name = name.or(s.name.take());
            s.frozen = Some(FrozenSession {
                at: Utc::now(),
                note: note.trim().to_string(),
            });
            found = Some(s.clone());
        }
    })?;
    let Some(session) = found else {
        bail!("{slug} has no Claude session {id}");
    };
    store::append_journal(
        slug,
        &format!("froze Claude session {}", session.label()),
        &format!(
            "{}
",
            note.trim()
        ),
    )?;
    store::commit(&format!(
        "Freeze Claude session {} in {slug}",
        session.label()
    ));
    Ok(session)
}

/// Clears a Claude session's frozen mark, once ctx has started resuming it.
pub fn mark_claude_thawed(slug: &str, id: &str) -> Result<()> {
    let mut label = None;
    store::update(slug, |p| {
        if let Some(s) = p.claude_sessions.iter_mut().find(|s| s.id == id) {
            if s.frozen.take().is_some() {
                label = Some(s.label());
            }
        }
    })?;
    if let Some(label) = label {
        store::append_journal(slug, &format!("thawed Claude session {label}"), "")?;
        store::commit(&format!("Thaw Claude session {label} in {slug}"));
    }
    Ok(())
}

/// How long `end_claude` waits for each way of ending Claude to work.
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Ends a running Claude session and waits for it to exit. An idle session in a known pane
/// gets `/exit`, so it shuts down cleanly and runs its SessionEnd hooks; otherwise, or if that
/// does not end it, the process gets SIGTERM.
pub fn end_claude(session: &LiveSession) -> Result<()> {
    if session.status == live::Status::Idle {
        if let Some(pane) = &session.pane {
            if tmux::type_line(pane, "/exit").is_ok() && exited(session, EXIT_WAIT) {
                return Ok(());
            }
        }
    }
    Command::new("kill")
        .args(["-TERM", &session.pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if exited(session, EXIT_WAIT) {
        return Ok(());
    }
    bail!(
        "Claude session {} (pid {}) did not exit; end it by hand",
        session.name.as_deref().unwrap_or(&session.id),
        session.pid
    )
}

fn exited(session: &LiveSession, within: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + within;
    while std::time::Instant::now() < until {
        if live::ended(session) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    live::ended(session)
}

/// Marks the Claude sessions running in the project's tmux session frozen, before the tmux
/// session closes and ends them, so they can be resumed.
fn freeze_claudes_in_session(cfg: &Config, project: &Project) -> Result<()> {
    for (session, running) in live_claudes(cfg, project) {
        let inside = running
            .pane
            .as_deref()
            .and_then(tmux::pane_session)
            .is_some_and(|s| s == project.session);
        if inside {
            mark_claude_frozen(
                &project.slug,
                &session.id,
                "The project was frozen.",
                running.name,
            )?;
        }
    }
    Ok(())
}

fn set_state(slug: &str, state: State, heading: &str) -> Result<Project> {
    store::append_journal(slug, heading, "")?;
    let project = store::update(slug, |p| {
        p.state = state;
        if state == State::Done {
            p.wait = None;
        }
    })?;
    store::commit(&format!("{} {slug}", capitalize(heading)));
    Ok(project)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Makes a frozen project live. With enough changes waiting, a Claude brief starts in the
/// background.
pub fn thaw(slug: &str) -> Result<()> {
    let project = store::load(slug)?;
    if project.state != State::Frozen {
        return Ok(());
    }
    let cache = cache::load();
    let found = since(&project, &cache.items, &store::notes(slug));
    set_state(slug, State::Live, "thawed")?;
    if found.changes.len() >= crate::brief::MIN_CHANGES {
        spawn_detached(&["brief", slug]);
    }
    Ok(())
}

pub fn done(slug: &str) -> Result<()> {
    set_state(slug, State::Done, "done").map(|_| ())
}

pub fn reopen(slug: &str) -> Result<()> {
    set_state(slug, State::Live, "reopened").map(|_| ())
}

pub fn set_wait(slug: &str, condition: Option<Condition>) -> Result<()> {
    let heading = match &condition {
        Some(c) => format!("waiting on {}", c.describe()),
        None => "wait removed".to_string(),
    };
    store::append_journal(slug, &heading, "")?;
    store::update(slug, |p| p.wait = condition.map(Wait::new))?;
    store::commit(&format!("Wait in {slug}"));
    evaluate(slug);
    Ok(())
}

/// Clears the project's wait condition if it is met, and says so in tmux. Returns the reason.
pub fn evaluate(slug: &str) -> Option<String> {
    let project = store::load(slug).ok()?;
    let wait = project.wait.as_ref().filter(|w| w.is_waiting())?;
    let reason = wait::check(wait, &cache::load().items, &store::events(slug), Utc::now())?;
    let cleared = Cleared {
        at: Utc::now(),
        reason: reason.clone(),
    };
    store::update(slug, |p| {
        if let Some(w) = p.wait.as_mut() {
            w.cleared = Some(cleared);
        }
    })
    .ok()?;
    let _ = store::append_journal(slug, &format!("ready: {reason}"), "");
    store::commit(&format!("Ready {slug}"));
    tmux::message(&format!("ctx · {}: {reason}", project.name));
    Some(reason)
}

pub fn add_item(slug: &str, item: ItemRef) -> Result<bool> {
    let project = store::load(slug)?;
    if project.has(&item) {
        return Ok(false);
    }
    store::append_journal(slug, &format!("added {}", item.describe()), "")?;
    store::update(slug, |p| {
        p.dismissed.retain(|k| *k != item.key());
        p.items.push(Entry {
            added: Utc::now(),
            item,
        });
    })?;
    reindex_project(slug);
    Ok(true)
}

pub fn remove_item(slug: &str, key: &str) -> Result<()> {
    store::update(slug, |p| p.items.retain(|e| e.item.key() != key))?;
    reindex_project(slug);
    Ok(())
}

pub fn dismiss(slug: &str, key: &str) -> Result<()> {
    store::update(slug, |p| {
        if !p.dismissed.iter().any(|k| k == key) {
            p.dismissed.push(key.to_string());
        }
    })?;
    Ok(())
}

pub fn set_next(slug: &str, next: &str) -> Result<()> {
    store::update(slug, |p| p.next = next.trim().to_string())?;
    reindex_project(slug);
    Ok(())
}

pub fn capture(slug: &str, title: &str, source: &str, link: &str, body: &str) -> Result<PathBuf> {
    let title = if title.trim().is_empty() {
        crate::item::snippet(body).chars().take(60).collect()
    } else {
        title.trim().to_string()
    };
    let path = store::write_note(slug, &title, source, link.trim(), body)?;
    reindex_project(slug);
    Ok(path)
}

/// Creates a project. Inside a git repository the project starts there, and the current branch
/// becomes its first item unless it is the default branch.
pub fn create(name: &str, cwd: &Path) -> Result<Project> {
    let repo = forge::git::repo_root(cwd).ok();
    let project = store::create(
        name,
        Some(repo.clone().unwrap_or_else(|| cwd.to_path_buf())),
    )?;
    if let Some(repo) = repo {
        if let Ok(branch) = forge::git::current_branch(&repo) {
            if !matches!(branch.as_str(), "master" | "main" | "develop") {
                add_item(&project.slug, ItemRef::Branch { repo, branch })?;
            }
        }
    }
    store::commit(&format!("Create {}", project.slug));
    reindex_project(&project.slug);
    store::load(&project.slug)
}

/// Whether two items name the same thing. A branch matches the same branch checked out in any
/// worktree of its repository.
pub fn same_item(a: &ItemRef, b: &ItemRef) -> bool {
    if a.key() == b.key() {
        return true;
    }
    match (a, b) {
        (
            ItemRef::Branch {
                repo: repo_a,
                branch: a,
            },
            ItemRef::Branch {
                repo: repo_b,
                branch: b,
            },
        ) => {
            a == b
                && matches!(
                    (forge::git::common_dir(repo_a), forge::git::common_dir(repo_b)),
                    (Ok(x), Ok(y)) if x == y
                )
        }
        _ => false,
    }
}

/// How `ctx update` found the projects it writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The projects list one of the items.
    Listed,
    /// One of the projects' items is linked to one of the items.
    Linked,
    /// No project holds the items, so they go to the session's project, or the one named.
    Current,
}

/// The projects that are not done and that an update about `items` belongs to: those that list
/// one of the items, else those holding an item linked to one, else `current`.
pub fn projects_for(
    projects: &[Project],
    items: &[ItemRef],
    cache: &BTreeMap<String, ItemState>,
    current: Option<&str>,
) -> Option<(Found, Vec<String>)> {
    let open: Vec<&Project> = projects.iter().filter(|p| p.state != State::Done).collect();
    let pick = |test: &dyn Fn(&ItemRef) -> bool| -> Vec<String> {
        open.iter()
            .filter(|p| p.refs().any(test))
            .map(|p| p.slug.clone())
            .collect()
    };
    let listed = pick(&|r| items.iter().any(|i| same_item(r, i)));
    if !listed.is_empty() {
        return Some((Found::Listed, listed));
    }
    let related = |item: &ItemRef| {
        cache
            .get(&item.key())
            .map(|s| s.related.clone())
            .unwrap_or_default()
    };
    let linked = pick(&|r| {
        items.iter().any(|i| {
            related(r).iter().any(|x| same_item(x, i)) || related(i).iter().any(|x| same_item(x, r))
        })
    });
    if !linked.is_empty() {
        return Some((Found::Linked, linked));
    }
    let current = current.filter(|slug| open.iter().any(|p| p.slug == *slug))?;
    Some((Found::Current, vec![current.to_string()]))
}

/// Writes Claude's summary of its work to the project's journal, adds `add` to the project, and
/// commits. Returns the items that were new to the project.
pub fn record_update(
    slug: &str,
    items: &[ItemRef],
    message: &str,
    add: &[ItemRef],
) -> Result<Vec<ItemRef>> {
    if !message.trim().is_empty() {
        let labels: Vec<String> = items.iter().map(ItemRef::label).collect();
        store::append_journal(slug, &format!("Claude: {}", labels.join(", ")), message)?;
    }
    let mut added = Vec::new();
    for item in add {
        if add_item(slug, item.clone())? {
            added.push(item.clone());
        }
    }
    reindex_project(slug);
    store::commit(&format!("Update {slug} from Claude"));
    Ok(added)
}

/// The branch checked out in `dir` when `dir` is in a linked worktree, stored against the
/// repository's main checkout so that it outlives the worktree. None for a main checkout, a
/// detached HEAD, or a directory outside git.
pub fn worktree_branch(dir: &Path) -> Option<ItemRef> {
    let here = forge::git::repo_root(dir).ok()?;
    let here = here.canonicalize().unwrap_or(here);
    let trees = forge::git::worktrees(&here).ok()?;
    let (main, linked) = trees.split_first()?;
    let tree = linked
        .iter()
        .find(|w| w.path.canonicalize().unwrap_or_else(|_| w.path.clone()) == here)?;
    let repo = if main.path.join(".git").exists() {
        main.path.clone()
    } else {
        tree.path.clone()
    };
    Some(ItemRef::Branch {
        repo,
        branch: tree.branch.clone()?,
    })
}

/// Adds the branch of the worktree Claude works in at `dir` to the project, unless the
/// project is done or already holds that branch. Returns the branch when it was added.
pub fn track_worktree(slug: &str, dir: &Path) -> Result<Option<ItemRef>> {
    let project = store::load(slug)?;
    if project.state == State::Done {
        return Ok(None);
    }
    let Some(branch) = worktree_branch(dir) else {
        return Ok(None);
    };
    if project.refs().any(|item| same_item(item, &branch)) {
        return Ok(None);
    }
    add_item(slug, branch.clone())?;
    store::commit(&format!("Add Claude's worktree branch to {slug}"));
    Ok(Some(branch))
}

/// Items related to the project's items that are neither in the project nor turned down.
pub fn suggestions(
    project: &Project,
    items: &BTreeMap<String, ItemState>,
) -> Vec<(ItemRef, String)> {
    let mut found: Vec<(ItemRef, String)> = Vec::new();
    for item in project.refs() {
        let Some(state) = items.get(&item.key()) else {
            continue;
        };
        for related in &state.related {
            let key = related.key();
            if project.has(related)
                || project.dismissed.contains(&key)
                || found.iter().any(|(r, _)| r.key() == key)
            {
                continue;
            }
            found.push((related.clone(), item.label()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Tone;
    use crate::store::testing::TempHome;

    fn project(slug: &str, state: State, items: &[ItemRef]) -> Project {
        Project {
            name: slug.into(),
            slug: slug.into(),
            state,
            created: Utc::now(),
            session: slug.into(),
            dir: None,
            next: String::new(),
            handoff: None,
            wait: None,
            claude_sessions: Vec::new(),
            dismissed: Vec::new(),
            layout: Vec::new(),
            items: items
                .iter()
                .map(|item| Entry {
                    added: Utc::now(),
                    item: item.clone(),
                })
                .collect(),
        }
    }

    fn pr(id: u64) -> ItemRef {
        ItemRef::AdoPr { id }
    }

    fn wi(id: u64) -> ItemRef {
        ItemRef::WorkItem { id }
    }

    fn linked(pairs: &[(ItemRef, Vec<ItemRef>)]) -> BTreeMap<String, ItemState> {
        pairs
            .iter()
            .map(|(item, related)| {
                let mut state = ItemState::new("title", "Active", Tone::Neutral);
                state.related = related.clone();
                (item.key(), state)
            })
            .collect()
    }

    #[test]
    fn projects_that_list_the_item_win_and_done_projects_are_skipped() {
        let projects = [
            project("one", State::Live, &[pr(1)]),
            project("two", State::Frozen, &[wi(5), pr(1)]),
            project("old", State::Done, &[pr(1)]),
            project("linked", State::Live, &[wi(9)]),
        ];
        let cache = linked(&[(wi(9), vec![pr(1)])]);
        assert_eq!(
            projects_for(&projects, &[pr(1)], &cache, Some("linked")),
            Some((Found::Listed, vec!["one".into(), "two".into()]))
        );
        assert_eq!(
            projects_for(&projects[2..3], &[pr(1)], &cache, Some("old")),
            None
        );
    }

    #[test]
    fn an_item_linked_to_a_project_item_finds_that_project() {
        let branch = ItemRef::Branch {
            repo: "/src/app".into(),
            branch: "feature/x".into(),
        };
        let projects = [
            project("by-branch", State::Live, &[branch.clone()]),
            project("by-pr", State::Live, &[pr(8)]),
            project("by-ticket", State::Frozen, &[wi(11)]),
        ];
        let cache = linked(&[
            (branch, vec![pr(7)]),
            (pr(8), vec![wi(9)]),
            (pr(10), vec![wi(11)]),
        ]);
        let found = |item| projects_for(&projects, &[item], &cache, None);
        assert_eq!(
            found(pr(7)),
            Some((Found::Linked, vec!["by-branch".into()]))
        );
        assert_eq!(found(wi(9)), Some((Found::Linked, vec!["by-pr".into()])));
        assert_eq!(
            found(pr(10)),
            Some((Found::Linked, vec!["by-ticket".into()]))
        );
        assert_eq!(found(pr(12)), None);
    }

    #[test]
    fn an_unknown_item_goes_to_the_current_project_unless_it_is_done() {
        let projects = [
            project("here", State::Live, &[pr(1)]),
            project("old", State::Done, &[]),
        ];
        let cache = BTreeMap::new();
        assert_eq!(
            projects_for(&projects, &[pr(2)], &cache, Some("here")),
            Some((Found::Current, vec!["here".into()]))
        );
        assert_eq!(projects_for(&projects, &[pr(2)], &cache, Some("old")), None);
        assert_eq!(projects_for(&projects, &[pr(2)], &cache, None), None);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repository at `<root>/main` with one commit, and a worktree at `<root>/tree` on a new
    /// branch `feature/x`. The paths are canonical, as git reports them.
    fn repo_with_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("ctx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("main")).unwrap();
        let root = root.canonicalize().unwrap();
        let (main, tree) = (root.join("main"), root.join("tree"));
        git_in(&main, &["init", "-q", "-b", "main"]);
        let commit = ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"];
        git_in(
            &main,
            &[&commit[..], &["--allow-empty", "-m", "one"]].concat(),
        );
        let path = tree.to_string_lossy();
        git_in(&main, &["worktree", "add", "-q", "-b", "feature/x", &path]);
        (root, main, tree)
    }

    fn branch(repo: &Path, name: &str) -> ItemRef {
        ItemRef::Branch {
            repo: repo.to_path_buf(),
            branch: name.into(),
        }
    }

    #[test]
    fn a_branch_in_a_worktree_matches_the_branch_in_the_main_checkout() {
        let (root, main, tree) = repo_with_worktree("worktree");
        let projects = [project("app", State::Live, &[branch(&main, "feature/x")])];

        let found = projects_for(
            &projects,
            &[branch(&tree, "feature/x")],
            &BTreeMap::new(),
            None,
        );
        assert_eq!(found, Some((Found::Listed, vec!["app".into()])));
        assert!(!same_item(
            &branch(&main, "feature/x"),
            &branch(&tree, "feature/y")
        ));
        assert!(!same_item(
            &branch(&main, "feature/x"),
            &branch(Path::new("/nonexistent"), "feature/x")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_linked_worktree_gives_its_branch_in_the_main_checkout() {
        let (root, main, tree) = repo_with_worktree("worktree-branch");
        assert_eq!(worktree_branch(&tree), Some(branch(&main, "feature/x")));
        std::fs::create_dir_all(tree.join("src")).unwrap();
        assert_eq!(
            worktree_branch(&tree.join("src")),
            Some(branch(&main, "feature/x"))
        );
        assert_eq!(worktree_branch(&main), None);
        let detached = root.join("detached");
        let path = detached.to_string_lossy();
        git_in(&main, &["worktree", "add", "-q", "--detach", &path]);
        assert_eq!(worktree_branch(&detached), None);
        assert_eq!(worktree_branch(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_worktree_branch_is_added_once_and_not_to_a_done_project() {
        let _home = TempHome::new("track-worktree");
        let (root, main, tree) = repo_with_worktree("track-worktree");
        let slug = store::create("Tables", None).unwrap().slug;

        assert_eq!(
            track_worktree(&slug, &tree).unwrap(),
            Some(branch(&main, "feature/x"))
        );
        assert_eq!(track_worktree(&slug, &tree).unwrap(), None);
        assert_eq!(track_worktree(&slug, &main).unwrap(), None);
        let items: Vec<ItemRef> = store::load(&slug).unwrap().refs().cloned().collect();
        assert_eq!(items, [branch(&main, "feature/x")]);

        let listed = store::create("Listed", None).unwrap().slug;
        add_item(&listed, branch(&tree, "feature/x")).unwrap();
        assert_eq!(track_worktree(&listed, &tree).unwrap(), None);

        let done = store::create("Old", None).unwrap().slug;
        store::update(&done, |p| p.state = State::Done).unwrap();
        assert_eq!(track_worktree(&done, &tree).unwrap(), None);
        assert_eq!(store::load(&done).unwrap().refs().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_update_is_journaled_and_adds_each_item_once() {
        let _home = TempHome::new("update");
        let slug = store::create("Tables", None).unwrap().slug;

        let added = record_update(&slug, &[pr(1), wi(4)], "Fixed the tests.\n", &[pr(2)]).unwrap();
        assert_eq!(added, [pr(2)]);
        let added = record_update(&slug, &[pr(1)], "", &[pr(2), wi(3)]).unwrap();
        assert_eq!(added, [wi(3)]);

        let entries = store::journal_entries(&store::journal(&slug));
        let claude: Vec<_> = entries
            .iter()
            .filter(|(h, _)| h.contains("· Claude: "))
            .collect();
        assert_eq!(claude.len(), 1, "{entries:?}");
        assert!(
            claude[0].0.ends_with("· Claude: PR 1, #4"),
            "{}",
            claude[0].0
        );
        assert_eq!(claude[0].1, "Fixed the tests.");
        let items: Vec<ItemRef> = store::load(&slug).unwrap().refs().cloned().collect();
        assert_eq!(items, [pr(2), wi(3)]);
    }
}
