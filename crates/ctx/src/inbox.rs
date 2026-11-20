//! Your open PRs, MRs, Bugs and PBIs, found on the servers, so that those in no project can be
//! filed from the home screen.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use forge::{ado, gitlab};
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::config::Config;
use crate::item::{ItemRef, ItemState};
use crate::refresh;
use crate::store::{self, Project};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default)]
    pub items: Vec<ItemRef>,
    /// One line per query that failed in the last run.
    #[serde(default)]
    pub errors: Vec<String>,
}

pub fn path() -> PathBuf {
    store::home().join("inbox.json")
}

pub fn load() -> Inbox {
    fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(inbox: &Inbox) -> Result<()> {
    store::write_atomic(&path(), &serde_json::to_vec(inbox)?)
}

/// Runs the three queries. When one fails, the error is kept along with what that query found
/// in `previous`, and the other queries still count.
pub fn discover(cfg: &Config, previous: &Inbox) -> Inbox {
    let (prs, mrs, work_items) = std::thread::scope(|s| {
        let prs = s.spawn(|| cfg.ado.as_ref().map_or(Ok(Vec::new()), ado::my_prs));
        let mrs = s.spawn(|| cfg.gitlab.as_ref().map_or(Ok(Vec::new()), gitlab::my_mrs));
        let work_items = s.spawn(|| cfg.ado.as_ref().map_or(Ok(Vec::new()), ado::my_work_items));
        (
            prs.join().unwrap_or_else(|_| panicked()),
            mrs.join().unwrap_or_else(|_| panicked()),
            work_items.join().unwrap_or_else(|_| panicked()),
        )
    });
    let mut inbox = Inbox {
        updated: Some(Utc::now()),
        ..Inbox::default()
    };
    let mut take =
        |found: Result<Vec<ItemRef>>, label: &str, kind: fn(&ItemRef) -> bool| match found {
            Ok(items) => inbox.items.extend(items),
            Err(e) => {
                inbox.errors.push(format!("{label}: {e:#}"));
                inbox
                    .items
                    .extend(previous.items.iter().filter(|i| kind(i)).cloned());
            }
        };
    take(
        prs.map(|ids| ids.into_iter().map(|id| ItemRef::AdoPr { id }).collect()),
        "ADO PRs",
        |i| matches!(i, ItemRef::AdoPr { .. }),
    );
    take(
        mrs.map(|mrs| {
            mrs.iter()
                .map(|mr| ItemRef::GitlabMr {
                    project: gitlab::project_path(mr),
                    iid: mr.iid,
                })
                .collect()
        }),
        "GitLab MRs",
        |i| matches!(i, ItemRef::GitlabMr { .. }),
    );
    take(
        work_items.map(|ids| ids.into_iter().map(|id| ItemRef::WorkItem { id }).collect()),
        "ADO work items",
        |i| matches!(i, ItemRef::WorkItem { .. }),
    );
    inbox
}

fn panicked<T>() -> Result<T> {
    Err(anyhow::anyhow!("the query panicked"))
}

/// Finds your items, saves them, and fetches the states of the unfiled ones older than
/// `max_age_secs`.
pub fn refresh(cfg: &Config, max_age_secs: i64) {
    let inbox = discover(cfg, &load());
    let _ = save(&inbox);
    let held = held(&store::list());
    let wanted: Vec<ItemRef> = inbox
        .items
        .into_iter()
        .filter(|i| !held.contains(&i.key()))
        .collect();
    let stale = refresh::stale(wanted, &cache::load(), max_age_secs);
    if !stale.is_empty() {
        refresh::fetch(cfg, &stale, &|_| {});
    }
}

fn held(projects: &[Project]) -> HashSet<String> {
    projects
        .iter()
        .flat_map(|p| p.refs().map(ItemRef::key))
        .collect()
}

/// Your items that no project holds and that are still open, PRs and MRs before work items.
pub fn unfiled(
    inbox: &Inbox,
    projects: &[Project],
    states: &BTreeMap<String, ItemState>,
) -> Vec<ItemRef> {
    let held = held(projects);
    let mut items: Vec<ItemRef> = inbox
        .items
        .iter()
        .filter(|i| !held.contains(&i.key()))
        .filter(|i| states.get(&i.key()).is_none_or(|s| s.open))
        .cloned()
        .collect();
    items.sort_by_key(|i| match i {
        ItemRef::AdoPr { .. } => 0,
        ItemRef::GitlabMr { .. } => 1,
        _ => 2,
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entry, State};

    fn state(open: bool) -> ItemState {
        serde_json::from_value(serde_json::json!({
            "title": "t", "status": "s", "open": open, "fetched": Utc::now()
        }))
        .unwrap()
    }

    #[test]
    fn unfiled_leaves_out_items_in_any_project_and_closed_ones() {
        let wi = |id| ItemRef::WorkItem { id };
        let pr = |id| ItemRef::AdoPr { id };
        let inbox = Inbox {
            updated: None,
            items: vec![wi(1), wi(2), pr(3), pr(4), wi(5)],
            errors: Vec::new(),
        };
        let mut done: Project = toml::from_str(
            "name = 'old'\nstate = 'done'\ncreated = '2026-01-01T00:00:00Z'\nsession = 'old'",
        )
        .unwrap();
        assert_eq!(done.state, State::Done);
        done.items.push(Entry {
            added: Utc::now(),
            item: wi(2),
        });
        let states: BTreeMap<String, ItemState> =
            [(pr(4).key(), state(false)), (wi(5).key(), state(true))].into();
        assert_eq!(
            unfiled(&inbox, &[done], &states),
            [pr(3), wi(1), wi(5)],
            "PRs first; #2 is in a done project; PR 4 is closed"
        );
    }
}
