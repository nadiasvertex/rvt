mod app;
mod ci;
mod config;
mod model;
mod post;
mod review;
mod ui;

use std::fs;
use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use forge::{ado, claude, exec, gitlab};
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, Command, DetailState};
use config::Config;
use model::{Details, Diagnosis, Discussion, Item, Review, Source};

/// How often the screen redraws while a spinner turns.
const TICK: Duration = Duration::from_millis(100);
/// How often an idle screen redraws, so that "updated N ago" and comment ages stay current.
const IDLE: Duration = Duration::from_secs(30);

fn frame_interval(animating: bool) -> Duration {
    if animating { TICK } else { IDLE }
}

enum Msg {
    Input(Event),
    InputFailed(String),
    Listed(Source, Result<Vec<Item>, String>),
    Details(String, Result<Details, String>),
    Discussions(String, Result<Vec<Discussion>, String>),
    Me(Source, String),
    ReviewProgress(String, String),
    Reviewed(String, Result<Review, String>),
    DiagnosisProgress(String, String),
    Diagnosed(String, Result<Diagnosis, String>),
    PostProgress(String, String),
    Posted(String, chrono::DateTime<chrono::Utc>, post::Outcome),
    Approved(String, Result<(), String>),
}

fn board_cache() -> PathBuf {
    config::cache_dir().join("board.json")
}

fn load_cached_board() -> Vec<Item> {
    fs::read(board_cache())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_board(items: &[Item]) {
    let path = board_cache();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(items) {
        let _ = fs::write(path, bytes);
    }
}

fn spawn_list(cfg: &Arc<Config>, tx: &Sender<Msg>, source: Source) {
    let (cfg, tx) = (Arc::clone(cfg), tx.clone());
    thread::spawn(move || {
        let result = match source {
            Source::Ado => cfg.ado().and_then(|ado| ado::list(ado, &cfg.repo_areas)),
            Source::GitLab => cfg.gitlab().and_then(gitlab::list),
        };
        let _ = tx.send(Msg::Listed(source, result.map_err(|e| format!("{e:#}"))));
    });
}

/// Forwards terminal events to the main loop. The thread stays blocked in `event::read` when rvt
/// quits, and ends with the process.
fn spawn_input(tx: &Sender<Msg>) {
    let tx = tx.clone();
    thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(event) => Msg::Input(event),
                Err(e) => Msg::InputFailed(e.to_string()),
            };
            let failed = matches!(msg, Msg::InputFailed(_));
            if tx.send(msg).is_err() || failed {
                break;
            }
        }
    });
}

fn spawn_me(cfg: &Arc<Config>, tx: &Sender<Msg>) {
    for source in cfg.sources() {
        let (cfg, tx) = (Arc::clone(cfg), tx.clone());
        thread::spawn(move || {
            let me = match source {
                Source::Ado => ado::current_user(),
                Source::GitLab => cfg.gitlab().and_then(gitlab::current_user),
            };
            if let Ok(me) = me {
                let _ = tx.send(Msg::Me(source, me));
            }
        });
    }
}

