use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Ado,
    GitLab,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Ado => "Azure DevOps",
            Source::GitLab => "GitLab",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Draft,
    Active,
    Approved,
    Waiting,
    Rejected,
    CiRunning,
    CiFailed,
    Conflicts,
    Ready,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Draft => "Draft",
            Status::Active => "Active",
            Status::Approved => "Approved",
            Status::Waiting => "Waiting",
            Status::Rejected => "Rejected",
            Status::CiRunning => "CI running",
            Status::CiFailed => "CI failed",
            Status::Conflicts => "Conflicts",
            Status::Ready => "Ready",
        }
    }

    /// Open for review: not a draft, not approved, and not sent back to the author.
    pub fn awaits_review(self) -> bool {
        matches!(self, Status::Active | Status::CiRunning)
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Status::Draft => "○",
            Status::Active => "●",
            Status::Approved | Status::Ready => "✓",
            Status::Waiting => "◷",
            Status::Rejected | Status::CiFailed => "✗",
            Status::CiRunning => "◐",
            Status::Conflicts => "⚠",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub source: Source,
    /// ADO pull request id, or GitLab merge request iid.
    pub id: u64,
    /// GitLab project id. Unused for ADO.
    pub project_id: Option<u64>,
    pub repo: String,
    pub group: String,
    pub title: String,
    pub author: String,
    pub author_name: String,
    pub reviewers: Vec<String>,
    /// Logins of the reviewers whose approval stands.
    #[serde(default)]
    pub approved_by: Vec<String>,
    pub status: Status,
    pub source_branch: String,
    pub target_branch: String,
    pub description: String,
    pub web_url: String,
    pub head_sha: Option<String>,
    pub created: DateTime<Utc>,
}

impl Item {
    pub fn key(&self) -> String {
        match self.source {
            Source::Ado => format!("ado-{}-{}", self.repo, self.id),
            Source::GitLab => format!("gitlab-{}-{}", self.repo, self.id),
        }
    }

    pub fn reference(&self) -> String {
        match self.source {
            Source::Ado => format!("PR {}", self.id),
            Source::GitLab => format!("!{}", self.id),
        }
    }

    pub fn involves(&self, login: &str) -> bool {
        self.author.eq_ignore_ascii_case(login)
            || self.reviewers.iter().any(|r| r.eq_ignore_ascii_case(login))
    }

    pub fn is_approved_by(&self, login: &str) -> bool {
        self.approved_by
            .iter()
            .any(|a| a.eq_ignore_ascii_case(login))
    }

    /// You reviewed it, the author answered or resolved every thread you wrote in, and neither
    /// you nor the host counts it as approved.
    pub fn awaits_my_approval(&self, me: &str, discussions: &[Discussion]) -> bool {
        if self.author.eq_ignore_ascii_case(me)
            || self.is_approved_by(me)
            || matches!(self.status, Status::Approved | Status::Ready)
        {
            return false;
        }
        let mut mine = discussions.iter().filter(|d| d.has_note_by(me)).peekable();
        mine.peek().is_some()
            && mine.all(|d| {
                d.is_resolved()
                    || d.last()
                        .is_some_and(|n| n.login.eq_ignore_ascii_case(&self.author))
            })
    }

    /// You wrote it, and someone else wrote the last note on an open thread.
    pub fn awaits_my_response(&self, me: &str, discussions: &[Discussion]) -> bool {
        self.author.eq_ignore_ascii_case(me)
            && discussions.iter().any(|d| {
                !d.is_resolved() && d.last().is_some_and(|n| !n.login.eq_ignore_ascii_case(me))
            })
    }

    /// The category this item is bumped into, if any.
    pub fn category(&self, me: &str, discussions: &[Discussion]) -> Option<Category> {
        if self.awaits_my_approval(me, discussions) {
            Some(Category::AwaitingApproval)
        } else if self.awaits_my_response(me, discussions) {
            Some(Category::AwaitingResponse)
        } else {
            None
        }
    }
}

/// A group at the top of the board for items that need you to act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    AwaitingApproval,
    AwaitingResponse,
}

impl Category {
    pub const ALL: [Category; 2] = [Category::AwaitingApproval, Category::AwaitingResponse];

    pub fn label(self) -> &'static str {
        match self {
            Category::AwaitingApproval => "Reviews Awaiting Approval",
            Category::AwaitingResponse => "PRs Awaiting Response",
        }
    }
}

/// Where a comment thread stands. ADO names several ways to close a thread; GitLab has only
/// resolved. A plain thread is one that cannot be resolved, such as a GitLab overview note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadStatus {
    Open,
    Pending,
    Resolved,
    WontFix,
    Closed,
    ByDesign,
    Plain,
}

