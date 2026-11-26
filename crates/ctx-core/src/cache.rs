use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::item::ItemState;
use crate::store;

/// The latest fetched state of every item, shared by the TUI and the watcher.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default)]
    pub items: BTreeMap<String, ItemState>,
    /// The last fetch error per item, cleared by the next successful fetch.
    #[serde(default)]
    pub errors: BTreeMap<String, String>,
}

pub fn path() -> PathBuf {
    store::home().join("cache.json")
}

pub fn load() -> Cache {
    fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Merges fetch results into the cache on disk. The file is read again first so that results
/// written by another process in the meantime are kept.
pub fn merge(results: Vec<(String, Result<ItemState, String>)>) -> Result<Cache> {
    static MERGING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MERGING.lock().unwrap_or_else(|e| e.into_inner());
    let mut cache = load();
    for (key, result) in results {
        match result {
            Ok(state) => {
                cache.items.insert(key.clone(), state);
                cache.errors.remove(&key);
            }
            Err(e) => {
                cache.errors.insert(key, e);
            }
        }
    }
    cache.updated = Some(Utc::now());
    store::write_atomic(&path(), &serde_json::to_vec(&cache)?)?;
    Ok(cache)
}
