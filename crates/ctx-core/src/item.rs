use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use forge::model::{Comment, Discussion, Status, ThreadStatus, comments_of, short_title};
use forge::{ado, git, gitlab};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A reference to something outside ctx that belongs to a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ItemRef {
    AdoPr { id: u64 },
    WorkItem { id: u64 },
    GitlabMr { project: String, iid: u64 },
    Branch { repo: PathBuf, branch: String },
    File { path: PathBuf },
    Url { url: String, title: String },
}

impl ItemRef {
    /// A stable key for the cache, snapshots and the search index.
    pub fn key(&self) -> String {
        match self {
            ItemRef::AdoPr { id } => format!("pr-{id}"),
            ItemRef::WorkItem { id } => format!("wi-{id}"),
            ItemRef::GitlabMr { project, iid } => format!("mr-{project}!{iid}"),
            ItemRef::Branch { repo, branch } => format!("branch-{}@{branch}", repo.display()),
            ItemRef::File { path } => format!("file-{}", path.display()),
            ItemRef::Url { url, .. } => format!("url-{url}"),
        }
    }

    /// A short name such as `PR 123`, `#456` or `parser!78`.
    pub fn label(&self) -> String {
        match self {
            ItemRef::AdoPr { id } => format!("PR {id}"),
            ItemRef::WorkItem { id } => format!("#{id}"),
            ItemRef::GitlabMr { project, iid } => {
                format!("{}!{iid}", project.rsplit('/').next().unwrap_or(project))
            }
            ItemRef::Branch { branch, .. } => branch.clone(),
            ItemRef::File { path } => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            ItemRef::Url { title, url } => {
                if title.is_empty() {
                    url.clone()
                } else {
                    title.clone()
                }
            }
        }
    }

    /// The label with its kind where the label alone is unclear, such as `work item #456`.
    pub fn describe(&self) -> String {
        match self {
            ItemRef::AdoPr { .. } => self.label(),
            _ => format!("{} {}", self.kind_label(), self.label()),
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            ItemRef::AdoPr { .. } => "PR",
            ItemRef::WorkItem { .. } => "work item",
            ItemRef::GitlabMr { .. } => "MR",
            ItemRef::Branch { .. } => "branch",
            ItemRef::File { .. } => "file",
            ItemRef::Url { .. } => "link",
        }
    }

    /// Items that change on a server and are worth refreshing.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            ItemRef::AdoPr { .. } | ItemRef::WorkItem { .. } | ItemRef::GitlabMr { .. }
        )
    }

    /// Items that only need a local check.
    pub fn is_local(&self) -> bool {
        matches!(self, ItemRef::Branch { .. } | ItemRef::File { .. })
    }
}

/// What `parse` needs to resolve short references such as `!12` or `branch`.
pub struct ParseContext<'a> {
    pub cwd: &'a Path,
    /// `None` when the config has no `[gitlab]` section.
    pub gitlab_host: Option<&'a str>,
    pub gitlab_group: Option<&'a str>,
}

