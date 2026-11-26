//! What the app shows of the store: projects, their items, journals and Claude sessions, read
//! through ctx-core on a background thread. The app reloads it when a file in the store changes.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

use chrono::NaiveDateTime;
use ctx_core::cache::{self, Cache};
use ctx_core::config::Config;
use ctx_core::inbox;
use ctx_core::item::{ItemRef, ItemState};
use ctx_core::live::{self, LiveSession};
use ctx_core::ops::{self, Since};
use ctx_core::order::{self, Group};
use ctx_core::store::{self, ClaudeSession, Project, State};

pub struct JournalEntry {
    pub time: Option<NaiveDateTime>,
    pub heading: String,
    pub body: String,
}

#[derive(Default)]
pub struct Snapshot {
    pub projects: Vec<Project>,
    pub order: Vec<String>,
    pub cache: Cache,
    /// Your open items that no project holds.
    pub unfiled: Vec<ItemRef>,
    pub since: HashMap<String, Since>,
    /// Each project's journal, newest last.
    pub journals: HashMap<String, Vec<JournalEntry>>,
    /// Each project's Claude sessions that run now.
    pub claude: HashMap<String, Vec<(ClaudeSession, LiveSession)>>,
}

impl Snapshot {
    pub fn load(cfg: Option<&Config>) -> Snapshot {
        let projects = store::list();
        let cache = cache::load();
        let unfiled = inbox::unfiled(&inbox::load(), &projects, &cache.items);
        let running = live::sessions(&ops::claude_config_dirs(
            cfg.and_then(|c| c.claude.config_path()),
            &projects,
        ));
        let mut since = HashMap::new();
        let mut journals = HashMap::new();
        let mut claude = HashMap::new();
        for p in projects.iter().filter(|p| p.state != State::Done) {
            let notes = store::notes(&p.slug);
            since.insert(p.slug.clone(), ops::since(p, &cache.items, &notes));
            journals.insert(p.slug.clone(), journal(&store::journal(&p.slug)));
            let live: Vec<_> = p
                .claude_sessions
                .iter()
                .filter_map(|s| running.get(&s.id).map(|l| (s.clone(), l.clone())))
                .collect();
            claude.insert(p.slug.clone(), live);
        }
        Snapshot {
            order: order::load(),
            projects,
            cache,
            unfiled,
            since,
            journals,
            claude,
        }
    }

    pub fn project(&self, slug: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.slug == slug)
    }

    pub fn groups(&self) -> Vec<(Group, Vec<&Project>)> {
        order::groups(&self.projects, &self.order)
    }

    pub fn state(&self, item: &ItemRef) -> Option<&ItemState> {
        self.cache.items.get(&item.key())
    }
}

/// Journal entries from the text of `journal.md`, whose headings start with the local time.
pub fn journal(text: &str) -> Vec<JournalEntry> {
    store::journal_entries(text)
        .into_iter()
        .map(|(heading, body)| {
            let (time, heading) = match heading.split_once(" · ") {
                Some((when, rest)) => match NaiveDateTime::parse_from_str(when, "%Y-%m-%d %H:%M") {
                    Ok(t) => (Some(t), rest.to_string()),
                    Err(_) => (None, heading),
                },
                None => (None, heading),
            };
            JournalEntry {
                time,
                heading,
                body,
            }
        })
        .collect()
}

/// A number that changes when a file the app shows changes: the project files, journals,
/// marks, events and notes, the item cache, the inbox and the order.
pub fn fingerprint() -> u64 {
    let mut hasher = DefaultHasher::new();
    let home = store::home();
    for name in ["cache.json", "inbox.json", "order"] {
        stamp(&home.join(name), &mut hasher);
    }
    let mut dirs: Vec<_> = fs::read_dir(store::projects_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        dir.hash(&mut hasher);
        for name in [
            "project.toml",
            "journal.md",
            "snapshot.json",
            "visit.json",
            "events.jsonl",
            "notes",
        ] {
            stamp(&dir.join(name), &mut hasher);
        }
    }
    hasher.finish()
}

fn stamp(path: &Path, hasher: &mut DefaultHasher) {
    if let Ok(meta) = fs::metadata(path) {
        meta.len().hash(hasher);
        meta.modified().ok().hash(hasher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_headings_split_into_time_and_text() {
        let text = "## 2026-11-19 16:04 · created\n\n## 2026-11-19 16:05 · frozen\n\n\
                    **Doing:** a meeting\n\n## odd heading\n";
        let entries = journal(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[1].time.unwrap().format("%H:%M").to_string(),
            "16:05"
        );
        assert_eq!(entries[1].heading, "frozen");
        assert_eq!(entries[1].body, "**Doing:** a meeting");
        assert!(entries[2].time.is_none());
        assert_eq!(entries[2].heading, "odd heading");
    }
}
