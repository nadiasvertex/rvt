use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::Path;
use std::process::Command as Process;

use anyhow::{Context, Result, bail};
use ctx_core::store::{Project, Window};
use ctx_core::terminals::Terminals;
use ctx_core::{brief, cache, config, inbox, item, ops, refresh, search, store, wait, watch};

use config::Config;
use item::{ItemRef, ParseContext};
use store::State;

const USAGE: &str = "ctx: switch between projects without losing your place

usage:
  ctx done [project]      hide a finished project (it stays searchable)
  ctx reopen <project>    make a done project live again
  ctx list                projects and their state
  ctx wait <condition>    set a wait condition; --clear removes it
        claude | cmd [label] | review <item> | closed <item> | ci <item>
        state <item> <state> | until <30m|2h|3d|thu|2026-11-20> | <free text>
  ctx note [title]        write a note in $EDITOR
  ctx clip                save the clipboard (or stdin) as a note
        [--title T] [--source teams|email|web|ai|local] [--link URL]
  ctx link <item>         add a URL, path, `pr 123`, `#456`, `project!78`, `!78` or `branch`
        [--title T]
  ctx update <item>...    add a summary of finished work to the journal of each project that
        [--add <item>]... holds the items; --add puts new items in those projects too
        [--message T | --message -] [--project NAME]
  ctx import <link|words> summarize a Teams thread or email into a note, through Claude
  ctx search <words>      search notes, handoffs and items [--all]
  ctx run -- <command>    run a command and wait on it [--no-wait]
  ctx refresh [project]   fetch items now
  ctx reindex             rebuild the search index
  ctx hooks               print the Claude Code hooks that report to ctx
  ctx skill               print the Claude skill that runs ctx update [--write]
  ctx summarize <project> ask Claude for a summary of the project's last handoff
  ctx brief <project>     ask Claude for a brief on picking the project back up

The ctx app, which ctx with no arguments will open, is not built yet.
Most commands take --project <name>; in a terminal of the ctx app they default to its project.";

/// The front end for commands run from a shell. It has no workspaces of its own and refuses
/// what only the ctx app can do. It reads the terminal it runs in from `CTX_TERMINAL`, which the
/// app sets in its terminals. `ops::current_project` reads `CTX_PROJECT` itself.
struct Cli;

