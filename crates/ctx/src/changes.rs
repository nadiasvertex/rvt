use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::item::{Ci, ItemRef, ItemState, Tone};
use crate::store::Note;

/// One line of "since you left".
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub label: String,
    pub text: String,
    pub tone: Tone,
}

/// What changed on each item between `before` and `after`. Items missing from either side are
/// skipped: an item added since the mark is not news, and an unfetched item has nothing to say.
pub fn diff(
    refs: &[&ItemRef],
    before: &BTreeMap<String, ItemState>,
    after: &BTreeMap<String, ItemState>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for item in refs {
        let key = item.key();
        let (Some(old), Some(new)) = (before.get(&key), after.get(&key)) else {
            continue;
        };
        let label = item.label();
        let mut push = |text: String, tone: Tone| {
            changes.push(Change {
                label: label.clone(),
                text,
                tone,
            })
        };
        if old.status != new.status {
            let text = if matches!(item, ItemRef::Branch { .. }) {
                new.status.clone()
            } else {
                format!("{} → {}", old.status, new.status)
            };
            push(text, new.tone);
        }
        if old.ci != new.ci
            || (new.ci.is_some() && old.head != new.head && !matches!(item, ItemRef::Branch { .. }))
        {
            match new.ci {
                Some(Ci::Failed) => push("CI failed".into(), Tone::Bad),
                Some(Ci::Passed) => push("CI green".into(), Tone::Good),
                Some(Ci::Running) if old.ci != Some(Ci::Running) => {
                    push("CI running".into(), Tone::Busy)
                }
                _ => {}
            }
        }
        if new.conflicts != old.conflicts {
            match new.conflicts {
                true => push("conflicts with its target".into(), Tone::Warn),
                false if new.open => push("conflicts resolved".into(), Tone::Good),
                false => {}
            }
        }
        if new.comments > old.comments {
            let n = new.comments - old.comments;
            let noun = if n == 1 { "comment" } else { "comments" };
            let text = match &new.last_comment {
                Some(c) => format!("{n} new {noun}, last from {}: {}", c.author, c.snippet),
                None => format!("{n} new {noun}"),
            };
            push(text, Tone::Warn);
        }
        if old.head != new.head && old.head.is_some() {
            let text = match item {
                ItemRef::File { .. } => "edited".to_string(),
                ItemRef::Branch { .. } => format!("new commits, now at: {}", new.title),
                _ => "new commits".to_string(),
            };
            push(text, Tone::Neutral);
        }
        if old.title != new.title && !matches!(item, ItemRef::Branch { .. }) {
            push(format!("renamed: {}", new.title), Tone::Muted);
        }
    }
    changes
}

/// Notes captured after `since`, as changes.
pub fn new_notes(notes: &[Note], since: Option<DateTime<Utc>>) -> Vec<Change> {
    let Some(since) = since else {
        return Vec::new();
    };
    notes
        .iter()
        .filter(|n| n.captured > since)
        .map(|n| Change {
            label: "note".into(),
            text: n.title.clone(),
            tone: Tone::Neutral,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::LastComment;

    fn state() -> ItemState {
        ItemState {
            title: "Serialize tables".into(),
            status: "Active".into(),
            tone: Tone::Neutral,
            open: true,
            approved: false,
            ci: Some(Ci::Running),
            conflicts: false,
            replies_due: 0,
            head: Some("aaa".into()),
            comments: 1,
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

    #[test]
    fn changes_cover_status_ci_comments_and_commits() {
        let mr = ItemRef::GitlabMr {
            project: "g/parser".into(),
            iid: 1431,
        };
        let key = mr.key();
        let old = state();
        let mut new = state();
        new.status = "Approved".into();
        new.tone = Tone::Good;
        new.ci = Some(Ci::Failed);
        new.conflicts = true;
        new.head = Some("bbb".into());
        new.comments = 3;
        new.last_comment = Some(LastComment {
            author: "jdoe".into(),
            date: Utc::now(),
            snippet: "looks good".into(),
        });
        let before = BTreeMap::from([(key.clone(), old)]);
        let after = BTreeMap::from([(key, new)]);
        let texts: Vec<String> = diff(&[&mr], &before, &after)
            .into_iter()
            .map(|c| format!("{}: {}", c.label, c.text))
            .collect();
        assert_eq!(
            texts,
            [
                "parser!1431: Active → Approved",
                "parser!1431: CI failed",
                "parser!1431: conflicts with its target",
                "parser!1431: 2 new comments, last from jdoe: looks good",
                "parser!1431: new commits",
            ]
        );
    }

    #[test]
    fn unchanged_new_and_unfetched_items_report_nothing() {
        let pr = ItemRef::AdoPr { id: 1 };
        let same = BTreeMap::from([(pr.key(), state())]);
        assert!(diff(&[&pr], &same, &same).is_empty());
        assert!(diff(&[&pr], &BTreeMap::new(), &same).is_empty());
        assert!(diff(&[&pr], &same, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_new_pipeline_on_new_commits_counts_even_with_the_same_result() {
        let mr = ItemRef::GitlabMr {
            project: "g/p".into(),
            iid: 2,
        };
        let mut old = state();
        old.ci = Some(Ci::Failed);
        let mut new = old.clone();
        new.head = Some("ccc".into());
        let texts: Vec<String> = diff(
            &[&mr],
            &BTreeMap::from([(mr.key(), old)]),
            &BTreeMap::from([(mr.key(), new)]),
        )
        .into_iter()
        .map(|c| c.text)
        .collect();
        assert_eq!(texts, ["CI failed", "new commits"]);
    }
}