fn execute(cfg: &Arc<Config>, tx: &Sender<Msg>, command: Command) {
    match command {
        Command::Refresh => {
            for source in cfg.sources() {
                spawn_list(cfg, tx, source);
            }
        }
        Command::LoadDetails(item) => {
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let result = review::details(&cfg, &item);
                let _ = tx.send(Msg::Details(
                    item.key(),
                    result.map_err(|e| format!("{e:#}")),
                ));
            });
        }
        Command::LoadDiscussions(item) => {
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let result = match item.source {
                    Source::Ado => cfg.ado().and_then(|ado| ado::discussions(ado, &item)),
                    Source::GitLab => cfg.gitlab().and_then(|gl| gitlab::discussions(gl, &item)),
                };
                let _ = tx.send(Msg::Discussions(
                    item.key(),
                    result.map_err(|e| format!("{e:#}")),
                ));
            });
        }
        Command::OpenUrl(url) => {
            let _ = Process::new("open").arg(url).spawn();
        }
        Command::RunReview(item) => {
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let key = item.key();
                let progress = |stage: &str| {
                    let _ = tx.send(Msg::ReviewProgress(key.clone(), stage.to_string()));
                };
                let result = review::run(&cfg, &item, &progress).and_then(|r| {
                    review::save(&r)?;
                    Ok(r)
                });
                let _ = tx.send(Msg::Reviewed(key, result.map_err(|e| format!("{e:#}"))));
            });
        }
        Command::SaveReview(r) => {
            let _ = review::save(&r);
        }
        Command::Diagnose(item) => {
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let key = item.key();
                let progress = |stage: &str| {
                    let _ = tx.send(Msg::DiagnosisProgress(key.clone(), stage.to_string()));
                };
                let result = ci::run(&cfg, &item, &progress).and_then(|d| {
                    ci::save(&d)?;
                    Ok(d)
                });
                let _ = tx.send(Msg::Diagnosed(key, result.map_err(|e| format!("{e:#}"))));
            });
        }
        Command::PostFindings {
            item,
            review,
            picks,
        } => {
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let key = item.key();
                let progress = |stage: &str| {
                    let _ = tx.send(Msg::PostProgress(key.clone(), stage.to_string()));
                };
                let outcome = post::post(&cfg, &item, &review, &picks, &progress);
                let _ = tx.send(Msg::Posted(key, review.created, outcome));
            });
        }
        Command::Approve(item) => {
            let (cfg, tx) = (Arc::clone(cfg), tx.clone());
            thread::spawn(move || {
                let result = match item.source {
                    Source::Ado => cfg.ado().and_then(|ado| ado::approve(ado, &item)),
                    Source::GitLab => cfg.gitlab().and_then(|gl| gitlab::approve(gl, &item)),
                };
                let _ = tx.send(Msg::Approved(
                    item.key(),
                    result.map_err(|e| format!("{e:#}")),
                ));
            });
        }
    }
}

fn apply(app: &mut App, msg: Msg) -> Vec<Command> {
    match msg {
        Msg::Listed(source, Ok(items)) => {
            app.set_items(source, items);
            if app.errors.is_empty() {
                save_board(&app.items);
            }
            return app.load_discussions();
        }
        Msg::Listed(source, Err(e)) => app.set_error(source, e),
        Msg::Details(key, Ok(d)) => app.set_details(key, DetailState::Loaded(d)),
        Msg::Details(key, Err(e)) => app.set_details(key, DetailState::Failed(e)),
        Msg::Me(source, me) => {
            app.me.insert(source, me);
            app.rebuild();
            return app.load_discussions();
        }
        Msg::Discussions(key, result) => return app.discussions_loaded(key, result),
        Msg::ReviewProgress(key, stage) => app.review_progress(&key, &stage),
        Msg::Reviewed(key, result) => return app.review_finished(key, result),
        Msg::DiagnosisProgress(key, stage) => app.diagnosis_progress(&key, &stage),
        Msg::Diagnosed(key, result) => app.diagnosis_finished(key, result),
        Msg::PostProgress(key, stage) => app.post_progress(&key, &stage),
        Msg::Posted(key, created, outcome) => return app.post_finished(key, created, outcome),
        Msg::Approved(key, result) => return app.approve_finished(key, result),
        Msg::Input(_) | Msg::InputFailed(_) => {}
    }
    Vec::new()
}

fn run(cfg: Arc<Config>, tx: Sender<Msg>, rx: Receiver<Msg>) -> Result<()> {
    let sources = cfg.sources();
    let cached = load_cached_board()
        .into_iter()
        .filter(|item| sources.contains(&item.source))
        .collect();
    let mut app = App::new(cached);
    app.sources = sources;
    app.max_parallel = cfg.review.max_parallel;
    app.refresh_interval = cfg.board.refresh_interval();
    app.reviews = review::load_all()
        .into_iter()
        .map(|r| (r.key.clone(), r))
        .collect();
    app.diagnoses = ci::load_all()
        .into_iter()
        .map(|d| (d.key.clone(), d))
        .collect();
    for command in app.refresh() {
        execute(&cfg, &tx, command);
    }
    spawn_me(&cfg, &tx);

    let mut terminal = ratatui::init();
    spawn_input(&tx);
    let mut tick = 0usize;
    let mut dirty = true;
    let mut deadline = Instant::now();
    let result = loop {
        if dirty {
            if let Err(e) = terminal.draw(|frame| ui::draw(frame, &mut app, tick)) {
                break Err(e.into());
            }
            dirty = false;
        }
        let wake = app
            .refresh_due_at()
            .map_or(deadline, |due| deadline.min(due));
        let mut next = match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => {
                for command in app.refresh_if_due(Instant::now()) {
                    execute(&cfg, &tx, command);
                }
                tick = tick.wrapping_add(1);
                dirty = true;
                if Instant::now() >= deadline {
                    deadline = Instant::now() + frame_interval(app.animating());
                }
                None
            }
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        };
        let mut failed = None;
        while let Some(msg) = next {
            match msg {
                Msg::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    for command in app.handle_key(key) {
                        execute(&cfg, &tx, command);
                    }
                    dirty = true;
                }
                Msg::Input(Event::Resize(..)) => dirty = true,
                Msg::Input(_) => {}
                Msg::InputFailed(e) => failed = Some(e),
                msg => {
                    for command in apply(&mut app, msg) {
                        execute(&cfg, &tx, command);
                    }
                    dirty = true;
                }
            }
            next = rx.try_recv().ok();
        }
        if let Some(e) = failed {
            break Err(anyhow::anyhow!(e));
        }
        deadline = deadline.min(Instant::now() + frame_interval(app.animating()));
        if app.quit {
            break Ok(());
        }
    };
    ratatui::restore();
    exec::kill_children();
    result
}

