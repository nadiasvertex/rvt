mod app;
mod tmux;
mod ui;

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use ctx_core::{
    brief, cache, config, inbox, item, ops, refresh, respond, search, store, wait, watch,
};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, Command, HomeRow, Modal, Screen};
use config::Config;
use item::{ItemRef, ParseContext};
use store::{Handoff, State};
use tmux::Tmux;

const USAGE: &str = "ctx: switch between projects without losing your place

usage:
  ctx                     home screen, or the project screen inside a project's session
  ctx pick                home screen for the tmux popup; Esc closes it
  ctx open <project>      project screen
  ctx new <name>          start a project here and switch to it
  ctx sw <project>        switch to a project's tmux session
  ctx freeze [project]    write a handoff and set the project aside
        [--doing T] [--next T] [--waiting T] [--wait CONDITION] [--close | --keep]
  ctx thaw [project]      make a frozen project live
  ctx freeze-claude <id>  end a Claude session and keep it in the project to resume later
        [--note T]        <id> is a session ID or its start, or the name Claude shows
  ctx done [project]      hide a finished project (it stays searchable)
  ctx reopen <project>    make a done project live again
  ctx list                projects and their state
  ctx wait <condition>    set a wait condition without freezing; --clear removes it
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
  ctx status              summary for the tmux status line
  ctx watch               refresh items and clear wait conditions [--daemon] [--once]
  ctx refresh [project]   fetch items now
  ctx reindex             rebuild the search index
  ctx tmux-config         print the tmux bindings [--write]
  ctx hooks               print the Claude Code hooks that report to ctx
  ctx skill               print the Claude skill that runs ctx update [--write]

Most commands take --project <name>; inside a project's session they default to it.";

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
    ops::resolve(&Tmux, &projects, name.as_deref())
}

