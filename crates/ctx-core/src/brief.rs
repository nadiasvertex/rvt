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

/// What `draft_handoff` is doing, for a progress display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    ReadingClaude,
    ReadingProject,
    Writing,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::ReadingClaude => "Reading the Claude sessions",
            Stage::ReadingProject => "Reading the journal and items",
            Stage::Writing => "Writing the handoff",
        }
    }

    /// How far along the draft is, from 0 to 1, when this stage starts.
    pub fn progress(self) -> f32 {
        match self {
            Stage::ReadingClaude => 0.1,
            Stage::ReadingProject => 0.3,
            Stage::Writing => 0.5,
        }
    }
}

/// A handoff drafted by Claude for the user to check before freezing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub doing: String,
    pub next: String,
    /// A wait condition in the form `ctx wait` takes, checked by its parser.
    pub wait: Option<String>,
}

/// How much of each Claude transcript goes into the prompt, from its end.
const TRANSCRIPT_LIMIT: usize = 6000;
/// How much of a transcript file is read, from its end.
const TRANSCRIPT_TAIL: u64 = 512 * 1024;

/// Drafts Doing, Next and a wait condition for freezing the project, from the same project text
/// the brief uses, the recent journal and the project's running Claude sessions. `stage` hears
/// each stage as it starts.
pub fn draft_handoff(cfg: &Config, slug: &str, stage: &dyn Fn(Stage)) -> Result<Draft> {
    let project = store::load(slug)?;
    stage(Stage::ReadingClaude);
    let mut sessions = String::new();
    for (session, live) in ops::live_claudes(cfg, &project) {
        let Some(path) = session.transcript() else {
            continue;
        };
        let text = transcript_text(&read_tail(&path, TRANSCRIPT_TAIL), TRANSCRIPT_LIMIT);
        if !text.is_empty() {
            let name = live.name.unwrap_or_else(|| session.label());
            let _ = writeln!(sessions, "\nClaude session {name}, latest messages:\n{text}");
        }
    }
    stage(Stage::ReadingProject);
    let mut journal: Vec<String> = store::journal_entries(&store::journal(slug))
        .into_iter()
        .rev()
        .take(8)
        .map(|(heading, body)| format!("## {heading}\n{}", clip(&body, 800)))
        .collect();
    journal.reverse();
    let prompt = format!(
        "The user is setting this project aside and needs a handoff for when they come back. \
         Using only the facts below, write what they were doing, in one or two plain sentences, \
         and the next step, in one sentence. If the project is clearly waiting on something, \
         suggest a wait condition in one of these forms, using an item label from the list: \
         `review <item>`, `closed <item>`, `ci <item>`, `state <item> <state>`, `until <30m, 2h, \
         3d, a weekday or YYYY-MM-DD>`, or a short free-text description. Otherwise leave wait \
         empty. Do not invent details.\n\n{}\nRecent journal:\n{}\n{}",
        context(&project, None),
        journal.join("\n"),
        sessions
    );
    let schema = json!({
        "type": "object",
        "properties": {
            "doing": { "type": "string" },
            "next": { "type": "string" },
            "wait": { "type": "string" }
        },
        "required": ["doing", "next", "wait"]
    });
    stage(Stage::Writing);
    let value = ask(
        cfg,
        &cfg.claude.summary_model,
        &store::project_dir(slug),
        schema,
        &[],
        &prompt,
    )?;
    let field = |name: &str| value[name].as_str().unwrap_or_default().trim().to_string();
    Ok(Draft {
        doing: field("doing"),
        next: field("next"),
        wait: checked_wait(&project, &field("wait")),
    })
}

/// The suggested wait condition, if `ctx wait`'s parser takes it.
fn checked_wait(project: &Project, text: &str) -> Option<String> {
    let text = text.trim().trim_matches('`').trim();
    if text.is_empty() {
        return None;
    }
    let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    crate::wait::parse_shell(&words, |arg| ops::find_item(project, arg))
        .ok()
        .map(|_| text.to_string())
}

fn read_tail(path: &Path, bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(bytes)));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The user's and Claude's text messages from the end of a transcript, oldest first, at most
/// `limit` characters, but always the last one. Tool calls and results are left out.
fn transcript_text(jsonl: &str, limit: usize) -> String {
    let mut messages = Vec::new();
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let who = match value["type"].as_str() {
            Some("user") => "User",
            Some("assistant") => "Claude",
            _ => continue,
        };
        let text = match &value["message"]["content"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        let text = text.trim();
        if !text.is_empty() {
            messages.push(format!("{who}: {}", clip(text, 1500)));
        }
    }
    let mut out = Vec::new();
    let mut used = 0;
    for m in messages.iter().rev() {
        used += m.chars().count();
        if used > limit && !out.is_empty() {
            break;
        }
        out.push(m.as_str());
    }
    out.reverse();
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entry, State};

    #[test]
    fn a_transcript_gives_its_latest_text_messages() {
        let jsonl = [
            r#"{"type":"user","message":{"role":"user","content":"fix the parser"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixed it."}]}}"#,
            r#"{"type":"cost-state","sessionId":"x"}"#,
            "not json",
        ]
        .join("\n");
        assert_eq!(
            transcript_text(&jsonl, 1000),
            "User: fix the parser\nClaude: Fixed it."
        );
        assert_eq!(transcript_text(&jsonl, 5), "Claude: Fixed it.");
    }

    #[test]
    fn a_suggested_wait_is_kept_only_when_it_parses() {
        let project = Project {
            name: "p".into(),
            slug: "p".into(),
            state: State::Live,
            created: Utc::now(),
            workspace: "p".into(),
            dir: None,
            next: String::new(),
            handoff: None,
            wait: None,
            claude_sessions: Vec::new(),
            dismissed: Vec::new(),
            layout: Vec::new(),
            items: vec![Entry {
                added: Utc::now(),
                item: ItemRef::AdoPr { id: 4711 },
            }],
        };
        assert_eq!(
            checked_wait(&project, "`closed 4711`").as_deref(),
            Some("closed 4711")
        );
        assert_eq!(checked_wait(&project, "closed 99"), None);
        assert_eq!(checked_wait(&project, "until soonish"), None);
        assert_eq!(
            checked_wait(&project, "answer from jdoe").as_deref(),
            Some("answer from jdoe")
        );
        assert_eq!(checked_wait(&project, "  "), None);
    }
}