impl Terminals for Cli {
    fn current_workspace(&self) -> Option<String> {
        None
    }
    fn current_terminal(&self) -> Option<String> {
        std::env::var("CTX_TERMINAL").ok().filter(|s| !s.is_empty())
    }
    fn is_open(&self, _: &Project) -> bool {
        false
    }
    fn open(&self, _: &Project) -> Result<()> {
        bail!("opening a project needs the ctx app")
    }
    fn show(&self, _: &Project, _: Option<&str>) -> Result<()> {
        bail!("showing a project needs the ctx app")
    }
    fn show_home(&self) -> Result<()> {
        bail!("showing home needs the ctx app")
    }
    fn close(&self, _: &Project) -> Result<()> {
        bail!("closing a project needs the ctx app")
    }
    fn layout(&self, _: &Project) -> Option<Vec<Window>> {
        None
    }
    fn message(&self, text: &str) {
        eprintln!("{text}");
    }
    fn type_line(&self, _: &str, _: &str) -> Result<()> {
        bail!("typing into a terminal needs the ctx app")
    }
    fn workspace_of(&self, _: &str) -> Option<String> {
        None
    }
    fn running(&self) -> bool {
        false
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ctx: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Removes `--name value` from the arguments and returns the value.
fn take_value(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        args.remove(i);
        return None;
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Some(value)
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

fn project_arg(args: &mut Vec<String>) -> Result<String> {
    let projects = store::list();
    let name = take_value(args, "--project").or_else(|| (!args.is_empty()).then(|| args.join(" ")));
    ops::resolve(&Cli, &projects, name.as_deref())
}

/// The project named by `--project`, or the current one.
fn flag_project(args: &mut Vec<String>) -> Result<String> {
    let projects = store::list();
    let name = take_value(args, "--project");
    ops::resolve(&Cli, &projects, name.as_deref())
}

fn run(mut args: Vec<String>) -> Result<i32> {
    let command = if args.is_empty() {
        None
    } else {
        Some(args.remove(0))
    };
    if matches!(command.as_deref(), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return Ok(0);
    }
    if command.as_deref() == Some("event") {
        // Hooks must never fail Claude, whatever the config says.
        let _ = watch::event(&Cli, args.first().map(String::as_str).unwrap_or_default());
        return Ok(0);
    }
    let cfg = config::load()?;
    match command.as_deref() {
        None => {
            // Step 3 of the GUI plan opens the ctx app here.
            println!("{USAGE}");
            Ok(0)
        }
        Some("done") => {
            let slug = project_arg(&mut args)?;
            ops::done(&slug)?;
            println!("{slug} is done");
            Ok(0)
        }
        Some("reopen") => {
            let slug = project_arg(&mut args)?;
            ops::reopen(&slug)?;
            println!("{slug} is live");
            Ok(0)
        }
        Some("list" | "ls") => cmd_list(),
        Some("wait") => cmd_wait(args),
        Some("note") => {
            let slug = flag_project(&mut args)?;
            let path = ops::new_note(&slug, &args.join(" "))?;
            edit(&path)?;
            ops::finish_note(&slug, &path);
            Ok(0)
        }
        Some("clip") => cmd_clip(args),
        Some("link") => cmd_link(&cfg, args),
        Some("update") => cmd_update(&cfg, args),
        Some("skill") => cmd_skill(&cfg, args),
        Some("import") => {
            let slug = flag_project(&mut args)?;
            let query = args.join(" ");
            if query.trim().is_empty() {
                bail!("usage: ctx import <Teams or Outlook link, or words to search for>");
            }
            eprintln!("asking Claude to find it in Microsoft 365…");
            let imported = brief::import(&cfg, &slug, &query)?;
            println!(
                "saved {} note \"{}\" to {}",
                imported.source,
                imported.title,
                imported.path.display()
            );
            if !imported.link.is_empty() {
                println!("source: {}", imported.link);
            }
            Ok(0)
        }
        Some("search") => {
            let all = take_flag(&mut args, "--all");
            let projects = store::list();
            let scope = if all {
                None
            } else {
                ops::current_project(&Cli, &projects)
            };
            for hit in search::search(&args.join(" "), scope.as_deref(), 50)? {
                let snippet = hit
                    .snippet
                    .replace([search::MATCH_START, search::MATCH_END], "");
                println!(
                    "{}  {}  {}\n    {}",
                    hit.project, hit.kind, hit.title, snippet
                );
            }
            Ok(0)
        }
        Some("run") => {
            let set_wait = !take_flag(&mut args, "--no-wait");
            if args.first().map(String::as_str) == Some("--") {
                args.remove(0);
            }
            watch::run_command(&Cli, &args, set_wait)
        }
        Some("refresh") => {
            let projects = store::list();
            let only = if args.is_empty() {
                None
            } else {
                Some(store::find(&projects, &args.join(" "))?)
            };
            if only.is_none() {
                eprintln!("looking for your PRs, MRs and work items…");
                inbox::refresh(&cfg, (cfg.watch.interval_minutes.max(1) * 60) as i64);
                for e in inbox::load().errors {
                    eprintln!("failed: {e}");
                }
            }
            let items = refresh::wanted(&projects, only.as_deref());
            eprintln!("fetching {} items…", items.len());
            refresh::fetch(&cfg, &items, &|key| eprintln!("  {key}"));
            let cache = cache::load();
            for (key, e) in items
                .iter()
                .filter_map(|i| cache.errors.get(&i.key()).map(|e| (i.key(), e)))
            {
                eprintln!("failed: {key}: {e}");
            }
            for p in store::list() {
                ops::evaluate(&Cli, &p.slug);
            }
            Ok(0)
        }
        Some("reindex") => {
            let n = search::reindex(&store::list(), &cache::load().items)?;
            println!("indexed {n} projects");
            Ok(0)
        }
        Some("hooks") => {
            println!(
                "# Merge this into the settings.json of each Claude config directory you use,\n\
                 # for example ~/.claude/settings.json or ~/.claude-work/settings.json.\n\
                 # The hooks do nothing outside a terminal of the ctx app."
            );
            println!("{}", watch::hooks_snippet());
            Ok(0)
        }
        Some("summarize") => {
            brief::summarize(
                &cfg,
                args.first().context("usage: ctx summarize <project>")?,
            )?;
            Ok(0)
        }
        Some("brief") => {
            brief::brief(&cfg, args.first().context("usage: ctx brief <project>")?)?;
            Ok(0)
        }
        Some(other) => bail!("unknown command {other}; ctx --help lists them"),
    }
}

/// Prompts on the terminal, showing the current value; Enter keeps it.
fn prompt(tty: &mut (impl BufRead + Write), label: &str, current: &str) -> Result<String> {
    if current.is_empty() {
        write!(tty, "{label}: ")?;
    } else {
        write!(tty, "{label} [{current}]: ")?;
    }
    tty.flush()?;
    let mut line = String::new();
    tty.read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() {
        current.to_string()
    } else {
        line.to_string()
    })
}

struct Tty {
    reader: BufReader<std::fs::File>,
    writer: std::fs::File,
}

impl Read for Tty {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl BufRead for Tty {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.reader.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.reader.consume(amt)
    }
}

impl Write for Tty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

fn open_tty() -> Result<Tty> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("no terminal to ask on")?;
    Ok(Tty {
        reader: BufReader::new(file.try_clone()?),
        writer: file,
    })
}

fn cmd_wait(mut args: Vec<String>) -> Result<i32> {
    let slug = flag_project(&mut args)?;
    if take_flag(&mut args, "--clear") {
        ops::set_wait(&Cli, &slug, None)?;
        println!("{slug} no longer waits");
        return Ok(0);
    }
    let project = store::load(&slug)?;
    let mut condition = wait::parse_shell(&args, |arg| ops::find_item(&project, arg))
        .map_err(anyhow::Error::msg)?;
    condition = condition.starting_now(&cache::load().items);
    if let wait::Condition::Claude { terminal } = &mut condition {
        *terminal = Cli
            .current_terminal()
            .filter(|_| !take_flag(&mut args, "--any"));
    }
    let described = condition.describe();
    ops::set_wait(&Cli, &slug, Some(condition))?;
    println!("{slug} waits on {described}");
    Ok(0)
}

fn cmd_list() -> Result<i32> {
    let projects = store::list();
    let current = ops::current_project(&Cli, &projects);
    for state in [State::Live, State::Frozen, State::Done] {
        let group: Vec<_> = projects.iter().filter(|p| p.state == state).collect();
        if group.is_empty() {
            continue;
        }
        println!("{}", state.label());
        for p in group {
            let mark = if current.as_deref() == Some(p.slug.as_str()) {
                ">"
            } else {
                " "
            };
            let mut line = format!(" {mark} {:<20} {} items", p.slug, p.items.len());
            if let Some(w) = &p.wait {
                match &w.cleared {
                    Some(c) => line.push_str(&format!(" · ready: {}", c.reason)),
                    None => line.push_str(&format!(" · waiting: {}", w.condition.describe())),
                }
            }
            if !p.next.is_empty() {
                line.push_str(&format!(" · next: {}", p.next));
            }
            println!("{line}");
        }
    }
    Ok(0)
}

fn clipboard() -> String {
    Process::new("pbpaste")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn cmd_clip(mut args: Vec<String>) -> Result<i32> {
    let slug = flag_project(&mut args)?;
    let title = take_value(&mut args, "--title");
    let source = take_value(&mut args, "--source");
    let link = take_value(&mut args, "--link").unwrap_or_default();
    let body = if std::io::stdin().is_terminal() {
        clipboard()
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    };
    if body.trim().is_empty() {
        bail!("the clipboard is empty");
    }
    let suggested: String = item::snippet(&body).chars().take(60).collect();
    let (title, source) = match (title, source) {
        (Some(t), Some(s)) => (t, s),
        (title, source) => match open_tty() {
            Ok(mut tty) => {
                let t = match title {
                    Some(t) => t,
                    None => prompt(&mut tty, "Title", &suggested)?,
                };
                let s = match source {
                    Some(s) => s,
                    None => prompt(&mut tty, "Source (teams, email, web, ai, local)", "teams")?,
                };
                (t, s)
            }
            Err(_) => (
                title.unwrap_or(suggested),
                source.unwrap_or_else(|| "local".into()),
            ),
        },
    };
    let path = ops::capture(&slug, &title, &source, &link, &body)?;
    println!("saved {}", path.display());
    Ok(0)
}

fn parse_context<'a>(cfg: &'a Config, cwd: &'a Path) -> ParseContext<'a> {
    ParseContext {
        cwd,
        gitlab_host: cfg.gitlab.as_ref().map(|g| g.host.as_str()),
        gitlab_group: cfg.gitlab.as_ref().map(|g| g.group.as_str()),
    }
}

fn cmd_link(cfg: &Config, mut args: Vec<String>) -> Result<i32> {
    let slug = flag_project(&mut args)?;
    let title = take_value(&mut args, "--title");
    let cwd = std::env::current_dir()?;
    let mut item = item::parse(&args.join(" "), &parse_context(cfg, &cwd))?;
    if let (ItemRef::Url { title: t, .. }, Some(title)) = (&mut item, title) {
        *t = title;
    }
    if !ops::add_item(&slug, item.clone())? {
        println!("{} is already in {slug}", item.label());
        return Ok(0);
    }
    refresh::fetch(cfg, std::slice::from_ref(&item), &|_| {});
    let cache = cache::load();
    match (cache.items.get(&item.key()), cache.errors.get(&item.key())) {
        (_, Some(e)) => println!(
            "added {} to {slug}, but fetching it failed: {e}",
            item.label()
        ),
        (Some(s), _) => println!(
            "added {} to {slug}: {} [{}]",
            item.describe(),
            s.title,
            s.status
        ),
        _ => println!("added {} to {slug}", item.label()),
    }
    Ok(0)
}

const SKILL: &str = include_str!("../skill/SKILL.md");

#[derive(Debug, Default, PartialEq)]
struct UpdateArgs {
    items: Vec<String>,
    add: Vec<String>,
    message: Option<String>,
    project: Option<String>,
}

/// Reads `ctx update` arguments. An item may be one argument, such as `pr:12`, or two, such as
/// `PR 12`.
fn update_args(args: &[String]) -> Result<UpdateArgs> {
    let mut out = UpdateArgs::default();
    let mut rest = args.iter();
    let item = |first: &String, rest: &mut std::slice::Iter<String>| -> String {
        if matches!(first.to_lowercase().as_str(), "pr" | "wi") {
            if let Some(id) = rest.clone().next().filter(|a| !a.starts_with("--")) {
                rest.next();
                return format!("{first} {id}");
            }
        }
        first.clone()
    };
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--message" | "-m" => {
                out.message = Some(rest.next().context("--message needs a value")?.clone());
            }
            "--project" => {
                out.project = Some(rest.next().context("--project needs a value")?.clone());
            }
            "--add" => {
                let first = rest.next().context("--add needs an item")?;
                out.add.push(item(first, &mut rest));
            }
            _ => out.items.push(item(arg, &mut rest)),
        }
    }
    Ok(out)
}