/// Reads an item from a URL, a path, or a short form: `pr 123`, `#456`, `wi 456`,
/// `project!78`, `!78` (in the current GitLab repository), `branch` (the current one) or
/// `branch:name`.
pub fn parse(input: &str, ctx: &ParseContext) -> Result<ItemRef> {
    let input = input.trim();
    if input.is_empty() {
        bail!("nothing to add");
    }
    if let Some(found) = parse_url(input, ctx.gitlab_host) {
        return Ok(found);
    }
    let lower = input.to_lowercase();
    for prefix in ["pr:", "pr ", "pr"] {
        if let Some(id) = lower
            .strip_prefix(prefix)
            .and_then(|r| r.trim().parse().ok())
        {
            return Ok(ItemRef::AdoPr { id });
        }
    }
    for prefix in ["wi:", "wi ", "#"] {
        if let Some(id) = lower
            .strip_prefix(prefix)
            .and_then(|r| r.trim().parse().ok())
        {
            return Ok(ItemRef::WorkItem { id });
        }
    }
    if let Some((project, iid)) = input.rsplit_once('!') {
        if let Ok(iid) = iid.trim().parse::<u64>() {
            let project = project.trim();
            let project = if project.is_empty() {
                current_gitlab_project(ctx.cwd)?
            } else if project.contains('/') {
                project.to_string()
            } else {
                let group = ctx
                    .gitlab_group
                    .context("set group under [gitlab] to use project!number")?;
                format!("{group}/{project}")
            };
            return Ok(ItemRef::GitlabMr { project, iid });
        }
    }
    if lower == "branch" || lower.starts_with("branch:") {
        let repo = git::repo_root(ctx.cwd).context("not inside a git repository")?;
        let branch = match input.split_once(':') {
            Some((_, name)) if !name.trim().is_empty() => name.trim().to_string(),
            _ => git::current_branch(&repo)?,
        };
        return Ok(ItemRef::Branch { repo, branch });
    }
    let path = forge::config::expand_home(input);
    let path = if path.is_relative() {
        ctx.cwd.join(path)
    } else {
        path
    };
    if path.exists() {
        let path = path.canonicalize().unwrap_or(path);
        return Ok(ItemRef::File { path });
    }
    bail!("not a PR, work item, MR, branch, URL or existing path: {input}")
}

fn current_gitlab_project(cwd: &Path) -> Result<String> {
    match git::parse_remote(&git::remote_url(cwd)?) {
        git::Remote::GitLab { project, .. } => Ok(project),
        _ => bail!("the current repository is not on GitLab"),
    }
}

fn parse_url(input: &str, gitlab_host: Option<&str>) -> Option<ItemRef> {
    if !input.starts_with("http://") && !input.starts_with("https://") {
        return None;
    }
    let clean = input
        .split(['?', '#'])
        .next()
        .unwrap_or(input)
        .trim_end_matches('/');
    let last_number = |s: &str| s.rsplit('/').next().and_then(|n| n.parse::<u64>().ok());
    if clean.contains("/pullrequest/") {
        if let Some(id) = last_number(clean) {
            return Some(ItemRef::AdoPr { id });
        }
    }
    if clean.contains("/_workitems/edit/") {
        if let Some(id) = last_number(clean) {
            return Some(ItemRef::WorkItem { id });
        }
    }
    if let Some((before, after)) = clean.split_once("/-/merge_requests/") {
        let iid = after.split('/').next().and_then(|n| n.parse().ok());
        let project = before
            .split_once("://")
            .map(|(_, rest)| rest)
            .and_then(|rest| {
                gitlab_host
                    .and_then(|host| rest.strip_prefix(host))
                    .or_else(|| rest.split_once('/').map(|(_, p)| p))
            })
            .map(|p| p.trim_start_matches('/').to_string());
        if let (Some(iid), Some(project)) = (iid, project) {
            return Some(ItemRef::GitlabMr { project, iid });
        }
    }
    Some(ItemRef::Url {
        url: input.to_string(),
        title: String::new(),
    })
}

/// How an item's status should read at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Good,
    Bad,
    Busy,
    Warn,
    #[default]
    Neutral,
    Muted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ci {
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastComment {
    pub author: String,
    pub date: DateTime<Utc>,
    pub snippet: String,
}

/// The fetched state of an item. Snapshots and visit marks keep copies of it, and the changes
/// "since you left" come from comparing two copies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemState {
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub tone: Tone,
    /// False once merged, closed, abandoned or done.
    pub open: bool,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub ci: Option<Ci>,
    /// An open PR or MR that cannot merge into its target without resolving conflicts.
    #[serde(default)]
    pub conflicts: bool,
    /// Open threads where someone else wrote last, on your own PR or in a thread you wrote in.
    #[serde(default)]
    pub replies_due: usize,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub comments: usize,
    #[serde(default)]
    pub last_comment: Option<LastComment>,
    #[serde(default)]
    pub url: Option<String>,
    /// Description and comment text, for the search index.
    #[serde(default)]
    pub text: String,
    /// The description of a PR, MR or work item, as Markdown.
    #[serde(default)]
    pub description: String,
    /// Comment threads, oldest first, each thread's first comment before its replies.
    #[serde(default)]
    pub conversation: Vec<Comment>,
    /// The login of whoever opened the PR or MR.
    #[serde(default)]
    pub author: String,
    /// Your login on the item's host, once known, so the detail pane can mark your comments.
    #[serde(default)]
    pub viewer: Option<String>,
    /// Items this one points at, offered as suggestions.
    #[serde(default)]
    pub related: Vec<ItemRef>,
    pub fetched: DateTime<Utc>,
}

