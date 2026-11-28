use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use forge::config::{AdoConfig, GitLabConfig, optional_path, require, require_a_forge};
use serde::Deserialize;

const DEFAULT_CONFIG: &str = r#"# Fill in [ado], [gitlab], or both. ctx follows PRs, MRs and work items from the ones that
# are set.

# [ado]
# organization = "https://dev.azure.com/your-org"
# project = "YourProject"
# area_root = 'YourProject\Your Team'
# # The work item types ctx lists as yours, and the states in which they are finished.
# work_item_types = ["Bug", "Product Backlog Item"]
# done_states = ["Done", "Removed", "Closed"]

# [gitlab]
# host = "gitlab.example.com"
# group = "your-group"

[workspace]
# Whether the freeze dialog starts with "close the project's terminals" checked.
close_on_freeze = false
# Show a short summary of what changed when you switch to a project.
switch_message = true

[watch]
# Minutes between refreshes of the items in projects that are not done.
interval_minutes = 3

[live]
# The live group is highlighted when it holds more projects than this.
soft_limit = 5

[claude]
# The Claude config directory that summaries, briefs and sessions started with C run under.
# Leave it empty to use your environment's.
config_dir = ""
# The command that starts a new Claude session.
command = "claude"
# Claude model aliases or full names. Summaries after a freeze and briefs on a thaw use
# summary_model. ctx import uses import_model.
summary_model = "haiku"
import_model = "sonnet"
# r on a PR or MR asks respond_model to judge the threads that wait on you, plan fixes and
# draft replies.
respond_model = "opus"
# Write a suggested summary after each freeze.
summarize_on_freeze = true