/// Writes Claude's summary of its work to the journal of each project that holds the items.
fn cmd_update(cfg: &Config, args: Vec<String>) -> Result<i32> {
    let args = update_args(&args)?;
    if args.items.is_empty() {
        bail!("usage: ctx update <item>... [--add <item>]... [--message TEXT | --message -]");
    }
    let message = match args.message.as_deref() {
        Some("-") => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
        Some(text) => text.to_string(),
        None => String::new(),
    };
    let cwd = std::env::current_dir()?;
    let context = parse_context(cfg, &cwd);
    let parse = |inputs: &[String]| -> Result<Vec<ItemRef>> {
        inputs.iter().map(|i| item::parse(i, &context)).collect()
    };
    let (items, add) = (parse(&args.items)?, parse(&args.add)?);
    if message.trim().is_empty() && add.is_empty() {
        bail!("nothing to record: give --message, or items to --add");
    }
    let projects = store::list();
    let (found, slugs) = match &args.project {
        Some(name) => (ops::Found::Current, vec![store::find(&projects, name)?]),
        None => {
            let mut found = ops::projects_for(&projects, &items, &cache::load().items, None);
            if found.is_none() {
                let cached = cache::load();
                let unknown: Vec<ItemRef> = items
                    .iter()
                    .filter(|i| i.is_remote() && !cached.items.contains_key(&i.key()))
                    .cloned()
                    .collect();
                if !unknown.is_empty() {
                    refresh::fetch(cfg, &unknown, &|_| {});
                }
                let current = ops::current_project(&Cli, &projects);
                found =
                    ops::projects_for(&projects, &items, &cache::load().items, current.as_deref());
            }
            found.with_context(|| {
                let labels: Vec<String> = items.iter().map(ItemRef::label).collect();
                format!(
                    "no open project holds {} or an item linked to it, and this is not a \
                     project's session. Name one with --project, or ctx link the item first.",
                    labels.join(", ")
                )
            })?
        }
    };
    let mut fetch: Vec<ItemRef> = Vec::new();
    for slug in &slugs {
        let mut to_add = add.clone();
        if found == ops::Found::Current {
            to_add.splice(0..0, items.iter().cloned());
        }
        let added = ops::record_update(slug, &items, &message, &to_add)?;
        if !message.trim().is_empty() {
            println!("wrote to the {slug} journal");
        }
        for item in added {
            println!("added {} to {slug}", item.describe());
            if !fetch.iter().any(|f| f.key() == item.key()) {
                fetch.push(item);
            }
        }
    }
    if !fetch.is_empty() {
        refresh::fetch(cfg, &fetch, &|_| {});
    }
    Ok(0)
}