fn main() -> Result<()> {
    let cfg = Arc::new(config::load()?);
    let (tx, rx) = mpsc::channel();
    run(cfg, tx, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn snapshot(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::draw(f, app, 0)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn redraws_on_a_timer_only_while_animating() {
        assert_eq!(frame_interval(true), TICK);
        assert_eq!(frame_interval(false), IDLE);
        assert!(IDLE >= Duration::from_secs(10));
    }

    #[test]
    fn panes_share_one_divider() {
        let screen = snapshot(&mut App::new(Vec::new()), 40, 6);
        let lines: Vec<&str> = screen.lines().collect();
        assert_eq!(lines[1].chars().filter(|&c| c == '┬').count(), 1);
        assert_eq!(lines[1].matches("╮╭").count(), 0);
        assert!(lines[1].ends_with('╮'), "{}", lines[1]);
        assert_eq!(lines[4].chars().filter(|&c| c == '┴').count(), 1);
        assert_eq!(lines[2].chars().filter(|&c| c == '│').count(), 3);
    }

    #[test]
    fn findings_view_lists_findings_and_shows_the_one_under_the_cursor() {
        let item: Item = serde_json::from_value(serde_json::json!({
            "source": "GitLab", "id": 1405, "project_id": 1, "repo": "parser",
            "group": "parser", "title": "Enable thread safety", "author": "pat",
            "author_name": "C", "reviewers": [], "status": "Active", "source_branch": "b",
            "target_branch": "master", "description": "", "web_url": "", "head_sha": "new",
            "created": "2026-10-01T00:00:00Z"
        }))
        .unwrap();
        let finding = |severity, file: &str, inline, selected, title: &str| model::Finding {
            severity,
            file: Some(file.into()),
            line: Some(41),
            title: title.into(),
            body: format!("Body of {title}."),
            inline,
            selected,
            posted: false,
        };
        let mut app = App::new(vec![item.clone()]);
        app.reviews.insert(
            item.key(),
            Review {
                key: item.key(),
                head_sha: "old".into(),
                base_sha: "b".into(),
                created: chrono::Utc::now(),
                findings: vec![
                    finding(
                        model::Severity::High,
                        "src/parse.cpp",
                        true,
                        true,
                        "Overflow",
                    ),
                    finding(model::Severity::Low, "docs/x.md", false, false, "Typo"),
                ],
            },
        );
        let board = snapshot(&mut app, 100, 12);
        assert!(board.contains("Active     R│"), "{board}");

        for c in "jfj".chars() {
            app.handle_key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Char(c),
            ));
        }
        let screen = snapshot(&mut app, 100, 14);
        assert!(screen.contains("1 of 2 selected · stale"), "{screen}");
        assert!(screen.contains("[x] HIGH src/parse.cpp:41"), "{screen}");
        assert!(screen.contains("[ ] LOW  docs/x.md:41"), "{screen}");
        assert!(
            screen.contains("Posts as a PR comment: docs/x.md:41 is outside the"),
            "{screen}"
        );
        assert!(screen.contains("Body of Typo."), "{screen}");
        assert!(!screen.contains("Body of Overflow."), "{screen}");
    }

    /// Reviews one live item and prints the findings. Run with
    /// `RVT_REVIEW=gitlab-parser-1403 cargo test live_review -- --ignored --nocapture`.
    #[test]
    #[ignore = "clones a repository and runs claude"]
    fn live_review() {
        let cfg = config::load().unwrap();
        let key = std::env::var("RVT_REVIEW").expect("set RVT_REVIEW to an item key");
        let items = if key.starts_with("ado-") {
            ado::list(cfg.ado().unwrap(), &cfg.repo_areas).unwrap()
        } else {
            gitlab::list(cfg.gitlab().unwrap()).unwrap()
        };
        let item = items
            .into_iter()
            .find(|i| i.key() == key)
            .expect("no such item");
        let started = std::time::Instant::now();
        let progress = |stage: &str| eprintln!("{:>6.1?} {stage}", started.elapsed());
        let review = review::run(&cfg, &item, &progress).unwrap();
        review::save(&review).unwrap();
        eprintln!("done in {:?}: {}", started.elapsed(), review.summary());
        let mut app = App::new(vec![item]);
        app.reviews.insert(review.key.clone(), review);
        app.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('j'),
        ));
        app.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('f'),
        ));
        println!("{}", snapshot(&mut app, 150, 30));
    }

    /// Diagnoses a failed GitLab run and prints the detail pane. Run with
    /// `RVT_DIAGNOSE=gitlab-parser-1366 cargo test live_diagnose -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads CI logs and runs claude"]
    fn live_diagnose() {
        let cfg = config::load().unwrap();
        let key = std::env::var("RVT_DIAGNOSE").expect("set RVT_DIAGNOSE to an item key");
        let item = gitlab::list(cfg.gitlab().unwrap())
            .unwrap()
            .into_iter()
            .find(|i| i.key() == key)
            .expect("no such item");
        let started = std::time::Instant::now();
        let progress = |stage: &str| eprintln!("{:>6.1?} {stage}", started.elapsed());
        let diagnosis = ci::run(&cfg, &item, &progress).unwrap();
        ci::save(&diagnosis).unwrap();
        eprintln!("done in {:?}", started.elapsed());
        let mut app = App::new(vec![item]);
        app.diagnoses.insert(diagnosis.key.clone(), diagnosis);
        app.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('j'),
        ));
        app.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Enter,
        ));
        println!("{}", snapshot(&mut app, 150, 40));
    }

    /// Loads the live board and prints it. Run with `cargo test live -- --ignored --nocapture`.
    #[test]
    #[ignore = "talks to Azure DevOps and GitLab"]
    fn live_snapshot() {
        let cfg = Arc::new(config::load().unwrap());
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(Vec::new());
        app.loading = app::SOURCES.into_iter().collect();
        execute(&cfg, &tx, Command::Refresh);
        spawn_me(&cfg, &tx);
        let started = std::time::Instant::now();
        let mut threads_pending = 0;
        while !app.loading.is_empty() || app.me.len() < 2 || threads_pending > 0 {
            let Ok(msg) = rx.recv_timeout(Duration::from_secs(120)) else {
                continue;
            };
            if matches!(msg, Msg::Discussions(..)) {
                threads_pending -= 1;
            }
            for command in apply(&mut app, msg) {
                if let Command::LoadDiscussions(_) = command {
                    threads_pending += 1;
                    execute(&cfg, &tx, command);
                }
            }
        }
        eprintln!("loaded in {:?}; errors {:?}", started.elapsed(), app.errors);
        let width = std::env::var("RVT_W")
            .ok()
            .and_then(|w| w.parse().ok())
            .unwrap_or(150);
        let height = std::env::var("RVT_H")
            .ok()
            .and_then(|h| h.parse().ok())
            .unwrap_or(45);
        if let Ok(keys) = std::env::var("RVT_KEYS") {
            for c in keys.chars() {
                let code = match c {
                    '\n' => ratatui::crossterm::event::KeyCode::Enter,
                    '\t' => ratatui::crossterm::event::KeyCode::Tab,
                    c => ratatui::crossterm::event::KeyCode::Char(c),
                };
                let key = ratatui::crossterm::event::KeyEvent::from(code);
                for command in app.handle_key(key) {
                    if let Command::LoadDetails(_) = command {
                        execute(&cfg, &tx, command);
                        if let Ok(msg) = rx.recv_timeout(Duration::from_secs(60)) {
                            apply(&mut app, msg);
                        }
                    }
                }
            }
        }
        println!("{}", snapshot(&mut app, width, height));
    }
}
