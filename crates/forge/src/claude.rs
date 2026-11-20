use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::exec;

pub const READ_ONLY_TOOLS: [&str; 6] = [
    "Read",
    "Grep",
    "Glob",
    "Bash(git diff:*)",
    "Bash(git log:*)",
    "Bash(git show:*)",
];
const DENIED_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];

/// One `claude -p` call. The prompt goes over stdin and the answer must match `schema`.
pub struct Request<'a> {
    /// The program to run, such as `claude`.
    pub command: &'a str,
    /// The `CLAUDE_CONFIG_DIR` to run under. `None` keeps the inherited environment.
    pub config_dir: Option<PathBuf>,
    pub model: &'a str,
    pub dir: &'a Path,
    pub schema: Value,
    pub tools: &'a [&'a str],
    pub prompt: &'a str,
}

fn command(req: &Request) -> Command {
    let schema = req.schema.to_string();
    let mut cmd = Command::new(req.command);
    cmd.current_dir(req.dir)
        .args(["-p", "--output-format", "json", "--no-session-persistence"])
        .args(["--json-schema", &schema])
        .args(["--permission-mode", "dontAsk"]);
    if !req.tools.is_empty() {
        cmd.arg("--allowedTools").args(req.tools);
    }
    cmd.arg("--disallowedTools").args(DENIED_TOOLS);
    // A shell alias that sets these variables does not reach a child process, so the config
    // names the directory and it is set here.
    if let Some(dir) = &req.config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    cmd.env("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD", "1");
    if !req.model.trim().is_empty() {
        cmd.args(["--model", req.model.trim()]);
    }
    cmd
}

/// Runs the request and returns the raw JSON that `claude --output-format json` printed.
pub fn ask(req: &Request) -> Result<String> {
    let mut cmd = command(req);
    let output = exec::capture(&mut cmd, Some(req.prompt))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() && stdout.trim().is_empty() {
        bail!("claude failed: {}", exec::first_line(&output.stderr));
    }
    Ok(stdout)
}

#[derive(Debug, Deserialize)]
struct Output {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    structured_output: Option<Value>,
}

/// The structured answer in `claude -p --output-format json` output. Falls back to parsing the
/// result text when the structured field is missing.
pub fn structured(stdout: &str) -> Result<Value> {
    let output: Output =
        serde_json::from_str(stdout.trim()).context("claude did not return JSON")?;
    if output.is_error {
        bail!(
            "claude failed: {}",
            output
                .result
                .or(output.subtype)
                .unwrap_or_else(|| "no reason given".into())
        );
    }
    match output.structured_output {
        Some(value) => Ok(value),
        None => output
            .result
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok())
            .context("claude returned no structured output"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(config_dir: Option<PathBuf>) -> Request<'static> {
        Request {
            command: "my-claude",
            config_dir,
            model: "",
            dir: Path::new("/"),
            schema: Value::Null,
            tools: &[],
            prompt: "",
        }
    }

    fn config_dir_env(cmd: &Command) -> Option<Option<&std::ffi::OsStr>> {
        cmd.get_envs()
            .find(|(key, _)| *key == "CLAUDE_CONFIG_DIR")
            .map(|(_, value)| value)
    }

    #[test]
    fn the_config_dir_is_set_only_when_configured() {
        let cmd = command(&request(Some(PathBuf::from("/c/claude"))));
        assert_eq!(cmd.get_program(), "my-claude");
        assert_eq!(config_dir_env(&cmd), Some(Some("/c/claude".as_ref())));
        assert_eq!(config_dir_env(&command(&request(None))), None);
    }

    #[test]
    fn structured_output_is_read_from_its_field() {
        let stdout = r#"{"is_error":false,"result":"ignored","structured_output":{"a":1}}"#;
        assert_eq!(structured(stdout).unwrap(), serde_json::json!({"a": 1}));
    }

    #[test]
    fn structured_output_falls_back_to_result_text() {
        let stdout = r#"{"is_error":false,"result":"{\"a\":2}"}"#;
        assert_eq!(structured(stdout).unwrap(), serde_json::json!({"a": 2}));
    }

    #[test]
    fn errors_report_the_reason_claude_gave() {
        let stdout = r#"{"is_error":true,"subtype":"error_max_turns"}"#;
        let err = structured(stdout).unwrap_err().to_string();
        assert_eq!(err, "claude failed: error_max_turns");
    }
}
