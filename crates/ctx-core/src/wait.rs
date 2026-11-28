use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::item::{Ci, ItemState};

/// Something a project waits on. Every kind except `Manual` can clear itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Condition {
    /// Free text with no check, such as "answer from jdoe".
    Manual { text: String },
    /// A reviewer responds to a PR or MR in any way: approves it, votes for changes, comments,
    /// or closes it. `after` and `status` are the comment count and status when the wait was set.
    /// Waits saved as `approved` by older versions have neither, so any comment dated after the
    /// wait, and any vote for changes, clears them.
    #[serde(alias = "approved")]
    Review {
        item: String,
        label: String,
        #[serde(default)]
        after: Option<usize>,
        #[serde(default)]
        status: Option<String>,
    },
    /// A PR or MR is merged or closed, or a work item is done.
    Closed { item: String, label: String },
    /// CI on a PR or MR passes or fails after the wait was set.
    CiDone { item: String, label: String },
    /// Someone comments on an item.
    Comment {
        item: String,
        label: String,
        after: usize,
    },
    /// A work item reaches a state, such as `Done` or `Committed`.
    WorkItemState {
        item: String,
        label: String,
        state: String,
    },
    /// A date passes.
    Until { at: DateTime<Utc> },
    /// A Claude Code session in the project stops or asks for input. With a terminal, only sessions
    /// in that terminal count.
    Claude {
        #[serde(default, rename = "pane")]
        terminal: Option<String>,
    },
    /// A command started with `ctx run` exits.
    Command {
        #[serde(default)]
        label: String,
    },
}

impl Condition {
    /// The key of the item the condition watches, for the kinds that watch one.
    pub fn item(&self) -> Option<&str> {
        match self {
            Condition::Review { item, .. }
            | Condition::Closed { item, .. }
            | Condition::CiDone { item, .. }
            | Condition::Comment { item, .. }
            | Condition::WorkItemState { item, .. } => Some(item),
            Condition::Manual { .. }
            | Condition::Until { .. }
            | Condition::Claude { .. }
            | Condition::Command { .. } => None,
        }
    }

    /// A short description for the home screen, such as `Claude` or `review on !1422`.
    pub fn describe(&self) -> String {
        match self {
            Condition::Manual { text } => text.clone(),
            Condition::Review { label, .. } => format!("review on {label}"),
            Condition::Closed { label, .. } => format!("{label} to close"),
            Condition::CiDone { label, .. } => format!("CI on {label}"),
            Condition::Comment { label, .. } => format!("comment on {label}"),
            Condition::WorkItemState { label, state, .. } => format!("{label} → {state}"),
            Condition::Until { at } => {
                let local = at.with_timezone(&chrono::Local);
                let days = (local.date_naive() - chrono::Local::now().date_naive()).num_days();
                if days == 0 {
                    format!("until {}", local.format("%H:%M"))
                } else if (0..7).contains(&days) {
                    format!("until {}", local.format("%a %H:%M"))
                } else {
                    format!("until {}", local.format("%b %-d"))
                }
            }
            Condition::Claude { .. } => "Claude".into(),
            Condition::Command { label } if label.is_empty() => "command".into(),
            Condition::Command { label } => label.clone(),
        }
    }
}

impl Condition {
    /// Records where the item stands now, so a review or comment wait clears only on what
    /// happens after it is set.
    pub fn starting_now(mut self, items: &BTreeMap<String, ItemState>) -> Self {
        match &mut self {
            Condition::Review {
                item,
                after,
                status,
                ..
            } => {
                let state = items.get(item.as_str());
                *after = Some(state.map_or(0, |s| s.comments));
                *status = Some(state.map(|s| s.status.clone()).unwrap_or_default());
            }
            Condition::Comment { item, after, .. } => {
                *after = items.get(item.as_str()).map_or(0, |s| s.comments);
            }
            _ => {}
        }
        self
    }
}

