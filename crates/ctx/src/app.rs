use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::brief::{self, Brief};
use crate::cache::{self, Cache};
use crate::changes::Change;
use crate::inbox::{self, Inbox};
use crate::item::{ItemRef, ItemState};
use crate::ops::{self, Since};
use crate::respond::{self, Evaluation};
use crate::search::{self, Hit};
use crate::store::{self, ClaudeSession, Handoff, Note, Project, State};
use crate::wait::{Condition, parse_when};

/// A single-line text field.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input {
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
}

impl Input {
    pub fn new(text: &str) -> Self {
        Input {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    fn byte(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    /// Edits the text for one key. Returns false for keys the field does not use.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.truncate(at);
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte(start), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char(_) if ctrl || key.modifiers.contains(KeyModifiers::ALT) => return false,
            KeyCode::Char(c) => {
                let at = self.byte(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let (a, b) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let (a, b) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.text.replace_range(a..b, "");
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Home,
    Project(String),
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Live,
    Ready,
    Waiting,
    Unfiled,
    Done,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Live => "Live",
            Group::Ready => "Ready",
            Group::Waiting => "Waiting",
            Group::Unfiled => "Unfiled",
            Group::Done => "Done",
        }
    }
}

/// A selectable row on the home screen: a project, or one of your items that no project holds.
#[derive(Debug, Clone, PartialEq)]
pub enum HomeRow {
    Project(String),
    Item(ItemRef),
}

/// A selectable row on the project screen.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Item(ItemRef),
    Suggestion(ItemRef),
    Note(PathBuf),
    Claude(String),
}

pub const FREEZE_FIELDS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct FreezeForm {
    pub slug: String,
    pub doing: Input,
    pub next: Input,
    pub waiting: Input,
    pub condition: Option<Condition>,
    pub close: bool,
    /// 0 doing, 1 next, 2 waiting on, 3 condition, 4 close session.
    pub focus: usize,
}

/// What a chosen wait option sets.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitChoice {
    Set(Condition),
    /// Asks for text: a manual wait, or a date for `until`.
    Ask(AskKind),
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    Manual,
    Until,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaitPicker {
    pub slug: String,
    pub options: Vec<(String, WaitChoice)>,
    pub selected: usize,
    pub asking: Option<(AskKind, Input)>,
    /// The picker fills in the freeze form instead of setting the wait.
    pub for_freeze: bool,
    pub error: Option<String>,
}

impl WaitPicker {
    /// Moves the selection to the first option that watches `item`, when there is one.
    pub fn select_item(&mut self, item: &ItemRef) {
        let key = item.key();
        if let Some(at) = self.options.iter().position(
            |(_, choice)| matches!(choice, WaitChoice::Set(c) if c.item() == Some(key.as_str())),
        ) {
            self.selected = at;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewProjectForm {
    pub name: Input,
    pub dir: Input,
    /// 0 name, 1 directory.
    pub focus: usize,
    pub error: Option<String>,
    /// An unfiled item the new project starts with.
    pub item: Option<ItemRef>,
}

/// Picks the project an unfiled item goes to.
#[derive(Debug, Clone, PartialEq)]
pub struct AssignPicker {
    pub item: ItemRef,
    pub filter: Input,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureForm {
    pub slug: String,
    pub title: Input,
    pub link: Input,
    pub source: usize,
    pub body: String,
    /// 0 title, 1 source, 2 link.
    pub focus: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Confirm {
    Done(String),
    Remove(String, String),
}

/// The responses Claude drafted for one PR's threads, shown over the project screen.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseView {
    pub slug: String,
    pub evaluation: Evaluation,
    pub cursor: usize,
    /// `p` asked to post the checked replies, and `y` confirms.
    pub confirm: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    Freeze(FreezeForm),
    Wait(WaitPicker),
    NewProject(NewProjectForm),
    Assign(AssignPicker),
    AddItem {
        slug: String,
        input: Input,
        error: Option<String>,
    },
    Next {
        slug: String,
        input: Input,
    },
    Capture(CaptureForm),
    Import {
        slug: String,
        input: Input,
    },
    Confirm(Confirm),
    Help,
}

/// Side effects for the main loop to run.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Refresh(Vec<ItemRef>),
    Switch(String),
    OpenUrl(String),
    Edit(PathBuf),
    NewNote(String),
    Freeze {
        slug: String,
        handoff: Handoff,
        condition: Option<Condition>,
        close: bool,
    },
    Thaw(String),
    Done(String),
    Reopen(String),
    SetWait(String, Option<Condition>),
    Create {
        name: String,
        dir: PathBuf,
        item: Option<ItemRef>,
    },
    /// Look for your PRs, MRs and work items again.
    RefreshInbox,
    AddItem(String, String),
    AddRef(String, ItemRef),
    Remove(String, String),
    Dismiss(String, String),
    SetNext(String, String),
    Capture {
        slug: String,
        title: String,
        source: String,
        link: String,
        body: String,
    },
    Import(String, String),
    ResumeClaude(String, ClaudeSession),
    NewClaude(String),
    ReadClipboard(String),
    /// Ask Claude to judge the threads that wait on you and draft responses.
    Evaluate(String, ItemRef),
    SaveResponses(String, Evaluation),
    EditReply(String, ItemRef, usize),
    PostReplies(String, ItemRef, Vec<usize>),
    /// Save the checked plans as a note and start Claude on them.
    HandFixes(String, ItemRef, Vec<usize>),
}

pub struct Flash {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

/// Claude's work on one item's threads: reading them, or posting replies.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub label: String,
    /// What it is doing now, such as "loading the threads".
    pub step: String,
    pub started: Instant,
}

/// How far J and K move the detail pane.
const DETAIL_STEP: i32 = 3;

pub struct App {
    pub projects: Vec<Project>,
    pub cache: Cache,
    pub notes: HashMap<String, Vec<Note>>,
    pub since: HashMap<String, Since>,
    pub briefs: HashMap<String, Brief>,
    pub journal: String,
    /// The project whose tmux session this process runs in.
    pub current: Option<String>,
    /// The project a window-0 instance belongs to. Esc on the home screen goes back to it.
    pub own: Option<String>,
    /// Running in the tmux popup: Esc on the home screen and every switch close ctx.
    pub popup: bool,
    pub screen: Screen,
    pub home_selected: usize,
    pub project_selected: usize,
    pub search_input: Input,
    pub search_all: bool,
    pub hits: Vec<Hit>,
    pub search_selected: usize,
    pub search_error: Option<String>,
    pub modal: Option<Modal>,
    /// A modal waiting under the wait picker, such as the freeze form.
    pub parked: Option<Modal>,
    pub flash: Option<Flash>,
    pub fetching: HashSet<String>,
    pub show_done: bool,
    pub close_default: bool,
    pub soft_limit: usize,
    /// The active pane when the popup opened, which a "Claude finishes" wait watches.
    pub pane: Option<String>,
    /// Where a new project starts unless the dialog changes it: the directory of the pane ctx
    /// was opened from.
    pub start_dir: PathBuf,
    pub quit: bool,
    /// The project a search started from, for the "this project" scope.
    pub own_search: Option<String>,
    /// Running tmux sessions, refreshed with the projects.
    pub sessions: HashSet<String>,
    /// Rows scrolled in the detail pane. Choosing another item starts again at the top.
    pub detail_scroll: u16,
    /// The detail pane fills the screen, for reading a long document or thread.
    pub reading: bool,
    /// Resolved comment threads show in the detail pane, grayed out.
    pub show_resolved: bool,
    /// The detail pane's row count and visible height when it was last drawn, which bound
    /// scrolling.
    pub detail_size: Cell<(u16, u16)>,
    /// Claude's responses to the threads of the selected PR or MR, while they are open.
    pub responses: Option<ResponseView>,
    /// Items whose threads Claude is reading or whose replies are posting, by item key.
    pub responding: HashMap<String, Job>,
    /// Why Claude last failed to answer an item's threads, until it is asked again.
    pub respond_failures: HashMap<String, String>,
    /// Replies Claude drafted that are not posted yet, by item key.
    pub drafts: HashMap<String, usize>,
    /// Your items found on the servers, some of which no project holds.
    pub inbox: Inbox,
    /// The inbox queries are running.
    pub discovering: bool,
    pending_g: bool,
}

impl App {
    pub fn new(current: Option<String>, popup: bool) -> Self {
        App {
            projects: Vec::new(),
            cache: Cache::default(),
            notes: HashMap::new(),
            since: HashMap::new(),
            briefs: HashMap::new(),
            journal: String::new(),
            current,
            own: None,
            popup,
            screen: Screen::Home,
            home_selected: 0,
            project_selected: 0,
            search_input: Input::default(),
            search_all: false,
            hits: Vec::new(),
            search_selected: 0,
            search_error: None,
            modal: None,
            parked: None,
            flash: None,
            fetching: HashSet::new(),
            show_done: false,
            close_default: false,
            soft_limit: 5,
            pane: None,
            start_dir: PathBuf::from("."),
            quit: false,
            own_search: None,
            sessions: HashSet::new(),
            detail_scroll: 0,
            reading: false,
            show_resolved: false,
            detail_size: Cell::new((0, 0)),
            responses: None,
            responding: HashMap::new(),
            respond_failures: HashMap::new(),
            drafts: HashMap::new(),
            inbox: Inbox::default(),
            discovering: false,
            pending_g: false,
        }
    }

    /// Reads projects, notes and marks from disk again.
    pub fn reload(&mut self) {
        self.cache = cache::load();
        self.inbox = inbox::load();
        self.projects = store::list();
        if self.clear_met_waits() {
            self.projects = store::list();
        }
        for p in &mut self.projects {
            p.claude_sessions.retain(|s| s.resumable());
        }
        self.sessions = crate::tmux::sessions().into_iter().collect();
        self.notes = self
            .projects
            .iter()
            .map(|p| (p.slug.clone(), store::notes(&p.slug)))
            .collect();
        self.since = self
            .projects
            .iter()
            .map(|p| {
                let notes = self.notes.get(&p.slug).map(Vec::as_slice).unwrap_or(&[]);
                (p.slug.clone(), ops::since(p, &self.cache.items, notes))
            })
            .collect();
        self.briefs = self
            .projects
            .iter()
            .filter_map(|p| brief::load_brief(&p.slug).map(|b| (p.slug.clone(), b)))
            .collect();
        self.reload_responses();
        self.drafts.clear();
        for p in &self.projects {
            for (key, n) in respond::unposted(&p.slug) {
                *self.drafts.entry(key).or_default() += n;
            }
        }
        if let Screen::Project(slug) = &self.screen {
            if !self.projects.iter().any(|p| &p.slug == slug) {
                self.screen = Screen::Home;
            } else {
                self.journal = store::journal(slug);
            }
        }
        self.clamp();
    }

    /// Reads the open responses again, since posting and editing change them on disk.
    fn reload_responses(&mut self) {
        let Some(view) = self.responses.take() else {
            return;
        };
        self.responses = respond::load(&view.slug, &view.evaluation.item).map(|evaluation| {
            let last = evaluation.responses.len().saturating_sub(1);
            ResponseView {
                cursor: view.cursor.min(last),
                evaluation,
                ..view
            }
        });
    }

    /// Clears wait conditions that are met, so the TUI works without `ctx watch` running.
    /// Returns true when any project changed.
    fn clear_met_waits(&self) -> bool {
        let now = Utc::now();
        let mut changed = false;
        for project in &self.projects {
            let Some(wait) = project.wait.as_ref().filter(|w| w.is_waiting()) else {
                continue;
            };
            let events = store::events(&project.slug);
            if crate::wait::check(wait, &self.cache.items, &events, now).is_some() {
                changed |= ops::evaluate(&project.slug).is_some();
            }
        }
        changed
    }

    pub fn reload_cache(&mut self) {
        self.cache = cache::load();
        self.since = self
            .projects
            .iter()
            .map(|p| {
                let notes = self.notes.get(&p.slug).map(Vec::as_slice).unwrap_or(&[]);
                (p.slug.clone(), ops::since(p, &self.cache.items, notes))
            })
            .collect();
    }

    pub fn project(&self, slug: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.slug == slug)
    }

    pub fn state(&self, item: &ItemRef) -> Option<&ItemState> {
        self.cache.items.get(&item.key())
    }

    pub fn changes(&self, slug: &str) -> &[Change] {
        self.since
            .get(slug)
            .map(|s| s.changes.as_slice())
            .unwrap_or(&[])
    }

    /// Records that Claude started work on `item`'s threads.
    pub fn start_job(&mut self, item: &ItemRef, step: &str) {
        self.responding.insert(
            item.key(),
            Job {
                label: item.label(),
                step: step.to_string(),
                started: Instant::now(),
            },
        );
    }

    pub fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub fn fail(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash {
            text: text.into(),
            error: true,
            at: Instant::now(),
        });
    }

    /// Projects in home screen order, grouped.
    pub fn groups(&self) -> Vec<(Group, Vec<&Project>)> {
        let visit = |p: &Project| self.since.get(&p.slug).and_then(|s| s.time);
        let mut live: Vec<&Project> = self
            .projects
            .iter()
            .filter(|p| p.state == State::Live)
            .collect();
        live.sort_by_key(|p| {
            let rank = if p.cleared().is_some() {
                0
            } else if p.waiting() {
                2
            } else {
                1
            };
            (rank, std::cmp::Reverse(visit(p)))
        });
        let frozen = self.projects.iter().filter(|p| p.state == State::Frozen);
        let mut ready: Vec<&Project> = frozen.clone().filter(|p| !p.waiting()).collect();
        ready.sort_by_key(|p| {
            let cleared_at = p
                .wait
                .as_ref()
                .and_then(|w| w.cleared.as_ref())
                .map(|c| c.at);
            (
                cleared_at.is_none(),
                std::cmp::Reverse(cleared_at),
                std::cmp::Reverse(p.handoff.as_ref().map(|h| h.time)),
            )
        });
        let mut waiting: Vec<&Project> = frozen.filter(|p| p.waiting()).collect();
        waiting.sort_by_key(|p| p.wait.as_ref().map(|w| w.set));
        let mut groups = vec![
            (Group::Live, live),
            (Group::Ready, ready),
            (Group::Waiting, waiting),
        ];
        if self.show_done {
            let done: Vec<&Project> = self
                .projects
                .iter()
                .filter(|p| p.state == State::Done)
                .collect();
            groups.push((Group::Done, done));
        }
        groups.retain(|(_, ps)| !ps.is_empty());
        groups
    }

    /// Your items that no project holds.
    pub fn unfiled(&self) -> Vec<ItemRef> {
        inbox::unfiled(&self.inbox, &self.projects, &self.cache.items)
    }

    /// The home screen's sections in display order: the project groups, with the unfiled items
    /// before Done.
    pub fn home_sections(&self) -> Vec<(Group, Vec<HomeRow>)> {
        let project_rows = |ps: Vec<&Project>| {
            ps.iter()
                .map(|p| HomeRow::Project(p.slug.clone()))
                .collect()
        };
        let mut sections: Vec<(Group, Vec<HomeRow>)> = Vec::new();
        let mut done = None;
        for (group, ps) in self.groups() {
            if group == Group::Done {
                done = Some(project_rows(ps));
            } else {
                sections.push((group, project_rows(ps)));
            }
        }
        let unfiled: Vec<HomeRow> = self.unfiled().into_iter().map(HomeRow::Item).collect();
        if !unfiled.is_empty() {
            sections.push((Group::Unfiled, unfiled));
        }
        if let Some(rows) = done {
            sections.push((Group::Done, rows));
        }
        sections
    }

    pub fn home_order(&self) -> Vec<HomeRow> {
        self.home_sections()
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .collect()
    }

    pub fn selected_row(&self) -> Option<HomeRow> {
        self.home_order().get(self.home_selected).cloned()
    }

    pub fn selected_slug(&self) -> Option<String> {
        match self.selected_row() {
            Some(HomeRow::Project(slug)) => Some(slug),
            _ => None,
        }
    }

    /// Projects an unfiled item can go to, filtered by name. Projects that already suggest the
    /// item come first.
    pub fn assign_choices(&self, picker: &AssignPicker) -> Vec<&Project> {
        let filter = picker.filter.text.trim().to_lowercase();
        let mut choices: Vec<&Project> = self
            .projects
            .iter()
            .filter(|p| p.state != State::Done)
            .filter(|p| filter.is_empty() || p.name.to_lowercase().contains(&filter))
            .collect();
        let key = picker.item.key();
        choices.sort_by_key(|p| {
            !ops::suggestions(p, &self.cache.items)
                .iter()
                .any(|(r, _)| r.key() == key)
        });
        choices
    }

    /// Selectable rows on the project screen, in display order.
    pub fn picks(&self, slug: &str) -> Vec<Pick> {
        let Some(project) = self.project(slug) else {
            return Vec::new();
        };
        let mut picks: Vec<Pick> = project.refs().cloned().map(Pick::Item).collect();
        picks.extend(
            ops::suggestions(project, &self.cache.items)
                .into_iter()
                .map(|(r, _)| Pick::Suggestion(r)),
        );
        if let Some(notes) = self.notes.get(slug) {
            picks.extend(notes.iter().map(|n| Pick::Note(n.path.clone())));
        }
        picks.extend(
            project
                .claude_sessions
                .iter()
                .rev()
                .map(|s| Pick::Claude(s.id.clone())),
        );
        picks
    }

    fn clamp(&mut self) {
        let home = self.home_order().len();
        self.home_selected = self.home_selected.min(home.saturating_sub(1));
        if let Screen::Project(slug) = &self.screen {
            let n = self.picks(slug).len();
            self.project_selected = self.project_selected.min(n.saturating_sub(1));
        }
        self.search_selected = self.search_selected.min(self.hits.len().saturating_sub(1));
    }

    pub fn open_project(&mut self, slug: &str) {
        self.screen = Screen::Project(slug.to_string());
        self.project_selected = 0;
        self.detail_scroll = 0;
        self.reading = false;
        self.responses = None;
        self.journal = store::journal(slug);
        self.clamp();
    }

    fn go_home(&mut self) {
        if let Some(slug) = self.current_slug() {
            if let Some(i) = self
                .home_order()
                .iter()
                .position(|r| *r == HomeRow::Project(slug.clone()))
            {
                self.home_selected = i;
            }
        }
        self.screen = Screen::Home;
    }

    fn current_slug(&self) -> Option<String> {
        match &self.screen {
            Screen::Project(slug) => Some(slug.clone()),
            _ => None,
        }
    }

    /// After a switch away from this process's session, a window-0 instance goes back to its own
    /// project, ready for the next visit.
    pub fn switched(&mut self, slug: &str) {
        if self.popup {
            self.quit = true;
            return;
        }
        if self.current.as_deref() == Some(slug) {
            self.open_project(slug);
            return;
        }
        match self.own.clone() {
            Some(own) => self.open_project(&own),
            None => self.screen = Screen::Home,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        if self.modal.is_some() {
            return self.modal_key(key);
        }
        let commands = match self.screen.clone() {
            Screen::Home => self.home_key(key),
            Screen::Project(slug) => self.project_key(&slug, key),
            Screen::Search => self.search_key(key),
        };
        self.clamp();
        commands
    }

    /// Movement shared by the lists: j k, arrows, gg G, Ctrl-d Ctrl-u.
    fn movement(&mut self, key: KeyEvent, selected: usize, len: usize) -> Option<usize> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = len.saturating_sub(1);
        let g = std::mem::take(&mut self.pending_g);
        let to = match key.code {
            KeyCode::Char('j') | KeyCode::Down => (selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => selected.saturating_sub(1),
            KeyCode::Char('d') if ctrl => (selected + 10).min(last),
            KeyCode::Char('u') if ctrl => selected.saturating_sub(10),
            KeyCode::Char('G') | KeyCode::End => last,
            KeyCode::Home => 0,
            KeyCode::Char('g') if g => 0,
            KeyCode::Char('g') => {
                self.pending_g = true;
                return Some(selected);
            }
            _ => return None,
        };
        Some(to)
    }

    fn home_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let len = self.home_order().len();
        if let Some(to) = self.movement(key, self.home_selected, len) {
            self.home_selected = to;
            return Vec::new();
        }
        let selected = self.selected_slug();
        if let Some(HomeRow::Item(item)) = self.selected_row() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('l' | 'o' | ' ') => return self.open_item(&item),
                KeyCode::Char('a') => {
                    self.modal = Some(Modal::Assign(AssignPicker {
                        item,
                        filter: Input::default(),
                        selected: 0,
                    }));
                    return Vec::new();
                }
                KeyCode::Char('n') => {
                    let name = self
                        .state(&item)
                        .map(|s| s.title.clone())
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| item.label());
                    self.modal = Some(Modal::NewProject(NewProjectForm {
                        name: Input::new(&name),
                        dir: Input::new(&tilde(&self.start_dir)),
                        focus: 0,
                        error: None,
                        item: Some(item),
                    }));
                    return Vec::new();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(slug) = selected {
                    if self.current.as_deref() == Some(slug.as_str()) && !self.popup {
                        self.open_project(&slug);
                    } else {
                        return vec![Command::Switch(slug)];
                    }
                }
            }
            KeyCode::Char('o') | KeyCode::Char(' ') => {
                if let Some(slug) = selected {
                    self.open_project(&slug);
                }
            }
            KeyCode::Esc | KeyCode::Char('h') => {
                if self.popup {
                    self.quit = true;
                } else if let Some(own) = self.own.clone().or_else(|| self.current.clone()) {
                    self.open_project(&own);
                }
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('D') => self.show_done = !self.show_done,
            KeyCode::Char('R') => {
                return vec![Command::Refresh(self.all_items()), Command::RefreshInbox];
            }
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => {
                self.modal = Some(Modal::NewProject(NewProjectForm {
                    name: Input::default(),
                    dir: Input::new(&tilde(&self.start_dir)),
                    focus: 0,
                    error: None,
                    item: None,
                }))
            }
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            _ => return self.project_action(selected, key),
        }
        Vec::new()
    }

    pub fn all_items(&self) -> Vec<ItemRef> {
        crate::refresh::wanted(&self.projects, None)
    }

    /// Keys that act on one project, from the home screen or the project screen.
    fn project_action(&mut self, slug: Option<String>, key: KeyEvent) -> Vec<Command> {
        let Some(slug) = slug else {
            return Vec::new();
        };
        let Some(project) = self.project(&slug).cloned() else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Char('f') if project.state != State::Done => {
                self.modal = Some(Modal::Freeze(self.freeze_form(&project)))
            }
            KeyCode::Char('t') => match project.state {
                State::Frozen => return vec![Command::Thaw(slug)],
                State::Done => return vec![Command::Reopen(slug)],
                State::Live => self.flash(format!("{} is already live", project.name)),
            },
            KeyCode::Char('d') if project.state != State::Done => {
                self.modal = Some(Modal::Confirm(Confirm::Done(slug)))
            }
            KeyCode::Char('w') => self.modal = Some(Modal::Wait(self.wait_picker(&project, false))),
            KeyCode::Char('W') if project.wait.is_some() => {
                return vec![Command::SetWait(slug, None)];
            }
            KeyCode::Char('c') => return vec![Command::ReadClipboard(slug)],
            KeyCode::Char('N') => return vec![Command::NewNote(slug)],
            KeyCode::Char('I') => {
                self.modal = Some(Modal::Import {
                    slug,
                    input: Input::default(),
                })
            }
            KeyCode::Char('a') => {
                self.modal = Some(Modal::AddItem {
                    slug,
                    input: Input::default(),
                    error: None,
                })
            }
            KeyCode::Char('e') => {
                self.modal = Some(Modal::Next {
                    slug,
                    input: Input::new(&project.next),
                })
            }
            KeyCode::Char('s') => return vec![Command::Switch(slug)],
            KeyCode::Char('C') => return vec![Command::NewClaude(slug)],
            _ => {}
        }
        Vec::new()
    }

    fn project_key(&mut self, slug: &str, key: KeyEvent) -> Vec<Command> {
        if self.reading {
            self.reader_key(key);
            return Vec::new();
        }
        if self.responses.is_some() {
            return self.responses_key(key);
        }
        let picks = self.picks(slug);
        if let Some(to) = self.movement(key, self.project_selected, picks.len()) {
            if to != self.project_selected {
                self.detail_scroll = 0;
            }
            self.project_selected = to;
            return Vec::new();
        }
        let pick = picks.get(self.project_selected).cloned();
        match (key.code, &pick) {
            (KeyCode::Esc | KeyCode::Char('h'), _) => {
                if self.popup && self.current.as_deref() != Some(slug) {
                    self.screen = Screen::Home;
                } else {
                    self.go_home();
                }
            }
            (KeyCode::Char('q'), _) => self.quit = true,
            (KeyCode::Char('/'), _) => self.start_search(),
            (KeyCode::Char('?'), _) => self.modal = Some(Modal::Help),
            (KeyCode::Char('J'), _) => self.scroll_detail(DETAIL_STEP),
            (KeyCode::Char('K'), _) => self.scroll_detail(-DETAIL_STEP),
            (KeyCode::Char('u'), _) => self.toggle_resolved(),
            (KeyCode::Char('v'), Some(_)) => self.reading = true,
            (KeyCode::Char('r'), Some(Pick::Item(item))) => return self.respond_to(slug, item),
            (KeyCode::Char('w'), Some(Pick::Item(item))) => {
                if let Some(project) = self.project(slug) {
                    let mut picker = self.wait_picker(project, false);
                    picker.select_item(item);
                    self.modal = Some(Modal::Wait(picker));
                }
            }
            (KeyCode::Char('R'), _) => {
                let items = self
                    .project(slug)
                    .map(|p| p.refs().cloned().collect())
                    .unwrap_or_default();
                return vec![Command::Refresh(items)];
            }
            (KeyCode::Enter | KeyCode::Char('o'), Some(Pick::Item(item))) => {
                return self.open_item(item);
            }
            (KeyCode::Enter | KeyCode::Char('a'), Some(Pick::Suggestion(item))) => {
                return vec![Command::AddRef(slug.to_string(), item.clone())];
            }
            (KeyCode::Char('x'), Some(Pick::Suggestion(item))) => {
                return vec![Command::Dismiss(slug.to_string(), item.key())];
            }
            (KeyCode::Char('x'), Some(Pick::Item(item))) => {
                self.modal = Some(Modal::Confirm(Confirm::Remove(
                    slug.to_string(),
                    item.key(),
                )));
            }
            (KeyCode::Enter | KeyCode::Char('o'), Some(Pick::Note(path))) => {
                return vec![Command::Edit(path.clone())];
            }
            (KeyCode::Enter | KeyCode::Char('o'), Some(Pick::Claude(id))) => {
                let session = self
                    .project(slug)
                    .and_then(|p| p.claude_sessions.iter().find(|s| &s.id == id).cloned());
                if let Some(session) = session {
                    return vec![Command::ResumeClaude(slug.to_string(), session)];
                }
            }
            _ => return self.project_action(Some(slug.to_string()), key),
        }
        Vec::new()
    }

    /// Keys while the detail pane fills the screen: scrolling, and leaving.
    fn reader_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = i32::from(self.detail_size.get().1.saturating_sub(2).max(1));
        let g = std::mem::take(&mut self.pending_g);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_detail(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_detail(-1),
            KeyCode::Char('d') if ctrl => self.scroll_detail(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_detail(-page / 2),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_detail(page),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_detail(-page),
            KeyCode::Char('g') if g => self.detail_scroll = 0,
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('G') | KeyCode::End => self.scroll_detail(i32::from(u16::MAX)),
            KeyCode::Home => self.detail_scroll = 0,
            KeyCode::Char('u') => self.toggle_resolved(),
            KeyCode::Esc | KeyCode::Char('q' | 'v' | 'h') => self.reading = false,
            _ => {}
        }
    }

    /// Opens Claude's responses to the item's threads, or asks for them when there are none.
    fn respond_to(&mut self, slug: &str, item: &ItemRef) -> Vec<Command> {
        if !matches!(item, ItemRef::AdoPr { .. } | ItemRef::GitlabMr { .. }) {
            self.flash("r answers the comment threads on a PR or MR");
            return Vec::new();
        }
        if self.responding.contains_key(&item.key()) {
            self.flash(format!("Claude is still working on {}", item.label()));
            return Vec::new();
        }
        match respond::load(slug, item) {
            Some(evaluation) => {
                self.responses = Some(ResponseView {
                    slug: slug.to_string(),
                    evaluation,
                    cursor: 0,
                    confirm: false,
                });
                self.detail_scroll = 0;
                Vec::new()
            }
            None => vec![Command::Evaluate(slug.to_string(), item.clone())],
        }
    }

    /// Keys in the responses view: move, check, edit, post, hand plans to Claude, run again.
    fn responses_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(mut view) = self.responses.take() else {
            return Vec::new();
        };
        let (slug, item) = (view.slug.clone(), view.evaluation.item.clone());
        if std::mem::take(&mut view.confirm) {
            let commands = if key.code == KeyCode::Char('y') {
                let picks = view.evaluation.to_post(view.cursor);
                vec![Command::PostReplies(slug, item, picks)]
            } else {
                self.flash("nothing posted");
                Vec::new()
            };
            self.responses = Some(view);
            return commands;
        }
        let last = view.evaluation.responses.len().saturating_sub(1);
        let mut commands = Vec::new();
        let mut changed = false;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                view.cursor = (view.cursor + 1).min(last);
                self.detail_scroll = 0;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                view.cursor = view.cursor.saturating_sub(1);
                self.detail_scroll = 0;
            }
            KeyCode::Char('J') => self.scroll_detail(DETAIL_STEP),
            KeyCode::Char('K') => self.scroll_detail(-DETAIL_STEP),
            KeyCode::Char(' ') => {
                if let Some(r) = view.evaluation.responses.get_mut(view.cursor) {
                    r.selected = !r.selected;
                    changed = true;
                }
            }
            KeyCode::Char('a') => {
                let all = view.evaluation.responses.iter().all(|r| r.selected);
                for r in &mut view.evaluation.responses {
                    r.selected = !all;
                }
                changed = true;
            }
            KeyCode::Char('e') => match view.evaluation.responses.get(view.cursor) {
                Some(r) if r.posted => self.flash("that reply is already posted"),
                Some(_) => {
                    commands.push(Command::EditReply(slug.clone(), item.clone(), view.cursor))
                }
                None => {}
            },
            KeyCode::Char('r') => match view.evaluation.responses.get_mut(view.cursor) {
                Some(r) if r.resolved => self.flash("that thread is already resolved"),
                Some(r) => {
                    r.resolve = !r.resolve;
                    changed = true;
                }
                None => {}
            },
            KeyCode::Char('p') => {
                if view.evaluation.to_post(view.cursor).is_empty() {
                    self.flash("this reply is posted; check others with space to post them");
                } else if self.responding.contains_key(&item.key()) {
                    self.flash(format!("still working on {}", item.label()));
                } else {
                    view.confirm = true;
                }
            }
            KeyCode::Char('C') => {
                let picks = view.evaluation.plans_to_hand();
                if picks.is_empty() {
                    self.flash("check a response that has a plan with space first");
                } else {
                    commands.push(Command::HandFixes(slug.clone(), item.clone(), picks));
                }
            }
            KeyCode::Char('R') => {
                if self.responding.contains_key(&item.key()) {
                    self.flash(format!("Claude is still working on {}", item.label()));
                } else {
                    return vec![Command::Evaluate(slug, item)];
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.state(&item).and_then(|s| s.url.clone()) {
                    commands.push(Command::OpenUrl(url));
                }
            }
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Esc | KeyCode::Char('q' | 'h') => return Vec::new(),
            _ => {}
        }
        if changed {
            commands.push(Command::SaveResponses(slug, view.evaluation.clone()));
        }
        self.responses = Some(view);
        commands
    }

    /// Moves the detail pane by `rows`, staying between its top and its last screenful.
    fn scroll_detail(&mut self, rows: i32) {
        let (total, height) = self.detail_size.get();
        let last = i32::from(total.saturating_sub(height));
        let to = (i32::from(self.detail_scroll) + rows).clamp(0, last.max(0));
        self.detail_scroll = u16::try_from(to).unwrap_or(0);
    }

    fn toggle_resolved(&mut self) {
        self.show_resolved = !self.show_resolved;
        self.detail_scroll = 0;
    }

    fn open_item(&mut self, item: &ItemRef) -> Vec<Command> {
        match item {
            ItemRef::File { path } => vec![Command::Edit(path.clone())],
            ItemRef::Branch { repo, branch } => {
                self.flash(format!("{branch} in {}", repo.display()));
                Vec::new()
            }
            _ => match self.state(item).and_then(|s| s.url.clone()) {
                Some(url) => vec![Command::OpenUrl(url)],
                None => {
                    self.flash("not fetched yet; R refreshes");
                    Vec::new()
                }
            },
        }
    }

    fn start_search(&mut self) {
        self.search_all = !matches!(self.screen, Screen::Project(_));
        if let Screen::Project(slug) = &self.screen {
            self.own_search = Some(slug.clone());
        }
        self.screen = Screen::Search;
        self.search_input = Input::default();
        self.hits.clear();
        self.search_error = None;
        self.search_selected = 0;
    }

    pub fn run_search(&mut self) {
        let scope = if self.search_all {
            None
        } else {
            self.own_search.clone()
        };
        match search::search(&self.search_input.text, scope.as_deref(), 100) {
            Ok(hits) => {
                self.hits = hits;
                self.search_error = None;
            }
            Err(e) => self.search_error = Some(format!("{e:#}")),
        }
        self.search_selected = 0;
    }

    fn search_key(&mut self, key: KeyEvent) -> Vec<Command> {
        match key.code {
            KeyCode::Esc => {
                self.screen = match self.own_search.take() {
                    Some(slug) => Screen::Project(slug),
                    None => Screen::Home,
                };
                if let Screen::Project(slug) = self.screen.clone() {
                    self.open_project(&slug);
                }
            }
            KeyCode::Tab => {
                if self.own_search.is_some() {
                    self.search_all = !self.search_all;
                    self.run_search();
                }
            }
            KeyCode::Down => {
                self.search_selected =
                    (self.search_selected + 1).min(self.hits.len().saturating_sub(1))
            }
            KeyCode::Up => self.search_selected = self.search_selected.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(hit) = self.hits.get(self.search_selected).cloned() {
                    return self.open_hit(&hit);
                }
            }
            _ => {
                if self.search_input.handle(key) {
                    self.run_search();
                }
            }
        }
        Vec::new()
    }

    fn open_hit(&mut self, hit: &Hit) -> Vec<Command> {
        if hit.kind == "note" {
            return vec![Command::Edit(PathBuf::from(&hit.reference))];
        }
        self.own_search = None;
        self.open_project(&hit.project);
        if hit.kind == "item" {
            if let Some(i) = self
                .picks(&hit.project)
                .iter()
                .position(|p| matches!(p, Pick::Item(r) if r.key() == hit.reference))
            {
                self.project_selected = i;
            }
        }
        Vec::new()
    }

    pub fn freeze_form(&self, project: &Project) -> FreezeForm {
        let previous = project.handoff.clone();
        FreezeForm {
            slug: project.slug.clone(),
            doing: Input::new(
                previous
                    .as_ref()
                    .map(|h| h.doing.as_str())
                    .unwrap_or_default(),
            ),
            next: Input::new(&project.next),
            waiting: Input::new(
                previous
                    .as_ref()
                    .map(|h| h.waiting.as_str())
                    .unwrap_or_default(),
            ),
            condition: project
                .wait
                .as_ref()
                .filter(|w| w.is_waiting())
                .map(|w| w.condition.clone()),
            close: self.close_default,
            focus: 0,
        }
    }

    /// Wait options built from the project's items, newest item first.
    pub fn wait_picker(&self, project: &Project, for_freeze: bool) -> WaitPicker {
        let mut options: Vec<(String, WaitChoice)> = Vec::new();
        let pane = if for_freeze { None } else { self.pane.clone() };
        let claude_label = if pane.is_some() {
            "Claude finishes in this pane"
        } else {
            "Claude finishes in this project"
        };
        options.push((
            claude_label.into(),
            WaitChoice::Set(Condition::Claude { pane }),
        ));
        options.push((
            "A ctx run command finishes".into(),
            WaitChoice::Set(Condition::Command {
                label: String::new(),
            }),
        ));
        for item in project.refs().collect::<Vec<_>>().into_iter().rev() {
            let label = item.label();
            let key = item.key();
            match item {
                ItemRef::AdoPr { .. } | ItemRef::GitlabMr { .. } => {
                    if matches!(item, ItemRef::GitlabMr { .. }) {
                        options.push((
                            format!("CI finishes on {label}"),
                            WaitChoice::Set(Condition::CiDone {
                                item: key.clone(),
                                label: label.clone(),
                            }),
                        ));
                    }
                    options.push((
                        format!("Review on {label}: an approval, vote or comment"),
                        WaitChoice::Set(
                            Condition::Review {
                                item: key.clone(),
                                label: label.clone(),
                                after: None,
                                status: None,
                            }
                            .starting_now(&self.cache.items),
                        ),
                    ));
                    options.push((
                        format!("{label} merged or closed"),
                        WaitChoice::Set(Condition::Closed { item: key, label }),
                    ));
                }
                ItemRef::WorkItem { .. } => {
                    options.push((
                        format!("New comment on {label}"),
                        WaitChoice::Set(
                            Condition::Comment {
                                item: key.clone(),
                                label: label.clone(),
                                after: 0,
                            }
                            .starting_now(&self.cache.items),
                        ),
                    ));
                    options.push((
                        format!("{label} done"),
                        WaitChoice::Set(Condition::Closed { item: key, label }),
                    ));
                }
                _ => {}
            }
        }
        options.push(("A date…".into(), WaitChoice::Ask(AskKind::Until)));
        options.push((
            "Something else (no check)…".into(),
            WaitChoice::Ask(AskKind::Manual),
        ));
        if project.wait.is_some() || for_freeze {
            options.push(("No wait condition".into(), WaitChoice::Remove));
        }
        WaitPicker {
            slug: project.slug.clone(),
            options,
            selected: 0,
            asking: None,
            for_freeze,
            error: None,
        }
    }

    fn modal_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(modal) = self.modal.take() else {
            return Vec::new();
        };
        let (keep, commands) = match modal {
            Modal::Help => (None, Vec::new()),
            Modal::Freeze(form) => self.freeze_key(form, key),
            Modal::Wait(picker) => self.wait_key(picker, key),
            Modal::NewProject(form) => new_project_key(form, key),
            Modal::Assign(picker) => self.assign_key(picker, key),
            Modal::AddItem {
                slug,
                mut input,
                error,
            } => match key.code {
                KeyCode::Esc => (None, Vec::new()),
                KeyCode::Enter if !input.text.trim().is_empty() => (
                    None,
                    vec![Command::AddItem(slug, input.text.trim().to_string())],
                ),
                _ => {
                    input.handle(key);
                    (Some(Modal::AddItem { slug, input, error }), Vec::new())
                }
            },
            Modal::Next { slug, mut input } => match key.code {
                KeyCode::Esc => (None, Vec::new()),
                KeyCode::Enter => (None, vec![Command::SetNext(slug, input.text.clone())]),
                _ => {
                    input.handle(key);
                    (Some(Modal::Next { slug, input }), Vec::new())
                }
            },
            Modal::Import { slug, mut input } => match key.code {
                KeyCode::Esc => (None, Vec::new()),
                KeyCode::Enter if !input.text.trim().is_empty() => (
                    None,
                    vec![Command::Import(slug, input.text.trim().to_string())],
                ),
                _ => {
                    input.handle(key);
                    (Some(Modal::Import { slug, input }), Vec::new())
                }
            },
            Modal::Capture(form) => self.capture_key(form, key),
            Modal::Confirm(confirm) => match key.code {
                KeyCode::Char('y') => (
                    None,
                    vec![match confirm {
                        Confirm::Done(slug) => Command::Done(slug),
                        Confirm::Remove(slug, key) => Command::Remove(slug, key),
                    }],
                ),
                _ => (None, Vec::new()),
            },
        };
        if keep.is_some() {
            self.modal = keep;
        } else if self.modal.is_none() {
            self.modal = self.parked.take();
        }
        commands
    }

    fn assign_key(
        &mut self,
        mut picker: AssignPicker,
        key: KeyEvent,
    ) -> (Option<Modal>, Vec<Command>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let choices: Vec<String> = self
            .assign_choices(&picker)
            .iter()
            .map(|p| p.slug.clone())
            .collect();
        match key.code {
            KeyCode::Esc => return (None, Vec::new()),
            KeyCode::Enter => {
                return match choices.get(picker.selected) {
                    Some(slug) => (None, vec![Command::AddRef(slug.clone(), picker.item)]),
                    None => (Some(Modal::Assign(picker)), Vec::new()),
                };
            }
            KeyCode::Down | KeyCode::Tab => {
                picker.selected = (picker.selected + 1).min(choices.len().saturating_sub(1))
            }
            KeyCode::Char('n') if ctrl => {
                picker.selected = (picker.selected + 1).min(choices.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::BackTab => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => picker.selected = picker.selected.saturating_sub(1),
            _ => {
                if picker.filter.handle(key) {
                    picker.selected = 0;
                }
            }
        }
        (Some(Modal::Assign(picker)), Vec::new())
    }

    fn freeze_key(&mut self, mut form: FreezeForm, key: KeyEvent) -> (Option<Modal>, Vec<Command>) {
        match key.code {
            KeyCode::Esc => return (None, Vec::new()),
            KeyCode::Enter => {
                let handoff = Handoff {
                    time: Utc::now(),
                    doing: form.doing.text.trim().to_string(),
                    next: form.next.text.trim().to_string(),
                    waiting: form.waiting.text.trim().to_string(),
                };
                let condition = form.condition.clone().or_else(|| {
                    (!handoff.waiting.is_empty()).then(|| Condition::Manual {
                        text: handoff.waiting.clone(),
                    })
                });
                return (
                    None,
                    vec![Command::Freeze {
                        slug: form.slug,
                        handoff,
                        condition,
                        close: form.close,
                    }],
                );
            }
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % FREEZE_FIELDS,
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + FREEZE_FIELDS - 1) % FREEZE_FIELDS
            }
            KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left if form.focus == 3 => {
                if let Some(project) = self.project(&form.slug).cloned() {
                    let picker = self.wait_picker(&project, true);
                    self.parked = Some(Modal::Freeze(form));
                    return (Some(Modal::Wait(picker)), Vec::new());
                }
            }
            KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left if form.focus == 4 => {
                form.close = !form.close
            }
            _ => {
                let field = match form.focus {
                    0 => Some(&mut form.doing),
                    1 => Some(&mut form.next),
                    2 => Some(&mut form.waiting),
                    _ => None,
                };
                if let Some(field) = field {
                    field.handle(key);
                }
            }
        }
        (Some(Modal::Freeze(form)), Vec::new())
    }

    fn wait_key(&mut self, mut picker: WaitPicker, key: KeyEvent) -> (Option<Modal>, Vec<Command>) {
        if let Some((kind, mut input)) = picker.asking.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let condition = match kind {
                        AskKind::Manual if !input.text.trim().is_empty() => {
                            Some(Condition::Manual {
                                text: input.text.trim().to_string(),
                            })
                        }
                        AskKind::Until => {
                            parse_when(&input.text, Utc::now()).map(|at| Condition::Until { at })
                        }
                        AskKind::Manual => None,
                    };
                    match condition {
                        Some(c) => return self.choose_wait(picker, Some(c)),
                        None => {
                            picker.error = Some(match kind {
                                AskKind::Until => {
                                    "use 30m, 2h, 3d, a weekday such as thu, or 2026-11-20".into()
                                }
                                AskKind::Manual => "say what you are waiting on".into(),
                            });
                            picker.asking = Some((kind, input));
                        }
                    }
                }
                _ => {
                    input.handle(key);
                    picker.error = None;
                    picker.asking = Some((kind, input));
                }
            }
            return (Some(Modal::Wait(picker)), Vec::new());
        }
        let len = picker.options.len();
        match key.code {
            KeyCode::Esc => return (None, Vec::new()),
            KeyCode::Char('j') | KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(len.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Enter => match picker.options.get(picker.selected).map(|(_, c)| c.clone()) {
                Some(WaitChoice::Set(c)) => return self.choose_wait(picker, Some(c)),
                Some(WaitChoice::Remove) => return self.choose_wait(picker, None),
                Some(WaitChoice::Ask(kind)) => picker.asking = Some((kind, Input::default())),
                None => {}
            },
            _ => {}
        }
        (Some(Modal::Wait(picker)), Vec::new())
    }

    fn choose_wait(
        &mut self,
        picker: WaitPicker,
        condition: Option<Condition>,
    ) -> (Option<Modal>, Vec<Command>) {
        if picker.for_freeze {
            if let Some(Modal::Freeze(mut form)) = self.parked.take() {
                form.condition = condition;
                return (Some(Modal::Freeze(form)), Vec::new());
            }
            return (None, Vec::new());
        }
        (None, vec![Command::SetWait(picker.slug, condition)])
    }

    pub fn start_capture(&mut self, slug: String, body: String) {
        let title = crate::item::snippet(&body)
            .chars()
            .take(60)
            .collect::<String>();
        self.modal = Some(Modal::Capture(CaptureForm {
            slug,
            title: Input::new(&title),
            link: Input::default(),
            source: 0,
            body,
            focus: 0,
        }));
    }

    fn capture_key(
        &mut self,
        mut form: CaptureForm,
        key: KeyEvent,
    ) -> (Option<Modal>, Vec<Command>) {
        let sources = store::SOURCES.len();
        match key.code {
            KeyCode::Esc => return (None, Vec::new()),
            KeyCode::Enter => {
                return (
                    None,
                    vec![Command::Capture {
                        slug: form.slug,
                        title: form.title.text,
                        source: store::SOURCES[form.source].to_string(),
                        link: form.link.text,
                        body: form.body,
                    }],
                );
            }
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + 2) % 3,
            KeyCode::Right | KeyCode::Char(' ') if form.focus == 1 => {
                form.source = (form.source + 1) % sources
            }
            KeyCode::Left if form.focus == 1 => form.source = (form.source + sources - 1) % sources,
            KeyCode::Char(c) if form.focus == 1 => {
                if let Some(i) = store::SOURCES.iter().position(|s| s.starts_with(c)) {
                    form.source = i;
                }
            }
            _ => {
                match form.focus {
                    0 => form.title.handle(key),
                    2 => form.link.handle(key),
                    _ => false,
                };
            }
        }
        (Some(Modal::Capture(form)), Vec::new())
    }

    pub fn animating(&self) -> bool {
        !self.fetching.is_empty() || !self.responding.is_empty() || self.discovering
    }

    pub fn last_refresh(&self) -> Option<DateTime<Utc>> {
        self.cache.updated
    }
}

