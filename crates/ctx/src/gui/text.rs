//! The words the app shows for projects, items and times.

use chrono::{DateTime, Local, NaiveDate, Utc};
use ctx_core::changes::Change;
use ctx_core::live::Status;
use ctx_core::order::Group;
use ctx_core::store::Project;

use super::snapshot::Snapshot;

/// How long ago, such as `just now`, `4m ago`, `3h ago` or `2d ago`.
pub fn ago(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let d = now.signed_duration_since(time);
    if d.num_days() > 0 {
        format!("{}d ago", d.num_days())
    } else if d.num_hours() > 0 {
        format!("{}h ago", d.num_hours())
    } else if d.num_minutes() > 0 {
        format!("{}m ago", d.num_minutes())
    } else {
        "just now".into()
    }
}

pub fn change(c: &Change) -> String {
    format!("{} {}", c.label, c.text)
}

/// The line under a project's name in the sidebar.
pub fn status(snapshot: &Snapshot, project: &Project, now: DateTime<Utc>) -> String {
    if let Some(cleared) = project.wait.as_ref().and_then(|w| w.cleared.as_ref()) {
        return cleared.reason.clone();
    }
    if let Some(wait) = project.wait.as_ref().filter(|w| w.is_waiting()) {
        return format!("waiting on {}", wait.condition.describe());
    }
    let first_change = snapshot
        .since
        .get(&project.slug)
        .and_then(|s| s.changes.first())
        .map(change);
    match Group::of(project) {
        Group::Live => {
            let claude = snapshot.claude.get(&project.slug);
            let busy = claude.is_some_and(|c| c.iter().any(|(_, l)| l.status != Status::Idle));
            if busy {
                "Claude working".into()
            } else if claude.is_some_and(|c| !c.is_empty()) {
                "Claude waiting for you".into()
            } else {
                first_change.unwrap_or_else(|| "open".into())
            }
        }
        _ => first_change.unwrap_or_else(|| match &project.handoff {
            Some(h) => format!("frozen {}", ago(h.time, now)),
            None => "frozen".into(),
        }),
    }
}

/// Markdown reduced to plain text for a short view: no emphasis markers or list bullets.
pub fn plain(markdown: &str) -> String {
    markdown
        .lines()
        .map(|l| {
            let l = l.trim();
            let l = l
                .strip_prefix("- ")
                .or_else(|| l.strip_prefix("* "))
                .unwrap_or(l);
            l.replace("**", "").replace('`', "")
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Today`, `Yesterday`, or a date such as `Mon 23 Nov`.
pub fn day(date: NaiveDate, today: NaiveDate) -> String {
    match (today - date).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => date.format("%a %-d %b").to_string(),
    }
}

pub fn greeting(now: DateTime<Local>) -> &'static str {
    match now.format("%H").to_string().parse::<u32>().unwrap_or(12) {
        0..12 => "Good morning",
        12..18 => "Good afternoon",
        _ => "Good evening",
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The line under the home view's greeting, such as `2 projects are ready for you, 2 are
/// waiting, and 1 item has no project.`
pub fn home_summary(ready: usize, waiting: usize, unfiled: usize) -> String {
    let mut parts = Vec::new();
    if ready > 0 {
        let verb = if ready == 1 { "is" } else { "are" };
        parts.push(format!(
            "{} {verb} ready for you",
            count(ready, "project", "projects")
        ));
    }
    if waiting > 0 {
        let verb = if waiting == 1 { "is" } else { "are" };
        if parts.is_empty() {
            parts.push(format!(
                "{} {verb} waiting",
                count(waiting, "project", "projects")
            ));
        } else {
            parts.push(format!("{waiting} {verb} waiting"));
        }
    }
    if unfiled > 0 {
        let verb = if unfiled == 1 { "has" } else { "have" };
        parts.push(format!(
            "{} {verb} no project",
            count(unfiled, "item", "items")
        ));
    }
    let sentence = match parts.as_slice() {
        [] => return "Nothing is waiting for you.".into(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    };
    format!("{sentence}.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ages_read_naturally() {
        let now = Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap();
        let at = |h, m| Utc.with_ymd_and_hms(2026, 11, 26, h, m, 0).unwrap();
        assert_eq!(ago(at(11, 59), now), "1m ago");
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(at(9, 0), now), "3h ago");
        assert_eq!(
            ago(Utc.with_ymd_and_hms(2026, 11, 24, 12, 0, 0).unwrap(), now),
            "2d ago"
        );
    }

    #[test]
    fn markdown_becomes_plain_lines() {
        assert_eq!(
            plain("**Doing:** a `thing`\n\n- one\n* two"),
            "Doing: a thing\none\ntwo"
        );
    }

    #[test]
    fn days_are_named_relative_to_today() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        assert_eq!(day(today, today), "Today");
        assert_eq!(day(today.pred_opt().unwrap(), today), "Yesterday");
        assert_eq!(
            day(NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(), today),
            "Mon 23 Nov"
        );
    }

    #[test]
    fn the_home_summary_lists_what_there_is() {
        assert_eq!(
            home_summary(2, 2, 1),
            "2 projects are ready for you, 2 are waiting, and 1 item has no project."
        );
        assert_eq!(home_summary(1, 0, 0), "1 project is ready for you.");
        assert_eq!(
            home_summary(0, 1, 3),
            "1 project is waiting and 3 items have no project."
        );
        assert_eq!(home_summary(0, 0, 0), "Nothing is waiting for you.");
    }
}
