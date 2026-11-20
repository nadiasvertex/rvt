//! Explains why a GitLab merge request's CI failed. A small model sorts the failure as trivial or
//! as a test failure. A test failure is then diagnosed by a larger model in a checkout of the
//! merge request head.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::claude::{self, READ_ONLY_TOOLS};
use crate::config::{self, Config};
use crate::gitlab::{self, FailedJob};
use crate::model::{Cause, Diagnosis, Item, Source};
use crate::review::{self, safe_name};

const MAX_LINES_PER_JOB: usize = 300;
const MAX_LOG_BYTES: usize = 60_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Trivial,
    Test,
}

#[derive(Debug, Deserialize)]
struct Triage {
    kind: Kind,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct Fix {
    cause: String,
    fix: String,
}

/// Diagnoses the failed CI run of `item`. Test failures are read in a checkout of the head.
pub fn run(cfg: &Config, item: &Item, progress: &dyn Fn(&str)) -> Result<Diagnosis> {
    if item.source != Source::GitLab {
        bail!("CI diagnosis is only available for GitLab merge requests");
    }
    progress("reading logs");
    let jobs = gitlab::failed_jobs(cfg.gitlab()?, item)?;
    if jobs.is_empty() {
        bail!("no failed jobs were found");
    }
    let logs = render_logs(&jobs);

    progress("triaging");
    let triage: Triage =
        serde_json::from_value(claude::structured(&claude::ask(&claude::Request {
            command: &cfg.review.claude_command,
            config_dir: cfg.review.claude_config_path(),
            model: &cfg.diagnose.triage_model,
            dir: &config::config_dir(),
            schema: triage_schema(),
            tools: &[],
            prompt: &triage_prompt(item, &logs),
        })?)?)
        .context("claude returned no triage")?;

    let cause = match triage.kind {
        Kind::Trivial => Cause::Trivial {
            summary: triage.summary,
        },
        Kind::Test => {
            let checkout = review::checkout(cfg, item, progress)?;
            progress("diagnosing");
            let fix: Fix =
                serde_json::from_value(claude::structured(&claude::ask(&claude::Request {
                    command: &cfg.review.claude_command,
                    config_dir: cfg.review.claude_config_path(),
                    model: &cfg.diagnose.fix_model,
                    dir: &checkout.worktree,
                    schema: fix_schema(),
                    tools: &READ_ONLY_TOOLS,
                    prompt: &fix_prompt(item, &logs),
                })?)?)
                .context("claude returned no diagnosis")?;
            Cause::Test {
                cause: fix.cause,
                fix: fix.fix,
            }
        }
    };

    Ok(Diagnosis {
        key: item.key(),
        head_sha: item.head_sha.clone().unwrap_or_default(),
        created: Utc::now(),
        jobs: jobs.into_iter().map(|j| j.name).collect(),
        cause,
    })
}

fn triage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "enum": ["trivial", "test"] },
            "summary": { "type": "string" }
        },
        "required": ["kind", "summary"]
    })
}

fn fix_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cause": { "type": "string" },
            "fix": { "type": "string" }
        },
        "required": ["cause", "fix"]
    })
}

fn triage_prompt(item: &Item, logs: &str) -> String {
    format!(
        "The CI pipeline for merge request {reference} in {repo}, \"{title}\", failed. The logs \
         of the failed jobs follow, each trimmed to its last lines.\n\n\
         Decide which kind of failure this is:\n\
         - test: a test ran and failed, for example an assertion that did not hold or a test \
         that crashed.\n\
         - trivial: anything else, for example a lint or formatting error, a build or dependency \
         error, a missing tool, or an infrastructure problem such as a runner, network, registry, \
         or timeout.\n\n\
         Give a summary of one or two plain sentences that says what failed and why.\n\n\
         ## Logs\n\n{logs}",
        reference = item.reference(),
        repo = item.repo,
        title = item.title,
    )
}

fn fix_prompt(item: &Item, logs: &str) -> String {
    format!(
        "The CI pipeline for merge request {reference} in {repo}, \"{title}\", failed with a test \
         failure. The working directory is a checkout of the merge request's head commit.\n\n\
         Find the cause in the code. Read the failing test and the code it exercises. Do not \
         modify any files.\n\n\
         Give the cause in plain sentences that name the files and functions involved. Give the \
         fix as the concrete change to make, naming the file and function.\n\n\
         ## Logs\n\n{logs}",
        reference = item.reference(),
        repo = item.repo,
        title = item.title,
    )
}

/// The end of each failed job's log, with terminal escapes removed.
fn render_logs(jobs: &[FailedJob]) -> String {
    let per_job = MAX_LOG_BYTES / jobs.len().max(1);
    jobs.iter()
        .map(|job| {
            let text = tail(&strip_ansi(&job.trace), MAX_LINES_PER_JOB, per_job);
            format!("### {}\n\n{text}", job.name)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The last `max_lines` lines of `text`, then the last `max_bytes` bytes of those.
fn tail(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let joined = lines[lines.len().saturating_sub(max_lines)..].join("\n");
    if joined.len() <= max_bytes {
        return joined;
    }
    let mut cut = joined.len() - max_bytes;
    while !joined.is_char_boundary(cut) {
        cut += 1;
    }
    joined[cut..].to_string()
}

/// Removes ANSI escape sequences and carriage returns from a job log.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

pub fn diagnoses_dir() -> PathBuf {
    config::cache_dir().join("diagnoses")
}

fn store_path(key: &str) -> PathBuf {
    diagnoses_dir().join(format!("{}.json", safe_name(key)))
}

pub fn save(diagnosis: &Diagnosis) -> Result<()> {
    let path = store_path(&diagnosis.key);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(diagnosis)?)
        .with_context(|| format!("could not write {}", path.display()))
}

pub fn load_all() -> Vec<Diagnosis> {
    let Ok(entries) = fs::read_dir(diagnoses_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_codes_and_carriage_returns_are_removed() {
        let raw = "\x1b[0Ksection_start:1:step\r\n\x1b[31;1mERROR\x1b[0m: failed\r\n";
        assert_eq!(strip_ansi(raw), "section_start:1:step\nERROR: failed\n");
    }

    #[test]
    fn tail_keeps_the_last_lines_within_the_byte_budget() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(tail(text, 2, 100), "d\ne");
        assert_eq!(tail(text, 10, 3), "d\ne");
    }

    #[test]
    fn tail_never_splits_a_character() {
        let text = "ééé";
        assert_eq!(tail(text, 10, 3), "é");
        assert_eq!(tail(text, 10, 4), "éé");
    }

    #[test]
    fn logs_are_labelled_by_job() {
        let jobs = vec![
            FailedJob {
                name: "unit".into(),
                trace: "one\ntwo".into(),
            },
            FailedJob {
                name: "lint".into(),
                trace: "bad".into(),
            },
        ];
        let logs = render_logs(&jobs);
        assert!(logs.starts_with("### unit\n\n"));
        assert!(logs.contains("### lint\n\nbad"));
    }

    #[test]
    fn triage_output_reads_either_kind() {
        let stdout = r#"{"structured_output":{"kind":"test","summary":"x"}}"#;
        let triage: Triage = serde_json::from_value(claude::structured(stdout).unwrap()).unwrap();
        assert!(matches!(triage.kind, Kind::Test));
        let stdout = r#"{"structured_output":{"kind":"trivial","summary":"x"}}"#;
        let triage: Triage = serde_json::from_value(claude::structured(stdout).unwrap()).unwrap();
        assert!(matches!(triage.kind, Kind::Trivial));
    }
}
