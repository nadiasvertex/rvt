use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

pub use forge::config::{AdoConfig, GitLabConfig, expand_home};
use forge::config::{optional_path, require, require_a_forge};
use forge::model::Source;

const DEFAULT_CONFIG: &str = r#"# Fill in [ado], [gitlab], or both. rvt shows PRs and MRs from the ones that are set.

# [ado]
# organization = "https://dev.azure.com/your-org"
# project = "YourProject"
# # PRs are listed when a linked work item is under this area path.
# area_root = 'YourProject\Your Team'
# # Work items changed within this many days are searched for PR links.
# lookback_days = 365
# # The SSH host that repositories are cloned from.
# ssh_host = "ssh.dev.azure.com"

# [gitlab]
# host = "gitlab.example.com"
# group = "your-group"

[board]
# Refresh the board this often, in minutes. 0 turns the timed refresh off.
refresh_minutes = 5

[review]
# Instructions Claude follows for every review. rvt adds the commits and output format.
prompt_file = "~/.config/rvt/review-prompt.md"
max_parallel = 2
# A claude model alias or full name.
model = "opus"
# The program that runs Claude.
claude_command = "claude"
# The Claude config directory reviews run under. Leave it empty to use your environment's.
claude_config_dir = ""

[diagnose]
# Sorts a failed CI run into a trivial failure or a test failure.
triage_model = "haiku"
# Explains a test failure and suggests a fix.
fix_model = "sonnet"

# Place a repository's PRs in an area when no linked work item names one.
# Example: "billing-service" = 'YourProject\Your Team\Billing'
[repo_areas]
"#;

const DEFAULT_PROMPT: &str = include_str!("review-prompt.md");

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ado: Option<AdoConfig>,
    #[serde(default)]
    pub gitlab: Option<GitLabConfig>,
    #[serde(default)]
    pub board: BoardConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub diagnose: DiagnoseConfig,
    #[serde(default)]
    pub repo_areas: HashMap<String, String>,
    /// The file this config was read from, for error messages.
    #[serde(skip)]
    pub path: PathBuf,
}

impl Config {
    pub fn ado(&self) -> Result<&AdoConfig> {
        require(&self.ado, "ado", &self.path)
    }

    pub fn gitlab(&self) -> Result<&GitLabConfig> {
        require(&self.gitlab, "gitlab", &self.path)
    }