[import]
# The tools ctx import lets Claude use to find a Teams thread or an email. These are the
# read-only tools of the claude.ai Microsoft 365 connector. An empty list turns import off.
tools = [
    "mcp__claude_ai_Microsoft_365__chat_message_search",
    "mcp__claude_ai_Microsoft_365__outlook_email_search",
    "mcp__claude_ai_Microsoft_365__read_resource",
    "mcp__claude_ai_Microsoft_365__teams_list_chats",
    "mcp__claude_ai_Microsoft_365__teams_list_teams",
    "mcp__claude_ai_Microsoft_365__teams_list_channels",
    "mcp__claude_ai_Microsoft_365__teams_list_channel_messages",
    "mcp__claude_ai_Microsoft_365__search_people",
    "mcp__claude_ai_Microsoft_365__get_me",
    "mcp__claude_ai_Microsoft_365__sharepoint_search",
]
"#;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ado: Option<AdoConfig>,
    #[serde(default)]
    pub gitlab: Option<GitLabConfig>,
    #[serde(default)]
    /// Read from `[workspace]`, or from `[tmux]` in configs written before the ctx app.
    #[serde(alias = "tmux")]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub watch: WatchConfig,
    #[serde(default)]
    pub live: LiveConfig,
    #[serde(default)]
    pub claude: ClaudeConfig,
    #[serde(default)]
    pub import: ImportConfig,
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

    /// The tools `ctx import` may use, or an error when import is turned off.
    pub fn import_tools(&self) -> Result<Vec<&str>> {
        if self.import.tools.is_empty() {
            bail!(
                "ctx import is off: [import] tools is empty in {}",
                self.path.display()
            );
        }
        Ok(self.import.tools.iter().map(String::as_str).collect())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub close_on_freeze: bool,
    pub switch_message: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        WorkspaceConfig {
            close_on_freeze: false,
            switch_message: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    pub interval_minutes: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            interval_minutes: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LiveConfig {
    pub soft_limit: usize,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig { soft_limit: 5 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub config_dir: String,
    pub command: String,
    pub summary_model: String,
    pub import_model: String,
    pub respond_model: String,
    pub summarize_on_freeze: bool,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        ClaudeConfig {
            config_dir: String::new(),
            command: "claude".into(),
            summary_model: "haiku".into(),
            import_model: "sonnet".into(),
            respond_model: "opus".into(),
            summarize_on_freeze: true,
        }
    }
}

impl ClaudeConfig {
    pub fn config_path(&self) -> Option<PathBuf> {
        optional_path(&self.config_dir)
    }

    /// Where `ctx skill --write` installs the ctx-update skill: under the configured Claude
    /// config directory, or the one Claude itself uses when none is configured.
    pub fn skill_path(&self) -> PathBuf {
        self.config_path()
            .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
            .join("skills")
            .join("ctx-update")
            .join("SKILL.md")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImportConfig {
    pub tools: Vec<String>,
}

impl Default for ImportConfig {
    fn default() -> Self {
        ImportConfig {
            tools: [
                "chat_message_search",
                "outlook_email_search",
                "read_resource",
                "teams_list_chats",
                "teams_list_teams",
                "teams_list_channels",
                "teams_list_channel_messages",
                "search_people",
                "get_me",
                "sharepoint_search",
            ]
            .iter()
            .map(|name| format!("mcp__claude_ai_Microsoft_365__{name}"))
            .collect(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CTX_CONFIG_HOME") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("ctx")
}

pub fn load() -> Result<Config> {
    let dir = config_dir();
    let path = dir.join("config.toml");
    if !path.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        fs::write(&path, DEFAULT_CONFIG)
            .with_context(|| format!("could not write {}", path.display()))?;
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
        assert!(!cfg.workspace.close_on_freeze);
        assert_eq!(cfg.watch.interval_minutes, 3);
        assert_eq!(cfg.live.soft_limit, 5);
        assert_eq!(cfg.claude.command, "claude");
        assert_eq!(cfg.claude.config_path(), None);
        assert_eq!(cfg.claude.summary_model, "haiku");
        assert_eq!(cfg.claude.import_model, "sonnet");
        assert_eq!(cfg.claude.respond_model, "opus");
    }

    #[test]
    fn a_tmux_section_from_an_older_config_still_counts() {
        let cfg = parse("[tmux]\nclose_on_freeze = true\n").unwrap();
        assert!(cfg.workspace.close_on_freeze);
    }

    #[test]
    fn the_template_lists_the_default_import_tools() {
        let cfg = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(cfg.import.tools, ImportConfig::default().tools);
        assert_eq!(cfg.import.tools.len(), 10);
    }

    #[test]
    fn optional_sections_take_defaults() {
        let cfg = parse(GITLAB).unwrap();
        assert_eq!(cfg.gitlab().unwrap().host, "gitlab.example.com");
        let err = cfg.ado().unwrap_err().to_string();
        assert!(err.starts_with("no [ado] section"), "{err}");
        assert!(cfg.workspace.switch_message);
        assert_eq!(cfg.watch.interval_minutes, 3);
        assert!(cfg.claude.summarize_on_freeze);
        assert_eq!(cfg.claude.command, "claude");
        assert_eq!(cfg.claude.config_path(), None);
        assert_eq!(cfg.claude.summary_model, "haiku");
        assert_eq!(cfg.claude.import_model, "sonnet");
        assert_eq!(cfg.claude.respond_model, "opus");
        assert_eq!(cfg.import_tools().unwrap().len(), 10);
    }

    #[test]
    fn an_empty_tool_list_turns_import_off() {
        let cfg = parse(&format!("{GITLAB}\n[import]\ntools = []\n")).unwrap();
        let err = cfg.import_tools().unwrap_err().to_string();
        assert!(err.starts_with("ctx import is off"), "{err}");
    }

    #[test]
    fn ado_work_item_filters_can_be_set() {
        let cfg = parse(
            r#"
[ado]
organization = "https://dev.azure.com/example-org"
project = "Project"
area_root = 'Project'
work_item_types = ["User Story"]
done_states = ["Closed"]
"#,
        )
        .unwrap();
        let ado = cfg.ado().unwrap();
        assert_eq!(ado.work_item_types, ["User Story"]);
        assert_eq!(ado.done_states, ["Closed"]);
    }

    #[test]
    fn the_skill_goes_under_the_configured_claude_dir() {
        let cfg = parse(&format!(
            "{GITLAB}\n[claude]\nconfig_dir = \"~/.claude-work\"\n"
        ))
        .unwrap();
        assert_eq!(
            cfg.claude.skill_path(),
            forge::config::expand_home("~/.claude-work/skills/ctx-update/SKILL.md")
        );
    }
}
