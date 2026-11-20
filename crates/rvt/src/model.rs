use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use forge::model::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub const ALL: [Severity; 3] = [Severity::High, Severity::Medium, Severity::Low];

    pub fn label(self) -> &'static str {
        match self {
            Severity::High => "HIGH",
            Severity::Medium => "MED",
            Severity::Low => "LOW",
        }
    }

    pub fn letter(self) -> char {
        self.label().chars().next().unwrap_or('?')
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub file: Option<String>,
    /// A line on the new side of the diff.
    pub line: Option<u32>,
    pub title: String,
    pub body: String,
    /// The file and line fall inside the diff, so the finding can be posted on that line.
    #[serde(default)]
    pub inline: bool,
    #[serde(default)]
    pub selected: bool,
    /// Already posted to the PR, so it is not posted again.
    #[serde(default)]
    pub posted: bool,
}

impl Finding {
    pub fn location(&self) -> Option<String> {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => Some(format!("{file}:{line}")),
            (Some(file), None) => Some(file.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub key: String,
    pub head_sha: String,
    pub base_sha: String,
    pub created: DateTime<Utc>,
    pub findings: Vec<Finding>,
}

/// Why a CI run failed. Trivial failures are lint, build, or infrastructure problems that the
/// author can usually fix without a diagnosis; test failures carry a suggested fix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Cause {
    Trivial { summary: String },
    Test { cause: String, fix: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub key: String,
    pub head_sha: String,
    pub created: DateTime<Utc>,
    /// Names of the failed jobs, which may sit in a child pipeline.
    pub jobs: Vec<String>,
    pub cause: Cause,
}

impl Diagnosis {
    /// The item has new commits since this diagnosis ran.
    pub fn is_stale(&self, item: &Item) -> bool {
        item.head_sha
            .as_deref()
            .is_some_and(|head| head != self.head_sha)
    }
}

impl Review {
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }

    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).min()
    }

    pub fn any_posted(&self) -> bool {
        self.findings.iter().any(|f| f.posted)
    }

    /// The item has new commits since this review ran.
    pub fn is_stale(&self, item: &Item) -> bool {
        item.head_sha
            .as_deref()
            .is_some_and(|head| head != self.head_sha)
    }

    /// Counts such as `2H 3M 1L`, leaving out severities with no findings.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = Severity::ALL
            .iter()
            .filter(|s| self.count(**s) > 0)
            .map(|s| format!("{}{}", self.count(*s), s.letter()))
            .collect();
        if parts.is_empty() {
            "no findings".into()
        } else {
            parts.join(" ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(severity: Severity) -> Finding {
        Finding {
            severity,
            file: None,
            line: None,
            title: String::new(),
            body: String::new(),
            inline: false,
            selected: false,
            posted: false,
        }
    }

    #[test]
    fn review_summary_counts_severities_present() {
        let mut review = Review {
            key: "k".into(),
            head_sha: "abc".into(),
            base_sha: "def".into(),
            created: Utc::now(),
            findings: vec![
                finding(Severity::Low),
                finding(Severity::High),
                finding(Severity::High),
            ],
        };
        assert_eq!(review.summary(), "2H 1L");
        assert_eq!(review.worst(), Some(Severity::High));
        review.findings.clear();
        assert_eq!(review.summary(), "no findings");
        assert_eq!(review.worst(), None);
    }
}
