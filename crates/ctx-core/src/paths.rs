//! Paths as ctx shows them.

use std::path::{Path, PathBuf};

/// The path with the home directory written as `~`.
pub fn tilde(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
