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

/// One `claude -p` call. The prompt goes over stdin and the answer must match `schema`. An empty
/// `tools` runs Claude with no tools at all.
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
    if req.tools.is_empty() {
        cmd.args(["--tools", ""]);
    } else {
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
    num_turns: Option<u32>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    structured_output: Option<Value>,
}

/// What a successful run answered: the structured value, or only prose.
enum Answer {
    Structured(Value),
    Prose(Output),
}

fn answer(stdout: &str) -> Result<Answer> {
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
    if let Some(value) = output.structured_output {
        return Ok(Answer::Structured(value));
    }
    match output.result.as_deref().and_then(json_in) {
        Some(value) => Ok(Answer::Structured(value)),
        None => Ok(Answer::Prose(output)),
    }
}

/// The JSON object in `text`: all of it, or the span from the first `{` to the last `}`, which
/// also covers a fenced code block.
fn json_in(text: &str) -> Option<Value> {
    let text = text.trim();
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    match serde_json::from_str(text.get(start..=end)?) {
        Ok(value @ Value::Object(_)) => Some(value),
        _ => None,
    }
}

fn no_structured_output(output: &Output) -> anyhow::Error {
    const EXCERPT_CHARS: usize = 200;
    let result = output.result.as_deref().unwrap_or("").trim();
    let mut excerpt: String = result.chars().take(EXCERPT_CHARS).collect();
    if result.chars().count() > EXCERPT_CHARS {
        excerpt.push('…');
    }
    if excerpt.is_empty() {
        excerpt = "empty result".into();
    }
    anyhow::anyhow!(
        "claude returned no structured output ({}, {} turns): {excerpt}",
        output.subtype.as_deref().unwrap_or("no subtype"),
        output
            .num_turns
            .map_or_else(|| "?".into(), |n| n.to_string()),
    )
}

/// The structured answer in `claude -p --output-format json` output. Falls back to the JSON
/// object in the result text when the structured field is missing.
pub fn structured(stdout: &str) -> Result<Value> {
    match answer(stdout)? {
        Answer::Structured(value) => Ok(value),
        Answer::Prose(output) => Err(no_structured_output(&output)),
    }
}

/// Runs the request and returns its structured answer. A run that ends in prose is followed by
/// one call without tools that restates the prose in the schema.
pub fn ask_structured(req: &Request) -> Result<Value> {
    let prose = match answer(&ask(req)?)? {
        Answer::Structured(value) => return Ok(value),
        Answer::Prose(output) => output,
    };
    let Some(text) = prose.result.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Err(no_structured_output(&prose));
    };
    let restate = format!(
        "Restate the answer below in the required structured format. Keep its content and \
         wording, and add nothing.\n\n## Answer\n\n{text}"
    );
    let restated = ask(&Request {
        command: req.command,
        config_dir: req.config_dir.clone(),
        model: req.model,
        dir: req.dir,
        schema: req.schema.clone(),
        tools: &[],
        prompt: &restate,
    })
    .and_then(|stdout| structured(&stdout));
    restated.map_err(|e| {
        anyhow::anyhow!(
            "{}; restating it failed: {e:#}",
            no_structured_output(&prose)
        )
    })
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
    fn structured_output_falls_back_to_a_fenced_object_in_prose() {
        let stdout = r#"{"is_error":false,"result":"Here:\n```json\n{\"a\": 3}\n```\nDone."}"#;
        assert_eq!(structured(stdout).unwrap(), serde_json::json!({"a": 3}));
    }

    #[test]
    fn prose_without_json_reports_what_claude_said() {
        let stdout =
            r#"{"is_error":false,"subtype":"success","num_turns":7,"result":"I looked around."}"#;
        let err = structured(stdout).unwrap_err().to_string();
        assert_eq!(
            err,
            "claude returned no structured output (success, 7 turns): I looked around."
        );
    }

    #[test]
    fn no_tools_turns_every_tool_off() {
        let args: Vec<String> = command(&request(None))
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args
            .iter()
            .position(|a| a == "--tools")
            .expect("--tools is passed");
        assert_eq!(args[at + 1], "");
        assert!(!args.iter().any(|a| a == "--allowedTools"));
    }

    #[test]
    fn errors_report_the_reason_claude_gave() {
        let stdout = r#"{"is_error":true,"subtype":"error_max_turns"}"#;
        let err = structured(stdout).unwrap_err().to_string();
        assert_eq!(err, "claude failed: error_max_turns");
    }
}