fn is_review(item: &str) -> bool {
    item.starts_with("pr-") || item.starts_with("mr-")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cleared {
    pub at: DateTime<Utc>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wait {
    pub set: DateTime<Utc>,
    pub condition: Condition,
    #[serde(default)]
    pub cleared: Option<Cleared>,
}

impl Wait {
    pub fn new(condition: Condition) -> Self {
        Wait {
            set: Utc::now(),
            condition,
            cleared: None,
        }
    }

    pub fn is_waiting(&self) -> bool {
        self.cleared.is_none()
    }
}

/// Something that happened in a project's workspace, recorded by `ctx event` or `ctx run`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub time: DateTime<Utc>,
    pub kind: EventKind,
    #[serde(default, rename = "pane")]
    pub terminal: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    ClaudeStart,
    ClaudeEnd,
    ClaudeStop,
    ClaudeNotification,
    CommandStart,
    CommandExit,
}

/// The reason the condition is met, or `None` while it still waits. Item conditions only count
/// item states fetched after the wait was set, so a stale cache cannot clear them.
pub fn check(
    wait: &Wait,
    items: &BTreeMap<String, ItemState>,
    events: &[Event],
    now: DateTime<Utc>,
) -> Option<String> {
    let fresh = |key: &str| items.get(key).filter(|s| s.fetched > wait.set);
    let after = |e: &&Event| e.time >= wait.set;
    match &wait.condition {
        Condition::Manual { .. } => None,
        Condition::Review {
            item,
            label,
            after: count,
            status,
        } => fresh(item).and_then(|s| {
            let voted = status.as_ref() != Some(&s.status);
            let commented = match count {
                Some(count) => s.comments > *count,
                None => s.last_comment.as_ref().is_some_and(|c| c.date > wait.set),
            };
            if !s.open {
                Some(format!("{label} {}", s.status.to_lowercase()))
            } else if s.approved {
                Some(format!("{label} approved"))
            } else if voted && s.status == "Waiting" {
                Some(format!("changes requested on {label}"))
            } else if voted && s.status == "Rejected" {
                Some(format!("{label} rejected"))
            } else if commented {
                Some(match &s.last_comment {
                    Some(c) => format!("{} commented on {label}", c.author),
                    None => format!("new comment on {label}"),
                })
            } else {
                None
            }
        }),
        Condition::Closed { item, label } => fresh(item)
            .filter(|s| !s.open)
            .map(|s| format!("{label} {}", s.status.to_lowercase())),
        Condition::CiDone { item, label } => fresh(item).and_then(|s| match s.ci {
            Some(Ci::Passed) => Some(format!("CI green on {label}")),
            Some(Ci::Failed) => Some(format!("CI failed on {label}")),
            _ => None,
        }),
        Condition::Comment {
            item,
            label,
            after: count,
        } => fresh(item)
            .filter(|s| s.comments > *count)
            .map(|s| match &s.last_comment {
                Some(c) => format!("{} replied on {label}", c.author),
                None => format!("new comment on {label}"),
            }),
        Condition::WorkItemState { item, label, state } => fresh(item)
            .filter(|s| s.status.eq_ignore_ascii_case(state))
            .map(|_| format!("{label} is {state}")),
        Condition::Until { at } => (now >= *at).then(|| "date reached".to_string()),
        Condition::Claude { terminal } => events
            .iter()
            .filter(after)
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::ClaudeStop | EventKind::ClaudeNotification
                )
            })
            .filter(|e| terminal.is_none() || e.terminal == *terminal)
            .next_back()
            .map(|e| match e.kind {
                EventKind::ClaudeNotification => "Claude needs input".to_string(),
                _ => "Claude finished".to_string(),
            }),
        Condition::Command { label } => events
            .iter()
            .filter(after)
            .filter(|e| e.kind == EventKind::CommandExit)
            .next_back()
            .map(|e| {
                let what = if label.is_empty() { "command" } else { label };
                format!("{what} finished, exit {}", e.detail)
            }),
    }
}

