use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};

use crate::exec;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = exec::run_with(Command::new("git").arg("-C").arg(dir).args(args), None)?;
    Ok(out.trim_end().to_string())
}

/// The git directory that every worktree of `dir`'s repository shares.
pub fn common_dir(dir: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    Ok(path.canonicalize().unwrap_or(path))
}

/// The top of the working tree that contains `dir`.
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?))
}

pub fn current_branch(dir: &Path) -> Result<String> {
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        bail!("{} has a detached HEAD", dir.display());
    }
    Ok(branch)
}

pub fn remote_url(dir: &Path) -> Result<String> {
    git(dir, &["remote", "get-url", "origin"])
}

/// Where a repository is hosted, read from its origin URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    /// Azure DevOps, with the repository name.
    Ado {
        repo: String,
    },
    /// GitLab, with the project path such as `group/parser`.
    GitLab {
        host: String,
        project: String,
    },
    Other,
}

pub fn parse_remote(url: &str) -> Remote {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    if url.contains("dev.azure.com") || url.contains("visualstudio.com") {
        let repo = url.rsplit('/').next().unwrap_or_default().to_string();
        return Remote::Ado { repo };
    }
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, project)) = rest.split_once(':') {
            return Remote::GitLab {
                host: host.to_string(),
                project: project.to_string(),
            };
        }
    }
    for scheme in ["https://", "http://", "ssh://git@"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            if let Some((host, project)) = rest.split_once('/') {
                let host = host.split(':').next().unwrap_or(host);
                return Remote::GitLab {
                    host: host.to_string(),
                    project: project.to_string(),
                };
            }
        }
    }
    Remote::Other
}

/// The state of a local branch, compared against its upstream when it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchState {
    pub head: String,
    pub subject: String,
    pub committed: DateTime<Utc>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: bool,
    pub current: bool,
}

pub fn branch_state(dir: &Path, branch: &str) -> Result<BranchState> {
    let log = git(
        dir,
        &["log", "-1", "--format=%H%x1f%cI%x1f%s", branch, "--"],
    )?;
    let mut parts = log.splitn(3, '\u{1f}');
    let head = parts.next().unwrap_or_default().to_string();
    let committed = parts
        .next()
        .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let subject = parts.next().unwrap_or_default().to_string();
    let upstream = format!("{branch}@{{upstream}}");
    let (ahead, behind) = git(
        dir,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{branch}...{upstream}"),
        ],
    )
    .ok()
    .and_then(|counts| {
        let mut n = counts.split_whitespace().map(|c| c.parse::<u32>().ok());
        Some((n.next()??, n.next()??))
    })
    .unwrap_or((0, 0));
    let current = current_branch(dir).is_ok_and(|b| b == branch);
    let dirty = current && !git(dir, &["status", "--porcelain"])?.is_empty();
    Ok(BranchState {
        head,
        subject,
        committed,
        ahead,
        behind,
        dirty,
        current,
    })
}

/// Commit subjects in `to` that are not in `from`, newest first.
pub fn commits_between(dir: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let out = git(dir, &["log", "--format=%s", &format!("{from}..{to}"), "--"])?;
    Ok(out.lines().map(str::to_string).collect())
}

/// A working tree of a repository and the branch checked out in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Every working tree of the repository that contains `dir`, the main one first.
pub fn worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    Ok(parse_worktrees(&git(
        dir,
        &["worktree", "list", "--porcelain"],
    )?))
}

fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    porcelain
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block.lines();
            let path = lines.next()?.strip_prefix("worktree ")?;
            let branch = lines
                .filter_map(|l| l.strip_prefix("branch "))
                .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string())
                .next();
            Some(Worktree {
                path: PathBuf::from(path),
                branch,
            })
        })
        .collect()
}

/// Adds a working tree at `path` with `branch` checked out, after fetching it. Git gives a
/// branch that exists only on `origin` a local branch that tracks it.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    git(repo, &["fetch", "--no-tags", "origin", branch])?;
    git(repo, &["worktree", "add", &path.to_string_lossy(), branch])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_are_recognized_by_host() {
        assert_eq!(
            parse_remote("git@ssh.dev.azure.com:v3/example-org/Project/api-server"),
            Remote::Ado {
                repo: "api-server".into()
            }
        );
        assert_eq!(
            parse_remote("https://example-org@dev.azure.com/example-org/Project/_git/idl"),
            Remote::Ado { repo: "idl".into() }
        );
        assert_eq!(
            parse_remote("git@gitlab.example.com:group/workspace.git"),
            Remote::GitLab {
                host: "gitlab.example.com".into(),
                project: "group/workspace".into()
            }
        );
        assert_eq!(
            parse_remote("https://gitlab.example.com/group/sub/group.git"),
            Remote::GitLab {
                host: "gitlab.example.com".into(),
                project: "group/sub/group".into()
            }
        );
        assert_eq!(parse_remote("/local/path"), Remote::Other);
    }

    #[test]
    fn a_worktree_is_added_for_a_local_branch_or_one_only_on_origin() {
        let root = std::env::temp_dir().join(format!("forge-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (origin, clone) = (root.join("origin"), root.join("clone"));
        std::fs::create_dir_all(&origin).unwrap();
        let run = |dir: &Path, args: &[&str]| git(dir, args).unwrap();
        run(&origin, &["init", "-q", "-b", "main"]);
        let commit = ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q"];
        run(
            &origin,
            &[&commit[..], &["--allow-empty", "-m", "one"]].concat(),
        );
        run(&origin, &["branch", "feature/local"]);
        let (o, c) = (origin.to_string_lossy(), clone.to_string_lossy());
        run(&root, &["clone", "-q", &o, &c]);
        run(&clone, &["branch", "feature/local", "origin/feature/local"]);
        run(&origin, &["branch", "feature/remote"]);

        add_worktree(&clone, &root.join("local"), "feature/local").unwrap();
        add_worktree(&clone, &root.join("remote"), "feature/remote").unwrap();

        let branches: Vec<Option<String>> = worktrees(&clone)
            .unwrap()
            .into_iter()
            .map(|w| w.branch)
            .collect();
        assert_eq!(
            branches,
            [
                Some("main".into()),
                Some("feature/local".into()),
                Some("feature/remote".into())
            ]
        );
        assert_eq!(
            run(
                &root.join("remote"),
                &["rev-parse", "--abbrev-ref", "@{upstream}"]
            ),
            "origin/feature/remote"
        );
        let shared = common_dir(&clone).unwrap();
        assert_eq!(common_dir(&root.join("local")).unwrap(), shared);
        assert_eq!(common_dir(&root.join("remote")).unwrap(), shared);
        assert_ne!(common_dir(&origin).unwrap(), shared);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worktrees_are_read_from_porcelain_output() {
        let porcelain = "worktree /src/app\nHEAD 1111\nbranch refs/heads/Feature/autofs\n\n\
                         worktree /src/app/.claude/worktrees/x\nHEAD 2222\nbranch refs/heads/Feature/649887-appctl\n\n\
                         worktree /tmp/review\nHEAD 3333\ndetached\n";
        let tree = |path: &str, branch: Option<&str>| Worktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
        };
        assert_eq!(
            parse_worktrees(porcelain),
            [
                tree("/src/app", Some("Feature/autofs")),
                tree(
                    "/src/app/.claude/worktrees/x",
                    Some("Feature/649887-appctl")
                ),
                tree("/tmp/review", None),
            ]
        );
    }
}
