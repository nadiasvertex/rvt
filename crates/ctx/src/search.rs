use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::item::{ItemRef, ItemState, read_text_file};
use crate::store::{self, Project};

/// Markers around matched words in snippets, picked to never appear in text.
pub const MATCH_START: char = '\u{2}';
pub const MATCH_END: char = '\u{3}';

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub project: String,
    pub kind: String,
    /// A note's path, an item key, or a journal heading.
    pub reference: String,
    pub title: String,
    pub snippet: String,
}

fn open() -> Result<Connection> {
    let path = store::home().join("index.db");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn =
        Connection::open(&path).with_context(|| format!("could not open {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(
            project UNINDEXED, kind UNINDEXED, reference UNINDEXED, title, body,
            tokenize = 'porter unicode61'
        );",
    )?;
    Ok(conn)
}

struct Doc {
    kind: &'static str,
    reference: String,
    title: String,
    body: String,
}

fn documents(project: &Project, cache: &BTreeMap<String, ItemState>) -> Vec<Doc> {
    let mut docs = vec![Doc {
        kind: "project",
        reference: project.slug.clone(),
        title: project.name.clone(),
        body: [
            project.next.clone(),
            project
                .handoff
                .as_ref()
                .map(store::handoff_body)
                .unwrap_or_default()
                .replace("**", ""),
        ]
        .join("\n"),
    }];
    for note in store::notes(&project.slug) {
        docs.push(Doc {
            kind: "note",
            reference: note.path.to_string_lossy().into_owned(),
            title: note.title,
            body: format!("{}\n{}\n{}", note.source, note.link, note.body),
        });
    }
    for (heading, body) in store::journal_entries(&store::journal(&project.slug)) {
        if body.is_empty() {
            continue;
        }
        docs.push(Doc {
            kind: "journal",
            reference: heading.clone(),
            title: heading,
            body: body.replace("**", ""),
        });
    }
    for item in project.refs() {
        let state = cache.get(&item.key());
        let body = match (item, state) {
            (ItemRef::File { path }, None) => read_text_file(path).unwrap_or_default(),
            (_, Some(s)) => s.text.clone(),
            _ => String::new(),
        };
        let title = match state {
            Some(s) if !s.title.is_empty() => format!("{} {}", item.label(), s.title),
            _ => item.label(),
        };
        docs.push(Doc {
            kind: "item",
            reference: item.key(),
            title,
            body,
        });
    }
    docs
}

/// Replaces the project's documents in the index.
pub fn index_project(project: &Project, cache: &BTreeMap<String, ItemState>) -> Result<()> {
    let mut conn = open()?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM docs WHERE project = ?1", params![project.slug])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO docs (project, kind, reference, title, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for doc in documents(project, cache) {
            insert.execute(params![
                project.slug,
                doc.kind,
                doc.reference,
                doc.title,
                doc.body
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Rebuilds the whole index from the project files and the cache.
pub fn reindex(projects: &[Project], cache: &BTreeMap<String, ItemState>) -> Result<usize> {
    open()?.execute("DELETE FROM docs", [])?;
    for project in projects {
        index_project(project, cache)?;
    }
    Ok(projects.len())
}

/// Turns free text into an FTS5 query: every word must match, as a prefix.
pub fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"*", w.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

pub fn search(text: &str, project: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
    let Some(query) = fts_query(text) else {
        return Ok(Vec::new());
    };
    let conn = open()?;
    let sql = format!(
        "SELECT project, kind, reference, title,
                snippet(docs, 4, char(2), char(3), '…', 14)
         FROM docs WHERE docs MATCH ?1 {}
         ORDER BY bm25(docs, 0.0, 0.0, 0.0, 4.0, 1.0) LIMIT ?3",
        if project.is_some() {
            "AND project = ?2"
        } else {
            "AND ?2 IS NULL"
        }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![query, project, limit as i64], |row| {
        Ok(Hit {
            project: row.get(0)?,
            kind: row.get(1)?,
            reference: row.get(2)?,
            title: row.get(3)?,
            snippet: row.get::<_, String>(4)?.replace('\n', " "),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::TempHome;

    #[test]
    fn queries_match_every_word_as_a_prefix() {
        assert_eq!(
            fts_query("zlib flag").as_deref(),
            Some("\"zlib\"* \"flag\"*")
        );
        assert_eq!(
            fts_query("  \"quoted\" (x) ").as_deref(),
            Some("\"quoted\"* \"x\"*")
        );
        assert_eq!(fts_query(" - "), None);
    }

    #[test]
    fn notes_journals_and_items_are_found_per_project_or_everywhere() {
        let _home = TempHome::new("search");
        let a = store::create("Wire v2", None).unwrap();
        let b = store::create("Zip", None).unwrap();
        store::write_note(
            &a.slug,
            "Teams thread",
            "teams",
            "",
            "jdoe says the zlib flags are wrong",
        )
        .unwrap();
        store::append_journal(&b.slug, "frozen", "**Next:** retry with new zlib").unwrap();
        index_project(&a, &BTreeMap::new()).unwrap();
        index_project(&b, &BTreeMap::new()).unwrap();

        let everywhere = search("zlib", None, 10).unwrap();
        assert_eq!(everywhere.len(), 2, "{everywhere:?}");
        let only_a = search("zli", Some(&a.slug), 10).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].kind, "note");
        assert_eq!(only_a[0].title, "Teams thread");
        assert!(
            only_a[0]
                .snippet
                .contains(&format!("{MATCH_START}zlib{MATCH_END}")),
            "{}",
            only_a[0].snippet
        );
        assert!(search("nothing-like-this", None, 10).unwrap().is_empty());
    }
}