/// The project named by `--project`, or the current one.
fn flag_project(args: &mut Vec<String>) -> Result<String> {
    let projects = store::list();
    let name = take_value(args, "--project");
    ops::resolve(&Tmux, &projects, name.as_deref())
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
        let _ = watch::event(&Tmux, args.first().map(String::as_str).unwrap_or_default());
        return Ok(0);
    }
    if command.as_deref() == Some("status") {
        println!("{}", watch::status_line());
        return Ok(0);
    }
    let cfg = config::load()?;
    match command.as_deref() {
        None => tui(cfg, Start::Default),
        Some("pick") => tui(cfg, Start::Popup),
        Some("open") => {
            let slug = store::find(&store::list(), &args.join(" "))?;
            tui(cfg, Start::Open(slug))
        }
        Some("new") => cmd_new(&cfg, args),
        Some("sw" | "switch") => {
            let projects = store::list();
            let slug = store::find(&projects, &args.join(" "))?;
            ops::switch(
                &cfg,
                &Tmux,
                &slug,
                ops::current_project(&Tmux, &projects).as_deref(),
            )?;
            Ok(0)
        }
        Some("freeze") => cmd_freeze(&cfg, args),
        Some("freeze-claude") => cmd_freeze_claude(&cfg, args),
        Some("thaw") => {
            let slug = project_arg(&mut args)?;
            ops::thaw(&slug)?;
            println!("{slug} is live");
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
                ops::current_project(&Tmux, &projects)
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
            watch::run_command(&Tmux, &args, set_wait)
        }
        Some("watch") => {
            let daemon = take_flag(&mut args, "--daemon");
            let once = take_flag(&mut args, "--once");
            watch::run(&cfg, &Tmux, daemon, once)?;
            Ok(0)
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
                ops::evaluate(&Tmux, &p.slug);
            }
            Ok(0)
        }
        Some("reindex") => {
            let n = search::reindex(&store::list(), &cache::load().items)?;
            println!("indexed {n} projects");
            Ok(0)
        }
        Some("tmux-config") => {
            let snippet = tmux::config_snippet();
            if take_flag(&mut args, "--write") {
                let path = config::config_dir().join("tmux.conf");
                store::write_atomic(&path, snippet.as_bytes())?;
                println!(
                    "wrote {}\nadd this line to ~/.tmux.conf, then run tmux source-file ~/.tmux.conf:\n  source-file {}",
                    path.display(),
                    path.display()
                );
            } else {
                print!("{snippet}");
            }
            Ok(0)
        }
        Some("hooks") => {
            println!(
                "# Merge this into the settings.json of each Claude config directory you use,\n\
                 # for example ~/.claude/settings.json or ~/.claude-work/settings.json.\n\
                 # The hooks do nothing outside a ctx project session."
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

fn cmd_new(cfg: &Config, mut args: Vec<String>) -> Result<i32> {
    let stay = take_flag(&mut args, "--no-switch");
    let name = args.join(" ");
    if name.trim().is_empty() {
        bail!("usage: ctx new <name>");
    }
    let cwd = std::env::current_dir()?;
    let project = ops::create(&name, &cwd)?;
    println!(
        "created {} in {}",
        project.slug,
        project.dir.as_deref().unwrap_or(&cwd).display()
    );
    for item in project.refs() {
        println!("  {} {}", item.kind_label(), item.label());
    }
    let items: Vec<ItemRef> = project.refs().cloned().collect();
    refresh::fetch(cfg, &items, &|_| {});
    if !stay && tmux::available() {
        let projects = store::list();
        ops::switch(
            cfg,
            &Tmux,
            &project.slug,
            ops::current_project(&Tmux, &projects).as_deref(),
        )?;
    }
    Ok(0)
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

fn cmd_freeze(cfg: &Config, mut args: Vec<String>) -> Result<i32> {
    let doing = take_value(&mut args, "--doing");
    let next = take_value(&mut args, "--next");
    let waiting = take_value(&mut args, "--waiting");
    let wait_arg = take_value(&mut args, "--wait");
    let close = if take_flag(&mut args, "--close") {
        true
    } else if take_flag(&mut args, "--keep") {
        false
    } else {
        cfg.tmux.close_on_freeze
    };
    let slug = project_arg(&mut args)?;
    let project = store::load(&slug)?;
    let previous = project.handoff.clone();
    let prev =
        |f: fn(&Handoff) -> &String| previous.as_ref().map(|h| f(h).clone()).unwrap_or_default();
    let interactive = doing.is_none() && next.is_none() && waiting.is_none() && wait_arg.is_none();
    let (doing, next, waiting) = if interactive {
        let mut tty = open_tty()?;
        writeln!(
            tty,
            "Freezing {}. Enter keeps the value in brackets.",
            project.name
        )?;
        let doing = prompt(&mut tty, "Doing", &prev(|h| &h.doing))?;
        let next = prompt(&mut tty, "Next", &project.next)?;
        let waiting = prompt(&mut tty, "Waiting on", &prev(|h| &h.waiting))?;
        (doing, next, waiting)
    } else {
        (
            doing.unwrap_or_else(|| prev(|h| &h.doing)),
            next.unwrap_or_else(|| project.next.clone()),
            waiting.unwrap_or_else(|| prev(|h| &h.waiting)),
        )
    };
    let condition = match wait_arg {
        Some(text) => {
            let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
            Some(
                wait::parse_shell(&words, |arg| ops::find_item(&project, arg))
                    .map_err(anyhow::Error::msg)?
                    .starting_now(&cache::load().items),
            )
        }
        None => match project.wait.as_ref().filter(|w| w.is_waiting()) {
            Some(w) => Some(w.condition.clone()),
            None => (!waiting.trim().is_empty()).then(|| wait::Condition::Manual {
                text: waiting.trim().to_string(),
            }),
        },
    };
    let handoff = Handoff {
        time: Utc::now(),
        doing,
        next,
        waiting,
    };
    ops::freeze(cfg, &Tmux, &slug, handoff, condition, close)?;
    println!("{slug} is frozen");
    Ok(0)
}

fn cmd_freeze_claude(cfg: &Config, mut args: Vec<String>) -> Result<i32> {
    let note = take_value(&mut args, "--note").unwrap_or_default();
    let slug = flag_project(&mut args)?;
    let Some(query) = args.first().cloned() else {
        bail!("ctx freeze-claude needs a session ID or name");
    };
    let project = store::load(&slug)?;
    let (session, running) = ops::find_claude(cfg, &project, &query)?;
    let frozen = ops::mark_claude_frozen(
        &slug,
        &session.id,
        &note,
        running.as_ref().and_then(|l| l.name.clone()),
    )?;
    if let Some(running) = running {
        ops::end_claude(&Tmux, &running)?;
    }
    println!("{} is frozen; ctx open {slug} lists it", frozen.label());
    Ok(0)
}

fn cmd_wait(mut args: Vec<String>) -> Result<i32> {
    let slug = flag_project(&mut args)?;
    if take_flag(&mut args, "--clear") {
        ops::set_wait(&Tmux, &slug, None)?;
        println!("{slug} no longer waits");
        return Ok(0);
    }
    let project = store::load(&slug)?;
    let mut condition = wait::parse_shell(&args, |arg| ops::find_item(&project, arg))
        .map_err(anyhow::Error::msg)?;
    condition = condition.starting_now(&cache::load().items);
    if let wait::Condition::Claude { pane } = &mut condition {
        *pane = std::env::var("TMUX_PANE")
            .ok()
            .filter(|_| !take_flag(&mut args, "--any"));
    }
    let described = condition.describe();
    ops::set_wait(&Tmux, &slug, Some(condition))?;
    println!("{slug} waits on {described}");
    Ok(0)
}

fn cmd_list() -> Result<i32> {
    let projects = store::list();
    let current = ops::current_project(&Tmux, &projects);
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
                let current = ops::current_project(&Tmux, &projects);
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

enum Start {
    Default,
    Popup,
    Open(String),
}

enum Msg {
    Input(Event),
    InputFailed(String),
    Fetched(String),
    InboxDone,
    Imported(Result<String, String>),
    Evaluated(ItemRef, Result<usize, String>),
    /// The step Claude's work on an item's threads has reached.
    Progress(ItemRef, String),
    Posted(String, ItemRef, respond::Outcome),
    /// A frozen Claude session's process exited, or could not be ended.
    ClaudeEnded(String, Result<(), String>),
}

/// Set while another program owns the terminal, so the input thread stops reading keys.
static PAUSED: AtomicBool = AtomicBool::new(false);

fn spawn_input(tx: &Sender<Msg>) {
    let tx = tx.clone();
    thread::spawn(move || {
        loop {
            if PAUSED.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(30));
                continue;
            }
            let msg = match event::poll(Duration::from_millis(50)) {
                Ok(false) => continue,
                Ok(true) if PAUSED.load(Ordering::SeqCst) => continue,
                Ok(true) => match event::read() {
                    Ok(e) => Msg::Input(e),
                    Err(e) => Msg::InputFailed(e.to_string()),
                },
                Err(e) => Msg::InputFailed(e.to_string()),
            };
            let failed = matches!(msg, Msg::InputFailed(_));
            if tx.send(msg).is_err() || failed {
                break;
            }
        }
    });
}

/// Hands the terminal to another program and takes it back afterwards.
fn suspended<R>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> R) -> R {
    PAUSED.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(80));
    ratatui::restore();
    let result = f();
    *terminal = ratatui::init();
    let _ = terminal.clear();
    PAUSED.store(false, Ordering::SeqCst);
    result
}

