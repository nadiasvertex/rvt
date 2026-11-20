use std::thread;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::GitLabConfig;
use crate::exec::{self, run_json};
use crate::model::{
    Details, Discussion, DiscussionNote, Item, Source, Status, ThreadStatus, comments_of,
};

const PAGE_SIZE: usize = 100;
const DETAIL_THREADS: usize = 8;
/// How many child pipelines deep a failure is followed.
const MAX_PIPELINE_DEPTH: u8 = 3;

#[derive(Debug, Deserialize)]
pub struct Mr {
    pub iid: u64,
    pub project_id: u64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub detailed_merge_status: Option<String>,
    #[serde(default)]
    pub has_conflicts: bool,
    #[serde(default)]
    pub head_pipeline: Option<Pipeline>,
    pub source_branch: String,
    pub target_branch: String,
    pub web_url: String,
    #[serde(default)]
    pub sha: Option<String>,
    pub created_at: DateTime<Utc>,
    pub references: References,
    pub author: User,
    #[serde(default)]
    pub reviewers: Vec<User>,
    /// `opened`, `merged`, `closed` or `locked`.
    #[serde(default)]
    pub state: Option<String>,
    /// The merge request endpoints leave approvals out, so these come from `/approvals`.
    #[serde(skip)]
    pub approved_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Approvals {
    #[serde(default)]
    approved_by: Vec<Approver>,
}

#[derive(Debug, Deserialize)]
struct Approver {
    user: User,
}

#[derive(Debug, Deserialize)]
pub struct Pipeline {
    pub id: u64,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct Bridge {
    downstream_pipeline: Option<Pipeline>,
}

#[derive(Debug, Deserialize)]
struct JobInfo {
    id: u64,
    name: String,
}

#[derive(Debug, Deserialize)]
pub struct References {
    pub full: String,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub username: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Note {
    pub body: String,
    pub author: User,
    pub created_at: DateTime<Utc>,
    pub system: bool,
    #[serde(default)]
    pub resolvable: bool,
    #[serde(default)]
    pub resolved: Option<bool>,
    #[serde(default)]
    pub position: Option<Position>,
}

#[derive(Debug, Deserialize)]
pub struct RawDiscussion {
    pub id: String,
    #[serde(default)]
    pub notes: Vec<Note>,
}

#[derive(Debug, Deserialize)]
pub struct Position {
    #[serde(default)]
    pub new_path: Option<String>,
    #[serde(default)]
    pub new_line: Option<u32>,
    #[serde(default)]
    pub old_path: Option<String>,
    #[serde(default)]
    pub old_line: Option<u32>,
}

fn api<T: serde::de::DeserializeOwned>(cfg: &GitLabConfig, path: &str) -> Result<T> {
    run_json("glab", &["api", "--hostname", &cfg.host, path])
}

fn text(cfg: &GitLabConfig, path: &str) -> Result<String> {
    exec::run("glab", &["api", "--hostname", &cfg.host, path])
}

/// The commits GitLab uses to place a comment on an MR's diff.
#[derive(Debug, Clone, Deserialize)]
pub struct DiffRefs {
    pub base_sha: String,
    pub start_sha: String,
    pub head_sha: String,
}

#[derive(Debug, Deserialize)]
struct MrDiffRefs {
    diff_refs: Option<DiffRefs>,
}

/// One file of an MR's diff. `diff` holds the hunks without file headers.
#[derive(Debug, Clone, Deserialize)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub diff: String,
}

fn mr_path(item: &Item) -> Result<String> {
    let project = item.project_id.context("the MR has no GitLab project")?;
    Ok(format!("projects/{project}/merge_requests/{}", item.id))
}

pub fn diff_refs(cfg: &GitLabConfig, item: &Item) -> Result<DiffRefs> {
    let mr: MrDiffRefs = api(cfg, &mr_path(item)?)?;
    mr.diff_refs
        .context("GitLab has not worked out the MR's diff yet")
}

pub fn file_diffs(cfg: &GitLabConfig, item: &Item) -> Result<Vec<FileDiff>> {
    let path = mr_path(item)?;
    let mut files = Vec::new();
    for page in 1.. {
        let batch: Vec<FileDiff> = api(
            cfg,
            &format!("{path}/diffs?per_page={PAGE_SIZE}&page={page}"),
        )?;
        let done = batch.len() < PAGE_SIZE;
        files.extend(batch);
        if done {
            break;
        }
    }
    Ok(files)
}

/// Starts a discussion on the MR. A body with a `position` is placed on a diff line.
pub fn create_discussion(cfg: &GitLabConfig, item: &Item, body: &serde_json::Value) -> Result<()> {
    let path = format!("{}/discussions", mr_path(item)?);
    exec::with_json_file(body, |file| {
        exec::run(
            "glab",
            &[
                "api",
                "--hostname",
                &cfg.host,
                "--method",
                "POST",
                "--header",
                "Content-Type: application/json",
                "--input",
                file,
                &path,
            ],
        )
    })?;
    Ok(())
}

/// Adds a reply to the end of an existing discussion.
pub fn reply(cfg: &GitLabConfig, item: &Item, discussion_id: &str, body: &str) -> Result<()> {
    let path = format!("{}/discussions/{discussion_id}/notes", mr_path(item)?);
    send(cfg, "POST", &path, &serde_json::json!({ "body": body }))
}

/// Marks a discussion resolved.
pub fn resolve(cfg: &GitLabConfig, item: &Item, discussion_id: &str) -> Result<()> {
    let path = format!("{}/discussions/{discussion_id}", mr_path(item)?);
    send(cfg, "PUT", &path, &serde_json::json!({ "resolved": true }))
}

fn send(cfg: &GitLabConfig, method: &str, path: &str, body: &serde_json::Value) -> Result<()> {
    exec::with_json_file(body, |file| {
        exec::run(
            "glab",
            &[
                "api",
                "--hostname",
                &cfg.host,
                "--method",
                method,
                "--header",
                "Content-Type: application/json",
                "--input",
                file,
                path,
            ],
        )
    })?;
    Ok(())
}

/// Approves the MR as the signed-in user.
pub fn approve(cfg: &GitLabConfig, item: &Item) -> Result<()> {
    let path = format!("{}/approve", mr_path(item)?);
    exec::run(
        "glab",
        &["api", "--hostname", &cfg.host, "--method", "POST", &path],
    )?;
    Ok(())
}

pub fn current_user(cfg: &GitLabConfig) -> Result<String> {
    let user: User = api(cfg, "user")?;
    Ok(user.username)
}

pub fn list(cfg: &GitLabConfig) -> Result<Vec<Item>> {
    let mut mrs: Vec<Mr> = Vec::new();
    for page in 1.. {
        let path = format!(
            "groups/{}/merge_requests?state=opened&per_page={PAGE_SIZE}&page={page}",
            cfg.group.replace('/', "%2F")
        );
        let batch: Vec<Mr> = api(cfg, &path)?;
        let done = batch.len() < PAGE_SIZE;
        mrs.extend(batch);
        if done {
            break;
        }
    }
    Ok(with_details(cfg, mrs)?
        .into_iter()
        .map(|mr| item_from_mr(mr, &cfg.group))
        .collect())
}

/// Open MRs you created, in any group.
pub fn my_mrs(cfg: &GitLabConfig) -> Result<Vec<Mr>> {
    let mut mrs: Vec<Mr> = Vec::new();
    for page in 1.. {
        let path = format!(
            "merge_requests?scope=created_by_me&state=opened&per_page={PAGE_SIZE}&page={page}"
        );
        let batch: Vec<Mr> = api(cfg, &path)?;
        let done = batch.len() < PAGE_SIZE;
        mrs.extend(batch);
        if done {
            break;
        }
    }
    Ok(mrs)
}

/// The list endpoint leaves out the head pipeline and approvals, so each MR is fetched again by
/// id along with its approvals.
fn with_details(cfg: &GitLabConfig, mrs: Vec<Mr>) -> Result<Vec<Mr>> {
    let mut detailed = Vec::with_capacity(mrs.len());
    for chunk in mrs.chunks(DETAIL_THREADS) {
        let fetched: Vec<Result<Mr>> = thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|mr| {
                    let path = format!("projects/{}/merge_requests/{}", mr.project_id, mr.iid);
                    scope.spawn(move || {
                        let approvals: Approvals = api(cfg, &format!("{path}/approvals"))?;
                        Ok(Mr {
                            approved_by: approvers(approvals),
                            ..api::<Mr>(cfg, &path)?
                        })
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("merge request fetch panicked"))
                .collect()
        });
        for mr in fetched {
            detailed.push(mr?);
        }
    }
    Ok(detailed)
}

fn approvers(approvals: Approvals) -> Vec<String> {
    approvals
        .approved_by
        .into_iter()
        .map(|a| a.user.username)
        .collect()
}

/// `detailed_merge_status` can read `checking` or `mergeable` after a pipeline has failed, so
/// the head pipeline decides the CI state.
pub fn status_of(mr: &Mr) -> Status {
    if mr.draft {
        return Status::Draft;
    }
    if mr.has_conflicts {
        return Status::Conflicts;
    }
    match mr.detailed_merge_status.as_deref() {
        Some("draft_status") => return Status::Draft,
        Some("conflict" | "need_rebase") => return Status::Conflicts,
        _ => {}
    }
    match mr.head_pipeline.as_ref().map(|p| p.status.as_str()) {
        Some("failed") => return Status::CiFailed,
        Some(
            "created" | "waiting_for_resource" | "preparing" | "pending" | "running" | "scheduled",
        ) => return Status::CiRunning,
        _ => {}
    }
    match mr.detailed_merge_status.as_deref() {
        Some("requested_changes") => Status::Waiting,
        Some("mergeable") => Status::Ready,
        _ => Status::Active,
    }
}

/// A failed job and the text of its log.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedJob {
    pub name: String,
    pub trace: String,
}

/// The failed jobs of an MR's head pipeline. A pipeline that only triggers a child pipeline
/// has no failed jobs of its own, so its failed children are searched.
pub fn failed_jobs(cfg: &GitLabConfig, item: &Item) -> Result<Vec<FailedJob>> {
    let project = item.project_id.context("the MR has no GitLab project")?;
    let mr: Mr = api(
        cfg,
        &format!("projects/{project}/merge_requests/{}", item.id),
    )?;
    let head = mr.head_pipeline.context("the MR has no pipeline")?;
    failed_in(cfg, project, head.id, MAX_PIPELINE_DEPTH)
}

fn failed_in(cfg: &GitLabConfig, project: u64, pipeline: u64, depth: u8) -> Result<Vec<FailedJob>> {
    let jobs: Vec<JobInfo> = api(
        cfg,
        &format!(
            "projects/{project}/pipelines/{pipeline}/jobs?scope[]=failed&per_page={PAGE_SIZE}"
        ),
    )?;
    if !jobs.is_empty() {
        return jobs
            .into_iter()
            .map(|job| {
                let trace = text(cfg, &format!("projects/{project}/jobs/{}/trace", job.id))?;
                Ok(FailedJob {
                    name: job.name,
                    trace,
                })
            })
            .collect();
    }
    if depth == 0 {
        return Ok(Vec::new());
    }
    let bridges: Vec<Bridge> = api(
        cfg,
        &format!("projects/{project}/pipelines/{pipeline}/bridges?per_page={PAGE_SIZE}"),
    )?;
    let mut found = Vec::new();
    for child in bridges
        .into_iter()
        .filter_map(|b| b.downstream_pipeline)
        .filter(|p| p.status == "failed")
    {
        found.extend(failed_in(cfg, project, child.id, depth - 1)?);
    }
    Ok(found)
}

/// `group/parser!1405` in group `group` becomes `parser`.
pub fn project_of(reference: &str, group: &str) -> String {
    let path = reference.split('!').next().unwrap_or(reference);
    path.strip_prefix(group)
        .map(|rest| rest.trim_start_matches('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
        .to_string()
}

pub fn item_from_mr(mr: Mr, group: &str) -> Item {
    let status = status_of(&mr);
    let repo = project_of(&mr.references.full, group);
    Item {
        source: Source::GitLab,
        id: mr.iid,
        project_id: Some(mr.project_id),
        group: repo.clone(),
        repo,
        title: mr.title,
        author: mr.author.username,
        author_name: mr.author.name,
        reviewers: mr.reviewers.into_iter().map(|r| r.username).collect(),
        approved_by: mr.approved_by,
        status,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        description: mr.description.unwrap_or_default(),
        web_url: mr.web_url,
        head_sha: mr.sha,
        created: mr.created_at,
    }
}

pub fn details(cfg: &GitLabConfig, item: &Item) -> Result<Details> {
    Ok(Details {
        description: item.description.clone(),
        comments: comments_of(&discussions_from_raw(fetch_discussions(cfg, item)?)),
    })
}

pub fn discussions(cfg: &GitLabConfig, item: &Item) -> Result<Vec<Discussion>> {
    Ok(discussions_from_raw(fetch_discussions(cfg, item)?))
}

fn fetch_discussions(cfg: &GitLabConfig, item: &Item) -> Result<Vec<RawDiscussion>> {
    let project = item.project_id.unwrap_or_default();
    let mut raw: Vec<RawDiscussion> = Vec::new();
    for page in 1.. {
        let path = format!(
            "projects/{project}/merge_requests/{}/discussions?per_page={PAGE_SIZE}&page={page}",
            item.id
        );
        let batch: Vec<RawDiscussion> = api(cfg, &path)?;
        let done = batch.len() < PAGE_SIZE;
        raw.extend(batch);
        if done {
            break;
        }
    }
    Ok(raw)
}

/// A thread is resolved once every note that can be resolved is. A thread with no such note,
/// such as a comment on the MR overview, is plain.
fn thread_status(notes: &[Note]) -> ThreadStatus {
    let mut resolvable = notes
        .iter()
        .filter(|n| n.resolvable || n.resolved.is_some())
        .peekable();
    if resolvable.peek().is_none() {
        ThreadStatus::Plain
    } else if resolvable.all(|n| n.resolved == Some(true)) {
        ThreadStatus::Resolved
    } else {
        ThreadStatus::Open
    }
}

pub fn discussions_from_raw(raw: Vec<RawDiscussion>) -> Vec<Discussion> {
    raw.into_iter()
        .filter_map(|d| {
            let notes: Vec<Note> = d.notes.into_iter().filter(|n| !n.system).collect();
            let location = notes.first().and_then(note_location);
            let status = thread_status(&notes);
            let notes: Vec<DiscussionNote> = notes
                .into_iter()
                .map(|n| DiscussionNote {
                    login: n.author.username,
                    author: n.author.name,
                    date: n.created_at,
                    body: n.body,
                })
                .collect();
            (!notes.is_empty()).then_some(Discussion {
                id: d.id,
                location,
                status,
                notes,
            })
        })
        .collect()
}

fn note_location(n: &Note) -> Option<String> {
    let p = n.position.as_ref()?;
    let (path, line) = match (&p.new_path, p.new_line) {
        (Some(path), Some(line)) => (path, Some(line)),
        _ => (p.old_path.as_ref().or(p.new_path.as_ref())?, p.old_line),
    };
    Some(match line {
        Some(line) => format!("{path}:{line}"),
        None => path.clone(),
    })
}

fn encode_path(path: &str) -> String {
    path.replace('/', "%2F")
}

/// One MR by project path and iid, whatever its state.
pub fn merge_request(cfg: &GitLabConfig, project: &str, iid: u64) -> Result<Mr> {
    api(
        cfg,
        &format!("projects/{}/merge_requests/{iid}", encode_path(project)),
    )
}

/// MRs in any state whose source branch is `branch`, newest first.
pub fn mrs_for_branch(cfg: &GitLabConfig, project: &str, branch: &str) -> Result<Vec<Mr>> {
    api(
        cfg,
        &format!(
            "projects/{}/merge_requests?source_branch={}&per_page=20",
            encode_path(project),
            branch.replace('/', "%2F").replace('#', "%23")
        ),
    )
}

/// The project path of an MR, such as `group/parser`.
pub fn project_path(mr: &Mr) -> String {
    mr.references
        .full
        .split('!')
        .next()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn approvers_are_read_from_the_approvals_response() {
        let approvals: Approvals = serde_json::from_str(
            r#"{"approved": false, "approvals_left": 1, "approved_by": [
                {"user": {"id": 3, "username": "ann", "name": "Ann"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(approvers(approvals), ["ann"]);
        let none: Approvals = serde_json::from_str(r#"{"approved_by": []}"#).unwrap();
        assert!(approvers(none).is_empty());
    }

    #[test]
    fn project_names_drop_the_group() {
        assert_eq!(project_of("group/parser!1405", "group"), "parser");
        assert_eq!(project_of("group/tools/mm!3", "group"), "tools/mm");
        assert_eq!(project_of("other/idl!2", "group"), "other/idl");
    }

    fn mr(draft: bool, conflicts: bool, merge: &str, pipeline: Option<&str>) -> Mr {
        let mut value = json!({
            "iid": 1, "project_id": 2, "title": "t", "draft": draft, "has_conflicts": conflicts,
            "detailed_merge_status": merge, "source_branch": "a", "target_branch": "master",
            "web_url": "u", "created_at": "2026-10-30T00:00:00Z",
            "references": {"full": "group/x!1"},
            "author": {"username": "jdoe", "name": "Doe, Jane"}
        });
        if let Some(status) = pipeline {
            value["head_pipeline"] = json!({"id": 9, "status": status});
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn status_follows_merge_state_without_a_pipeline() {
        assert_eq!(status_of(&mr(true, true, "mergeable", None)), Status::Draft);
        assert_eq!(
            status_of(&mr(false, true, "mergeable", None)),
            Status::Conflicts
        );
        assert_eq!(
            status_of(&mr(false, false, "mergeable", None)),
            Status::Ready
        );
        assert_eq!(
            status_of(&mr(false, false, "requested_changes", None)),
            Status::Waiting
        );
        assert_eq!(
            status_of(&mr(false, false, "not_approved", None)),
            Status::Active
        );
    }

    #[test]
    fn a_failed_head_pipeline_is_ci_failed_whatever_the_merge_status() {
        // Merge statuses seen on MRs whose head pipeline had failed.
        assert_eq!(
            status_of(&mr(false, false, "mergeable", Some("failed"))),
            Status::CiFailed
        );
        assert_eq!(
            status_of(&mr(false, false, "checking", Some("failed"))),
            Status::CiFailed
        );
    }

    #[test]
    fn a_running_head_pipeline_is_ci_running() {
        assert_eq!(
            status_of(&mr(false, false, "checking", Some("running"))),
            Status::CiRunning
        );
        assert_eq!(
            status_of(&mr(false, false, "checking", Some("pending"))),
            Status::CiRunning
        );
    }

    #[test]
    fn a_passed_head_pipeline_does_not_mark_ci_running() {
        assert_eq!(
            status_of(&mr(false, false, "checking", Some("success"))),
            Status::Active
        );
        assert_eq!(
            status_of(&mr(false, false, "mergeable", Some("success"))),
            Status::Ready
        );
    }

    fn mrs_fixture_first() -> Mr {
        let mut mrs: Vec<Mr> = serde_json::from_str(&fixture("gitlab_mrs.json")).unwrap();
        mrs.swap_remove(0)
    }

    #[test]
    fn builds_items_from_fixture_mrs() {
        let mrs: Vec<Mr> = serde_json::from_str(&fixture("gitlab_mrs.json")).unwrap();
        let items: Vec<Item> = mrs.into_iter().map(|m| item_from_mr(m, "group")).collect();
        assert_eq!(items[0].id, 1405);
        assert_eq!(items[0].group, "parser");
        assert_eq!(items[0].status, Status::Active);
        let mut running = mrs_fixture_first();
        running.head_pipeline = Some(Pipeline {
            id: 9,
            status: "running".into(),
        });
        assert_eq!(item_from_mr(running, "group").status, Status::CiRunning);
        assert_eq!(items[0].author, "jdoe");
    }

    #[test]
    fn comments_follow_their_threads_and_take_its_status() {
        // GitLab sends `resolved` only on notes that can be resolved.
        let note = |user: &str, hour: u32, system: bool, resolvable: bool, resolved: bool| {
            let mut note = json!({"body": format!("{user} at {hour}"),
                   "author": {"username": user, "name": user},
                   "created_at": format!("2026-10-30T{hour:02}:00:00Z"), "system": system,
                   "resolvable": resolvable});
            if resolvable {
                note["resolved"] = json!(resolved);
            }
            note
        };
        let mut inline = note("ann", 2, false, true, true);
        inline["position"] = json!({"new_path": "a.rs", "new_line": 7});
        let raw: Vec<RawDiscussion> = serde_json::from_value(json!([
            {"id": "late", "notes": [note("cy", 5, false, false, false)]},
            {"id": "done", "notes": [inline, note("bob", 3, false, true, true)]},
            {"id": "sys", "notes": [note("bob", 1, true, false, false)]},
            {"id": "open", "notes": [note("ann", 4, false, true, true),
                                     note("bob", 6, false, true, false)]}
        ]))
        .unwrap();
        let comments = comments_of(&discussions_from_raw(raw));
        let summary: Vec<_> = comments
            .iter()
            .map(|c| (c.login.as_str(), c.is_reply, c.status))
            .collect();
        assert_eq!(
            summary,
            [
                ("ann", false, ThreadStatus::Resolved),
                ("bob", true, ThreadStatus::Resolved),
                ("ann", false, ThreadStatus::Open),
                ("bob", true, ThreadStatus::Open),
                ("cy", false, ThreadStatus::Plain),
            ]
        );
        assert_eq!(comments[0].location.as_deref(), Some("a.rs:7"));
        assert_eq!(comments[1].location.as_deref(), Some("a.rs:7"));
        assert_eq!(comments[2].location, None);
    }

    #[test]
    fn discussions_take_logins_and_resolution_from_their_notes() {
        let raw: Vec<RawDiscussion> = serde_json::from_value(json!([
            {"id": "abc", "notes": [
                {"body": "why?", "author": {"username": "me", "name": "Me"},
                 "created_at": "2026-10-30T00:00:00Z", "system": false, "resolved": false,
                 "position": {"new_path": "a.rs", "new_line": 4}},
                {"body": "because", "author": {"username": "bob", "name": "Bob"},
                 "created_at": "2026-10-30T01:00:00Z", "system": false, "resolved": false}
            ]},
            {"id": "sys", "notes": [
                {"body": "added 1 commit", "author": {"username": "bob", "name": "Bob"},
                 "created_at": "2026-10-30T00:00:00Z", "system": true}
            ]},
            {"id": "done", "notes": [
                {"body": "x", "author": {"username": "me", "name": "Me"},
                 "created_at": "2026-10-30T00:00:00Z", "system": false, "resolved": true},
                {"body": "y", "author": {"username": "bob", "name": "Bob"},
                 "created_at": "2026-10-30T01:00:00Z", "system": false, "resolved": true}
            ]}
        ]))
        .unwrap();
        let discussions = discussions_from_raw(raw);
        assert_eq!(discussions.len(), 2);
        assert_eq!(discussions[0].location.as_deref(), Some("a.rs:4"));
        assert!(discussions[0].waits_on("me"));
        assert!(discussions[1].is_resolved());
        assert!(!discussions[1].waits_on("me"));
    }
}
