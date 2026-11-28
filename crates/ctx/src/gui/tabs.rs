//! The parts of a project's tabs that need no window: their names, the layout they come back
//! from and are saved to, and terminal IDs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ctx_core::store::{self, Project, Terminal, Window};

/// The name of the tmux window that ran ctx itself, which the GUI does not restore.
const TMUX_CTX_WINDOW: &str = "ctx";

/// The layout string of a window saved by the GUI. tmux applied a window's layout string only
/// to windows with several terminals, so ctx 0.3.0 restores these windows too.
const GUI_LAYOUT: &str = "gui";

/// A tab's automatic name: its program and directory, such as `zsh · rvt`.
pub fn auto_name(program: &str, dir: Option<&Path>, home: Option<&Path>) -> String {
    let place = match dir {
        Some(d) if Some(d) == home => Some("~".to_string()),
        Some(d) => d
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| Some(d.to_string_lossy().into_owned())),
        None => None,
    };
    match place {
        Some(place) => format!("{program} · {place}"),
        None => program.to_string(),
    }
}

/// The name the user typed for a tab, or `None` to go back to the automatic name.
pub fn typed_name(text: &str) -> Option<String> {
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Where a project's new terminals start: its directory, or the home directory.
pub fn start_dir(project: &Project) -> PathBuf {
    project
        .dir
        .clone()
        .filter(|d| d.is_dir())
        .or_else(dirs_home)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// A terminal tab to open when a workspace opens.
#[derive(Debug, Clone, PartialEq)]
pub struct Restored {
    pub dir: PathBuf,
    /// The name the user set, if any.
    pub name: Option<String>,
}

/// The tabs a project's workspace opens with: one per saved window, in the directory of the
/// window's first terminal, or one shell in `fallback` when nothing is saved. A tmux window's
/// other terminals are not restored.
pub fn restore(layout: &[Window], fallback: &Path) -> Vec<Restored> {
    let tabs: Vec<Restored> = layout
        .iter()
        .filter(|w| w.name != TMUX_CTX_WINDOW)
        .filter_map(|w| {
            let first = w.terminals.first()?;
            Some(Restored {
                dir: if first.path.is_dir() {
                    first.path.clone()
                } else {
                    fallback.to_path_buf()
                },
                name: (!w.auto_name).then(|| w.name.clone()),
            })
        })
        .collect();
    if tabs.is_empty() {
        vec![Restored {
            dir: fallback.to_path_buf(),
            name: None,
        }]
    } else {
        tabs
    }
}

/// A terminal tab as the layout keeps it.
pub struct Saved {
    pub shown: String,
    pub set: Option<String>,
    pub dir: PathBuf,
}

/// The layout to save for a workspace's terminal tabs. A tab keeps its name only when the user
/// set one.
pub fn save(tabs: &[Saved]) -> Vec<Window> {
    tabs.iter()
        .map(|t| Window {
            name: t.set.clone().unwrap_or_else(|| t.shown.clone()),
            auto_name: t.set.is_none(),
            layout: GUI_LAYOUT.into(),
            terminals: vec![Terminal {
                path: t.dir.clone(),
            }],
        })
        .collect()
}

/// The file that holds the last terminal number handed out.
pub fn counter_path() -> PathBuf {
    store::home().join("terminal-id")
}

/// A new terminal ID, `gui:<n>`, counting up from the number saved in `path` so IDs stay unique
/// across restarts.
pub fn next_id(path: &Path) -> Result<String> {
    let last: u64 = fs::read_to_string(path)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    let next = last + 1;
    store::write_atomic(path, format!("{next}\n").as_bytes())?;
    Ok(format!("gui:{next}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(name: &str, auto: bool, dirs: &[&Path]) -> Window {
        Window {
            name: name.into(),
            auto_name: auto,
            layout: "b25d,200x50,0,0,1".into(),
            terminals: dirs
                .iter()
                .map(|d| Terminal {
                    path: d.to_path_buf(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_tab_is_named_after_its_program_and_directory() {
        let home = Path::new("/Users/me");
        assert_eq!(
            auto_name("zsh", Some(Path::new("/Users/me/src/rvt")), Some(home)),
            "zsh · rvt"
        );
        assert_eq!(auto_name("vim", Some(home), Some(home)), "vim · ~");
        assert_eq!(auto_name("htop", None, Some(home)), "htop");
        assert_eq!(
            auto_name("zsh", Some(Path::new("/")), Some(home)),
            "zsh · /"
        );
    }

    #[test]
    fn an_empty_name_goes_back_to_the_automatic_one() {
        assert_eq!(typed_name("  build  "), Some("build".into()));
        assert_eq!(typed_name("   "), None);
    }

    #[test]
    fn a_tmux_layout_opens_one_tab_per_window_in_its_first_directory() {
        let tmp = std::env::temp_dir();
        let gone = Path::new("/no/such/dir");
        let layout = vec![
            window("ctx", false, &[&tmp]),
            window("zsh", true, &[&tmp, gone]),
            window("build", false, &[gone]),
            window("empty", false, &[]),
        ];
        let fallback = Path::new("/fallback");
        assert_eq!(
            restore(&layout, fallback),
            vec![
                Restored {
                    dir: tmp.clone(),
                    name: None
                },
                Restored {
                    dir: fallback.into(),
                    name: Some("build".into())
                },
            ]
        );
        assert_eq!(
            restore(&[], fallback),
            vec![Restored {
                dir: fallback.into(),
                name: None
            }]
        );
    }

    #[test]
    fn a_saved_layout_keeps_set_names_and_comes_back() {
        let tmp = std::env::temp_dir();
        let saved = save(&[
            Saved {
                shown: "zsh · tmp".into(),
                set: None,
                dir: tmp.clone(),
            },
            Saved {
                shown: "zsh · tmp".into(),
                set: Some("server".into()),
                dir: tmp.clone(),
            },
        ]);
        assert!(saved[0].auto_name);
        assert_eq!(saved[1].name, "server");
        assert_eq!(saved[1].terminals.len(), 1);
        let back = restore(&saved, Path::new("/"));
        assert_eq!(back[0].name, None);
        assert_eq!(back[1].name.as_deref(), Some("server"));
    }

    #[test]
    fn terminal_ids_count_up_from_the_saved_number() {
        let dir = std::env::temp_dir().join(format!("ctx-tabs-{}", std::process::id()));
        let path = dir.join("terminal-id");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(next_id(&path).unwrap(), "gui:1");
        assert_eq!(next_id(&path).unwrap(), "gui:2");
        fs::write(&path, "41\n").unwrap();
        assert_eq!(next_id(&path).unwrap(), "gui:42");
        let _ = fs::remove_dir_all(&dir);
    }
}