/// Parses the shell form of a wait condition, as `ctx wait` takes it: `claude`, `cmd`,
/// `review <item>`, `closed <item>`, `ci <item>`, `state <item> <state>`, `until <date or
/// duration>`, or any other text as a manual wait. `approved`, `comment` and `reply` are other
/// names for `review`, which on a work item waits for a comment.
pub fn parse_shell(
    args: &[String],
    resolve: impl Fn(&str) -> Option<(String, String)>,
) -> Result<Condition, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let item = |i: usize| -> Result<(String, String), String> {
        let arg = words.get(i).ok_or("name the item to watch")?;
        resolve(arg).ok_or_else(|| format!("no item in the project matches {arg}"))
    };
    match words.first().map(|w| w.to_lowercase()).as_deref() {
        None => Err("say what to wait on".into()),
        Some("claude") => Ok(Condition::Claude { terminal: None }),
        Some("cmd" | "command") => Ok(Condition::Command {
            label: words[1..].join(" "),
        }),
        Some("review" | "approved" | "approval" | "comment" | "reply") => {
            item(1).map(|(item, label)| {
                if is_review(&item) {
                    Condition::Review {
                        item,
                        label,
                        after: None,
                        status: None,
                    }
                } else {
                    Condition::Comment {
                        item,
                        label,
                        after: 0,
                    }
                }
            })
        }
        Some("closed" | "merged" | "done") => {
            item(1).map(|(item, label)| Condition::Closed { item, label })
        }
        Some("ci") => item(1).map(|(item, label)| Condition::CiDone { item, label }),
        Some("state") => {
            let (item, label) = item(1)?;
            let state = words[2..].join(" ");
            if state.is_empty() {
                return Err("name the state to wait for".into());
            }
            Ok(Condition::WorkItemState { item, label, state })
        }
        Some("until") => parse_when(&words[1..].join(" "), Utc::now())
            .map(|at| Condition::Until { at })
            .ok_or_else(|| {
                "use a duration such as 30m, 2h or 3d, or a date such as 2026-11-20".into()
            }),
        Some(_) => Ok(Condition::Manual {
            text: words.join(" "),
        }),
    }
}

/// Reads `30m`, `2h`, `3d`, a weekday such as `thu`, or `YYYY-MM-DD` (9:00 local that day).
pub fn parse_when(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }
    let nine = |date: NaiveDate| {
        Local
            .from_local_datetime(&date.and_hms_opt(9, 0, 0)?)
            .earliest()
            .map(|d| d.with_timezone(&Utc))
    };
    if let Ok(date) = NaiveDate::parse_from_str(&text, "%Y-%m-%d") {
        return nine(date);
    }
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    if let Ok(n) = number.parse::<i64>() {
        let span = match unit.trim() {
            "m" | "min" | "mins" | "minutes" => Duration::minutes(n),
            "h" | "hr" | "hrs" | "hours" => Duration::hours(n),
            "d" | "day" | "days" => Duration::days(n),
            "w" | "week" | "weeks" => Duration::weeks(n),
            _ => return None,
        };
        return Some(now + span);
    }
    let weekdays = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let target = weekdays.iter().position(|d| text.starts_with(d))? as i64;
    let today = now.with_timezone(&Local).date_naive();
    let current = today.weekday().num_days_from_monday() as i64;
    let ahead = (target - current).rem_euclid(7);
    let at = nine(today + Duration::days(ahead))?;
    if at > now {
        Some(at)
    } else {
        nine(today + Duration::days(ahead + 7))
    }
}

