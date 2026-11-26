//! The order of projects in the sidebar, and the groups they show in.
//!
//! The order is one list of project slugs in the `order` file, one per line. Each group shows
//! its projects in that list's order, so a project keeps its place among the others when it
//! moves to another group. Projects missing from the file, such as ones made before it existed,
//! come above the rest, newest first.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use crate::store::{self, Project, State};

pub fn path() -> PathBuf {
    store::home().join("order")
}

/// The saved order. A missing or unreadable file is an empty order.
pub fn load() -> Vec<String> {
    fs::read_to_string(path())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

pub fn save(order: &[String]) -> Result<()> {
    let mut text = order.join("\n");
    text.push('\n');
    store::write_atomic(&path(), text.as_bytes())
}

/// The projects in sidebar order: those missing from `order` first, newest first, then the rest
/// in `order`'s order.
pub fn arrange<'a>(projects: &'a [Project], order: &[String]) -> Vec<&'a Project> {
    let rank = |p: &Project| order.iter().position(|s| *s == p.slug);
    let mut missing: Vec<&Project> = projects.iter().filter(|p| rank(p).is_none()).collect();
    missing.sort_by_key(|p| std::cmp::Reverse(p.created));
    let mut listed: Vec<&Project> = projects.iter().filter(|p| rank(p).is_some()).collect();
    listed.sort_by_key(|p| rank(p));
    missing.extend(listed);
    missing
}

/// Puts a new project at the top of the saved order.
pub fn add_to_top(slug: &str) -> Result<()> {
    let mut order = load();
    order.retain(|s| s != slug);
    order.insert(0, slug.to_string());
    save(&order)
}

/// `order` with `slug` moved just above or below `target`. Both must be in `order`; otherwise
/// the order comes back unchanged.
pub fn moved(mut order: Vec<String>, slug: &str, target: &str, above: bool) -> Vec<String> {
    if slug == target || !order.iter().any(|s| s == target) {
        return order;
    }
    let Some(from) = order.iter().position(|s| s == slug) else {
        return order;
    };
    let moving = order.remove(from);
    let at = order.iter().position(|s| s == target).unwrap_or(0);
    order.insert(if above { at } else { at + 1 }, moving);
    order
}

/// Moves a project just above or below another and saves the order. The saved list becomes the
/// full sidebar order, so projects that were missing from it keep the places they showed in,
/// and projects that no longer exist drop out.
pub fn place(projects: &[Project], slug: &str, target: &str, above: bool) -> Result<()> {
    let full: Vec<String> = arrange(projects, &load())
        .into_iter()
        .map(|p| p.slug.clone())
        .collect();
    save(&moved(full, slug, target, above))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// Projects whose workspace is open.
    Live,
    /// Frozen projects that wait on nothing, or whose wait cleared.
    Ready,
    /// Frozen projects whose wait has not cleared.
    Waiting,
    Done,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Live => "Live",
            Group::Ready => "Ready",
            Group::Waiting => "Waiting",
            Group::Done => "Done",
        }
    }

    pub fn of(project: &Project) -> Group {
        match project.state {
            State::Live => Group::Live,
            State::Frozen if project.waiting() => Group::Waiting,
            State::Frozen => Group::Ready,
            State::Done => Group::Done,
        }
    }
}

/// The Live, Ready and Waiting groups, each in sidebar order. Groups with no projects are left
/// out, and so are done projects.
pub fn groups<'a>(projects: &'a [Project], order: &[String]) -> Vec<(Group, Vec<&'a Project>)> {
    let arranged = arrange(projects, order);
    [Group::Live, Group::Ready, Group::Waiting]
        .into_iter()
        .map(|g| {
            let members = arranged
                .iter()
                .copied()
                .filter(|p| Group::of(p) == g)
                .collect::<Vec<_>>();
            (g, members)
        })
        .filter(|(_, ps)| !ps.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::TempHome;
    use crate::wait::{Condition, Wait};
    use chrono::{TimeZone, Utc};

    fn project(slug: &str, state: State, day: u32) -> Project {
        let mut p = store::create(slug, None).unwrap();
        p.state = state;
        p.created = Utc.with_ymd_and_hms(2026, 11, day, 0, 0, 0).unwrap();
        p
    }

    fn slugs(ps: &[&Project]) -> Vec<String> {
        ps.iter().map(|p| p.slug.clone()).collect()
    }

    fn owned(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_projects_come_first_newest_first() {
        let _home = TempHome::new("order-arrange");
        let ps = vec![
            project("a", State::Live, 1),
            project("b", State::Live, 2),
            project("c", State::Live, 3),
            project("d", State::Live, 4),
        ];
        let order = owned(&["c", "gone", "a"]);
        assert_eq!(slugs(&arrange(&ps, &order)), ["d", "b", "c", "a"]);
    }

    #[test]
    fn a_moved_project_lands_next_to_its_target() {
        let order = owned(&["a", "b", "c", "d"]);
        assert_eq!(moved(order.clone(), "d", "b", true), ["a", "d", "b", "c"]);
        assert_eq!(moved(order.clone(), "a", "c", false), ["b", "c", "a", "d"]);
        assert_eq!(moved(order.clone(), "a", "a", true), order);
        assert_eq!(moved(order.clone(), "x", "a", true), order);
        assert_eq!(moved(order.clone(), "a", "x", true), order);
    }

    #[test]
    fn a_project_keeps_its_place_when_it_changes_group() {
        let _home = TempHome::new("order-groups");
        let mut ps = vec![
            project("a", State::Frozen, 1),
            project("b", State::Live, 2),
            project("c", State::Frozen, 3),
            project("d", State::Frozen, 4),
        ];
        ps[2].wait = Some(Wait::new(Condition::Manual { text: "x".into() }));
        let order = owned(&["d", "c", "b", "a"]);
        let grouped = groups(&ps, &order);
        let names: Vec<(Group, Vec<String>)> =
            grouped.iter().map(|(g, ps)| (*g, slugs(ps))).collect();
        assert_eq!(
            names,
            [
                (Group::Live, owned(&["b"])),
                (Group::Ready, owned(&["d", "a"])),
                (Group::Waiting, owned(&["c"])),
            ]
        );
        // c's wait clears, so it moves to Ready, between d and a as in the order.
        ps[2].wait = None;
        let grouped = groups(&ps, &order);
        assert_eq!(slugs(&grouped[1].1), ["d", "c", "a"]);
    }

    #[test]
    fn placing_saves_the_whole_order_and_new_projects_go_on_top() {
        let _home = TempHome::new("order-place");
        let ps = vec![
            project("a", State::Live, 1),
            project("b", State::Live, 2),
            project("c", State::Live, 3),
        ];
        // With no file, the sidebar shows c, b, a.
        place(&ps, "a", "c", true).unwrap();
        assert_eq!(load(), ["a", "c", "b"]);
        add_to_top("new").unwrap();
        assert_eq!(load(), ["new", "a", "c", "b"]);
        add_to_top("a").unwrap();
        assert_eq!(load(), ["a", "new", "c", "b"]);
    }
}