const TICK: Duration = Duration::from_millis(100);
const RELOAD: Duration = Duration::from_secs(2);

fn tui(cfg: Config, start: Start) -> Result<i32> {
    let cfg = Arc::new(cfg);
    let projects = store::list();
    let current = ops::current_project(&Tmux, &projects);
    let popup = matches!(start, Start::Popup);
    let mut app = App::new(current.clone(), popup);
    app.close_default = cfg.tmux.close_on_freeze;
    app.soft_limit = cfg.live.soft_limit;
    app.claude_config = cfg.claude.config_path();
    app.start_dir = tmux::active_path()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    if popup {
        app.pane = tmux::active_pane()
            .filter(|p| std::env::var("TMUX_PANE").ok().as_deref() != Some(p.as_str()));
    }
    app.reload();
    match start {
        Start::Open(slug) => {
            app.own = Some(slug.clone());
            app.open_project(&slug);
        }
        Start::Default => {
            if let Some(slug) = current.clone() {
                app.own = Some(slug.clone());
                app.open_project(&slug);
            }
        }
        Start::Popup => {
            if let Some(i) = current.as_ref().and_then(|c| {
                app.home_order()
                    .iter()
                    .position(|r| *r == HomeRow::Project(c.clone()))
            }) {
                app.home_selected = i;
            }
        }
    }
    let (tx, rx) = mpsc::channel();
    let max_age = (cfg.watch.interval_minutes.max(1) * 60) as i64;
    let first = match &app.screen {
        Screen::Project(slug) => refresh::wanted(&app.projects, Some(slug)),
        _ => refresh::wanted(&app.projects, None),
    };
    let age_limit = if watch::running().is_some() {
        i64::MAX
    } else {
        max_age
    };
    let stale = refresh::stale(first, &app.cache, age_limit);
    if !stale.is_empty() {
        start_refresh(&cfg, &tx, &mut app, stale);
    }
    let inbox_stale = app
        .inbox
        .updated
        .is_none_or(|t| (Utc::now() - t).num_seconds() > age_limit);
    if inbox_stale {
        start_inbox(&cfg, &tx, &mut app);
    }
    let mut terminal = ratatui::init();
    spawn_input(&tx);
    let result = event_loop(&cfg, &tx, &rx, &mut app, &mut terminal);
    ratatui::restore();
    forge::exec::kill_children();
    if let Some(slug) = app_attach(&app) {
        ops::switch(&cfg, &Tmux, &slug, None)?;
    }
    result.map(|_| 0)
}

