//! Claude summaries. Each runs in its own background process, writes its result to the project,
//! and never blocks a switch.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use forge::claude::{self, Request};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cache;
use crate::config::Config;
use crate::item::{Ci, ItemRef};
use crate::ops;
use crate::store::{self, MarkKind, Project};

/// A thaw with at least this many changes gets a brief.
pub const MIN_CHANGES: usize = 4;

const TEXT_LIMIT: usize = 6000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brief {
    pub time: DateTime<Utc>,
    pub text: String,
}

fn brief_path(slug: &str) -> std::path::PathBuf {
    store::project_dir(slug).join("brief.json")
}

pub fn load_brief(slug: &str) -> Option<Brief> {
    serde_json::from_slice(&std::fs::read(brief_path(slug)).ok()?).ok()
}

fn clip(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    forge::exec::run_with(
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args),
        None,
    )
    .unwrap_or_default()
}

/// The project as text for a prompt: handoff, items with their state, and recent branch work.
fn context(project: &Project, since: Option<DateTime<Utc>>) -> String {
    let cache = cache::load();
    let mut out = format!(
        "Project: {}\nState: {}\n",
        project.name,
        project.state.label()
    );
    if !project.next.is_empty() {
        let _ = writeln!(out, "Next step: {}", project.next);
    }
    if let Some(h) = &project.handoff {
        let _ = writeln!(
            out,
            "\nLast handoff ({}):\n{}",
            h.time
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M"),
            store::handoff_body(h)
        );
    }
    let _ = writeln!(out, "\nItems:");
    for item in project.refs() {
        match cache.items.get(&item.key()) {
            Some(s) => {
                let _ = writeln!(
                    out,
                    "- {} {}: {} [{}], {} comments",
                    item.kind_label(),
                    item.label(),
                    s.title,
                    s.status,
                    s.comments
                );
                let mut flags = Vec::new();
                if s.conflicts {
                    flags.push("merge conflicts".to_string());
                }
                match s.ci {
                    Some(Ci::Passed) => flags.push("build passed".into()),
                    Some(Ci::Failed) => flags.push("build failed".into()),
                    Some(Ci::Running) => flags.push("build running".into()),
                    None => {}
                }
                if s.replies_due > 0 {
                    flags.push(format!("{} threads wait for my reply", s.replies_due));
                }
                if !flags.is_empty() {
                    let _ = writeln!(out, "  {}", flags.join(", "));
                }
                if let Some(c) = &s.last_comment {
                    let _ = writeln!(
                        out,
                        "  last comment by {} on {}: {}",
                        c.author,
                        c.date.format("%Y-%m-%d"),
                        c.snippet
                    );
                }
            }
            None => {
                let _ = writeln!(out, "- {} {}", item.kind_label(), item.label());
            }
        }
        if let ItemRef::Branch { repo, branch } = item {
            let since_arg = since.map(|t| format!("--since={}", t.to_rfc3339()));
            let mut args = vec!["log", "--format=%h %cI %s", "-15", branch.as_str()];
            if let Some(s) = &since_arg {
                args.push(s);
            }
            let log = git(repo, &args);
            if !log.trim().is_empty() {
                let _ = writeln!(out, "  recent commits:\n{}", indent(&log));
            }
            let stat = git(repo, &["diff", "--stat", "HEAD"]);
            if !stat.trim().is_empty() {
                let _ = writeln!(out, "  uncommitted changes:\n{}", indent(clip(&stat, 1500)));
            }
        }
    }
    clip(&out, TEXT_LIMIT * 2).to_string()
}

fn indent(text: &str) -> String {
    text.lines().map(|l| format!("    {l}\n")).collect()
}

fn ask(
    cfg: &Config,
    model: &str,
    dir: &Path,
    schema: serde_json::Value,
    tools: &[&str],
    prompt: &str,
) -> Result<serde_json::Value> {
    let out = claude::ask(&Request {
        command: &cfg.claude.command,
        config_dir: cfg.claude.config_path(),
        model,
        dir,
        schema,
        tools,
        prompt,
    })?;
    claude::structured(&out)
}

