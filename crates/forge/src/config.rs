use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AdoConfig {
    pub organization: String,
    pub project: String,
    pub area_root: String,
    #[serde(default = "default_lookback")]
    pub lookback_days: u32,
    /// The work item types listed as yours. The defaults match the Scrum process.
    #[serde(default = "default_work_item_types")]
    pub work_item_types: Vec<String>,
    /// The states in which a work item is no longer yours to do.
    #[serde(default = "default_done_states")]
    pub done_states: Vec<String>,
    /// The SSH host that repositories are cloned from.
    #[serde(default = "default_ssh_host")]
    pub ssh_host: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitLabConfig {
    pub host: String,
    pub group: String,
}

fn default_lookback() -> u32 {
    365
}

fn default_work_item_types() -> Vec<String> {
    vec!["Bug".into(), "Product Backlog Item".into()]
}

fn default_done_states() -> Vec<String> {
    vec!["Done".into(), "Removed".into(), "Closed".into()]
}

fn default_ssh_host() -> String {
    "ssh.dev.azure.com".into()
}

pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

/// An optional directory setting: `None` when it is unset or blank.
pub fn optional_path(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    (!path.is_empty()).then(|| expand_home(path))
}

/// The `[name]` section, or an error that says where to add it.
pub fn require<'a, T>(section: &'a Option<T>, name: &str, file: &Path) -> Result<&'a T> {
    section
        .as_ref()
        .ok_or_else(|| anyhow!("no [{name}] section in {}", file.display()))
}

/// Fails when a config names neither forge, so a fresh template stops with a clear message.
pub fn require_a_forge(ado: bool, gitlab: bool, file: &Path) -> Result<()> {
    if ado || gitlab {
        return Ok(());
    }
    Err(anyhow!(
        "{} has no [ado] or [gitlab] section. Fill in at least one of them.",
        file.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ado_defaults_cover_the_scrum_process_and_azure_devops_services() {
        let ado: AdoConfig =
            serde_json::from_str(r#"{"organization": "o", "project": "p", "area_root": "r"}"#)
                .unwrap();
        assert_eq!(ado.work_item_types, ["Bug", "Product Backlog Item"]);
        assert_eq!(ado.done_states, ["Done", "Removed", "Closed"]);
        assert_eq!(ado.ssh_host, "ssh.dev.azure.com");
        assert_eq!(ado.lookback_days, 365);
    }

    #[test]
    fn a_missing_section_names_itself_and_the_file() {
        let none: Option<GitLabConfig> = None;
        let err = require(&none, "gitlab", Path::new("/c/config.toml")).unwrap_err();
        assert_eq!(err.to_string(), "no [gitlab] section in /c/config.toml");
    }

    #[test]
    fn a_config_needs_at_least_one_forge() {
        let file = Path::new("/c/config.toml");
        assert!(require_a_forge(true, false, file).is_ok());
        assert!(require_a_forge(false, true, file).is_ok());
        let err = require_a_forge(false, false, file).unwrap_err();
        assert!(
            err.to_string().starts_with("/c/config.toml has no"),
            "{err}"
        );
    }

    #[test]
    fn blank_paths_are_unset() {
        assert_eq!(optional_path(""), None);
        assert_eq!(optional_path("  "), None);
        assert_eq!(optional_path("/a/b"), Some(PathBuf::from("/a/b")));
    }
}