/// Outside tmux, choosing a project quits the TUI and attaches to the project's session.
fn app_attach(app: &App) -> Option<String> {
    ATTACH
        .lock()
        .ok()
        .and_then(|mut a| a.take())
        .filter(|_| app.quit)
}

static ATTACH: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// A shell command that runs Claude, under the configured Claude config directory when there
/// is one.
fn claude_command(program: &str, config_dir: Option<&Path>, args: &str) -> String {
    let mut command = program.trim().to_string();
    if !args.is_empty() {
        command = format!("{command} {args}");
    }
    match config_dir {
        Some(dir) => format!(
            "CLAUDE_CONFIG_DIR={} CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1 {command}",
            tmux::quote(&dir.to_string_lossy())
        ),
        None => command,
    }
}

/// Where a Claude command runs in the project's session.
#[derive(Clone, Copy, PartialEq)]
enum ClaudePlace {
    NewWindow,
    /// A new pane beside the current window's panes, unless the session already runs Claude,
    /// in which case a new window.
    SplitUnlessRunning,
}

/// Runs a Claude command in the project's session and moves there. Outside tmux it runs in
/// this terminal instead.
fn claude_window(
    cfg: &Arc<Config>,
    app: &mut App,
    terminal: &mut DefaultTerminal,
    project: &store::Project,
    dir: &Path,
    command: &str,
    place: ClaudePlace,
) -> Result<()> {
    if !tmux::inside() {
        suspended(terminal, || {
            Process::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(dir)
                .status()
        })?;
        return Ok(());
    }
    tmux::ensure_session(project)?;
    if place == ClaudePlace::SplitUnlessRunning && !tmux::has_claude(&project.session) {
        tmux::split_window(&project.session, dir, command)?;
    } else {
        tmux::new_window(&project.session, dir, "claude", command)?;
    }
    if app.current.as_deref() != Some(project.slug.as_str()) {
        ops::switch(cfg, &Tmux, &project.slug, app.current.as_deref())?;
        app.switched(&project.slug);
    }
    Ok(())
}

fn start_refresh(cfg: &Arc<Config>, tx: &Sender<Msg>, app: &mut App, items: Vec<ItemRef>) {
    let fresh: Vec<ItemRef> = items
        .into_iter()
        .filter(|i| !app.fetching.contains(&i.key()))
        .collect();
    if fresh.is_empty() {
        return;
    }
    app.fetching.extend(fresh.iter().map(ItemRef::key));
    let (cfg, tx) = (Arc::clone(cfg), tx.clone());
    thread::spawn(move || {
        let tx = std::sync::Mutex::new(tx);
        refresh::fetch(&cfg, &fresh, &|key| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Msg::Fetched(key.to_string()));
            }
        });
    });
}