fn new_project_key(mut form: NewProjectForm, key: KeyEvent) -> (Option<Modal>, Vec<Command>) {
    match key.code {
        KeyCode::Esc => return (None, Vec::new()),
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
            form.focus = 1 - form.focus;
        }
        KeyCode::Enter => {
            let name = form.name.text.trim().to_string();
            let dir = forge::config::expand_home(form.dir.text.trim());
            if name.is_empty() {
                form.focus = 0;
                form.error = Some("the project needs a name".into());
            } else if !dir.is_dir() {
                form.focus = 1;
                form.error = Some(format!("{} is not a directory", form.dir.text.trim()));
            } else {
                let item = form.item.take();
                return (None, vec![Command::Create { name, dir, item }]);
            }
        }
        _ => {
            let field = if form.focus == 0 {
                &mut form.name
            } else {
                &mut form.dir
            };
            if field.handle(key) {
                form.error = None;
            }
        }
    }
    (Some(Modal::NewProject(form)), Vec::new())
}

/// The path with the home directory written as `~`.
pub fn tilde(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(PathBuf::from)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wait::{Cleared, Wait};

    fn project(name: &str, state: State) -> Project {
        Project {
            name: name.into(),
            slug: store::slugify(name),
            state,
            created: Utc::now(),
            session: store::slugify(name),
            dir: None,
            next: String::new(),
            handoff: None,
            wait: None,
            claude_sessions: Vec::new(),
            dismissed: Vec::new(),
            layout: Vec::new(),
            items: Vec::new(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn waiting(mut p: Project, cleared: bool) -> Project {
        let mut wait = Wait::new(Condition::Claude { pane: None });
        if cleared {
            wait.cleared = Some(Cleared {
                at: Utc::now(),
                reason: "Claude finished".into(),
            });
        }
        p.wait = Some(wait);
        p
    }

    #[test]
    fn home_puts_cleared_live_projects_first_and_waiting_ones_last() {
        let mut app = App::new(None, false);
        app.projects = vec![
            waiting(project("live waiting", State::Live), false),
            project("live plain", State::Live),
            waiting(project("live cleared", State::Live), true),
            waiting(project("frozen waiting", State::Frozen), false),
            waiting(project("frozen cleared", State::Frozen), true),
            project("frozen plain", State::Frozen),
            project("finished", State::Done),
        ];
        let groups: Vec<(Group, Vec<String>)> = app
            .groups()
            .into_iter()
            .map(|(g, ps)| (g, ps.into_iter().map(|p| p.name.clone()).collect()))
            .collect();
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            groups,
            [
                (
                    Group::Live,
                    names(&["live cleared", "live plain", "live waiting"])
                ),
                (Group::Ready, names(&["frozen cleared", "frozen plain"])),
                (Group::Waiting, names(&["frozen waiting"])),
            ]
        );
        app.show_done = true;
        assert_eq!(app.groups().last().map(|(g, _)| *g), Some(Group::Done));
    }

    #[test]
    fn freezing_prefills_the_last_handoff_and_turns_waiting_text_into_a_manual_wait() {
        let mut p = project("wire", State::Live);
        p.next = "rebase".into();
        p.handoff = Some(Handoff {
            time: Utc::now(),
            doing: "bindings".into(),
            next: "old".into(),
            waiting: String::new(),
        });
        let mut app = App::new(None, false);
        app.projects = vec![p];
        app.handle_key(key(KeyCode::Char('f')));
        let Some(Modal::Freeze(form)) = &app.modal else {
            panic!("f opens the freeze form");
        };
        assert_eq!(form.doing.text, "bindings");
        assert_eq!(form.next.text, "rebase");
        assert!(!form.close);

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        typed(&mut app, "jdoe");
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char(' ')));
        let commands = app.handle_key(key(KeyCode::Enter));
        let [
            Command::Freeze {
                slug,
                handoff,
                condition,
                close,
            },
        ] = commands.as_slice()
        else {
            panic!("Enter freezes: {commands:?}");
        };
        assert_eq!(slug, "wire");
        assert_eq!(handoff.waiting, "jdoe");
        assert_eq!(
            condition,
            &Some(Condition::Manual {
                text: "jdoe".into()
            })
        );
        assert!(*close, "space toggled close session");
        assert!(app.modal.is_none());
    }

    #[test]
    fn esc_cancels_a_freeze() {
        let mut app = App::new(None, false);
        app.projects = vec![project("wire", State::Live)];
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.modal.is_none());
    }

    #[test]
    fn the_wait_picker_fills_the_freeze_form_and_returns_to_it() {
        let mut app = App::new(None, false);
        app.projects = vec![project("wire", State::Live)];
        app.handle_key(key(KeyCode::Char('f')));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(
            matches!(app.modal, Some(Modal::Wait(_))),
            "space on Condition opens the picker"
        );
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        let Some(Modal::Freeze(form)) = &app.modal else {
            panic!("the picker returns to the freeze form: {:?}", app.modal);
        };
        assert_eq!(form.condition, Some(Condition::Claude { pane: None }));
    }

    #[test]
    fn w_sets_a_wait_on_a_live_project_without_freezing() {
        let mut app = App::new(None, true);
        app.pane = Some("%7".into());
        app.projects = vec![project("wire", State::Live)];
        app.handle_key(key(KeyCode::Char('w')));
        let commands = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            commands,
            [Command::SetWait(
                "wire".into(),
                Some(Condition::Claude {
                    pane: Some("%7".into())
                })
            )]
        );
    }

    #[test]
    fn a_date_wait_asks_for_text_and_rejects_nonsense() {
        let mut app = App::new(None, false);
        app.projects = vec![project("wire", State::Live)];
        app.handle_key(key(KeyCode::Char('w')));
        let Some(Modal::Wait(picker)) = &app.modal else {
            panic!("w opens the picker");
        };
        let date = picker
            .options
            .iter()
            .position(|(_, c)| *c == WaitChoice::Ask(AskKind::Until))
            .unwrap();
        for _ in 0..date {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Enter));
        typed(&mut app, "soon");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        let Some(Modal::Wait(picker)) = &app.modal else {
            panic!("nonsense keeps the picker open");
        };
        assert!(picker.error.is_some());
        app.handle_key(ctrl('u'));
        typed(&mut app, "2h");
        let commands = app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(
                commands.as_slice(),
                [Command::SetWait(_, Some(Condition::Until { .. }))]
            ),
            "{commands:?}"
        );
    }

    fn reading_app() -> App {
        let mut app = App::new(None, true);
        let mut p = project("wire", State::Live);
        for name in ["a.md", "b.md"] {
            p.items.push(store::Entry {
                added: Utc::now(),
                item: ItemRef::File {
                    path: PathBuf::from(name),
                },
            });
        }
        app.projects = vec![p];
        app.screen = Screen::Project("wire".into());
        app.detail_size.set((100, 20));
        app
    }

    #[test]
    fn the_detail_pane_scrolls_within_its_rows_and_starts_over_on_another_item() {
        let mut app = reading_app();
        app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(app.detail_scroll, 3);
        app.handle_key(key(KeyCode::Char('K')));
        app.handle_key(key(KeyCode::Char('K')));
        assert_eq!(app.detail_scroll, 0);
        for _ in 0..40 {
            app.handle_key(key(KeyCode::Char('J')));
        }
        assert_eq!(app.detail_scroll, 80, "stops at the last screenful");
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.project_selected, 1);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn v_reads_the_selection_full_screen_until_esc() {
        let mut app = reading_app();
        app.handle_key(key(KeyCode::Char('v')));
        assert!(app.reading);
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.project_selected, 0, "j scrolls instead of moving");
        assert_eq!(app.detail_scroll, 1 + 18);
        app.handle_key(key(KeyCode::Char('G')));
        assert_eq!(app.detail_scroll, 80);
        app.handle_key(key(KeyCode::Char('g')));
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.detail_scroll, 0);
        app.handle_key(key(KeyCode::Char('u')));
        assert!(app.show_resolved);
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.reading);
        assert_eq!(app.screen, Screen::Project("wire".into()));
    }

    fn responding_app() -> (App, ItemRef) {
        let mut app = App::new(None, true);
        let mut p = project("wire", State::Live);
        let pr = ItemRef::AdoPr { id: 7 };
        for item in [
            ItemRef::File {
                path: PathBuf::from("a.md"),
            },
            pr.clone(),
        ] {
            p.items.push(store::Entry {
                added: Utc::now(),
                item,
            });
        }
        app.projects = vec![p];
        app.screen = Screen::Project("wire".into());
        (app, pr)
    }

    #[test]
    fn w_on_an_item_selects_the_first_wait_for_that_item() {
        let (mut app, pr) = responding_app();
        let wi = ItemRef::WorkItem { id: 9 };
        app.projects[0].items.push(store::Entry {
            added: Utc::now(),
            item: wi.clone(),
        });
        let chosen = |app: &App| match &app.modal {
            Some(Modal::Wait(p)) => p.options[p.selected].1.clone(),
            other => panic!("w opens the picker: {other:?}"),
        };
        let at = |app: &App, item: &ItemRef| {
            app.picks("wire")
                .iter()
                .position(|p| *p == Pick::Item(item.clone()))
                .unwrap()
        };

        app.project_selected = at(&app, &pr);
        app.handle_key(key(KeyCode::Char('w')));
        assert!(
            matches!(chosen(&app), WaitChoice::Set(Condition::Review { item, .. }) if item == pr.key()),
            "the PR's first wait is its review"
        );

        app.modal = None;
        app.project_selected = at(&app, &wi);
        app.handle_key(key(KeyCode::Char('w')));
        assert!(
            matches!(chosen(&app), WaitChoice::Set(Condition::Comment { item, .. }) if item == wi.key()),
            "the work item's first wait is a new comment, not the PR's"
        );

        app.modal = None;
        app.project_selected = at(
            &app,
            &ItemRef::File {
                path: PathBuf::from("a.md"),
            },
        );
        app.handle_key(key(KeyCode::Char('w')));
        assert_eq!(
            chosen(&app),
            WaitChoice::Set(Condition::Claude { pane: None })
        );
    }

    fn evaluation(item: &ItemRef) -> Evaluation {
        let response = |id: &str, plan: &str| respond::Response {
            thread_id: id.into(),
            location: None,
            thread: Vec::new(),
            assessment: respond::Assessment::Agree,
            summary: "s".into(),
            plan: plan.into(),
            reply: "r".into(),
            selected: false,
            posted: false,
            handed: false,
            resolve: false,
            resolved: false,
        };
        Evaluation {
            item: item.clone(),
            created: Utc::now(),
            dir: PathBuf::from("/src"),
            local_head: "abc".into(),
            head: None,
            responses: vec![response("41", "- fix"), response("42", "")],
        }
    }

    #[test]
    fn r_asks_claude_about_a_pr_once_and_opens_what_it_wrote() {
        let _home = store::testing::TempHome::new("respond-key");
        let (mut app, pr) = responding_app();
        assert!(
            app.handle_key(key(KeyCode::Char('r'))).is_empty(),
            "a file has no threads"
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            [Command::Evaluate("wire".into(), pr.clone())]
        );
        app.start_job(&pr, "loading the threads");
        assert!(app.handle_key(key(KeyCode::Char('r'))).is_empty());
        assert!(app.flash.as_ref().unwrap().text.contains("still working"));
        app.responding.clear();

        respond::save("wire", &evaluation(&pr)).unwrap();
        assert!(app.handle_key(key(KeyCode::Char('r'))).is_empty());
        let view = app.responses.as_ref().expect("the responses open");
        assert_eq!(view.evaluation.responses.len(), 2);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.responses, None);
        assert_eq!(app.screen, Screen::Project("wire".into()));
    }

    #[test]
    fn reload_counts_the_unposted_drafts_of_each_item_across_projects() {
        let _home = store::testing::TempHome::new("respond-drafts");
        let (mut app, pr) = responding_app();
        let mut other = app.projects[0].clone();
        other.slug = "other".into();
        store::save(&app.projects[0]).unwrap();
        store::save(&other).unwrap();
        let mut e = evaluation(&pr);
        respond::save("wire", &e).unwrap();
        e.responses[0].posted = true;
        respond::save("other", &e).unwrap();
        app.reload();
        assert_eq!(app.drafts.get(&pr.key()), Some(&3));
        e.responses[1].posted = true;
        respond::save("other", &e).unwrap();
        respond::save("wire", &e).unwrap();
        app.reload();
        assert_eq!(app.drafts.get(&pr.key()), Some(&0));
    }

    #[test]
    fn checked_replies_post_only_after_y_and_plans_go_to_claude() {
        let _home = store::testing::TempHome::new("respond-post");
        let (mut app, pr) = responding_app();
        respond::save("wire", &evaluation(&pr)).unwrap();
        app.project_selected = 1;
        app.handle_key(key(KeyCode::Char('r')));

        assert!(app.handle_key(key(KeyCode::Char('p'))).is_empty());
        assert!(app.responses.as_ref().unwrap().confirm);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            [Command::PostReplies("wire".into(), pr.clone(), vec![0])],
            "with nothing checked, p posts the reply at the cursor"
        );
        let toggled = app.handle_key(key(KeyCode::Char('r')));
        assert!(
            matches!(&toggled[..], [Command::SaveResponses(_, e)] if e.responses[0].resolve),
            "r marks the thread to be resolved: {toggled:?}"
        );
        app.responses.as_mut().unwrap().evaluation.responses[0].resolved = true;
        app.handle_key(key(KeyCode::Char('r')));
        assert!(app.responses.as_ref().unwrap().evaluation.responses[0].resolve);
        assert!(
            app.flash
                .as_ref()
                .unwrap()
                .text
                .contains("already resolved")
        );
        app.responses.as_mut().unwrap().evaluation.responses[0].resolved = false;
        app.handle_key(key(KeyCode::Char('r')));
        assert!(
            !app.responses.as_ref().unwrap().evaluation.responses[0].resolve,
            "r again leaves the thread open"
        );

        let saved = app.handle_key(key(KeyCode::Char(' ')));
        assert!(
            matches!(&saved[..], [Command::SaveResponses(_, e)] if e.responses[0].selected),
            "{saved:?}"
        );
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.handle_key(key(KeyCode::Char('p'))).is_empty());
        assert!(
            app.handle_key(key(KeyCode::Char('n'))).is_empty(),
            "n cancels"
        );
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            [Command::PostReplies("wire".into(), pr.clone(), vec![0, 1])]
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('C'))),
            [Command::HandFixes("wire".into(), pr.clone(), vec![0])],
            "only the response with a plan"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('e'))),
            [Command::EditReply("wire".into(), pr.clone(), 1)]
        );
        let cleared = app.handle_key(key(KeyCode::Char('a')));
        assert!(
            matches!(&cleared[..], [Command::SaveResponses(_, e)] if e.responses.iter().all(|r| !r.selected)),
            "a clears when all are checked: {cleared:?}"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('R'))),
            [Command::Evaluate("wire".into(), pr)]
        );
        assert_eq!(app.responses, None, "R closes the view while Claude works");
    }

    #[test]
    fn capital_c_starts_claude_from_home_and_from_the_project_screen() {
        let mut app = App::new(None, true);
        app.projects = vec![project("wire", State::Live)];
        assert_eq!(
            app.handle_key(key(KeyCode::Char('C'))),
            [Command::NewClaude("wire".into())]
        );
        app.screen = Screen::Project("wire".into());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('C'))),
            [Command::NewClaude("wire".into())]
        );
    }

    #[test]
    fn a_new_project_starts_in_the_pane_directory_unless_changed() {
        let start = std::env::temp_dir().canonicalize().unwrap();
        let mut app = App::new(None, true);
        app.start_dir = start.clone();
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        let Some(Modal::NewProject(form)) = &app.modal else {
            panic!("an empty name keeps the dialog open");
        };
        assert_eq!(form.dir.text, tilde(&start));
        assert!(form.error.is_some());

        typed(&mut app, "wire");
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(ctrl('u'));
        typed(&mut app, "/no/such/ctx/dir");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        let Some(Modal::NewProject(form)) = &app.modal else {
            panic!("a missing directory keeps the dialog open");
        };
        assert_eq!(form.name.text, "wire");
        assert_eq!(form.focus, 1);
        assert!(form.error.as_deref().unwrap().contains("/no/such/ctx/dir"));

        app.handle_key(ctrl('u'));
        typed(&mut app, &start.to_string_lossy());
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            [Command::Create {
                name: "wire".into(),
                dir: start,
                item: None,
            }]
        );
        assert!(app.modal.is_none());
    }

    fn unfiled_app() -> App {
        let mut app = App::new(None, true);
        let mut held = project("wire", State::Live);
        held.items.push(store::Entry {
            added: Utc::now(),
            item: ItemRef::WorkItem { id: 1 },
        });
        app.projects = vec![
            held,
            project("tcu zip", State::Frozen),
            project("finished", State::Done),
        ];
        app.inbox = Inbox {
            updated: None,
            items: vec![ItemRef::WorkItem { id: 1 }, ItemRef::WorkItem { id: 2 }],
            errors: Vec::new(),
        };
        app.show_done = true;
        app
    }

    #[test]
    fn unfiled_items_sit_between_the_projects_and_done() {
        let app = unfiled_app();
        let sections: Vec<(Group, Vec<HomeRow>)> = app.home_sections();
        let groups: Vec<Group> = sections.iter().map(|(g, _)| *g).collect();
        assert_eq!(
            groups,
            [Group::Live, Group::Ready, Group::Unfiled, Group::Done]
        );
        assert_eq!(
            sections[2].1,
            [HomeRow::Item(ItemRef::WorkItem { id: 2 })],
            "#1 is in wire"
        );
    }

    #[test]
    fn a_files_an_unfiled_item_into_the_project_typed() {
        let mut app = unfiled_app();
        app.home_selected = 2;
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.modal.is_none(), "project keys do nothing on an item");
        app.handle_key(key(KeyCode::Char('a')));
        let Some(Modal::Assign(picker)) = &app.modal else {
            panic!("a opens the project picker: {:?}", app.modal);
        };
        assert_eq!(
            app.assign_choices(picker)
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["wire", "tcu zip"],
            "done projects are left out"
        );
        typed(&mut app, "wi");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            [Command::AddRef("wire".into(), ItemRef::WorkItem { id: 2 })]
        );
        assert!(app.modal.is_none());
    }

    #[test]
    fn n_on_an_unfiled_item_starts_a_project_named_after_it() {
        let start = std::env::temp_dir().canonicalize().unwrap();
        let mut app = unfiled_app();
        app.start_dir = start.clone();
        let state: ItemState = serde_json::from_value(serde_json::json!({
            "title": "Fix the parser", "status": "New", "open": true, "fetched": Utc::now()
        }))
        .unwrap();
        app.cache.items.insert("wi-2".into(), state);
        app.home_selected = 2;
        app.handle_key(key(KeyCode::Char('n')));
        let Some(Modal::NewProject(form)) = &app.modal else {
            panic!("n opens the new project dialog");
        };
        assert_eq!(form.name.text, "Fix the parser");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            [Command::Create {
                name: "Fix the parser".into(),
                dir: start,
                item: Some(ItemRef::WorkItem { id: 2 }),
            }]
        );
    }

    #[test]
    fn input_edits_at_the_cursor() {
        let mut input = Input::new("hello world");
        input.handle(ctrl('w'));
        assert_eq!(input.text, "hello ");
        input.handle(key(KeyCode::Home));
        input.handle(key(KeyCode::Char('>')));
        assert_eq!(input.text, ">hello ");
        input.handle(key(KeyCode::End));
        input.handle(key(KeyCode::Backspace));
        assert_eq!(input.text, ">hello");
    }
}