/// Summarizes a project after a freeze and adds the summary to the journal as a suggestion.
pub fn summarize(cfg: &Config, slug: &str) -> Result<()> {
    let project = store::load(slug)?;
    let previous = store::journal_entries(&store::journal(slug))
        .iter()
        .rev()
        .filter(|(h, _)| h.ends_with("· frozen"))
        .nth(1)
        .and_then(|(h, _)| {
            chrono::NaiveDateTime::parse_from_str(h.get(..16)?, "%Y-%m-%d %H:%M").ok()
        })
        .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
        .map(|t| t.with_timezone(&Utc));
    let prompt = format!(
        "The user just set this project aside. Using only the facts below, write a short summary \
         of where the work stands, in two to four plain sentences, and suggest the next step in one \
         sentence. Do not invent details.\n\n{}",
        context(&project, previous)
    );
    let schema = json!({
        "type": "object",
        "properties": { "summary": { "type": "string" }, "next": { "type": "string" } },
        "required": ["summary", "next"]
    });
    let dir = store::project_dir(slug);
    let value = ask(cfg, &cfg.claude.summary_model, &dir, schema, &[], &prompt)?;
    let summary = value["summary"].as_str().unwrap_or_default().trim();
    let next = value["next"].as_str().unwrap_or_default().trim();
    if summary.is_empty() {
        return Ok(());
    }
    let mut body = format!("{summary}\n");
    if !next.is_empty() {
        body.push_str(&format!("\n**Suggested next:** {next}\n"));
    }
    store::append_journal(slug, "summary from Claude", &body)?;
    store::commit(&format!("Summarize {slug}"));
    ops::reindex_project(slug);
    Ok(())
}

/// Writes a short brief of what changed while a project was frozen.
pub fn brief(cfg: &Config, slug: &str) -> Result<()> {
    let project = store::load(slug)?;
    let cache = cache::load();
    let snapshot = store::load_mark(slug, MarkKind::Snapshot);
    let found = ops::since(&project, &cache.items, &store::notes(slug));
    let mut changes = String::new();
    for c in &found.changes {
        let _ = writeln!(changes, "- {}: {}", c.label, c.text);
    }
    let prompt = format!(
        "The user is returning to this project after a break. In three sentences or fewer, tell \
         them what changed and what most likely needs their attention first. Use only the facts \
         below.\n\nChanges since they left:\n{changes}\n{}",
        context(&project, snapshot.time)
    );
    let schema = json!({
        "type": "object",
        "properties": { "brief": { "type": "string" } },
        "required": ["brief"]
    });
    let value = ask(
        cfg,
        &cfg.claude.summary_model,
        &store::project_dir(slug),
        schema,
        &[],
        &prompt,
    )?;
    let text = value["brief"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !text.is_empty() {
        let brief = Brief {
            time: Utc::now(),
            text,
        };
        store::write_atomic(&brief_path(slug), &serde_json::to_vec(&brief)?)?;
    }
    Ok(())
}

pub struct Imported {
    pub title: String,
    pub source: String,
    pub link: String,
    pub path: std::path::PathBuf,
}

/// Finds a Teams thread or an email by link or search words through the Microsoft 365
/// connector, and saves a summary of it as a note.
pub fn import(cfg: &Config, slug: &str, query: &str) -> Result<Imported> {
    let tools = cfg.import_tools()?;
    let project = store::load(slug)?;
    let prompt = format!(
        "Find the Microsoft Teams conversation or Outlook email described below, using the \
         Microsoft 365 tools. If it is a link, open it. If it is a description, search for the \
         best match. Then summarize it for the project \"{}\": who said what, decisions, open \
         questions and anything asked of the user. Keep names and dates. Quote short key \
         passages exactly. If nothing matches, set found to false.\n\nWhat to find: {query}",
        project.name
    );
    let schema = json!({
        "type": "object",
        "properties": {
            "found": { "type": "boolean" },
            "title": { "type": "string" },
            "source": { "type": "string", "enum": ["teams", "email"] },
            "link": { "type": "string" },
            "summary": { "type": "string" }
        },
        "required": ["found", "title", "source", "link", "summary"]
    });
    let dir = store::project_dir(slug);
    let value = ask(cfg, &cfg.claude.import_model, &dir, schema, &tools, &prompt)?;
    if !value["found"].as_bool().unwrap_or(false) {
        anyhow::bail!("Claude found nothing in Microsoft 365 matching {query}");
    }
    let field = |name: &str| value[name].as_str().unwrap_or_default().trim().to_string();
    let (title, source, link, summary) = (
        field("title"),
        field("source"),
        field("link"),
        field("summary"),
    );
    let path = ops::capture(
        slug,
        &title,
        if source.is_empty() { "teams" } else { &source },
        &link,
        &summary,
    )
    .context("could not save the imported note")?;
    Ok(Imported {
        title,
        source,
        link,
        path,
    })
}