fn cmd_skill(cfg: &Config, mut args: Vec<String>) -> Result<i32> {
    if !take_flag(&mut args, "--write") {
        print!("{SKILL}");
        return Ok(0);
    }
    let path = cfg.claude.skill_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    store::write_atomic(&path, SKILL.as_bytes())?;
    println!("wrote {}", path.display());
    Ok(0)
}

fn edit(path: &Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = Process::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("could not start {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod update_tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(String::from).collect()
    }

    #[test]
    fn update_items_may_be_one_word_or_two() {
        let parsed = update_args(
            &[
                args("PR 12 wi:4 WI 5 --add pr 13 --add !7 --project tables wi"),
                vec!["--message".into(), "Fixed it.".into()],
            ]
            .concat(),
        )
        .unwrap();
        assert_eq!(
            parsed,
            UpdateArgs {
                items: vec!["PR 12".into(), "wi:4".into(), "WI 5".into(), "wi".into()],
                add: vec!["pr 13".into(), "!7".into()],
                message: Some("Fixed it.".into()),
                project: Some("tables".into()),
            }
        );
        assert!(update_args(&args("pr:1 --message")).is_err());
        assert!(update_args(&args("pr:1 --add")).is_err());
    }

    #[test]
    fn the_skill_names_itself_ctx_update() {
        assert!(
            SKILL.starts_with("---\nname: ctx-update\ndescription: "),
            "{SKILL}"
        );
    }
}