/// Looks for your PRs, MRs and work items in the background, and fetches the unfiled ones.
fn start_inbox(cfg: &Arc<Config>, tx: &Sender<Msg>, app: &mut App) {
    if app.discovering {
        return;
    }
    app.discovering = true;
    let max_age = (cfg.watch.interval_minutes.max(1) * 60) as i64;
    let (cfg, tx) = (Arc::clone(cfg), tx.clone());
    thread::spawn(move || {
        inbox::refresh(&cfg, max_age);
        let _ = tx.send(Msg::InboxDone);
    });
}

fn event_loop(
    cfg: &Arc<Config>,
    tx: &Sender<Msg>,
    rx: &Receiver<Msg>,
    app: &mut App,
    terminal: &mut DefaultTerminal,
) -> Result<()> {
    let mut tick = 0usize;
    let mut last_reload = Instant::now();
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app, tick))?;
            dirty = false;
        }
        let wait = if app.animating() {
            TICK
        } else {
            Duration::from_millis(500)
        };
        let mut next = match rx.recv_timeout(wait) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => {
                tick = tick.wrapping_add(1);
                dirty = app.animating() || app.flash.is_some();
                None
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        while let Some(msg) = next {
            match msg {
                Msg::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    for command in app.handle_key(key) {
                        execute(cfg, tx, app, terminal, command);
                    }
                }
                Msg::Input(_) => {}
                Msg::InputFailed(e) => bail!(e),
                Msg::Fetched(key) => {
                    app.fetching.remove(&key);
                    app.reload_cache();
                }
                Msg::InboxDone => {
                    app.discovering = false;
                    app.reload();
                    if let Some(e) = app.inbox.errors.first().cloned() {
                        app.fail(format!("could not list your items: {e}"));
                    }
                }
                Msg::Imported(Ok(title)) => {
                    app.flash(format!("imported \"{title}\""));
                    app.reload();
                }
                Msg::Imported(Err(e)) => app.fail(e),
                Msg::Evaluated(item, result) => {
                    app.responding.remove(&item.key());
                    match result {
                        Ok(n) => app.flash(format!(
                            "Claude is done: {} ready for {}; r opens {}",
                            count(n, "response", "responses"),
                            item.label(),
                            if n == 1 { "it" } else { "them" }
                        )),
                        Err(e) => {
                            app.respond_failures.insert(item.key(), e.clone());
                            app.fail(format!("Claude could not answer {}: {e}", item.label()));
                        }
                    }
                    app.reload();
                }
                Msg::Progress(item, step) => {
                    if let Some(job) = app.responding.get_mut(&item.key()) {
                        job.step = step;
                    }
                }
                Msg::ClaudeEnded(label, result) => {
                    match result {
                        Ok(()) => app.flash(format!("froze {label}; t resumes it")),
                        Err(e) => app.fail(e),
                    }
                    app.reload();
                }
                Msg::Posted(slug, item, outcome) => {
                    app.responding.remove(&item.key());
                    posted(app, &slug, &item, &outcome);
                    start_refresh(cfg, tx, app, vec![item]);
                    app.reload();
                }
            }
            dirty = true;
            next = rx.try_recv().ok();
        }
        if last_reload.elapsed() >= RELOAD {
            app.reload();
            last_reload = Instant::now();
            dirty = true;
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Records which replies reached their threads and says how the post went.
fn posted(app: &mut App, slug: &str, item: &ItemRef, outcome: &respond::Outcome) {
    let Some(mut evaluation) = respond::load(slug, item) else {
        app.fail(format!("the responses to {} are gone", item.label()));
        return;
    };
    let summary = respond::record_posts(&mut evaluation, outcome);
    let done = post_summary(&summary, item);
    if let Err(e) = respond::save(slug, &evaluation) {
        app.fail(format!("{done}, but could not record it: {e:#}"));
        return;
    }
    match summary.error {
        Some(e) => app.fail(format!("{done}; {e}")),
        None => app.flash(done),
    }
}

/// What posting did, such as "posted 2 replies and resolved 1 thread on PR 7".
fn post_summary(summary: &respond::PostSummary, item: &ItemRef) -> String {
    let mut parts = Vec::new();
    if summary.replied > 0 {
        parts.push(format!(
            "posted {}",
            count(summary.replied, "reply", "replies")
        ));
    }
    if summary.resolved > 0 {
        parts.push(format!(
            "resolved {}",
            count(summary.resolved, "thread", "threads")
        ));
    }
    if parts.is_empty() {
        return format!("nothing reached {}", item.label());
    }
    format!("{} on {}", parts.join(" and "), item.label())
}

fn execute(
    cfg: &Arc<Config>,
    tx: &Sender<Msg>,
    app: &mut App,
    terminal: &mut DefaultTerminal,
    command: Command,
) {
    let result = run_command(cfg, tx, app, terminal, command);
    if let Err(e) = result {
        app.fail(format!("{e:#}"));
    }
    app.reload();
}

fn run_command(
    cfg: &Arc<Config>,
    tx: &Sender<Msg>,
    app: &mut App,
    terminal: &mut DefaultTerminal,
    command: Command,
) -> Result<()> {
    match command {
        Command::Refresh(items) => start_refresh(cfg, tx, app, items),
        Command::RefreshInbox => start_inbox(cfg, tx, app),
        Command::Switch(slug) => {
            if !tmux::inside() {
                if let Ok(mut a) = ATTACH.lock() {
                    *a = Some(slug);
                }
                app.quit = true;
                return Ok(());
            }
            ops::switch(cfg, &Tmux, &slug, app.current.as_deref())?;
            app.switched(&slug);
        }
        Command::OpenUrl(url) => {
            Process::new("open").arg(url).spawn()?;
        }
        Command::Edit(path) => {
            suspended(terminal, || edit(&path))?;
            if let Some(slug) = owning_project(app, &path) {
                ops::reindex_project(&slug);
            }
        }
        Command::NewNote(slug) => {
            let path = ops::new_note(&slug, "")?;
            let result = suspended(terminal, || edit(&path));
            ops::finish_note(&slug, &path);
            result?;
        }
        Command::Freeze {
            slug,
            handoff,
            condition,
            close,
        } => {
            ops::freeze(cfg, &Tmux, &slug, handoff, condition, close)?;
            app.flash(format!("froze {slug}"));
            if matches!(&app.screen, Screen::Project(s) if *s == slug) {
                app.screen = Screen::Home;
            }
        }
        Command::Thaw(slug) => {
            ops::thaw(&slug)?;
            app.flash(format!("{slug} is live"));
        }
        Command::Done(slug) => {
            ops::done(&slug)?;
            app.flash(format!("{slug} is done"));
            if matches!(&app.screen, Screen::Project(s) if *s == slug) {
                app.screen = Screen::Home;
            }
        }
        Command::Reopen(slug) => {
            ops::reopen(&slug)?;
            app.flash(format!("{slug} is live again"));
        }
        Command::SetWait(slug, condition) => {
            let described = condition.as_ref().map(|c| c.describe());
            ops::set_wait(&Tmux, &slug, condition)?;
            match described {
                Some(d) => app.flash(format!("{slug} waits on {d}")),
                None => app.flash(format!("{slug} no longer waits")),
            }
        }
        Command::Create { name, dir, item } => {
            let project = ops::create(&name, &dir)?;
            if let Some(item) = &item {
                ops::add_item(&project.slug, item.clone())?;
            }
            let items: Vec<ItemRef> = store::load(&project.slug)?.refs().cloned().collect();
            start_refresh(cfg, tx, app, items);
            app.reload();
            if let Some(item) = item {
                app.flash(format!("created {} with {}", project.name, item.label()));
            } else if tmux::inside() {
                ops::switch(cfg, &Tmux, &project.slug, app.current.as_deref())?;
                app.switched(&project.slug);
            } else {
                app.open_project(&project.slug);
            }
        }
        Command::AddItem(slug, text) => {
            let dir = app
                .project(&slug)
                .and_then(|p| p.dir.clone())
                .filter(|d| d.is_dir())
                .map(Ok)
                .unwrap_or_else(std::env::current_dir)?;
            match item::parse(&text, &parse_context(cfg, &dir)) {
                Ok(item) => {
                    if ops::add_item(&slug, item.clone())? {
                        app.flash(format!("added {}", item.label()));
                        start_refresh(cfg, tx, app, vec![item]);
                    } else {
                        app.flash(format!("{} is already here", item.label()));
                    }
                }
                Err(e) => {
                    app.modal = Some(Modal::AddItem {
                        slug,
                        input: app::Input::new(&text),
                        error: Some(format!("{e:#}")),
                    });
                }
            }
        }
        Command::AddRef(slug, item) => {
            ops::add_item(&slug, item.clone())?;
            app.flash(format!("added {}", item.label()));
            start_refresh(cfg, tx, app, vec![item]);
        }
        Command::Remove(slug, key) => ops::remove_item(&slug, &key)?,
        Command::Dismiss(slug, key) => ops::dismiss(&slug, &key)?,
        Command::SetNext(slug, next) => ops::set_next(&slug, &next)?,
        Command::Capture {
            slug,
            title,
            source,
            link,
            body,
        } => {
            ops::capture(&slug, &title, &source, &link, &body)?;
            app.flash(format!("saved note to {slug}"));
        }
        Command::Import(slug, query) => {
            app.flash("asking Claude to find it in Microsoft 365…");
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let result = brief::import(&cfg, &slug, &query)
                    .map(|i| i.title)
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Imported(result));
            });
        }
        Command::ResumeClaude(slug, session) => {
            let project = store::load(&slug)?;
            let running = match app.live.get(&session.id) {
                Some(live) => live.pane.clone(),
                None if app.live.is_empty() => {
                    wait::claude_pane(&store::events(&slug), &session.id)
                        .filter(|pane| tmux::busy_pane_session(pane).is_some())
                }
                None => None,
            };
            if let Some(pane) = running {
                tmux::select_pane(&pane)?;
                return run_command(cfg, tx, app, terminal, Command::Switch(slug));
            }
            let dir = session
                .cwd
                .clone()
                .or(project.dir.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let command = claude_command(
                "claude",
                session.config_dir.as_deref(),
                &format!("--resume {}", tmux::quote(&session.id)),
            );
            claude_window(
                cfg,
                app,
                terminal,
                &project,
                &dir,
                &command,
                ClaudePlace::NewWindow,
            )?;
            ops::mark_claude_thawed(&slug, &session.id)?;
        }
        Command::FreezeClaude { slug, id, note } => {
            let live = app.live.get(&id).cloned();
            let session = ops::mark_claude_frozen(
                &slug,
                &id,
                &note,
                live.as_ref().and_then(|l| l.name.clone()),
            )?;
            let label = session.label();
            match live {
                Some(live) => {
                    app.flash(format!("freezing {label}: ending Claude"));
                    let tx = tx.clone();
                    thread::spawn(move || {
                        let result = ops::end_claude(&Tmux, &live).map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Msg::ClaudeEnded(label, result));
                    });
                }
                None => app.flash(format!("froze {label}; t resumes it")),
            }
        }
        Command::NewClaude(slug) => {
            let project = store::load(&slug)?;
            let dir = project
                .dir
                .clone()
                .filter(|d| d.is_dir())
                .unwrap_or_else(|| app.start_dir.clone());
            let command =
                claude_command(&cfg.claude.command, cfg.claude.config_path().as_deref(), "");
            claude_window(
                cfg,
                app,
                terminal,
                &project,
                &dir,
                &command,
                ClaudePlace::SplitUnlessRunning,
            )?;
        }
        Command::Evaluate(slug, item) => {
            app.start_job(&item, "starting");
            app.respond_failures.remove(&item.key());
            app.responses = None;
            app.flash(format!(
                "Claude is answering the threads on {}; this takes a few minutes, and the row \
                 shows when it is done",
                item.label()
            ));
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let progress = |step: String| {
                    let _ = tx.send(Msg::Progress(item.clone(), step));
                };
                let result = respond::run(&cfg, &slug, &item, &progress)
                    .map(|e| e.responses.len())
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Evaluated(item, result));
            });
        }
        Command::SaveResponses(slug, evaluation) => respond::save(&slug, &evaluation)?,
        Command::EditReply(slug, item, index) => {
            let mut evaluation = respond::load(&slug, &item).context("the responses are gone")?;
            let response = evaluation
                .responses
                .get_mut(index)
                .context("the response is gone")?;
            let path = respond::draft_path(&slug);
            store::write_atomic(&path, format!("{}\n", response.reply).as_bytes())?;
            let result = suspended(terminal, || edit(&path));
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_file(&path);
            result?;
            if text.trim().is_empty() {
                app.flash("an empty reply is not saved; the draft is unchanged");
            } else {
                response.reply = text.trim().to_string();
                respond::save(&slug, &evaluation)?;
            }
        }
        Command::PostReplies(slug, item, picks) => {
            let evaluation = respond::load(&slug, &item).context("the responses are gone")?;
            app.start_job(&item, "posting");
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let progress = |step: String| {
                    let _ = tx.send(Msg::Progress(item.clone(), step));
                };
                let outcome = respond::post(&cfg, &evaluation, &picks, &progress);
                let _ = tx.send(Msg::Posted(slug, item, outcome));
            });
        }
        Command::HandFixes(slug, item, picks) => {
            let mut evaluation = respond::load(&slug, &item).context("the responses are gone")?;
            let url = app
                .state(&item)
                .and_then(|s| s.url.clone())
                .unwrap_or_default();
            let note = ops::capture(
                &slug,
                &format!("Fix plan for {}", item.label()),
                "ai",
                &url,
                &respond::fix_note(&evaluation, &picks),
            )?;
            for i in &picks {
                if let Some(r) = evaluation.responses.get_mut(*i) {
                    r.handed = true;
                }
            }
            respond::save(&slug, &evaluation)?;
            let project = store::load(&slug)?;
            let command = claude_command(
                &cfg.claude.command,
                cfg.claude.config_path().as_deref(),
                &tmux::quote(&respond::fix_prompt(&note)),
            );
            claude_window(
                cfg,
                app,
                terminal,
                &project,
                &evaluation.dir,
                &command,
                ClaudePlace::SplitUnlessRunning,
            )?;
        }
        Command::ReadClipboard(slug) => {
            let body = clipboard();
            if body.trim().is_empty() {
                app.fail("the clipboard is empty");
            } else {
                app.start_capture(slug, body);
            }
        }
    }
    Ok(())
}