    /// The forges this config names, in board order.
    pub fn sources(&self) -> Vec<Source> {
        let mut sources = Vec::new();
        if self.ado.is_some() {
            sources.push(Source::Ado);
        }
        if self.gitlab.is_some() {
            sources.push(Source::GitLab);
        }
        sources
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BoardConfig {
    pub refresh_minutes: u64,
}

impl Default for BoardConfig {
    fn default() -> Self {
        BoardConfig { refresh_minutes: 5 }
    }
}

impl BoardConfig {
    pub fn refresh_interval(&self) -> Option<Duration> {
        (self.refresh_minutes > 0).then(|| Duration::from_secs(self.refresh_minutes * 60))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    pub prompt_file: String,
    pub max_parallel: usize,
    pub model: String,
    pub claude_command: String,
    pub claude_config_dir: String,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        ReviewConfig {
            prompt_file: "~/.config/rvt/review-prompt.md".into(),
            max_parallel: 2,
            model: "opus".into(),
            claude_command: "claude".into(),
            claude_config_dir: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiagnoseConfig {
    pub triage_model: String,
    pub fix_model: String,
}

impl Default for DiagnoseConfig {
    fn default() -> Self {
        DiagnoseConfig {
            triage_model: "haiku".into(),
            fix_model: "sonnet".into(),
        }
    }
}

impl ReviewConfig {
    pub fn prompt_path(&self) -> PathBuf {
        expand_home(&self.prompt_file)
    }

    pub fn claude_config_path(&self) -> Option<PathBuf> {
        optional_path(&self.claude_config_dir)
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RVT_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("rvt")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_default().join("rvt")
}

pub fn load() -> Result<Config> {
    let dir = config_dir();
    let path = dir.join("config.toml");
    if !path.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        fs::write(&path, DEFAULT_CONFIG)
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    let prompt = dir.join("review-prompt.md");
    if !prompt.exists() {
        fs::write(&prompt, DEFAULT_PROMPT)
            .with_context(|| format!("could not write {}", prompt.display()))?;
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
    let mut cfg = parse(&text).with_context(|| format!("invalid {}", path.display()))?;
    require_a_forge(cfg.ado.is_some(), cfg.gitlab.is_some(), &path)?;
    cfg.path = path;
    Ok(cfg)
}

pub fn parse(text: &str) -> Result<Config> {
    Ok(toml::from_str(text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADO: &str = r#"
[ado]
organization = "https://dev.azure.com/example-org"
project = "Project"
area_root = 'Project\Team'
"#;

    const GITLAB: &str = r#"
[gitlab]
host = "gitlab.example.com"
group = "group"
"#;

    #[test]
    fn default_config_parses_without_a_forge() {
        let cfg = parse(DEFAULT_CONFIG).unwrap();
        assert!(cfg.ado.is_none());
        assert!(cfg.gitlab.is_none());
        assert!(cfg.repo_areas.is_empty());
        assert_eq!(cfg.review.max_parallel, 2);
        assert_eq!(cfg.review.claude_command, "claude");
        assert_eq!(cfg.review.claude_config_path(), None);
        assert_eq!(
            cfg.board.refresh_interval(),
            Some(Duration::from_secs(5 * 60))
        );
        assert_eq!(cfg.diagnose.triage_model, "haiku");
        assert_eq!(cfg.diagnose.fix_model, "sonnet");
        assert_eq!(
            cfg.review.prompt_path(),
            expand_home("~/.config/rvt/review-prompt.md")
        );
    }

    #[test]
    fn the_template_names_no_organization() {
        let uncommented: String = DEFAULT_CONFIG
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        assert!(!uncommented.contains("[ado]"), "{uncommented}");
        assert!(!uncommented.contains("[gitlab]"), "{uncommented}");
    }

    #[test]
    fn each_forge_is_optional() {
        let cfg = parse(&format!("{ADO}{DEFAULT_CONFIG}")).unwrap();
        assert_eq!(cfg.ado().unwrap().area_root, r"Project\Team");
        assert!(cfg.gitlab().is_err());
        assert_eq!(cfg.sources(), [Source::Ado]);

        let cfg = parse(&format!("{GITLAB}{DEFAULT_CONFIG}")).unwrap();
        assert_eq!(cfg.gitlab().unwrap().group, "group");
        let err = cfg.ado().unwrap_err().to_string();
        assert!(err.starts_with("no [ado] section"), "{err}");
        assert_eq!(cfg.sources(), [Source::GitLab]);

        let cfg = parse(&format!("{ADO}{GITLAB}{DEFAULT_CONFIG}")).unwrap();
        assert_eq!(cfg.sources(), [Source::Ado, Source::GitLab]);
    }

    #[test]
    fn review_section_is_optional() {
        let without = DEFAULT_CONFIG.split("[review]").next().unwrap();
        let cfg = parse(&format!("{ADO}{without}")).unwrap();
        assert_eq!(cfg.review.max_parallel, 2);
        assert_eq!(cfg.review.model, "opus");
        assert_eq!(cfg.review.claude_command, "claude");
        assert_eq!(cfg.review.claude_config_path(), None);
    }

    #[test]
    fn a_configured_claude_dir_expands_home() {
        let cfg = parse(&DEFAULT_CONFIG.replace(
            r#"claude_config_dir = """#,
            r#"claude_config_dir = "~/.claude-work""#,
        ))
        .unwrap();
        assert_eq!(
            cfg.review.claude_config_path(),
            Some(expand_home("~/.claude-work"))
        );
    }

    #[test]
    fn board_section_is_optional() {
        let without = DEFAULT_CONFIG
            .replace("[board]\n", "")
            .replace("refresh_minutes = 5\n", "");
        assert!(!without.contains("refresh_minutes"));
        let cfg = parse(&without).unwrap();
        assert_eq!(cfg.board.refresh_minutes, 5);
    }

    #[test]
    fn zero_minutes_turns_the_timed_refresh_off() {
        let cfg =
            parse(&DEFAULT_CONFIG.replace("refresh_minutes = 5", "refresh_minutes = 0")).unwrap();
        assert_eq!(cfg.board.refresh_interval(), None);
        let cfg =
            parse(&DEFAULT_CONFIG.replace("refresh_minutes = 5", "refresh_minutes = 2")).unwrap();
        assert_eq!(cfg.board.refresh_interval(), Some(Duration::from_secs(120)));
    }
}
