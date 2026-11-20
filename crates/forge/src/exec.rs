use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub fn run(program: &str, args: &[&str]) -> Result<String> {
    run_with(Command::new(program).args(args), None)
}

/// Runs `cmd` with `stdin` as its input and returns stdout, or an error naming the first line of
/// stderr.
pub fn run_with(cmd: &mut Command, stdin: Option<&str>) -> Result<String> {
    let output = capture(cmd, stdin)?;
    if !output.status.success() {
        bail!("{} failed: {}", describe(cmd), first_line(&output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `cmd` to completion whatever its exit status. The child is killed by `kill_children` if
/// the app quits first.
pub fn capture(cmd: &mut Command, stdin: Option<&str>) -> Result<Output> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("could not start {}", cmd.get_program().to_string_lossy()))?;
    let pid = child.id();
    CHILDREN.lock().unwrap().push(pid);
    let writer = match (stdin, child.stdin.take()) {
        (Some(text), Some(mut pipe)) => {
            let text = text.to_string();
            Some(thread::spawn(move || pipe.write_all(text.as_bytes())))
        }
        _ => None,
    };
    let output = child.wait_with_output();
    CHILDREN.lock().unwrap().retain(|p| *p != pid);
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(output?)
}

/// Stops every command still running, so a quit does not leave a review running unseen.
pub fn kill_children() {
    for pid in CHILDREN.lock().unwrap().drain(..) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
}

pub fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error output")
        .to_string()
}

fn describe(cmd: &Command) -> String {
    let program = cmd.get_program().to_string_lossy().into_owned();
    match cmd
        .get_args()
        .find(|a| !a.to_string_lossy().starts_with('-'))
    {
        Some(arg) => format!("{program} {}", arg.to_string_lossy()),
        None => program,
    }
}

pub fn run_json<T: DeserializeOwned>(program: &str, args: &[&str]) -> Result<T> {
    let out = run(program, args)?;
    serde_json::from_str(&out)
        .with_context(|| format!("unexpected JSON from {program} {}", args.join(" ")))
}

/// Writes `value` to a private temp file, passes its path to `f`, and removes it afterwards.
pub fn with_json_file<R>(
    value: &serde_json::Value,
    f: impl FnOnce(&str) -> Result<R>,
) -> Result<R> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("forge-{}-{n}.json", std::process::id()));
    fs::write(&path, serde_json::to_vec(value)?)
        .with_context(|| format!("could not write {}", path.display()))?;
    let result = f(&path.to_string_lossy());
    let _ = fs::remove_file(&path);
    result
}