fn owning_project(app: &App, path: &Path) -> Option<String> {
    let projects_dir = store::projects_dir();
    if let Ok(rest) = path.strip_prefix(&projects_dir) {
        return rest
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
    }
    app.projects
        .iter()
        .find(|p| {
            p.refs()
                .any(|r| matches!(r, ItemRef::File { path: f } if f == path))
        })
        .map(|p| p.slug.clone())
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

#[cfg(test)]
mod claude_tests {
    use super::*;

    #[test]
    fn claude_command_sets_the_config_environment() {
        assert_eq!(
            claude_command("claude", Some(Path::new("/c/d")), "--resume 'x'"),
            "CLAUDE_CONFIG_DIR='/c/d' CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1 claude --resume 'x'"
        );
        assert_eq!(claude_command(" claude ", None, ""), "claude");
    }

    #[test]
    fn the_post_summary_names_replies_and_resolved_threads() {
        let pr = ItemRef::AdoPr { id: 7 };
        let summary = |replied, resolved| respond::PostSummary {
            replied,
            resolved,
            error: None,
        };
        assert_eq!(
            post_summary(&summary(2, 1), &pr),
            "posted 2 replies and resolved 1 thread on PR 7"
        );
        assert_eq!(post_summary(&summary(1, 0), &pr), "posted 1 reply on PR 7");
        assert_eq!(
            post_summary(&summary(0, 2), &pr),
            "resolved 2 threads on PR 7"
        );
        assert_eq!(post_summary(&summary(0, 0), &pr), "nothing reached PR 7");
    }
}