impl ItemState {
    pub(crate) fn new(title: impl Into<String>, status: impl Into<String>, tone: Tone) -> Self {
        ItemState {
            title: title.into(),
            status: status.into(),
            tone,
            open: true,
            approved: false,
            ci: None,
            conflicts: false,
            replies_due: 0,
            head: None,
            comments: 0,
            last_comment: None,
            url: None,
            text: String::new(),
            description: String::new(),
            conversation: Vec::new(),
            author: String::new(),
            viewer: None,
            related: Vec::new(),
            fetched: Utc::now(),
        }
    }
}

pub fn status_tone(status: Status) -> Tone {
    match status {
        Status::Draft => Tone::Muted,
        Status::Active => Tone::Neutral,
        Status::Approved | Status::Ready => Tone::Good,
        Status::Waiting | Status::Conflicts => Tone::Warn,
        Status::Rejected | Status::CiFailed => Tone::Bad,
        Status::CiRunning => Tone::Busy,
    }
}

/// Fetches the current state of `item`. `previous` lets a fetch skip work, such as loading work
/// item comments when the comment count has not changed.
pub fn fetch(cfg: &Config, item: &ItemRef, previous: Option<&ItemState>) -> Result<ItemState> {
    match item {
        ItemRef::AdoPr { id } => fetch_pr(cfg, *id),
        ItemRef::WorkItem { id } => fetch_work_item(cfg, *id, previous),
        ItemRef::GitlabMr { project, iid } => fetch_mr(cfg, project, *iid),
        ItemRef::Branch { repo, branch } => fetch_branch(cfg, repo, branch),
        ItemRef::File { path } => Ok(fetch_file(path)),
        ItemRef::Url { url, title } => {
            let mut state = ItemState::new(title.clone(), "link", Tone::Muted);
            state.url = Some(url.clone());
            Ok(state)
        }
    }
}

fn apply_discussions(state: &mut ItemState, discussions: &[Discussion]) {
    state.conversation = comments_of(discussions);
    let notes = discussions.iter().flat_map(|d| d.notes.iter());
    state.comments = notes.clone().count();
    state.last_comment = notes.clone().max_by_key(|n| n.date).map(|n| LastComment {
        author: n.author.clone(),
        date: n.date,
        snippet: snippet(&n.body),
    });
    let mut sorted: Vec<_> = notes.collect();
    sorted.sort_by_key(|n| n.date);
    for note in sorted {
        state.text.push_str(&format!(
            "\n\n{} · {}\n{}",
            note.author,
            note.date.format("%Y-%m-%d"),
            note.body.trim()
        ));
    }
}

static ADO_LOGIN: OnceLock<String> = OnceLock::new();
static GITLAB_LOGIN: OnceLock<String> = OnceLock::new();

/// Your login, looked up once it succeeds and kept for the rest of the run.
fn login(
    cell: &'static OnceLock<String>,
    lookup: impl FnOnce() -> Result<String>,
) -> Option<&'static str> {
    if let Some(login) = cell.get() {
        return Some(login);
    }
    let found = lookup().ok()?;
    Some(cell.get_or_init(|| found))
}

fn replies_due(me: Option<&str>, author: &str, discussions: &[Discussion]) -> usize {
    me.map_or(0, |me| {
        discussions
            .iter()
            .filter(|d| d.waits_for_reply(me, author))
            .count()
    })
}

/// The worst result among a PR's build policies. Required builds decide when there are any.
fn build_ci(policies: &[ado::PolicyEvaluation]) -> Option<Ci> {
    ado::counted_builds(policies)
        .into_iter()
        .filter_map(|p| match p.status.as_deref()? {
            "approved" => Some(Ci::Passed),
            "rejected" | "broken" => Some(Ci::Failed),
            "running" => Some(Ci::Running),
            "queued" if p.current_build().is_some() => Some(Ci::Running),
            _ => None,
        })
        .max_by_key(|ci| match ci {
            Ci::Passed => 0,
            Ci::Running => 1,
            Ci::Failed => 2,
        })
}

