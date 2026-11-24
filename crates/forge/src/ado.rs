use std::collections::HashMap;
use std::thread;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::config::AdoConfig;
use crate::exec::{run, run_json, with_json_file};
use crate::model::{
    Comment, Details, Discussion, DiscussionNote, FailedJob, Item, Source, Status, ThreadStatus,
    comments_of, login_of,
};

const BATCH_SIZE: usize = 200;
const PARALLEL_BATCHES: usize = 6;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub pull_request_id: u64,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_draft: Option<bool>,
    #[serde(default)]
    pub merge_status: Option<String>,
    pub creation_date: DateTime<Utc>,
    pub source_ref_name: String,
    pub target_ref_name: String,
    pub created_by: Identity,
    pub repository: Repository,
    #[serde(default)]
    pub reviewers: Vec<Reviewer>,
    /// `active`, `completed` or `abandoned`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub last_merge_source_commit: Option<CommitRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub display_name: String,
    #[serde(default)]
    pub unique_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reviewer {
    #[serde(default)]
    pub unique_name: Option<String>,
    #[serde(default)]
    pub vote: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRef {
    pub commit_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkItemBatch {
    pub value: Vec<WorkItem>,
}

#[derive(Debug, Deserialize)]
pub struct WorkItem {
    #[serde(default)]
    pub id: u64,
    pub fields: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub relations: Option<Vec<Relation>>,
}

#[derive(Debug, Deserialize)]
pub struct Relation {
    pub rel: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct ThreadList {
    pub value: Vec<Thread>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub thread_context: Option<ThreadContext>,
    #[serde(default)]
    pub comments: Vec<ThreadComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadContext {
    pub file_path: Option<String>,
    pub right_file_start: Option<FilePosition>,
    pub left_file_start: Option<FilePosition>,
}

#[derive(Debug, Deserialize)]
pub struct FilePosition {
    pub line: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadComment {
    pub author: Identity,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub comment_type: Option<String>,
    #[serde(default)]
    pub is_deleted: Option<bool>,
    #[serde(default)]
    pub parent_comment_id: u64,
    pub published_date: DateTime<Utc>,
}

pub fn current_user() -> Result<String> {
    let name = run(
        "az",
        &["account", "show", "--query", "user.name", "-o", "tsv"],
    )?;
    Ok(login_of(name.trim()))
}

/// Open PRs in the project, grouped by the area of their linked work items. `repo_areas`
/// places a repository's PRs in an area when no linked work item names one.
pub fn list(ado: &AdoConfig, repo_areas: &HashMap<String, String>) -> Result<Vec<Item>> {
    let prs: Vec<Pr> = run_json(
        "az",
        &[
            "repos",
            "pr",
            "list",
            "--status",
            "active",
            "--top",
            "2000",
            "--organization",
            &ado.organization,
            "--project",
            &ado.project,
            "-o",
            "json",
        ],
    )?;
    let ids = area_work_items(ado)?;
    let items = fetch_work_items(ado, &ids)?;
    let areas = pr_areas(&items);
    Ok(prs
        .into_iter()
        .filter_map(|pr| {
            let area = areas
                .get(&pr.pull_request_id)
                .or_else(|| repo_areas.get(&pr.repository.name))?;
            Some(item_from_pr(
                pr,
                area_label(area, &ado.area_root),
                &ado.organization,
                &ado.project,
            ))
        })
        .collect())
}

fn area_work_items(ado: &AdoConfig) -> Result<Vec<u64>> {
    let since = (Utc::now() - Duration::days(i64::from(ado.lookback_days))).format("%Y-%m-%d");
    wiql(
        ado,
        &format!(
            "SELECT [System.Id] FROM WorkItems WHERE [System.AreaPath] UNDER '{}' \
             AND [System.ChangedDate] >= '{since}' AND [System.ExternalLinkCount] > 0",
            ado.area_root.replace('\'', "''")
        ),
    )
}

/// Work items of the configured types assigned to you and not in a done state.
pub fn my_work_items(ado: &AdoConfig) -> Result<Vec<u64>> {
    wiql(ado, &my_work_items_query(ado))
}

fn wiql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn wiql_list(values: &[String]) -> String {
    values
        .iter()
        .map(|v| wiql_string(v))
        .collect::<Vec<_>>()
        .join(", ")
}

fn my_work_items_query(ado: &AdoConfig) -> String {
    let mut query = format!(
        "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = {} \
         AND [System.AssignedTo] = @Me",
        wiql_string(&ado.project)
    );
    if !ado.work_item_types.is_empty() {
        query += &format!(
            " AND [System.WorkItemType] IN ({})",
            wiql_list(&ado.work_item_types)
        );
    }
    if !ado.done_states.is_empty() {
        query += &format!(
            " AND [System.State] NOT IN ({})",
            wiql_list(&ado.done_states)
        );
    }
    query
}

/// Active PRs you created.
pub fn my_prs(ado: &AdoConfig) -> Result<Vec<u64>> {
    let me = run(
        "az",
        &["account", "show", "--query", "user.name", "-o", "tsv"],
    )?;
    let prs: Vec<Pr> = run_json(
        "az",
        &[
            "repos",
            "pr",
            "list",
            "--creator",
            me.trim(),
            "--status",
            "active",
            "--top",
            "500",
            "--organization",
            &ado.organization,
            "--project",
            &ado.project,
            "-o",
            "json",
        ],
    )?;
    Ok(prs.into_iter().map(|pr| pr.pull_request_id).collect())
}

/// The ids of the work items a WIQL query finds.
fn wiql(ado: &AdoConfig, query: &str) -> Result<Vec<u64>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WiqlResult {
        work_items: Vec<WorkItemRef>,
    }
    #[derive(Deserialize)]
    struct WorkItemRef {
        id: u64,
    }
    let result: WiqlResult = with_json_file(&json!({ "query": query }), |path| {
        let project = format!("project={}", ado.project);
        run_json(
            "az",
            &[
                "devops",
                "invoke",
                "--area",
                "wit",
                "--resource",
                "wiql",
                "--route-parameters",
                &project,
                "--http-method",
                "POST",
                "--in-file",
                path,
                "--api-version",
                "7.1",
                "--organization",
                &ado.organization,
                "-o",
                "json",
            ],
        )
    })?;
    Ok(result.work_items.into_iter().map(|w| w.id).collect())
}

fn fetch_work_items(ado: &AdoConfig, ids: &[u64]) -> Result<Vec<WorkItem>> {
    let chunks: Vec<&[u64]> = ids.chunks(BATCH_SIZE).collect();
    let mut all = Vec::with_capacity(ids.len());
    for wave in chunks.chunks(PARALLEL_BATCHES) {
        let results: Vec<Result<WorkItemBatch>> = thread::scope(|s| {
            let handles: Vec<_> = wave
                .iter()
                .map(|chunk| s.spawn(move || fetch_batch(ado, chunk)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("work item fetch panicked")))
                })
                .collect()
        });
        for r in results {
            all.extend(r?.value);
        }
    }
    Ok(all)
}

fn fetch_batch(ado: &AdoConfig, ids: &[u64]) -> Result<WorkItemBatch> {
    with_json_file(&json!({ "ids": ids, "$expand": "relations" }), |path| {
        run_json(
            "az",
            &[
                "devops",
                "invoke",
                "--area",
                "wit",
                "--resource",
                "workitemsbatch",
                "--http-method",
                "POST",
                "--in-file",
                path,
                "--api-version",
                "7.1",
                "--organization",
                &ado.organization,
                "-o",
                "json",
            ],
        )
    })
}

/// Parses `vstfs:///Git/PullRequestId/<project>%2F<repo>%2F<id>` into the PR id.
pub fn parse_pr_link(url: &str) -> Option<u64> {
    let lower = url.to_ascii_lowercase();
    let rest = lower.strip_prefix("vstfs:///git/pullrequestid/")?;
    let parts: Vec<&str> = rest.split("%2f").collect();
    match parts.as_slice() {
        [_, _, id] => id.parse().ok(),
        _ => None,
    }
}

/// Maps each linked PR id to the area path of a work item that links it. Current areas win over
/// `_Historical` ones when a PR is linked from both.
pub fn pr_areas(items: &[WorkItem]) -> HashMap<u64, String> {
    let mut areas: HashMap<u64, String> = HashMap::new();
    for item in items {
        let Some(area) = item.fields.get("System.AreaPath").and_then(|v| v.as_str()) else {
            continue;
        };
        for rel in item.relations.iter().flatten() {
            if rel.rel != "ArtifactLink" {
                continue;
            }
            let Some(pr) = parse_pr_link(&rel.url) else {
                continue;
            };
            let replace = match areas.get(&pr) {
                None => true,
                Some(existing) => is_historical(existing) && !is_historical(area),
            };
            if replace {
                areas.insert(pr, area.to_string());
            }
        }
    }
    areas
}

fn is_historical(area: &str) -> bool {
    area.contains("\\_Historical")
}

/// Drops the configured root from an area path, so `Root\Core Library` reads `Core Library`.
pub fn area_label(area: &str, root: &str) -> String {
    let trimmed = area
        .strip_prefix(root)
        .map(|rest| rest.trim_start_matches('\\'))
        .unwrap_or(area);
    if trimmed.is_empty() {
        root.rsplit('\\').next().unwrap_or(root).to_string()
    } else {
        trimmed.replace('\\', " / ")
    }
}

pub fn status_of(pr: &Pr) -> Status {
    if pr.is_draft.unwrap_or(false) {
        return Status::Draft;
    }
    if pr.merge_status.as_deref() == Some("conflicts") {
        return Status::Conflicts;
    }
    let votes = pr.reviewers.iter().map(|r| r.vote);
    let min = votes.clone().min().unwrap_or(0);
    let max = votes.max().unwrap_or(0);
    match (min, max) {
        (-10, _) => Status::Rejected,
        (-5, _) => Status::Waiting,
        (_, 5..) => Status::Approved,
        _ => Status::Active,
    }
}

fn strip_ref(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

pub fn item_from_pr(pr: Pr, group: String, organization: &str, project: &str) -> Item {
    let status = status_of(&pr);
    let author = pr
        .created_by
        .unique_name
        .as_deref()
        .map(login_of)
        .unwrap_or_else(|| pr.created_by.display_name.clone());
    let web_url = format!(
        "{}/{}/_git/{}/pullrequest/{}",
        organization.trim_end_matches('/'),
        project,
        pr.repository.name,
        pr.pull_request_id
    );
    Item {
        source: Source::Ado,
        id: pr.pull_request_id,
        project_id: None,
        group,
        title: pr.title,
        author,
        author_name: pr.created_by.display_name,
        reviewers: pr
            .reviewers
            .iter()
            .filter_map(|r| r.unique_name.as_deref().map(login_of))
            .collect(),
        approved_by: pr
            .reviewers
            .iter()
            .filter(|r| r.vote >= 5)
            .filter_map(|r| r.unique_name.as_deref().map(login_of))
            .collect(),
        status,
        source_branch: strip_ref(&pr.source_ref_name),
        target_branch: strip_ref(&pr.target_ref_name),
        description: pr.description.unwrap_or_default(),
        web_url,
        head_sha: pr.last_merge_source_commit.map(|c| c.commit_id),
        created: pr.creation_date,
        repo: pr.repository.name,
    }
}

pub fn details(ado: &AdoConfig, item: &Item) -> Result<Details> {
    let id = item.id.to_string();
    let (description, threads) = thread::scope(|s| {
        let description = s.spawn(|| -> Result<Option<String>> {
            run_json(
                "az",
                &[
                    "repos",
                    "pr",
                    "show",
                    "--id",
                    &id,
                    "--organization",
                    &ado.organization,
                    "--query",
                    "description",
                    "-o",
                    "json",
                ],
            )
        });
        let threads = s.spawn(|| fetch_threads(ado, item));
        (description.join(), threads.join())
    });
    let description = description
        .map_err(|_| anyhow::anyhow!("description fetch panicked"))?
        .context("could not load description")?;
    let threads = threads
        .map_err(|_| anyhow::anyhow!("thread fetch panicked"))?
        .context("could not load comments")?;
    Ok(Details {
        description: description.unwrap_or_default().trim_end().to_string(),
        comments: comments_of(&discussions_from_threads(threads)),
    })
}

fn fetch_threads(ado: &AdoConfig, item: &Item) -> Result<Vec<Thread>> {
    let route = [
        format!("project={}", ado.project),
        format!("repositoryId={}", item.repo),
        format!("pullRequestId={}", item.id),
    ];
    let list: ThreadList = run_json(
        "az",
        &[
            "devops",
            "invoke",
            "--area",
            "git",
            "--resource",
            "pullRequestThreads",
            "--route-parameters",
            &route[0],
            &route[1],
            &route[2],
            "--api-version",
            "7.1",
            "--organization",
            &ado.organization,
            "-o",
            "json",
        ],
    )?;
    Ok(list.value)
}

pub fn discussions(ado: &AdoConfig, item: &Item) -> Result<Vec<Discussion>> {
    Ok(discussions_from_threads(fetch_threads(ado, item)?))
}

/// The PR's current source commit.
pub fn head_commit(ado: &AdoConfig, item: &Item) -> Result<String> {
    let id = item.id.to_string();
    let sha = run(
        "az",
        &[
            "repos",
            "pr",
            "show",
            "--id",
            &id,
            "--organization",
            &ado.organization,
            "--query",
            "lastMergeSourceCommit.commitId",
            "-o",
            "tsv",
        ],
    )?;
    Ok(sha.trim().to_string())
}

/// Votes to approve the PR as the signed-in user.
pub fn approve(ado: &AdoConfig, item: &Item) -> Result<()> {
    let id = item.id.to_string();
    run(
        "az",
        &[
            "repos",
            "pr",
            "set-vote",
            "--id",
            &id,
            "--vote",
            "approve",
            "--organization",
            &ado.organization,
            "-o",
            "none",
        ],
    )?;
    Ok(())
}

/// Creates a comment thread on the PR from a `GitPullRequestCommentThread` body.
pub fn create_thread(ado: &AdoConfig, item: &Item, thread: &serde_json::Value) -> Result<()> {
    let route = [
        format!("project={}", ado.project),
        format!("repositoryId={}", item.repo),
        format!("pullRequestId={}", item.id),
    ];
    with_json_file(thread, |path| {
        run(
            "az",
            &[
                "devops",
                "invoke",
                "--area",
                "git",
                "--resource",
                "pullRequestThreads",
                "--route-parameters",
                &route[0],
                &route[1],
                &route[2],
                "--http-method",
                "POST",
                "--in-file",
                path,
                "--api-version",
                "7.1",
                "--organization",
                &ado.organization,
                "-o",
                "json",
            ],
        )
    })?;
    Ok(())
}

/// Adds a reply to the end of an existing thread.
pub fn reply(ado: &AdoConfig, item: &Item, thread_id: &str, body: &str) -> Result<()> {
    let comment = json!({ "content": body, "commentType": "text" });
    on_thread(
        ado,
        item,
        thread_id,
        "pullRequestThreadComments",
        "POST",
        &comment,
    )
}

/// Sets a thread's status: `fixed`, which the web shows as "Resolved", or `wontFix`.
pub fn resolve(ado: &AdoConfig, item: &Item, thread_id: &str, status: &str) -> Result<()> {
    let status = json!({ "status": status });
    on_thread(ado, item, thread_id, "pullRequestThreads", "PATCH", &status)
}

fn on_thread(
    ado: &AdoConfig,
    item: &Item,
    thread_id: &str,
    resource: &str,
    method: &str,
    body: &serde_json::Value,
) -> Result<()> {
    let route = [
        format!("project={}", ado.project),
        format!("repositoryId={}", item.repo),
        format!("pullRequestId={}", item.id),
        format!("threadId={thread_id}"),
    ];
    with_json_file(body, |path| {
        run(
            "az",
            &[
                "devops",
                "invoke",
                "--area",
                "git",
                "--resource",
                resource,
                "--route-parameters",
                &route[0],
                &route[1],
                &route[2],
                &route[3],
                "--http-method",
                method,
                "--in-file",
                path,
                "--api-version",
                "7.1",
                "--organization",
                &ado.organization,
                "-o",
                "json",
            ],
        )
    })?;
    Ok(())
}

impl Thread {
    fn location(&self) -> Option<String> {
        let c = self.thread_context.as_ref()?;
        let path = c.file_path.as_deref()?.trim_start_matches('/');
        let line = c.right_file_start.as_ref().or(c.left_file_start.as_ref());
        Some(match line {
            Some(p) => format!("{path}:{}", p.line),
            None => path.to_string(),
        })
    }

    fn status(&self) -> ThreadStatus {
        match self.status.as_deref() {
            Some("active") => ThreadStatus::Open,
            Some("pending") => ThreadStatus::Pending,
            Some("fixed") => ThreadStatus::Resolved,
            Some("wontFix") => ThreadStatus::WontFix,
            Some("closed") => ThreadStatus::Closed,
            Some("byDesign") => ThreadStatus::ByDesign,
            _ => ThreadStatus::Plain,
        }
    }

    /// Comments a person wrote, leaving out system messages and deleted or empty ones.
    fn human_comments(self) -> impl Iterator<Item = (ThreadComment, String)> {
        self.comments.into_iter().filter_map(|c| {
            if c.comment_type.as_deref() == Some("system") || c.is_deleted.unwrap_or(false) {
                return None;
            }
            let body = c.content.clone().filter(|b| !b.trim().is_empty())?;
            Some((c, body))
        })
    }
}

pub fn discussions_from_threads(threads: Vec<Thread>) -> Vec<Discussion> {
    threads
        .into_iter()
        .filter(|t| !t.is_deleted)
        .map(|thread| {
            let (id, location, status) = (thread.id, thread.location(), thread.status());
            let mut notes: Vec<DiscussionNote> = thread
                .human_comments()
                .map(|(c, body)| DiscussionNote {
                    login: c
                        .author
                        .unique_name
                        .as_deref()
                        .map(login_of)
                        .unwrap_or_default(),
                    author: c.author.display_name,
                    date: c.published_date,
                    body,
                })
                .collect();
            notes.sort_by_key(|n| n.date);
            Discussion {
                id: id.to_string(),
                location,
                status,
                notes,
            }
        })
        .filter(|d| !d.notes.is_empty())
        .collect()
}

/// One PR by id, whatever its state.
pub fn pull_request(ado: &AdoConfig, id: u64) -> Result<Pr> {
    run_json(
        "az",
        &[
            "repos",
            "pr",
            "show",
            "--id",
            &id.to_string(),
            "--organization",
            &ado.organization,
            "-o",
            "json",
        ],
    )
}

/// How one branch policy, such as a required build or reviewer count, stands on a PR.
#[derive(Debug, Deserialize)]
pub struct PolicyEvaluation {
    /// `queued`, `running`, `approved`, `rejected`, `notApplicable` or `broken`.
    #[serde(default)]
    pub status: Option<String>,
    pub configuration: PolicyConfiguration,
    #[serde(default)]
    pub context: Option<PolicyContext>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyConfiguration {
    #[serde(default)]
    pub is_blocking: bool,
    #[serde(rename = "type")]
    pub kind: PolicyType,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyType {
    pub display_name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PolicyContext {
    pub build_id: Option<u64>,
    pub is_expired: bool,
}

impl PolicyEvaluation {
    pub fn is_build(&self) -> bool {
        self.configuration.kind.display_name == "Build"
    }

    /// The build this evaluation ran, unless it was never started or has expired.
    pub fn current_build(&self) -> Option<u64> {
        self.context
            .as_ref()
            .filter(|c| !c.is_expired)
            .and_then(|c| c.build_id)
    }

    pub fn has_failed(&self) -> bool {
        matches!(self.status.as_deref(), Some("rejected" | "broken"))
    }
}

/// The build policies that decide a PR's CI state: the required ones when there are any,
/// otherwise all of them.
pub fn counted_builds(policies: &[PolicyEvaluation]) -> Vec<&PolicyEvaluation> {
    let builds: Vec<_> = policies.iter().filter(|p| p.is_build()).collect();
    if builds.iter().any(|p| p.configuration.is_blocking) {
        builds
            .into_iter()
            .filter(|p| p.configuration.is_blocking)
            .collect()
    } else {
        builds
    }
}

/// The ids of the failed builds among a PR's counted build policies.
pub fn failed_builds(policies: &[PolicyEvaluation]) -> Vec<u64> {
    counted_builds(policies)
        .into_iter()
        .filter(|p| p.has_failed())
        .filter_map(PolicyEvaluation::current_build)
        .collect()
}

/// The policy evaluations of a PR, builds among them.
pub fn policies(ado: &AdoConfig, id: u64) -> Result<Vec<PolicyEvaluation>> {
    run_json(
        "az",
        &[
            "repos",
            "pr",
            "policy",
            "list",
            "--id",
            &id.to_string(),
            "--organization",
            &ado.organization,
            "-o",
            "json",
        ],
    )
}

#[derive(Debug, Deserialize)]
struct Timeline {
    #[serde(default)]
    records: Option<Vec<TimelineRecord>>,
}

/// One stage, job or task of a build.
#[derive(Debug, Deserialize)]
struct TimelineRecord {
    /// `Stage`, `Phase`, `Job` or `Task`.
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    log: Option<LogRef>,
    #[serde(default)]
    issues: Option<Vec<Issue>>,
}

#[derive(Debug, Deserialize)]
struct LogRef {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct Issue {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct LogLines {
    #[serde(default)]
    value: Vec<String>,
}

fn build_resource(ado: &AdoConfig, resource: &str, route: &[String]) -> Result<String> {
    let mut args = vec![
        "devops",
        "invoke",
        "--area",
        "build",
        "--resource",
        resource,
        "--route-parameters",
    ];
    args.extend(route.iter().map(String::as_str));
    args.extend(["--organization", &ado.organization, "-o", "json"]);
    run("az", &args)
}

/// The failed tasks of each build, with their logs. A build that failed before any task ran,
/// such as one that found no agent, reports its failed jobs and stages instead.
pub fn failed_jobs(ado: &AdoConfig, builds: &[u64]) -> Result<Vec<FailedJob>> {
    let mut found = Vec::new();
    for build in builds {
        let route = [
            format!("project={}", ado.project),
            format!("buildId={build}"),
        ];
        let timeline: Timeline = serde_json::from_str(&build_resource(ado, "timeline", &route)?)
            .with_context(|| format!("could not read the timeline of build {build}"))?;
        for record in failed_records(timeline.records.unwrap_or_default()) {
            let trace = match &record.log {
                Some(log) => {
                    let mut route = route.to_vec();
                    route.push(format!("logId={}", log.id));
                    let lines: LogLines = serde_json::from_str(&build_resource(
                        ado, "logs", &route,
                    )?)
                    .with_context(|| format!("could not read log {} of build {build}", log.id))?;
                    lines
                        .value
                        .iter()
                        .map(|l| without_timestamp(l))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                None => String::new(),
            };
            let errors: Vec<_> = record
                .issues
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.kind == "error")
                .map(|i| i.message)
                .collect();
            found.push(FailedJob {
                name: record.name,
                trace,
                reason: (!errors.is_empty()).then(|| errors.join("\n")),
            });
        }
    }
    Ok(found)
}

fn failed_records(records: Vec<TimelineRecord>) -> Vec<TimelineRecord> {
    let failed: Vec<_> = records
        .into_iter()
        .filter(|r| r.result.as_deref() == Some("failed"))
        .collect();
    if failed.iter().any(|r| r.kind == "Task") {
        failed.into_iter().filter(|r| r.kind == "Task").collect()
    } else {
        failed
            .into_iter()
            .filter(|r| r.issues.as_ref().is_some_and(|i| !i.is_empty()))
            .collect()
    }
}

/// `2026-11-23T21:46:16.9597879Z text` becomes `text`.
fn without_timestamp(line: &str) -> &str {
    match line.split_once(' ') {
        Some((stamp, rest))
            if stamp.len() >= 20
                && stamp.ends_with('Z')
                && stamp.as_bytes().get(10) == Some(&b'T')
                && stamp.starts_with(|c: char| c.is_ascii_digit()) =>
        {
            rest
        }
        _ => line,
    }
}

/// PRs in any state whose source branch is `branch`, newest first.
pub fn prs_for_branch(ado: &AdoConfig, repo: &str, branch: &str) -> Result<Vec<Pr>> {
    run_json(
        "az",
        &[
            "repos",
            "pr",
            "list",
            "--repository",
            repo,
            "--source-branch",
            branch,
            "--status",
            "all",
            "--organization",
            &ado.organization,
            "--project",
            &ado.project,
            "-o",
            "json",
        ],
    )
}

#[derive(Debug, Deserialize)]
struct LinkedWorkItem {
    id: u64,
}

/// Ids of the work items linked to a PR.
pub fn pr_work_items(ado: &AdoConfig, id: u64) -> Result<Vec<u64>> {
    let linked: Vec<LinkedWorkItem> = run_json(
        "az",
        &[
            "repos",
            "pr",
            "work-item",
            "list",
            "--id",
            &id.to_string(),
            "--organization",
            &ado.organization,
            "-o",
            "json",
        ],
    )?;
    Ok(linked.into_iter().map(|w| w.id).collect())
}

/// One work item with its relations.
pub fn work_item(ado: &AdoConfig, id: u64) -> Result<WorkItem> {
    run_json(
        "az",
        &[
            "boards",
            "work-item",
            "show",
            "--id",
            &id.to_string(),
            "--organization",
            &ado.organization,
            "-o",
            "json",
        ],
    )
}

#[derive(Debug, Deserialize)]
struct WorkItemComments {
    #[serde(default)]
    comments: Vec<WorkItemComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkItemComment {
    created_by: Identity,
    created_date: DateTime<Utc>,
    #[serde(default)]
    text: String,
}

/// A work item's discussion, oldest first.
pub fn work_item_comments(ado: &AdoConfig, id: u64) -> Result<Vec<Comment>> {
    let work_item = format!("workItemId={id}");
    let project = format!("project={}", ado.project);
    let list: WorkItemComments = run_json(
        "az",
        &[
            "devops",
            "invoke",
            "--area",
            "wit",
            "--resource",
            "comments",
            "--route-parameters",
            &project,
            &work_item,
            "--api-version",
            "7.1-preview",
            "--organization",
            &ado.organization,
            "-o",
            "json",
        ],
    )?;
    Ok(work_item_comments_from(list.comments))
}

fn work_item_comments_from(comments: Vec<WorkItemComment>) -> Vec<Comment> {
    let mut out: Vec<Comment> = comments
        .into_iter()
        .map(|c| Comment {
            login: c
                .created_by
                .unique_name
                .as_deref()
                .map(login_of)
                .unwrap_or_default(),
            author: c.created_by.display_name,
            date: c.created_date,
            location: None,
            body: c.text,
            is_reply: false,
            status: ThreadStatus::Plain,
        })
        .collect();
    out.sort_by_key(|c| c.date);
    out
}

/// The ids of a work item's children and of the PRs linked to it.
pub fn work_item_links(item: &WorkItem) -> (Vec<u64>, Vec<u64>) {
    let mut children = Vec::new();
    let mut prs = Vec::new();
    for relation in item.relations.iter().flatten() {
        if relation.rel == "System.LinkTypes.Hierarchy-Forward" {
            if let Some(id) = relation.url.rsplit('/').next().and_then(|s| s.parse().ok()) {
                children.push(id);
            }
        } else if let Some(pr) = parse_pr_link(&relation.url) {
            prs.push(pr);
        }
    }
    (children, prs)
}

pub fn work_item_url(ado: &AdoConfig, id: u64) -> String {
    format!(
        "{}/{}/_workitems/edit/{id}",
        ado.organization.trim_end_matches('/'),
        ado.project
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_policy(status: &str, blocking: bool, build: Option<u64>) -> serde_json::Value {
        json!({
            "status": status,
            "configuration": {"isBlocking": blocking, "type": {"displayName": "Build"}},
            "context": build.map(|id| json!({"buildId": id, "isExpired": false}))
        })
    }

    fn evaluations(values: Vec<serde_json::Value>) -> Vec<PolicyEvaluation> {
        serde_json::from_value(serde_json::Value::Array(values)).unwrap()
    }

    #[test]
    fn failed_builds_come_from_required_builds_when_there_are_any() {
        let reviewers = json!({
            "status": "rejected",
            "configuration": {"isBlocking": true, "type": {"displayName": "Minimum number of reviewers"}},
            "context": null
        });
        let policies = evaluations(vec![
            reviewers,
            build_policy("rejected", true, Some(1)),
            build_policy("approved", true, Some(2)),
            build_policy("rejected", false, Some(3)),
        ]);
        assert_eq!(failed_builds(&policies), [1]);

        let optional = evaluations(vec![
            build_policy("broken", false, Some(4)),
            build_policy("rejected", false, None),
        ]);
        assert_eq!(
            failed_builds(&optional),
            [4],
            "a build that never started has no id"
        );
    }

    fn record(kind: &str, name: &str, result: &str, issues: &[&str]) -> serde_json::Value {
        json!({
            "type": kind, "name": name, "result": result,
            "issues": issues.iter().map(|m| json!({"type": "error", "message": m})).collect::<Vec<_>>()
        })
    }

    fn names(records: Vec<serde_json::Value>) -> Vec<String> {
        let records: Vec<TimelineRecord> =
            serde_json::from_value(serde_json::Value::Array(records)).unwrap();
        failed_records(records)
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn failed_tasks_are_chosen_over_their_jobs_and_stages() {
        let records = vec![
            record("Stage", "Build and test", "failed", &[]),
            record("Job", "Build and test", "failed", &[]),
            record("Task", "Checkout", "succeeded", &[]),
            record(
                "Task",
                "Build status-dashboard",
                "failed",
                &["Bash exited with code '1'."],
            ),
        ];
        assert_eq!(names(records), ["Build status-dashboard"]);
    }

    #[test]
    fn a_build_with_no_failed_task_reports_the_records_that_say_why() {
        let records = vec![
            record("Stage", "Build", "failed", &[]),
            record("Job", "Build", "failed", &["No agent found in pool Mac."]),
        ];
        assert_eq!(names(records), ["Build"]);
    }

    #[test]
    fn log_timestamps_are_removed() {
        assert_eq!(
            without_timestamp("2026-11-23T21:46:16.9725572Z ##[error]Bash exited"),
            "##[error]Bash exited"
        );
        assert_eq!(without_timestamp("plain text line"), "plain text line");
        assert_eq!(
            without_timestamp("2026-11-23T21:46:16.9Z"),
            "2026-11-23T21:46:16.9Z"
        );
    }

    #[test]
    fn work_item_links_split_children_from_prs() {
        let item = WorkItem {
            id: 1,
            fields: HashMap::new(),
            relations: Some(vec![
                Relation {
                    rel: "System.LinkTypes.Hierarchy-Forward".into(),
                    url: "https://dev.azure.com/example-org/_apis/wit/workItems/702390".into(),
                },
                Relation {
                    rel: "System.LinkTypes.Hierarchy-Reverse".into(),
                    url: "https://dev.azure.com/example-org/_apis/wit/workItems/1".into(),
                },
                Relation {
                    rel: "ArtifactLink".into(),
                    url: "vstfs:///Git/PullRequestId/abc%2Fdef%2F48211".into(),
                },
            ]),
        };
        assert_eq!(work_item_links(&item), (vec![702390], vec![48211]));
    }

    #[test]
    fn work_item_comments_come_oldest_first() {
        let comment = |name: &str, date: &str| WorkItemComment {
            created_by: Identity {
                display_name: name.into(),
                unique_name: None,
            },
            created_date: date.parse().unwrap(),
            text: format!("from {name}"),
        };
        let comments = work_item_comments_from(vec![
            comment("Newer", "2026-11-02T10:00:00Z"),
            comment("Older", "2026-11-01T10:00:00Z"),
        ]);
        let authors: Vec<&str> = comments.iter().map(|c| c.author.as_str()).collect();
        assert_eq!(authors, ["Older", "Newer"]);
    }

    const ROOT: &str = r"Project\Team";

    fn ado_config(types: &[&str], states: &[&str]) -> AdoConfig {
        AdoConfig {
            organization: "https://dev.azure.com/example-org".into(),
            project: "Bob's Project".into(),
            area_root: ROOT.into(),
            lookback_days: 365,
            work_item_types: types.iter().map(|s| s.to_string()).collect(),
            done_states: states.iter().map(|s| s.to_string()).collect(),
            ssh_host: "ssh.dev.azure.com".into(),
        }
    }

    #[test]
    fn my_work_items_query_uses_the_configured_types_and_states() {
        assert_eq!(
            my_work_items_query(&ado_config(&["User Story", "Bug"], &["Closed"])),
            "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = 'Bob''s Project' \
             AND [System.AssignedTo] = @Me \
             AND [System.WorkItemType] IN ('User Story', 'Bug') \
             AND [System.State] NOT IN ('Closed')"
        );
        assert_eq!(
            my_work_items_query(&ado_config(&[], &[])),
            "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = 'Bob''s Project' \
             AND [System.AssignedTo] = @Me"
        );
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn parses_pr_links_in_either_case() {
        assert_eq!(
            parse_pr_link("vstfs:///Git/PullRequestId/3c9f%2F71f5%2F140114"),
            Some(140114)
        );
        assert_eq!(
            parse_pr_link("vstfs:///Git/PullRequestId/3c9f%2f14f8%2f170775"),
            Some(170775)
        );
        assert_eq!(parse_pr_link("vstfs:///Git/Commit/3c9f%2F71f5%2Fabc"), None);
        assert_eq!(
            parse_pr_link("vstfs:///Git/PullRequestId/3c9f%2F140114"),
            None
        );
    }

    #[test]
    fn maps_fixture_work_items_to_prs() {
        let batch: WorkItemBatch = serde_json::from_str(&fixture("ado_workitems.json")).unwrap();
        let areas = pr_areas(&batch.value);
        assert_eq!(
            areas.get(&140114).map(String::as_str),
            Some(r"Project\Team\_Historical\Libraries\Platform")
        );
        assert_eq!(areas.get(&33502299), None, "relation ids are not PR ids");
    }

    #[test]
    fn current_area_wins_over_historical() {
        let item = |area: &str| WorkItem {
            id: 0,
            fields: HashMap::from([("System.AreaPath".to_string(), json!(area))]),
            relations: Some(vec![Relation {
                rel: "ArtifactLink".into(),
                url: "vstfs:///Git/PullRequestId/p%2Fr%2F7".into(),
            }]),
        };
        let historical = format!(r"{ROOT}\_Historical\Libraries");
        let current = format!(r"{ROOT}\Core Library");
        for order in [[&historical, &current], [&current, &historical]] {
            let areas = pr_areas(&[item(order[0]), item(order[1])]);
            assert_eq!(areas[&7], current);
        }
    }

    #[test]
    fn area_labels_drop_the_root() {
        assert_eq!(
            area_label(&format!(r"{ROOT}\Core Library"), ROOT),
            "Core Library"
        );
        assert_eq!(area_label(ROOT, ROOT), "Team");
        assert_eq!(
            area_label(&format!(r"{ROOT}\_Historical\Libraries\Core"), ROOT),
            "_Historical / Libraries / Core"
        );
        assert_eq!(area_label(r"Other\Area", ROOT), "Other / Area");
    }

    fn pr(draft: bool, merge: &str, votes: &[i32]) -> Pr {
        let mut p: Pr = serde_json::from_value(json!({
            "pullRequestId": 1, "title": "t", "creationDate": "2026-10-30T00:00:00Z",
            "sourceRefName": "refs/heads/a", "targetRefName": "refs/heads/master",
            "createdBy": {"displayName": "Doe, Jane", "uniqueName": "jdoe@example.com"},
            "repository": {"name": "x"}
        }))
        .unwrap();
        p.is_draft = Some(draft);
        p.merge_status = Some(merge.into());
        p.reviewers = votes
            .iter()
            .map(|&vote| Reviewer {
                unique_name: None,
                vote,
            })
            .collect();
        p
    }

    #[test]
    fn status_follows_draft_conflicts_and_votes() {
        assert_eq!(status_of(&pr(true, "conflicts", &[-10])), Status::Draft);
        assert_eq!(status_of(&pr(false, "conflicts", &[10])), Status::Conflicts);
        assert_eq!(
            status_of(&pr(false, "succeeded", &[10, -10])),
            Status::Rejected
        );
        assert_eq!(
            status_of(&pr(false, "succeeded", &[10, -5])),
            Status::Waiting
        );
        assert_eq!(
            status_of(&pr(false, "succeeded", &[0, 5])),
            Status::Approved
        );
        assert_eq!(status_of(&pr(false, "succeeded", &[0, 0])), Status::Active);
        assert_eq!(status_of(&pr(false, "succeeded", &[])), Status::Active);
    }

    #[test]
    fn builds_items_from_fixture_prs() {
        let prs: Vec<Pr> = serde_json::from_str(&fixture("ado_prs.json")).unwrap();
        let first = prs.into_iter().next().unwrap();
        let item = item_from_pr(
            first,
            "Frontend".into(),
            "https://dev.azure.com/example-org/",
            "Project",
        );
        assert_eq!(item.id, 183267);
        assert_eq!(item.repo, "web-renderer");
        assert!(!item.source_branch.starts_with("refs/"));
        assert_eq!(
            item.web_url,
            "https://dev.azure.com/example-org/Project/_git/web-renderer/pullrequest/183267"
        );
        assert!(!item.author.contains('@'));
    }

    #[test]
    fn threads_drop_system_comments() {
        let threads: ThreadList = serde_json::from_str(&fixture("ado_threads.json")).unwrap();
        let total = threads.value.len();
        let comments = comments_of(&discussions_from_threads(threads.value));
        assert!(total > comments.len());
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "Roe, Sam");
        assert_eq!(comments[0].location, None);
    }

    #[test]
    fn inline_threads_carry_file_and_line() {
        let threads: ThreadList = serde_json::from_value(json!({"value": [{
            "status": "fixed",
            "threadContext": {"filePath": "/src/a.cpp", "rightFileStart": {"line": 41, "offset": 1}},
            "comments": [
                {"author": {"displayName": "A"}, "content": "x", "commentType": "text",
                 "parentCommentId": 0, "publishedDate": "2026-10-30T00:00:00Z"},
                {"author": {"displayName": "B"}, "content": "y", "commentType": "text",
                 "parentCommentId": 1, "publishedDate": "2026-10-30T01:00:00Z"}
            ]
        }]}))
        .unwrap();
        let comments = comments_of(&discussions_from_threads(threads.value));
        assert_eq!(comments[0].location.as_deref(), Some("src/a.cpp:41"));
        assert_eq!(comments[0].status, ThreadStatus::Resolved);
        assert!(!comments[0].is_reply);
        assert!(comments[1].is_reply);
    }

    #[test]
    fn discussions_carry_logins_and_skip_system_and_empty_threads() {
        let threads: ThreadList = serde_json::from_value(json!({"value": [
            {"id": 7, "status": "active",
             "threadContext": {"filePath": "/src/a.cpp", "rightFileStart": {"line": 3, "offset": 1}},
             "comments": [
                {"author": {"displayName": "Me", "uniqueName": "ME@example.org"}, "content": "why?",
                 "commentType": "text", "parentCommentId": 0, "publishedDate": "2026-10-30T00:00:00Z"},
                {"author": {"displayName": "Bob", "uniqueName": "bob@example.org"}, "content": "because",
                 "commentType": "text", "parentCommentId": 1, "publishedDate": "2026-10-30T01:00:00Z"}
             ]},
            {"id": 8, "comments": [
                {"author": {"displayName": "System"}, "content": "voted", "commentType": "system",
                 "parentCommentId": 0, "publishedDate": "2026-10-30T00:00:00Z"}
             ]}
        ]}))
        .unwrap();
        let discussions = discussions_from_threads(threads.value);
        assert_eq!(discussions.len(), 1);
        let d = &discussions[0];
        assert_eq!(d.id, "7");
        assert_eq!(d.location.as_deref(), Some("src/a.cpp:3"));
        assert_eq!(d.notes[0].login, "me");
        assert_eq!(d.notes[1].author, "Bob");
        assert!(d.waits_on("me"));
    }
}