/// The terminal a Claude session runs in: where it last started, unless it ended or another
/// session started in that terminal since.
pub fn claude_terminal(events: &[Event], id: &str) -> Option<String> {
    let start = events
        .iter()
        .rposition(|e| e.kind == EventKind::ClaudeStart && e.session.as_deref() == Some(id))?;
    let terminal = events[start].terminal.clone()?;
    let gone = events[start + 1..].iter().any(|e| match e.kind {
        EventKind::ClaudeStart => e.terminal.as_deref() == Some(terminal.as_str()),
        EventKind::ClaudeEnd => e.session.as_deref() == Some(id),
        _ => false,
    });
    (!gone).then_some(terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{LastComment, Tone};
    use chrono::Duration;

    fn state(fetched: DateTime<Utc>) -> ItemState {
        ItemState {
            title: "t".into(),
            status: "Active".into(),
            tone: Tone::Neutral,
            open: true,
            approved: false,
            ci: None,
            conflicts: false,
            replies_due: 0,
            head: None,
            comments: 2,
            last_comment: None,
            url: None,
            text: String::new(),
            description: String::new(),
            conversation: Vec::new(),
            author: String::new(),
            viewer: None,
            related: Vec::new(),
            fetched,
        }
    }

    fn wait(condition: Condition, set: DateTime<Utc>) -> Wait {
        Wait {
            set,
            condition,
            cleared: None,
        }
    }

    #[test]
    fn item_conditions_clear_only_on_states_fetched_after_the_wait() {
        let set = Utc::now();
        let ci = wait(
            Condition::CiDone {
                item: "mr-x".into(),
                label: "!1".into(),
            },
            set,
        );
        let mut old = state(set - Duration::minutes(1));
        old.ci = Some(Ci::Passed);
        let mut items = BTreeMap::from([("mr-x".to_string(), old)]);
        assert_eq!(
            check(&ci, &items, &[], set),
            None,
            "a fetch from before the wait"
        );

        let mut running = state(set + Duration::minutes(1));
        running.ci = Some(Ci::Running);
        items.insert("mr-x".into(), running.clone());
        assert_eq!(check(&ci, &items, &[], set), None);

        running.ci = Some(Ci::Failed);
        items.insert("mr-x".into(), running);
        assert_eq!(
            check(&ci, &items, &[], set).as_deref(),
            Some("CI failed on !1")
        );
    }

    #[test]
    fn comment_closed_and_state_conditions() {
        let set = Utc::now();
        let later = set + Duration::minutes(5);
        let mut s = state(later);
        s.comments = 3;
        s.last_comment = Some(LastComment {
            author: "jdoe".into(),
            date: later,
            snippet: String::new(),
        });
        let items = BTreeMap::from([("pr-1".to_string(), s.clone())]);
        let comment = |after| {
            wait(
                Condition::Comment {
                    item: "pr-1".into(),
                    label: "PR 1".into(),
                    after,
                },
                set,
            )
        };
        assert_eq!(
            check(&comment(2), &items, &[], later).as_deref(),
            Some("jdoe replied on PR 1")
        );
        assert_eq!(check(&comment(3), &items, &[], later), None);

        let closed = wait(
            Condition::Closed {
                item: "pr-1".into(),
                label: "PR 1".into(),
            },
            set,
        );
        assert_eq!(check(&closed, &items, &[], later), None);
        s.open = false;
        s.status = "Merged".into();
        let items = BTreeMap::from([("pr-1".to_string(), s.clone())]);
        assert_eq!(
            check(&closed, &items, &[], later).as_deref(),
            Some("PR 1 merged")
        );

        let done = wait(
            Condition::WorkItemState {
                item: "pr-1".into(),
                label: "#1".into(),
                state: "merged".into(),
            },
            set,
        );
        assert_eq!(
            check(&done, &items, &[], later).as_deref(),
            Some("#1 is merged")
        );
    }

    fn review(after: usize, status: &str, set: DateTime<Utc>) -> Wait {
        wait(
            Condition::Review {
                item: "pr-1".into(),
                label: "PR 1".into(),
                after: Some(after),
                status: Some(status.into()),
            },
            set,
        )
    }

    #[test]
    fn a_review_wait_clears_on_an_approval_a_vote_a_comment_or_a_close() {
        let set = Utc::now();
        let later = set + Duration::minutes(5);
        let quiet = state(later);
        let reason = |w: &Wait, s: &ItemState| {
            check(
                w,
                &BTreeMap::from([("pr-1".to_string(), s.clone())]),
                &[],
                later,
            )
        };
        assert_eq!(reason(&review(2, "Active", set), &quiet), None);

        let mut commented = quiet.clone();
        commented.comments = 3;
        commented.last_comment = Some(LastComment {
            author: "jdoe".into(),
            date: later,
            snippet: String::new(),
        });
        assert_eq!(
            reason(&review(2, "Active", set), &commented).as_deref(),
            Some("jdoe commented on PR 1")
        );
        assert_eq!(reason(&review(3, "Active", set), &commented), None);

        let mut approved = quiet.clone();
        approved.approved = true;
        approved.status = "Approved".into();
        assert_eq!(
            reason(&review(2, "Active", set), &approved).as_deref(),
            Some("PR 1 approved")
        );

        let mut waiting = quiet.clone();
        waiting.status = "Waiting".into();
        assert_eq!(
            reason(&review(2, "Active", set), &waiting).as_deref(),
            Some("changes requested on PR 1")
        );
        assert_eq!(
            reason(&review(2, "Waiting", set), &waiting),
            None,
            "a vote from before the wait"
        );
        let mut rejected = quiet.clone();
        rejected.status = "Rejected".into();
        assert_eq!(
            reason(&review(2, "Active", set), &rejected).as_deref(),
            Some("PR 1 rejected")
        );

        let mut merged = quiet.clone();
        merged.open = false;
        merged.status = "Merged".into();
        assert_eq!(
            reason(&review(2, "Active", set), &merged).as_deref(),
            Some("PR 1 merged")
        );
    }

    #[test]
    fn starting_now_takes_the_comment_count_and_status_of_the_item() {
        let mut s = state(Utc::now());
        s.comments = 4;
        s.status = "Waiting".into();
        let items = BTreeMap::from([("pr-1".to_string(), s)]);
        let set = review(0, "", Utc::now()).condition.starting_now(&items);
        assert_eq!(set, review(4, "Waiting", Utc::now()).condition);
        let comment = Condition::Comment {
            item: "pr-1".into(),
            label: "PR 1".into(),
            after: 0,
        };
        assert_eq!(
            comment.starting_now(&items),
            Condition::Comment {
                item: "pr-1".into(),
                label: "PR 1".into(),
                after: 4,
            }
        );
    }

    #[test]
    fn an_approval_wait_saved_by_an_older_version_clears_on_comments_dated_after_it() {
        #[derive(Deserialize)]
        struct Saved {
            wait: Wait,
        }
        let saved: Saved = toml::from_str(
            r#"
            [wait]
            set = "2026-11-01T10:00:00Z"
            [wait.condition]
            kind = "approved"
            item = "pr-1"
            label = "PR 1"
            "#,
        )
        .unwrap();
        assert_eq!(
            saved.wait.condition,
            Condition::Review {
                item: "pr-1".into(),
                label: "PR 1".into(),
                after: None,
                status: None,
            }
        );
        let set = saved.wait.set;
        let mut s = state(set + Duration::minutes(5));
        s.comments = 9;
        s.last_comment = Some(LastComment {
            author: "jdoe".into(),
            date: set - Duration::minutes(1),
            snippet: String::new(),
        });
        let items = |s: &ItemState| BTreeMap::from([("pr-1".to_string(), s.clone())]);
        assert_eq!(
            check(&saved.wait, &items(&s), &[], set),
            None,
            "a comment from before the wait"
        );
        s.last_comment.as_mut().unwrap().date = set + Duration::minutes(1);
        assert_eq!(
            check(&saved.wait, &items(&s), &[], set).as_deref(),
            Some("jdoe commented on PR 1")
        );
    }

    fn event(kind: EventKind, time: DateTime<Utc>, terminal: Option<&str>) -> Event {
        Event {
            time,
            kind,
            terminal: terminal.map(str::to_string),
            session: None,
            detail: "0".into(),
        }
    }

    #[test]
    fn a_claude_session_runs_in_the_terminal_it_last_started_in_until_replaced_or_ended() {
        let t = Utc::now();
        let ev = |kind, terminal: &str, id: &str| Event {
            session: Some(id.into()),
            ..event(kind, t, Some(terminal))
        };
        let resumed = [
            ev(EventKind::ClaudeStart, "%2", "a"),
            ev(EventKind::ClaudeStop, "%2", "a"),
            ev(EventKind::ClaudeStart, "%3", "a"),
            ev(EventKind::ClaudeStart, "%2", "b"),
        ];
        assert_eq!(claude_terminal(&resumed, "a").as_deref(), Some("%3"));
        assert_eq!(claude_terminal(&resumed, "b").as_deref(), Some("%2"));
        let replaced = [
            ev(EventKind::ClaudeStart, "%2", "a"),
            ev(EventKind::ClaudeStart, "%2", "b"),
        ];
        assert_eq!(claude_terminal(&replaced, "a"), None);
        let ended = [
            ev(EventKind::ClaudeStart, "%2", "a"),
            ev(EventKind::ClaudeEnd, "%2", "a"),
        ];
        assert_eq!(claude_terminal(&ended, "a"), None);
        assert_eq!(claude_terminal(&ended, "never"), None);
    }

    #[test]
    fn claude_and_command_conditions_clear_on_later_events() {
        let set = Utc::now();
        let before = set - Duration::seconds(5);
        let after = set + Duration::seconds(5);
        let any = wait(Condition::Claude { terminal: None }, set);
        let in_terminal = wait(
            Condition::Claude {
                terminal: Some("%3".into()),
            },
            set,
        );
        let old = [event(EventKind::ClaudeStop, before, Some("%3"))];
        assert_eq!(check(&any, &BTreeMap::new(), &old, after), None);

        let other_terminal = [event(EventKind::ClaudeStop, after, Some("%4"))];
        assert_eq!(
            check(&any, &BTreeMap::new(), &other_terminal, after).as_deref(),
            Some("Claude finished")
        );
        assert_eq!(
            check(&in_terminal, &BTreeMap::new(), &other_terminal, after),
            None
        );

        let asking = [event(EventKind::ClaudeNotification, after, Some("%3"))];
        assert_eq!(
            check(&in_terminal, &BTreeMap::new(), &asking, after).as_deref(),
            Some("Claude needs input")
        );

        let cmd = wait(
            Condition::Command {
                label: "just test".into(),
            },
            set,
        );
        let started = [event(EventKind::CommandStart, after, None)];
        assert_eq!(check(&cmd, &BTreeMap::new(), &started, after), None);
        let exited = [event(EventKind::CommandExit, after, None)];
        assert_eq!(
            check(&cmd, &BTreeMap::new(), &exited, after).as_deref(),
            Some("just test finished, exit 0")
        );
    }

    #[test]
    fn dates_clear_once_passed_and_parse_from_short_forms() {
        let now = Utc::now();
        let until = wait(
            Condition::Until {
                at: now + Duration::hours(1),
            },
            now,
        );
        assert_eq!(check(&until, &BTreeMap::new(), &[], now), None);
        assert!(check(&until, &BTreeMap::new(), &[], now + Duration::hours(2)).is_some());

        assert_eq!(parse_when("30m", now), Some(now + Duration::minutes(30)));
        assert_eq!(parse_when("2h", now), Some(now + Duration::hours(2)));
        assert_eq!(parse_when("3d", now), Some(now + Duration::days(3)));
        let thu = parse_when("thu", now).unwrap();
        assert!(thu > now && thu <= now + Duration::days(7));
        assert!(parse_when("2026-11-20", now).is_some());
        assert_eq!(parse_when("soon", now), None);
    }

    #[test]
    fn a_weekday_is_its_next_nine_oclock_still_ahead() {
        use chrono::{Local, NaiveDate, TimeZone};
        let thursday = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        let local = |date: NaiveDate, hour: u32| {
            Local
                .from_local_datetime(&date.and_hms_opt(hour, 0, 0).unwrap())
                .earliest()
                .unwrap()
                .with_timezone(&Utc)
        };
        let next_thursday = thursday + Duration::days(7);
        assert_eq!(
            parse_when("thu", local(thursday, 8)),
            Some(local(thursday, 9))
        );
        assert_eq!(
            parse_when("thu", local(thursday, 10)),
            Some(local(next_thursday, 9))
        );
        assert_eq!(
            parse_when("fri", local(thursday, 10)),
            Some(local(thursday + Duration::days(1), 9))
        );
    }

    #[test]
    fn shell_forms_parse_to_conditions() {
        let resolve = |arg: &str| (arg == "!1").then(|| ("mr-x!1".to_string(), "x!1".to_string()));
        let work_item = |arg: &str| (arg == "#5").then(|| ("wi-5".to_string(), "#5".to_string()));
        let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            parse_shell(&words("claude"), resolve),
            Ok(Condition::Claude { terminal: None })
        );
        assert_eq!(
            parse_shell(&words("ci !1"), resolve),
            Ok(Condition::CiDone {
                item: "mr-x!1".into(),
                label: "x!1".into()
            })
        );
        assert!(parse_shell(&words("ci !2"), resolve).is_err());
        for word in ["review", "approved", "comment"] {
            assert_eq!(
                parse_shell(&words(&format!("{word} !1")), resolve),
                Ok(Condition::Review {
                    item: "mr-x!1".into(),
                    label: "x!1".into(),
                    after: None,
                    status: None,
                }),
                "{word}"
            );
        }
        assert_eq!(
            parse_shell(&words("comment #5"), work_item),
            Ok(Condition::Comment {
                item: "wi-5".into(),
                label: "#5".into(),
                after: 0,
            })
        );
        assert_eq!(
            parse_shell(&words("answer from jdoe"), resolve),
            Ok(Condition::Manual {
                text: "answer from jdoe".into()
            })
        );
        assert!(parse_shell(&words("until whenever"), resolve).is_err());
    }
}