fn fetch_pr(cfg: &Config, id: u64) -> Result<ItemState> {
    let ado = cfg.ado()?;
    let mut pr = ado::pull_request(ado, id)?;
    let lifecycle = pr.status.clone().unwrap_or_default();
    let conflicts = pr.merge_status.take().as_deref() == Some("conflicts");
    let item = ado::item_from_pr(pr, String::new(), &ado.organization, &ado.project);
    let mut state = ItemState::new(
        short_title(&item.title),
        item.status.label(),
        status_tone(item.status),
    );
    match lifecycle.as_str() {
        "completed" => {
            state.status = "Merged".into();
            state.tone = Tone::Good;
            state.open = false;
        }
        "abandoned" => {
            state.status = "Abandoned".into();
            state.tone = Tone::Muted;
            state.open = false;
        }
        _ => {}
    }
    state.approved = matches!(item.status, Status::Approved | Status::Ready);
    state.head = item.head_sha.clone();
    state.url = Some(item.web_url.clone());
    state.text = item.description.clone();
    state.description = item.description.clone();
    state.author = item.author.clone();
    let discussions = ado::discussions(ado, &item).unwrap_or_default();
    apply_discussions(&mut state, &discussions);
    let me = login(&ADO_LOGIN, ado::current_user);
    state.viewer = me.map(str::to_string);
    if state.open {
        state.conflicts = conflicts;
        state.ci = ado::policies(ado, id).ok().and_then(|p| build_ci(&p));
        state.replies_due = replies_due(me, &item.author, &discussions);
    }
    if let Ok(ids) = ado::pr_work_items(ado, id) {
        state.related = ids.into_iter().map(|id| ItemRef::WorkItem { id }).collect();
    }
    Ok(state)
}

/// A work item's comments as one thread that cannot be resolved, with their HTML made plain.
fn work_item_thread(comments: &[Comment]) -> Vec<Comment> {
    comments
        .iter()
        .enumerate()
        .map(|(n, c)| Comment {
            body: html_to_text(&c.body).trim().to_string(),
            is_reply: n > 0,
            status: ThreadStatus::Plain,
            ..c.clone()
        })
        .collect()
}

