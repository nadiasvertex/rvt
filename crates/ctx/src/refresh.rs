use std::collections::BTreeSet;
use std::thread;

use crate::cache::{self, Cache};
use crate::config::Config;
use crate::item::{self, ItemRef, ItemState};
use crate::search;
use crate::store::{self, Project, State};

const PARALLEL: usize = 6;

/// Fetches each item, merges every result into the cache as it arrives, and reindexes the
/// projects that hold the items. Related items that were never fetched are fetched too, once,
/// so suggestions show their titles. `done` hears about each key as its result is stored.
pub fn fetch(cfg: &Config, items: &[ItemRef], done: &(dyn Fn(&str) + Sync)) {
    fetch_wave(cfg, items, done);
    let cache = cache::load();
    let keys: BTreeSet<String> = items.iter().map(ItemRef::key).collect();
    let mut seen = BTreeSet::new();
    let related: Vec<ItemRef> = items
        .iter()
        .filter_map(|i| cache.items.get(&i.key()))
        .flat_map(|s| s.related.iter().cloned())
        .filter(|r| {
            let key = r.key();
            !keys.contains(&key)
                && !cache.items.contains_key(&key)
                && !cache.errors.contains_key(&key)
                && seen.insert(key)
        })
        .collect();
    if !related.is_empty() {
        fetch_wave(cfg, &related, &|_| {});
    }
    let cache = cache::load();
    for project in store::list() {
        if project.refs().any(|r| keys.contains(&r.key())) {
            let _ = search::index_project(&project, &cache.items);
        }
    }
}

fn fetch_wave(cfg: &Config, items: &[ItemRef], done: &(dyn Fn(&str) + Sync)) {
    let previous = cache::load();
    for wave in items.chunks(PARALLEL) {
        thread::scope(|s| {
            for item in wave {
                let prev = previous.items.get(&item.key());
                s.spawn(move || {
                    let result = item::fetch(cfg, item, prev).map_err(|e| format!("{e:#}"));
                    let key = item.key();
                    let _ = cache::merge(vec![(key.clone(), result)]);
                    done(&key);
                });
            }
        });
    }
}

/// The items worth fetching: those of projects that are not done, or only of `only`.
pub fn wanted(projects: &[Project], only: Option<&str>) -> Vec<ItemRef> {
    let mut seen = BTreeSet::new();
    projects
        .iter()
        .filter(|p| match only {
            Some(slug) => p.slug == slug,
            None => p.state != State::Done,
        })
        .flat_map(|p| p.refs().cloned())
        .filter(|r| seen.insert(r.key()))
        .collect()
}

/// Items with no cached state, or a state older than `max_age_secs`.
pub fn stale(items: Vec<ItemRef>, cache: &Cache, max_age_secs: i64) -> Vec<ItemRef> {
    let now = chrono::Utc::now();
    items
        .into_iter()
        .filter(|r| {
            cache
                .items
                .get(&r.key())
                .is_none_or(|s: &ItemState| (now - s.fetched).num_seconds() > max_age_secs)
        })
        .collect()
}