impl ThreadStatus {
    pub fn is_resolved(self) -> bool {
        matches!(
            self,
            ThreadStatus::Resolved
                | ThreadStatus::WontFix
                | ThreadStatus::Closed
                | ThreadStatus::ByDesign
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            ThreadStatus::Open => "Open",
            ThreadStatus::Pending => "Pending",
            ThreadStatus::Resolved => "Resolved",
            ThreadStatus::WontFix => "Won't fix",
            ThreadStatus::Closed => "Closed",
            ThreadStatus::ByDesign => "By design",
            ThreadStatus::Plain => "Comment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    /// Empty when the host does not say, as for some service accounts.
    pub login: String,
    pub author: String,
    pub date: DateTime<Utc>,
    pub location: Option<String>,
    pub body: String,
    /// Every comment after the first in its thread is a reply.
    pub is_reply: bool,
    /// The status of the whole thread, repeated on each of its comments.
    pub status: ThreadStatus,
}

impl Comment {
    pub fn is_resolved(&self) -> bool {
        self.status.is_resolved()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Details {
    pub description: String,
    pub comments: Vec<Comment>,
}

/// A comment thread on a PR or MR, its notes oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct Discussion {
    pub id: String,
    pub location: Option<String>,
    pub status: ThreadStatus,
    pub notes: Vec<DiscussionNote>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscussionNote {
    pub login: String,
    pub author: String,
    pub date: DateTime<Utc>,
    pub body: String,
}

/// The comments of every thread, oldest thread first, each thread's first note before its replies.
pub fn comments_of(discussions: &[Discussion]) -> Vec<Comment> {
    let mut threads: Vec<&Discussion> =
        discussions.iter().filter(|d| !d.notes.is_empty()).collect();
    threads.sort_by_key(|d| d.notes.first().map(|n| n.date));
    threads
        .into_iter()
        .flat_map(|d| {
            d.notes.iter().enumerate().map(move |(n, note)| Comment {
                login: note.login.clone(),
                author: note.author.clone(),
                date: note.date,
                location: d.location.clone(),
                body: note.body.clone(),
                is_reply: n > 0,
                status: d.status,
            })
        })
        .collect()
}

impl Discussion {
    pub fn is_resolved(&self) -> bool {
        self.status.is_resolved()
    }

    pub fn last(&self) -> Option<&DiscussionNote> {
        self.notes.last()
    }

    pub fn has_note_by(&self, login: &str) -> bool {
        self.notes
            .iter()
            .any(|n| n.login.eq_ignore_ascii_case(login))
    }

    /// `me` took part, the thread is open, and someone else wrote the last note.
    pub fn waits_on(&self, me: &str) -> bool {
        !self.is_resolved()
            && self.has_note_by(me)
            && self
                .last()
                .is_some_and(|n| !n.login.eq_ignore_ascii_case(me))
    }

    /// The thread is open and someone else wrote the last note, either on `me`'s own PR or in a
    /// thread `me` took part in. A review bot has no login and counts as someone else.
    pub fn waits_for_reply(&self, me: &str, pr_author: &str) -> bool {
        !self.is_resolved()
            && (pr_author.eq_ignore_ascii_case(me) || self.has_note_by(me))
            && self
                .last()
                .is_some_and(|n| !n.login.eq_ignore_ascii_case(me))
    }
}

/// The part of an ADO unique name or email before the `@`.
pub fn login_of(unique_name: &str) -> String {
    unique_name
        .split('@')
        .next()
        .unwrap_or(unique_name)
        .to_lowercase()
}

const WORK_ITEM_TYPES: [&str; 7] = [
    "Item",
    "Bug",
    "Task",
    "Feature",
    "Epic",
    "PBI",
    "User Story",
];

/// Drops a leading work item reference such as `Item #702381: ` or `#704784 `, which every Core
/// Team PR title carries and which the board has no room for.
pub fn short_title(title: &str) -> &str {
    let Some(hash) = title.find('#') else {
        return title;
    };
    let kind = title[..hash].trim_end();
    if !kind.is_empty() && !WORK_ITEM_TYPES.contains(&kind) {
        return title;
    }
    let rest = &title[hash + 1..];
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return title;
    }
    let after = rest[digits..].trim_start_matches(':');
    match after.strip_prefix(' ') {
        Some(stripped) if !stripped.trim().is_empty() => stripped.trim_start(),
        _ => title,
    }
}

pub fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let d = now.signed_duration_since(since);
    if d.num_days() > 0 {
        format!("{}d", d.num_days())
    } else if d.num_hours() > 0 {
        format!("{}h", d.num_hours())
    } else {
        format!("{}m", d.num_minutes().max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_title_drops_work_item_prefixes_only() {
        assert_eq!(
            short_title("Item #702381: Serve article2"),
            "Serve article2"
        );
        assert_eq!(short_title("Bug #683614: Time out hung"), "Time out hung");
        assert_eq!(
            short_title("#704784 Photons: Let the user"),
            "Photons: Let the user"
        );
        assert_eq!(
            short_title("build: Enable thread-safety"),
            "build: Enable thread-safety"
        );
        assert_eq!(
            short_title("Fix #12 in parser"),
            "Fix #12 in parser".split_at(0).1
        );
        assert_eq!(short_title("Use C# records"), "Use C# records");
        assert_eq!(short_title("Item #12:"), "Item #12:");
    }

    fn discussion(resolved: bool, logins: &[&str]) -> Discussion {
        Discussion {
            id: "1".into(),
            location: None,
            status: if resolved {
                ThreadStatus::Resolved
            } else {
                ThreadStatus::Open
            },
            notes: logins
                .iter()
                .map(|login| DiscussionNote {
                    login: login.to_string(),
                    author: login.to_string(),
                    date: Utc::now(),
                    body: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_discussion_waits_on_me_when_someone_else_answered_last() {
        assert!(discussion(false, &["me", "bob"]).waits_on("Me"));
        assert!(discussion(false, &["bob", "me", "ann"]).waits_on("me"));
        assert!(!discussion(false, &["bob", "me"]).waits_on("me"));
        assert!(!discussion(true, &["me", "bob"]).waits_on("me"));
        assert!(!discussion(false, &["bob", "ann"]).waits_on("me"));
        assert!(!discussion(false, &[]).waits_on("me"));
    }

    #[test]
    fn a_thread_waits_for_my_reply_on_my_pr_or_where_i_spoke() {
        assert!(discussion(false, &["bob"]).waits_for_reply("me", "Me"));
        assert!(discussion(false, &["me", "bob"]).waits_for_reply("me", "ann"));
        assert!(!discussion(false, &["bob"]).waits_for_reply("me", "ann"));
        assert!(!discussion(false, &["bob", "me"]).waits_for_reply("me", "me"));
        assert!(!discussion(true, &["bob"]).waits_for_reply("me", "me"));
        assert!(
            discussion(false, &[""]).waits_for_reply("me", "me"),
            "a review bot without a login"
        );
        assert!(discussion(false, &["me", ""]).waits_for_reply("me", "ann"));
        assert!(!discussion(false, &[]).waits_for_reply("me", "me"));
    }

    fn item_by(author: &str, status: Status) -> Item {
        Item {
            source: Source::Ado,
            id: 1,
            project_id: None,
            repo: "repo".into(),
            group: "group".into(),
            title: String::new(),
            author: author.into(),
            author_name: author.into(),
            reviewers: Vec::new(),
            approved_by: Vec::new(),
            status,
            source_branch: String::new(),
            target_branch: String::new(),
            description: String::new(),
            web_url: String::new(),
            head_sha: None,
            created: Utc::now(),
        }
    }

    #[test]
    fn a_review_awaits_approval_once_the_author_answered_or_resolved_every_thread_of_mine() {
        let pr = item_by("bob", Status::Active);
        let answered = discussion(false, &["me", "Bob"]);
        let resolved = discussion(true, &["me"]);
        let unanswered = discussion(false, &["me"]);
        let third_party = discussion(false, &["me", "ann"]);
        let not_mine = discussion(false, &["ann"]);

        assert!(pr.awaits_my_approval("Me", &[answered.clone()]));
        assert!(pr.awaits_my_approval("me", &[answered.clone(), resolved.clone()]));
        assert!(pr.awaits_my_approval("me", &[resolved.clone(), not_mine.clone()]));
        assert!(!pr.awaits_my_approval("me", &[answered.clone(), unanswered]));
        assert!(!pr.awaits_my_approval("me", &[third_party]));
        assert!(!pr.awaits_my_approval("me", &[not_mine]));
        assert!(!pr.awaits_my_approval("me", &[]));
        for status in [Status::Approved, Status::Ready] {
            assert!(!item_by("bob", status).awaits_my_approval("me", &[answered.clone()]));
        }
        let approved_by_me = Item {
            approved_by: vec!["Me".into()],
            ..item_by("bob", Status::CiRunning)
        };
        assert!(
            !approved_by_me.awaits_my_approval("me", &[resolved.clone()]),
            "an MR I approved that is not yet mergeable"
        );
        let approved_by_ann = Item {
            approved_by: vec!["ann".into()],
            ..item_by("bob", Status::Active)
        };
        assert!(approved_by_ann.awaits_my_approval("me", &[resolved.clone()]));
        assert!(
            !item_by("me", Status::Active).awaits_my_approval("me", &[resolved]),
            "my own PR is never a review"
        );
    }

    #[test]
    fn my_pr_awaits_response_when_someone_else_spoke_last_on_an_open_thread() {
        let pr = item_by("me", Status::Active);
        assert!(pr.awaits_my_response("me", &[discussion(false, &["bob"])]));
        assert!(pr.awaits_my_response("ME", &[discussion(false, &["bob", "me", "bob"])]));
        assert!(!pr.awaits_my_response("me", &[discussion(false, &["bob", "me"])]));
        assert!(!pr.awaits_my_response("me", &[discussion(true, &["bob"])]));
        assert!(!pr.awaits_my_response("me", &[]));
        assert!(
            !item_by("bob", Status::Active)
                .awaits_my_response("me", &[discussion(false, &["ann"])]),
            "someone else's PR never awaits my response"
        );
    }
}