fn field_str(item: &ado::WorkItem, name: &str) -> String {
    item.fields
        .get(name)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn work_item_tone(state: &str) -> (Tone, bool) {
    match state {
        "Done" | "Closed" | "Resolved" | "Removed" => (Tone::Good, false),
        "New" | "To Do" | "Proposed" | "Approved" => (Tone::Neutral, true),
        "Committed" | "In Progress" | "Active" | "Doing" => (Tone::Busy, true),
        _ => (Tone::Neutral, true),
    }
}

fn fetch_work_item(cfg: &Config, id: u64, previous: Option<&ItemState>) -> Result<ItemState> {
    let ado = cfg.ado()?;
    let wi = ado::work_item(ado, id)?;
    let wi_state = field_str(&wi, "System.State");
    let kind = field_str(&wi, "System.WorkItemType");
    let (tone, open) = work_item_tone(&wi_state);
    let mut state = ItemState::new(field_str(&wi, "System.Title"), wi_state, tone);
    state.open = open;
    state.url = Some(ado::work_item_url(ado, id));
    let assigned = wi
        .fields
        .get("System.AssignedTo")
        .and_then(|a| a.get("displayName"))
        .and_then(|n| n.as_str())
        .unwrap_or("nobody");
    let description = html_to_text(&field_str(&wi, "System.Description"));
    state.text = format!("{kind} · assigned to {assigned}\n\n{description}");
    state.description = description;
    let count = wi
        .fields
        .get("System.CommentCount")
        .and_then(|c| c.as_u64())
        .unwrap_or(0) as usize;
    state.comments = count;
    let unchanged = previous
        .filter(|p| p.comments == count && p.last_comment.is_some() && !p.conversation.is_empty());
    match unchanged {
        Some(p) => {
            state.last_comment = p.last_comment.clone();
            state.conversation = p.conversation.clone();
            if let Some((_, comments)) = p.text.split_once("\n\n--\n") {
                state.text.push_str("\n\n--\n");
                state.text.push_str(comments);
            }
        }
        None if count > 0 => {
            let comments = ado::work_item_comments(ado, id).unwrap_or_default();
            state.last_comment = comments.last().map(|c| LastComment {
                author: c.author.clone(),
                date: c.date,
                snippet: snippet(&html_to_text(&c.body)),
            });
            state.conversation = work_item_thread(&comments);
            state.text.push_str("\n\n--\n");
            for c in comments {
                state.text.push_str(&format!(
                    "\n\n{} · {}\n{}",
                    c.author,
                    c.date.format("%Y-%m-%d"),
                    html_to_text(&c.body).trim()
                ));
            }
        }
        None => {}
    }
    let (children, prs) = ado::work_item_links(&wi);
    state.related = children
        .into_iter()
        .map(|id| ItemRef::WorkItem { id })
        .chain(prs.into_iter().map(|id| ItemRef::AdoPr { id }))
        .collect();
    Ok(state)
}

fn fetch_mr(cfg: &Config, project: &str, iid: u64) -> Result<ItemState> {
    let gl = cfg.gitlab()?;
    let mut mr = gitlab::merge_request(gl, project, iid)?;
    let lifecycle = mr.state.clone().unwrap_or_default();
    let approved_status = mr.detailed_merge_status.as_deref() == Some("mergeable");
    let conflicts = mr.has_conflicts || mr.detailed_merge_status.as_deref() == Some("conflict");
    mr.has_conflicts = false;
    if mr.detailed_merge_status.as_deref() == Some("conflict") {
        mr.detailed_merge_status = None;
    }
    let pipeline = mr.head_pipeline.take().map(|p| p.status);
    let item = gitlab::item_from_mr(mr, &gl.group);
    let mut state = ItemState::new(
        short_title(&item.title),
        item.status.label(),
        status_tone(item.status),
    );
    state.ci = pipeline.as_deref().and_then(|p| match p {
        "success" => Some(Ci::Passed),
        "failed" => Some(Ci::Failed),
        "canceled" | "skipped" | "manual" => None,
        _ => Some(Ci::Running),
    });
    match lifecycle.as_str() {
        "merged" => {
            state.status = "Merged".into();
            state.tone = Tone::Good;
            state.open = false;
        }
        "closed" => {
            state.status = "Closed".into();
            state.tone = Tone::Muted;
            state.open = false;
        }
        _ => {}
    }
    state.approved = approved_status;
    state.head = item.head_sha.clone();
    state.url = Some(item.web_url.clone());
    state.text = item.description.clone();
    state.description = item.description.clone();
    state.author = item.author.clone();
    let discussions = gitlab::discussions(gl, &item).unwrap_or_default();
    apply_discussions(&mut state, &discussions);
    let me = login(&GITLAB_LOGIN, || gitlab::current_user(gl));
    state.viewer = me.map(str::to_string);
    if state.open {
        state.conflicts = conflicts;
        state.replies_due = replies_due(me, &item.author, &discussions);
    }
    Ok(state)
}

fn fetch_branch(cfg: &Config, repo: &Path, branch: &str) -> Result<ItemState> {
    let b = git::branch_state(repo, branch)?;
    let mut parts = Vec::new();
    if b.ahead > 0 {
        parts.push(format!("{} ahead", b.ahead));
    }
    if b.behind > 0 {
        parts.push(format!("{} behind", b.behind));
    }
    if b.dirty {
        parts.push("uncommitted changes".into());
    }
    let status = if parts.is_empty() {
        "up to date".to_string()
    } else {
        parts.join(", ")
    };
    let tone = if b.dirty { Tone::Warn } else { Tone::Neutral };
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut state = ItemState::new(format!("{repo_name}: {}", b.subject), status, tone);
    state.head = Some(b.head);
    state.text = format!(
        "{} in {}\nlast commit {}",
        branch,
        repo.display(),
        b.committed.format("%Y-%m-%d %H:%M")
    );
    state.related = branch_reviews(cfg, repo, branch);
    Ok(state)
}

/// The PRs or MRs whose source branch is `branch`.
fn branch_reviews(cfg: &Config, repo: &Path, branch: &str) -> Vec<ItemRef> {
    let Ok(url) = git::remote_url(repo) else {
        return Vec::new();
    };
    match git::parse_remote(&url) {
        git::Remote::Ado { repo } => cfg
            .ado()
            .and_then(|ado| ado::prs_for_branch(ado, &repo, branch))
            .map(|prs| {
                prs.into_iter()
                    .map(|pr| ItemRef::AdoPr {
                        id: pr.pull_request_id,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        git::Remote::GitLab { project, .. } => cfg
            .gitlab()
            .and_then(|gl| gitlab::mrs_for_branch(gl, &project, branch))
            .map(|mrs| {
                mrs.into_iter()
                    .map(|mr| ItemRef::GitlabMr {
                        project: gitlab::project_path(&mr),
                        iid: mr.iid,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        git::Remote::Other => Vec::new(),
    }
}

const MAX_INDEXED_FILE: u64 = 512 * 1024;

fn fetch_file(path: &Path) -> ItemState {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match std::fs::metadata(path) {
        Ok(meta) => {
            let modified: Option<DateTime<Utc>> = meta.modified().ok().map(DateTime::from);
            let mut state = ItemState::new(name, "file", Tone::Muted);
            state.head = modified.map(|m| m.to_rfc3339());
            state.text = read_text_file(path).unwrap_or_default();
            state
        }
        Err(_) => {
            let mut state = ItemState::new(name, "missing", Tone::Bad);
            state.open = false;
            state
        }
    }
}

/// The file's text, when it is small and looks like text.
pub fn read_text_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INDEXED_FILE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The first non-blank line, with runs of whitespace collapsed.
pub fn snippet(body: &str) -> String {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strips tags and decodes the common entities in ADO's HTML fields.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                if matches!(name.as_str(), "br" | "br/" | "p" | "div" | "li" | "tr") {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ParseContext<'static> {
        ParseContext {
            cwd: Path::new("/nonexistent"),
            gitlab_host: Some("gitlab.example.com"),
            gitlab_group: Some("group"),
        }
    }

    #[test]
    fn without_gitlab_a_bare_project_needs_a_group_but_urls_still_parse() {
        let ctx = ParseContext {
            cwd: Path::new("/nonexistent"),
            gitlab_host: None,
            gitlab_group: None,
        };
        let err = parse("parser!3", &ctx).unwrap_err().to_string();
        assert_eq!(err, "set group under [gitlab] to use project!number");
        assert_eq!(
            parse("group/parser!3", &ctx).unwrap(),
            ItemRef::GitlabMr {
                project: "group/parser".into(),
                iid: 3
            }
        );
        assert_eq!(
            parse(
                "https://gitlab.example.com/group/parser/-/merge_requests/3",
                &ctx
            )
            .unwrap(),
            ItemRef::GitlabMr {
                project: "group/parser".into(),
                iid: 3
            }
        );
    }

    #[test]
    fn work_item_comments_become_one_plain_thread_of_text() {
        let comment = |author: &str, body: &str| Comment {
            login: author.into(),
            author: author.into(),
            date: Utc::now(),
            location: None,
            body: body.into(),
            is_reply: false,
            status: ThreadStatus::Plain,
        };
        let thread = work_item_thread(&[
            comment("ann", "<p>first</p>"),
            comment("bob", "<b>next</b>"),
        ]);
        let summary: Vec<_> = thread
            .iter()
            .map(|c| (c.author.as_str(), c.body.as_str(), c.is_reply, c.status))
            .collect();
        assert_eq!(
            summary,
            [
                ("ann", "first", false, ThreadStatus::Plain),
                ("bob", "next", true, ThreadStatus::Plain),
            ]
        );
    }

    #[test]
    fn urls_and_short_forms_parse_to_items() {
        let cases = [
            (
                "https://dev.azure.com/example-org/Project/_git/idl/pullrequest/48211",
                ItemRef::AdoPr { id: 48211 },
            ),
            (
                "https://dev.azure.com/example-org/Project/_workitems/edit/702381/",
                ItemRef::WorkItem { id: 702381 },
            ),
            (
                "https://gitlab.example.com/group/parser/-/merge_requests/1416/diffs",
                ItemRef::GitlabMr {
                    project: "group/parser".into(),
                    iid: 1416,
                },
            ),
            ("PR 48211", ItemRef::AdoPr { id: 48211 }),
            ("pr:7", ItemRef::AdoPr { id: 7 }),
            ("#702381", ItemRef::WorkItem { id: 702381 }),
            ("wi 5", ItemRef::WorkItem { id: 5 }),
            (
                "parser!1416",
                ItemRef::GitlabMr {
                    project: "group/parser".into(),
                    iid: 1416,
                },
            ),
            (
                "other/group!3",
                ItemRef::GitlabMr {
                    project: "other/group".into(),
                    iid: 3,
                },
            ),
            (
                "https://example.com/doc",
                ItemRef::Url {
                    url: "https://example.com/doc".into(),
                    title: String::new(),
                },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse(input, &ctx()).unwrap(), expected, "{input}");
        }
        assert!(parse("nonsense words", &ctx()).is_err());
        assert!(parse("", &ctx()).is_err());
    }

    #[test]
    fn keys_tell_items_apart() {
        let mr = ItemRef::GitlabMr {
            project: "group/parser".into(),
            iid: 9,
        };
        assert_eq!(mr.key(), "mr-group/parser!9");
        assert_eq!(mr.label(), "parser!9");
        assert_ne!(
            ItemRef::AdoPr { id: 9 }.key(),
            ItemRef::WorkItem { id: 9 }.key()
        );
    }

    #[test]
    fn html_fields_become_plain_text() {
        assert_eq!(
            html_to_text(
                "<div>Fix the <b>parser</b>&nbsp;now</div><div><br></div><div>a &lt; b &amp; c</div>"
            ),
            "Fix the parser now\n\na < b & c"
        );
    }

    fn policies(builds: &[(&str, bool, &str)]) -> Vec<ado::PolicyEvaluation> {
        let mut all = vec![serde_json::json!({
            "status": "rejected",
            "configuration": {"isBlocking": true, "type": {"displayName": "Minimum number of reviewers"}},
            "context": null
        })];
        for (status, blocking, context) in builds {
            let context = match *context {
                "started" => serde_json::json!({"buildId": 7, "isExpired": false}),
                "expired" => serde_json::json!({"buildId": 7, "isExpired": true}),
                _ => serde_json::json!({}),
            };
            all.push(serde_json::json!({
                "status": status,
                "configuration": {"isBlocking": blocking, "type": {"displayName": "Build"}},
                "context": context
            }));
        }
        serde_json::from_value(serde_json::Value::Array(all)).unwrap()
    }

    #[test]
    fn the_worst_required_build_decides_the_ci_state() {
        let ci = |builds: &[(&str, bool, &str)]| super::build_ci(&policies(builds));
        assert_eq!(ci(&[]), None);
        assert_eq!(ci(&[("approved", true, "started")]), Some(Ci::Passed));
        assert_eq!(
            ci(&[("approved", true, "started"), ("running", true, "started")]),
            Some(Ci::Running)
        );
        assert_eq!(
            ci(&[("rejected", true, "started"), ("running", true, "started")]),
            Some(Ci::Failed)
        );
        assert_eq!(
            ci(&[
                ("approved", true, "started"),
                ("rejected", false, "started")
            ]),
            Some(Ci::Passed)
        );
        assert_eq!(ci(&[("rejected", false, "started")]), Some(Ci::Failed));
        assert_eq!(ci(&[("queued", true, "started")]), Some(Ci::Running));
        assert_eq!(ci(&[("queued", true, "expired")]), None);
        assert_eq!(ci(&[("queued", true, "none")]), None);
        assert_eq!(ci(&[("notApplicable", true, "none")]), None);
    }
}
