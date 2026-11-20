use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{
    Category, Details, Diagnosis, Discussion, Item, Review, Severity, Source, Status,
};

pub const SOURCES: [Source; 2] = [Source::Ado, Source::GitLab];
/// How many items have their comment threads fetched at once.
const DISCUSSION_PARALLEL: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Board,
    Detail,
}

/// What the detail pane shows for the open item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Details,
    Findings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Queued,
    Running(String),
    Failed(String),
}

/// The state of an approval vote, kept until the next key press once it has finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Spacer,
    /// Heads items bumped to the top of the board because they need you to act.
    Category {
        category: Category,
        count: usize,
        folded: bool,
    },
    /// Heads the threads where someone answered you and you have not replied.
    Needs {
        count: usize,
        folded: bool,
    },
    Waiting {
        item: usize,
        discussion: String,
    },
    Section(Source),
    Group {
        source: Source,
        name: String,
        count: usize,
        folded: bool,
    },
    Item(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Selection {
    Category(Category),
    Needs,
    Waiting(String, String),
    Group(Source, String),
    Item(String),
}

/// A foldable heading on the board.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fold {
    Category(Category),
    Needs,
    Group(Source, String),
}

impl Fold {
    fn selection(self) -> Selection {
        match self {
            Fold::Category(c) => Selection::Category(c),
            Fold::Needs => Selection::Needs,
            Fold::Group(source, name) => Selection::Group(source, name),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DetailState {
    Loading,
    Loaded(Details),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Refresh,
    LoadDetails(Box<Item>),
    LoadDiscussions(Box<Item>),
    OpenUrl(String),
    RunReview(Box<Item>),
    SaveReview(Box<Review>),
    Diagnose(Box<Item>),
    /// Posts the findings at `picks` from `review` to the item's PR.
    PostFindings {
        item: Box<Item>,
        review: Box<Review>,
        picks: Vec<usize>,
    },
    Approve(Box<Item>),
}

pub struct App {
    pub items: Vec<Item>,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub mode: Mode,
    pub filter: String,
    pub mine_only: bool,
    pub review_only: bool,
    pub unresolved_only: bool,
    pub me: HashMap<Source, String>,
    pub focus: Pane,
    pub open: Option<String>,
    pub details: HashMap<String, DetailState>,
    pub detail_scroll: u16,
    pub detail_cache: crate::ui::DetailCache,
    pub view: View,
    pub reviews: HashMap<String, Review>,
    pub jobs: HashMap<String, Job>,
    pub diagnoses: HashMap<String, Diagnosis>,
    pub diagnosing: HashMap<String, Job>,
    pub posting: HashMap<String, Job>,
    pub discussions: HashMap<String, Vec<Discussion>>,
    discussion_queue: VecDeque<Item>,
    discussions_loading: HashSet<String>,
    needs_folded: bool,
    folded_categories: HashSet<Category>,
    /// The item whose checked findings wait for `y` before they are posted.
    pub confirm_post: Option<String>,
    /// The item that waits for `y` before it is approved.
    pub confirm_approve: Option<String>,
    pub approval: Option<(String, Approval)>,
    pub finding_cursor: usize,
    pub max_parallel: usize,
    /// The forges the config names. Refreshes and the header cover only these.
    pub sources: Vec<Source>,
    pub loading: HashSet<Source>,
    pub errors: HashMap<Source, String>,
    pub refreshed: Option<DateTime<Utc>>,
    /// How often the board refreshes on its own. `None` turns the timed refresh off.
    pub refresh_interval: Option<Duration>,
    refresh_started: Option<Instant>,
    pub show_help: bool,
    pub quit: bool,
    pub page: usize,
    pub board_offset: usize,
    folded: HashSet<(Source, String)>,
    pending: Option<char>,
    queue: VecDeque<Item>,
}

impl Default for App {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl App {
    pub fn new(items: Vec<Item>) -> Self {
        let mut app = App {
            items,
            rows: Vec::new(),
            cursor: 0,
            mode: Mode::Normal,
            filter: String::new(),
            mine_only: false,
            review_only: false,
            unresolved_only: true,
            me: HashMap::new(),
            focus: Pane::Board,
            open: None,
            details: HashMap::new(),
            detail_scroll: 0,
            detail_cache: crate::ui::DetailCache::default(),
            view: View::Details,
            reviews: HashMap::new(),
            jobs: HashMap::new(),
            diagnoses: HashMap::new(),
            diagnosing: HashMap::new(),
            posting: HashMap::new(),
            discussions: HashMap::new(),
            discussion_queue: VecDeque::new(),
            discussions_loading: HashSet::new(),
            needs_folded: false,
            folded_categories: HashSet::new(),
            confirm_post: None,
            confirm_approve: None,
            approval: None,
            finding_cursor: 0,
            max_parallel: 2,
            sources: SOURCES.to_vec(),
            loading: HashSet::new(),
            errors: HashMap::new(),
            refreshed: None,
            refresh_interval: None,
            refresh_started: None,
            show_help: false,
            quit: false,
            page: 10,
            board_offset: 0,
            folded: HashSet::new(),
            pending: None,
            queue: VecDeque::new(),
        };
        app.rebuild();
        app
    }

    pub fn set_items(&mut self, source: Source, items: Vec<Item>) {
        let at_top = self.first_selectable() == Some(self.cursor);
        let selection = if at_top {
            (None, None)
        } else {
            self.selection_with_group()
        };
        self.items.retain(|i| i.source != source);
        self.items.extend(items);
        self.errors.remove(&source);
        self.loading.remove(&source);
        if self.loading.is_empty() {
            self.refreshed = Some(Utc::now());
        }
        self.rebuild_keeping(selection);
        if at_top {
            self.move_to_end(false);
        }
    }

    /// Items whose threads are worth watching: ones you wrote or review, and ones you posted
    /// findings to.
    fn watches(&self, item: &Item) -> bool {
        self.is_mine(item)
            || self
                .reviews
                .get(&item.key())
                .is_some_and(Review::any_posted)
    }

    /// Queues a thread fetch for every watched item not already queued or loading.
    pub fn load_discussions(&mut self) -> Vec<Command> {
        let queued: HashSet<String> = self.discussion_queue.iter().map(Item::key).collect();
        let wanted: Vec<Item> = self
            .items
            .iter()
            .filter(|i| self.watches(i))
            .filter(|i| {
                let key = i.key();
                !queued.contains(&key) && !self.discussions_loading.contains(&key)
            })
            .cloned()
            .collect();
        self.discussion_queue.extend(wanted);
        self.dispatch_discussions()
    }

    fn dispatch_discussions(&mut self) -> Vec<Command> {
        let mut cmds = Vec::new();
        while self.discussions_loading.len() < DISCUSSION_PARALLEL {
            let Some(item) = self.discussion_queue.pop_front() else {
                break;
            };
            self.discussions_loading.insert(item.key());
            cmds.push(Command::LoadDiscussions(Box::new(item)));
        }
        cmds
    }

    /// Stores fetched threads. A failed fetch keeps whatever was there before.
    pub fn discussions_loaded(
        &mut self,
        key: String,
        result: Result<Vec<Discussion>, String>,
    ) -> Vec<Command> {
        self.discussions_loading.remove(&key);
        if let Ok(discussions) = result {
            self.discussions.insert(key, discussions);
            self.rebuild();
        }
        self.dispatch_discussions()
    }

    /// Threads on `item` that wait on you, newest answer first.
    pub fn waiting(&self, item: &Item) -> Vec<&Discussion> {
        let Some(me) = self.me.get(&item.source) else {
            return Vec::new();
        };
        let mut waiting: Vec<&Discussion> = self
            .discussions
            .get(&item.key())
            .into_iter()
            .flatten()
            .filter(|d| d.waits_on(me))
            .collect();
        waiting.sort_by_key(|d| std::cmp::Reverse(d.last().map(|n| n.date)));
        waiting
    }

    /// The category `item` is bumped into, once its threads have loaded.
    pub fn category_of(&self, item: &Item) -> Option<Category> {
        let me = self.me.get(&item.source)?;
        let discussions = self.discussions.get(&item.key())?;
        item.category(me, discussions)
    }

    fn latest_note(&self, item: &Item) -> Option<DateTime<Utc>> {
        self.discussions
            .get(&item.key())?
            .iter()
            .filter_map(|d| d.last().map(|n| n.date))
            .max()
    }

    pub fn discussion(&self, item: usize, id: &str) -> Option<&Discussion> {
        let item = self.items.get(item)?;
        self.discussions
            .get(&item.key())?
            .iter()
            .find(|d| d.id == id)
    }

    pub fn set_error(&mut self, source: Source, error: String) {
        self.errors.insert(source, error);
        self.loading.remove(&source);
        self.rebuild();
    }

    pub fn set_details(&mut self, key: String, state: DetailState) {
        self.details.insert(key, state);
    }

    /// Queues a review of `item` and starts it if a slot is free. A review already queued or
    /// running is left alone.
    pub fn start_review(&mut self, item: Item) -> Vec<Command> {
        let key = item.key();
        if matches!(self.jobs.get(&key), Some(Job::Queued | Job::Running(_))) {
            return Vec::new();
        }
        self.jobs.insert(key, Job::Queued);
        self.queue.push_back(item);
        self.dispatch()
    }

    fn dispatch(&mut self) -> Vec<Command> {
        let mut cmds = Vec::new();
        let running = |jobs: &HashMap<String, Job>| {
            jobs.values()
                .filter(|j| matches!(j, Job::Running(_)))
                .count()
        };
        while running(&self.jobs) < self.max_parallel.max(1) {
            let Some(item) = self.queue.pop_front() else {
                break;
            };
            self.jobs
                .insert(item.key(), Job::Running("starting".into()));
            cmds.push(Command::RunReview(Box::new(item)));
        }
        cmds
    }

    pub fn review_progress(&mut self, key: &str, stage: &str) {
        if let Some(Job::Running(current)) = self.jobs.get_mut(key) {
            *current = stage.to_string();
        }
    }

    pub fn review_finished(&mut self, key: String, result: Result<Review, String>) -> Vec<Command> {
        match result {
            Ok(review) => {
                self.jobs.remove(&key);
                if self.open.as_ref() == Some(&key) {
                    self.finding_cursor = 0;
                }
                self.reviews.insert(key, review);
            }
            Err(e) => {
                self.jobs.insert(key, Job::Failed(e));
            }
        }
        self.dispatch()
    }

    /// Starts a CI diagnosis of `item` unless one is already running.
    pub fn start_diagnosis(&mut self, item: Item) -> Vec<Command> {
        let key = item.key();
        if matches!(self.diagnosing.get(&key), Some(Job::Running(_))) {
            return Vec::new();
        }
        self.diagnosing.insert(key, Job::Running("starting".into()));
        vec![Command::Diagnose(Box::new(item))]
    }

    pub fn diagnosis_progress(&mut self, key: &str, stage: &str) {
        if let Some(Job::Running(current)) = self.diagnosing.get_mut(key) {
            *current = stage.to_string();
        }
    }

    pub fn diagnosis_finished(&mut self, key: String, result: Result<Diagnosis, String>) {
        match result {
            Ok(diagnosis) => {
                self.diagnosing.remove(&key);
                self.diagnoses.insert(key, diagnosis);
            }
            Err(e) => {
                self.diagnosing.insert(key, Job::Failed(e));
            }
        }
    }

    /// Indices of the findings that are checked and not yet posted.
    pub fn post_picks(&self, key: &str) -> Vec<usize> {
        self.reviews.get(key).map_or_else(Vec::new, |review| {
            review
                .findings
                .iter()
                .enumerate()
                .filter(|(_, f)| f.selected && !f.posted)
                .map(|(i, _)| i)
                .collect()
        })
    }

    /// Asks for confirmation before posting the open item's checked findings. A stale review is
    /// refused, because its lines may no longer match the change.
    fn request_post(&mut self) {
        let Some(item) = self.open_item().cloned() else {
            return;
        };
        let key = item.key();
        if matches!(self.posting.get(&key), Some(Job::Running(_))) {
            return;
        }
        let Some(review) = self.reviews.get(&key) else {
            return;
        };
        if review.is_stale(&item) {
            self.posting.insert(
                key,
                Job::Failed("The review is stale, so nothing was posted. Press r to review the change again.".into()),
            );
            return;
        }
        if !self.post_picks(&key).is_empty() {
            self.confirm_post = Some(key);
        }
    }

    fn start_post(&mut self, key: String) -> Vec<Command> {
        let item = self.items.iter().find(|i| i.key() == key).cloned();
        let review = self.reviews.get(&key).cloned();
        let picks = self.post_picks(&key);
        let (Some(item), Some(review)) = (item, review) else {
            return Vec::new();
        };
        if picks.is_empty() {
            return Vec::new();
        }
        self.posting.insert(key, Job::Running("starting".into()));
        vec![Command::PostFindings {
            item: Box::new(item),
            review: Box::new(review),
            picks,
        }]
    }

    pub fn post_progress(&mut self, key: &str, stage: &str) {
        if let Some(Job::Running(current)) = self.posting.get_mut(key) {
            *current = stage.to_string();
        }
    }

    /// Marks the findings that were posted. Results for a review that has since been replaced
    /// are dropped, since their indices point into the old findings.
    pub fn post_finished(
        &mut self,
        key: String,
        created: DateTime<Utc>,
        outcome: Vec<(usize, Result<(), String>)>,
    ) -> Vec<Command> {
        let errors: Vec<String> = outcome
            .iter()
            .filter_map(|(_, r)| r.as_ref().err().cloned())
            .collect();
        if errors.is_empty() {
            self.posting.remove(&key);
        } else {
            let failed = if errors.len() == outcome.len() {
                "Nothing was posted".to_string()
            } else {
                format!(
                    "{} of {} findings were not posted",
                    errors.len(),
                    outcome.len()
                )
            };
            self.posting
                .insert(key.clone(), Job::Failed(format!("{failed}: {}", errors[0])));
        }
        let Some(review) = self.reviews.get_mut(&key).filter(|r| r.created == created) else {
            return Vec::new();
        };
        for (index, result) in &outcome {
            if let (Ok(()), Some(finding)) = (result, review.findings.get_mut(*index)) {
                finding.posted = true;
            }
        }
        vec![Command::SaveReview(Box::new(review.clone()))]
    }

    /// Asks for confirmation before approving the open item, or the selected one on the board.
    fn request_approve(&mut self) {
        if matches!(self.approval, Some((_, Approval::Running))) {
            return;
        }
        let item = match self.focus {
            Pane::Detail => self.open_item(),
            Pane::Board => self.selected_item(),
        };
        self.confirm_approve = item.map(Item::key);
    }

    fn start_approve(&mut self, key: String) -> Vec<Command> {
        let Some(item) = self.items.iter().find(|i| i.key() == key).cloned() else {
            return Vec::new();
        };
        self.approval = Some((key, Approval::Running));
        vec![Command::Approve(Box::new(item))]
    }

    /// Records the vote and refreshes the board, so the item shows its new status.
    pub fn approve_finished(&mut self, key: String, result: Result<(), String>) -> Vec<Command> {
        match result {
            Ok(()) => {
                self.approval = Some((key, Approval::Done));
                self.refresh()
            }
            Err(e) => {
                self.approval = Some((key, Approval::Failed(e)));
                Vec::new()
            }
        }
    }

    pub fn selected_item(&self) -> Option<&Item> {
        match self.rows.get(self.cursor) {
            Some(Row::Item(i) | Row::Waiting { item: i, .. }) => self.items.get(*i),
            _ => None,
        }
    }

    pub fn open_item(&self) -> Option<&Item> {
        let key = self.open.as_ref()?;
        self.items.iter().find(|i| &i.key() == key)
    }

    /// Whether a spinner is on screen: a source is loading, or a review or post runs.
    pub fn animating(&self) -> bool {
        let running =
            |jobs: &HashMap<String, Job>| jobs.values().any(|j| matches!(j, Job::Running(_)));
        !self.loading.is_empty()
            || running(&self.jobs)
            || running(&self.posting)
            || matches!(self.approval, Some((_, Approval::Running)))
    }

    pub fn count(&self, source: Source) -> usize {
        self.items.iter().filter(|i| i.source == source).count()
    }

    pub fn mine_count(&self) -> usize {
        self.items.iter().filter(|i| self.is_mine(i)).count()
    }

    pub fn review_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status.awaits_review())
            .count()
    }

    fn is_mine(&self, item: &Item) -> bool {
        self.me
            .get(&item.source)
            .is_some_and(|me| item.involves(me))
    }

    fn matches(&self, item: &Item) -> bool {
        if self.mine_only && !self.is_mine(item) {
            return false;
        }
        if self.review_only && !item.status.awaits_review() {
            return false;
        }
        if self.filter.is_empty() {
            return true;
        }
        let needle = self.filter.to_lowercase();
        [
            &item.title,
            &item.author,
            &item.author_name,
            &item.repo,
            &item.group,
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(&needle))
            || id_query(&needle).is_some_and(|digits| item.id.to_string().contains(digits))
    }

    fn selection(&self) -> Option<Selection> {
        self.row_selection(self.rows.get(self.cursor)?)
    }

    /// The selected row, plus the heading to fall back to if that row disappears.
    fn selection_with_group(&self) -> (Option<Selection>, Option<Selection>) {
        (self.selection(), self.fold_target().map(Fold::selection))
    }

    pub fn rebuild(&mut self) {
        let selection = self.selection_with_group();
        self.rebuild_keeping(selection);
    }

    fn rebuild_keeping(
        &mut self,
        (selection, selected_group): (Option<Selection>, Option<Selection>),
    ) {
        let mut groups: BTreeMap<(usize, String), Vec<usize>> = BTreeMap::new();
        let mut bumped: HashMap<Category, Vec<usize>> = HashMap::new();
        for (idx, item) in self.items.iter().enumerate() {
            if !self.matches(item) {
                continue;
            }
            if let Some(category) = self.category_of(item) {
                bumped.entry(category).or_default().push(idx);
                continue;
            }
            let order = SOURCES.iter().position(|s| *s == item.source).unwrap_or(0);
            groups
                .entry((order, item.group.clone()))
                .or_default()
                .push(idx);
        }

        let mut rows = Vec::new();
        for category in Category::ALL {
            let Some(mut members) = bumped.remove(&category) else {
                continue;
            };
            if !rows.is_empty() {
                rows.extend([Row::Spacer, Row::Spacer]);
            }
            let folded = self.folded_categories.contains(&category);
            rows.push(Row::Category {
                category,
                count: members.len(),
                folded,
            });
            if !folded {
                members.sort_by_key(|i| std::cmp::Reverse(self.latest_note(&self.items[*i])));
                rows.extend(members.into_iter().map(Row::Item));
            }
        }
        let mut waiting: Vec<(usize, &Discussion)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| self.matches(item))
            .flat_map(|(idx, item)| self.waiting(item).into_iter().map(move |d| (idx, d)))
            .collect();
        waiting.sort_by_key(|(_, d)| std::cmp::Reverse(d.last().map(|n| n.date)));
        if !waiting.is_empty() {
            if !rows.is_empty() {
                rows.extend([Row::Spacer, Row::Spacer]);
            }
            rows.push(Row::Needs {
                count: waiting.len(),
                folded: self.needs_folded,
            });
            if !self.needs_folded {
                rows.extend(waiting.iter().map(|(idx, d)| Row::Waiting {
                    item: *idx,
                    discussion: d.id.clone(),
                }));
            }
        }
        for (order, source) in SOURCES.iter().enumerate() {
            let mut source_groups = groups
                .range((order, String::new())..(order + 1, String::new()))
                .peekable();
            let show = source_groups.peek().is_some()
                || self.loading.contains(source)
                || self.errors.contains_key(source);
            if !show {
                continue;
            }
            if !rows.is_empty() {
                rows.extend([Row::Spacer, Row::Spacer]);
            }
            rows.push(Row::Section(*source));
            for (n, ((_, name), members)) in source_groups.enumerate() {
                if n > 0 {
                    rows.push(Row::Spacer);
                }
                let folded = self.folded.contains(&(*source, name.clone()));
                rows.push(Row::Group {
                    source: *source,
                    name: name.clone(),
                    count: members.len(),
                    folded,
                });
                if !folded {
                    let mut members = members.clone();
                    members.sort_by(|a, b| self.items[*b].created.cmp(&self.items[*a].created));
                    rows.extend(members.into_iter().map(Row::Item));
                }
            }
        }
        self.rows = rows;

        let find = |sel: &Selection| {
            self.rows
                .iter()
                .position(|r| self.row_selection(r).as_ref() == Some(sel))
        };
        self.cursor = selection
            .as_ref()
            .and_then(find)
            .or_else(|| selected_group.as_ref().and_then(find))
            .unwrap_or_else(|| self.cursor.min(self.rows.len().saturating_sub(1)));
        if !self.selectable(self.cursor) {
            self.cursor = self.next_selectable(self.cursor, 1).unwrap_or(self.cursor);
        }
    }

    fn row_selection(&self, row: &Row) -> Option<Selection> {
        match row {
            Row::Category { category, .. } => Some(Selection::Category(*category)),
            Row::Needs { .. } => Some(Selection::Needs),
            Row::Waiting { item, discussion } => Some(Selection::Waiting(
                self.items[*item].key(),
                discussion.clone(),
            )),
            Row::Group { source, name, .. } => Some(Selection::Group(*source, name.clone())),
            Row::Item(i) => Some(Selection::Item(self.items[*i].key())),
            Row::Section(_) | Row::Spacer => None,
        }
    }

    fn selectable(&self, idx: usize) -> bool {
        matches!(
            self.rows.get(idx),
            Some(
                Row::Category { .. }
                    | Row::Needs { .. }
                    | Row::Waiting { .. }
                    | Row::Group { .. }
                    | Row::Item(_)
            )
        )
    }

    fn next_selectable(&self, from: usize, step: isize) -> Option<usize> {
        let mut idx = from as isize + step;
        while idx >= 0 && (idx as usize) < self.rows.len() {
            if self.selectable(idx as usize) {
                return Some(idx as usize);
            }
            idx += step;
        }
        None
    }

    fn move_by(&mut self, delta: isize) {
        let step = delta.signum();
        for _ in 0..delta.unsigned_abs() {
            match self.next_selectable(self.cursor, step) {
                Some(next) => self.cursor = next,
                None => break,
            }
        }
    }

    fn first_selectable(&self) -> Option<usize> {
        (0..self.rows.len()).find(|i| self.selectable(*i))
    }

    fn move_to_end(&mut self, end: bool) {
        let found = if end {
            (0..self.rows.len()).rev().find(|i| self.selectable(*i))
        } else {
            self.first_selectable()
        };
        if let Some(i) = found {
            self.cursor = i;
        }
    }

    /// The heading that folds the row under the cursor. An item bumped into a category folds
    /// that category, not the group it was taken from.
    fn fold_target(&self) -> Option<Fold> {
        match self.rows.get(self.cursor)? {
            Row::Category { category, .. } => Some(Fold::Category(*category)),
            Row::Needs { .. } => Some(Fold::Needs),
            Row::Group { source, name, .. } => Some(Fold::Group(*source, name.clone())),
            Row::Item(i) => {
                let item = self.items.get(*i)?;
                Some(match self.category_of(item) {
                    Some(category) => Fold::Category(category),
                    None => Fold::Group(item.source, item.group.clone()),
                })
            }
            Row::Waiting { .. } | Row::Section(_) | Row::Spacer => None,
        }
    }

    /// Folds `target`, opens it, or toggles it when `fold` is `None`.
    fn set_fold(&mut self, target: Fold, fold: Option<bool>) {
        fn apply<T: Eq + std::hash::Hash>(set: &mut HashSet<T>, key: T, fold: Option<bool>) {
            if fold.unwrap_or(!set.contains(&key)) {
                set.insert(key);
            } else {
                set.remove(&key);
            }
        }
        match target {
            Fold::Category(c) => apply(&mut self.folded_categories, c, fold),
            Fold::Needs => self.needs_folded = fold.unwrap_or(!self.needs_folded),
            Fold::Group(source, name) => apply(&mut self.folded, (source, name), fold),
        }
        self.rebuild();
    }

    fn on_heading(&self) -> bool {
        matches!(
            self.rows.get(self.cursor),
            Some(Row::Category { .. } | Row::Needs { .. } | Row::Group { .. })
        )
    }

    fn fold_current(&mut self, fold: Option<bool>) {
        if let Some(target) = self.fold_target() {
            self.set_fold(target, fold);
        }
    }

    fn fold_all(&mut self, fold: bool) {
        self.needs_folded = fold;
        self.folded_categories = if fold {
            Category::ALL.into_iter().collect()
        } else {
            HashSet::new()
        };
        self.folded = if fold {
            self.items
                .iter()
                .map(|i| (i.source, i.group.clone()))
                .collect()
        } else {
            HashSet::new()
        };
        self.rebuild();
    }

    fn scroll_detail(&mut self, delta: i32) {
        self.detail_scroll = (i32::from(self.detail_scroll) + delta).max(0) as u16;
    }

    fn open_selected(&mut self, view: View) -> Vec<Command> {
        let Some(item) = self.selected_item().cloned() else {
            return Vec::new();
        };
        let key = item.key();
        if self.open.as_ref() != Some(&key) {
            self.finding_cursor = 0;
            self.unresolved_only = true;
        }
        self.open = Some(key.clone());
        self.view = view;
        self.detail_scroll = 0;
        match self.details.get(&key) {
            Some(DetailState::Loaded(_) | DetailState::Loading) => Vec::new(),
            _ => {
                self.details.insert(key, DetailState::Loading);
                vec![Command::LoadDetails(Box::new(item))]
            }
        }
    }

    pub fn refresh(&mut self) -> Vec<Command> {
        self.start_refresh(Instant::now(), false)
    }

    /// When the next timed refresh is due. `None` while a refresh runs or when timed refresh is
    /// off, so the caller never waits on a deadline that has already passed.
    pub fn refresh_due_at(&self) -> Option<Instant> {
        if !self.loading.is_empty() {
            return None;
        }
        Some(self.refresh_started? + self.refresh_interval?)
    }

    /// Starts a timed refresh once it is due. The open item's details stay on screen until the
    /// new ones arrive, so the pane does not blank while you read it.
    pub fn refresh_if_due(&mut self, now: Instant) -> Vec<Command> {
        match self.refresh_due_at() {
            Some(due) if due <= now => self.start_refresh(now, true),
            _ => Vec::new(),
        }
    }

    fn start_refresh(&mut self, now: Instant, keep_open: bool) -> Vec<Command> {
        self.refresh_started = Some(now);
        self.loading = self.sources.iter().copied().collect();
        self.rebuild();
        let open = self.open.clone();
        self.details.retain(|key, state| {
            matches!(state, DetailState::Loading)
                || (keep_open
                    && open.as_ref() == Some(key)
                    && matches!(state, DetailState::Loaded(_)))
        });
        let mut cmds = vec![Command::Refresh];
        if let Some(item) = self.open_item().cloned() {
            self.details
                .entry(item.key())
                .or_insert(DetailState::Loading);
            cmds.push(Command::LoadDetails(Box::new(item)));
        }
        cmds
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if self.show_help {
            self.show_help = false;
            return Vec::new();
        }
        match self.mode {
            Mode::Filter => {
                self.handle_filter_key(key);
                Vec::new()
            }
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => return,
        }
        self.rebuild();
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !matches!(self.approval, Some((_, Approval::Running))) {
            self.approval = None;
        }
        if let Some(approve_key) = self.confirm_approve.take() {
            return match key.code {
                KeyCode::Char('y') => self.start_approve(approve_key),
                _ => Vec::new(),
            };
        }
        if let Some(post_key) = self.confirm_post.take() {
            return match key.code {
                KeyCode::Char('y') => self.start_post(post_key),
                _ => Vec::new(),
            };
        }
        if let Some(prefix) = self.pending.take() {
            match (prefix, key.code) {
                ('g', KeyCode::Char('g')) => match self.focus {
                    Pane::Board => self.move_to_end(false),
                    Pane::Detail => {
                        self.finding_cursor = 0;
                        self.detail_scroll = 0;
                    }
                },
                ('z', KeyCode::Char('a')) => self.fold_current(None),
                ('z', KeyCode::Char('c')) => self.fold_current(Some(true)),
                ('z', KeyCode::Char('o')) => self.fold_current(Some(false)),
                ('z', KeyCode::Char('M')) => self.fold_all(true),
                ('z', KeyCode::Char('R')) => self.fold_all(false),
                _ => {}
            }
            return Vec::new();
        }

        if self.focus == Pane::Detail && self.view == View::Findings {
            if let Some(cmds) = self.handle_findings_key(key.code) {
                return cmds;
            }
        }

        let half = (self.page / 2).max(1) as isize;
        match key.code {
            KeyCode::Char('d') if ctrl => self.motion(half),
            KeyCode::Char('u') if ctrl => self.motion(-half),
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('j') | KeyCode::Down => self.motion(1),
            KeyCode::Char('k') | KeyCode::Up => self.motion(-1),
            KeyCode::Char('J') => self.scroll_detail(1),
            KeyCode::Char('K') => self.scroll_detail(-1),
            KeyCode::Char('G') => match self.focus {
                Pane::Board => self.move_to_end(true),
                Pane::Detail => self.detail_scroll = u16::MAX / 2,
            },
            KeyCode::Char(c @ ('g' | 'z')) => self.pending = Some(c),
            KeyCode::Char(' ') | KeyCode::Enter if self.on_heading() => self.fold_current(None),
            KeyCode::Enter => return self.open_selected(View::Details),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Pane::Board if self.open.is_some() => Pane::Detail,
                    _ => Pane::Board,
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.focus = Pane::Board,
            KeyCode::Char('l') | KeyCode::Right if self.open.is_some() => self.focus = Pane::Detail,
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.focus = Pane::Board;
            }
            KeyCode::Esc => {
                if self.focus == Pane::Detail {
                    self.focus = Pane::Board;
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                }
            }
            KeyCode::Char('m') => {
                self.mine_only = !self.mine_only;
                self.rebuild();
            }
            KeyCode::Char('w') => {
                self.review_only = !self.review_only;
                self.rebuild();
            }
            KeyCode::Char('u') => {
                self.unresolved_only = !self.unresolved_only;
                self.detail_scroll = 0;
            }
            KeyCode::Char('o') => {
                let url = match self.focus {
                    Pane::Detail => self.open_item(),
                    Pane::Board => self.selected_item(),
                }
                .map(|i| i.web_url.clone());
                return url.map(Command::OpenUrl).into_iter().collect();
            }
            KeyCode::Char('R') => return self.refresh(),
            KeyCode::Char('A') => self.request_approve(),
            KeyCode::Char('r') => {
                let mut cmds = Vec::new();
                if self.focus == Pane::Board {
                    if self.selected_item().is_none() {
                        return cmds;
                    }
                    cmds = self.open_selected(View::Findings);
                }
                self.view = View::Findings;
                if let Some(item) = self.open_item().cloned() {
                    cmds.extend(self.start_review(item));
                }
                return cmds;
            }
            KeyCode::Char('d') => {
                let mut cmds = Vec::new();
                if self.focus == Pane::Board {
                    if self.selected_item().is_none() {
                        return cmds;
                    }
                    cmds = self.open_selected(View::Details);
                }
                self.view = View::Details;
                if let Some(item) = self.open_item().cloned() {
                    if item.status == Status::CiFailed {
                        cmds.extend(self.start_diagnosis(item));
                    }
                }
                return cmds;
            }
            KeyCode::Char('f') => match self.focus {
                Pane::Board => {
                    let cmds = self.open_selected(View::Findings);
                    if self.open.is_some() {
                        self.focus = Pane::Detail;
                    }
                    return cmds;
                }
                Pane::Detail => {
                    self.view = match self.view {
                        View::Details => View::Findings,
                        View::Findings => View::Details,
                    };
                    self.detail_scroll = 0;
                }
            },
            _ => {}
        }
        Vec::new()
    }

    /// Keys that act on the findings list. Returns `None` for keys the normal handler owns.
    fn handle_findings_key(&mut self, code: KeyCode) -> Option<Vec<Command>> {
        let key = self.open.clone()?;
        let count = self.reviews.get(&key).map_or(0, |r| r.findings.len());
        let review = self.reviews.get_mut(&key);
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.finding_cursor = (self.finding_cursor + 1).min(count.saturating_sub(1));
                self.detail_scroll = 0;
                Some(Vec::new())
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.finding_cursor = self.finding_cursor.saturating_sub(1);
                self.detail_scroll = 0;
                Some(Vec::new())
            }
            KeyCode::Char('G') => {
                self.finding_cursor = count.saturating_sub(1);
                self.detail_scroll = 0;
                Some(Vec::new())
            }
            KeyCode::Char(' ') => {
                let review = review?;
                let finding = review.findings.get_mut(self.finding_cursor)?;
                finding.selected = !finding.selected;
                Some(vec![Command::SaveReview(Box::new(review.clone()))])
            }
            KeyCode::Char('a') => {
                let review = review?;
                let all = review.findings.iter().all(|f| f.selected);
                review.findings.iter_mut().for_each(|f| f.selected = !all);
                Some(vec![Command::SaveReview(Box::new(review.clone()))])
            }
            KeyCode::Char(c @ '1'..='3') => {
                let review = review?;
                let severity = Severity::ALL[usize::from(c as u8 - b'1')];
                let all = review
                    .findings
                    .iter()
                    .filter(|f| f.severity == severity)
                    .all(|f| f.selected);
                review
                    .findings
                    .iter_mut()
                    .filter(|f| f.severity == severity)
                    .for_each(|f| f.selected = !all);
                Some(vec![Command::SaveReview(Box::new(review.clone()))])
            }
            KeyCode::Char('p') => {
                self.request_post();
                Some(Vec::new())
            }
            _ => None,
        }
    }

    fn motion(&mut self, delta: isize) {
        match self.focus {
            Pane::Board => self.move_by(delta),
            Pane::Detail => self.scroll_detail(delta as i32),
        }
    }
}

/// The digits of a lowercased filter that names an ID, as in `183267`, `!1405`, `#12` or `PR 12`.
fn id_query(needle: &str) -> Option<&str> {
    let trimmed = needle.trim();
    let digits = trimmed
        .strip_prefix(['!', '#'])
        .or_else(|| trimmed.strip_prefix("pr"))
        .unwrap_or(trimmed)
        .trim_start();
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Finding, Status};
    use chrono::TimeZone;

    fn item(source: Source, group: &str, id: u64, author: &str, day: u32) -> Item {
        Item {
            source,
            id,
            project_id: None,
            repo: "repo".into(),
            group: group.into(),
            title: format!("Change {id}"),
            author: author.into(),
            author_name: author.into(),
            reviewers: Vec::new(),
            approved_by: Vec::new(),
            status: Status::Active,
            source_branch: "b".into(),
            target_branch: "master".into(),
            description: String::new(),
            web_url: format!("https://example/{id}"),
            head_sha: None,
            created: Utc.with_ymd_and_hms(2026, 10, day, 0, 0, 0).unwrap(),
        }
    }

    fn sample() -> App {
        App::new(vec![
            item(Source::GitLab, "parser", 1405, "pat", 30),
            item(Source::Ado, "Core Library", 10, "jdoe", 1),
            item(Source::Ado, "API", 11, "pat", 2),
            item(Source::Ado, "Core Library", 12, "asmith", 3),
        ])
    }

    fn press(app: &mut App, keys: &str) -> Vec<Command> {
        keys.chars()
            .flat_map(|c| {
                let code = match c {
                    '\n' => KeyCode::Enter,
                    '\x1b' => KeyCode::Esc,
                    '\t' => KeyCode::Tab,
                    c => KeyCode::Char(c),
                };
                app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
            })
            .collect()
    }

    fn describe(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|r| match r {
                Row::Spacer => String::new(),
                Row::Category {
                    category,
                    count,
                    folded,
                } => format!(
                    "{} {} {count}",
                    if *folded { "+" } else { "-" },
                    category.label().to_lowercase()
                ),
                Row::Section(s) => format!("# {}", s.label()),
                Row::Group {
                    name,
                    count,
                    folded,
                    ..
                } => {
                    format!("{} {name} {count}", if *folded { "+" } else { "-" })
                }
                Row::Item(i) => format!("  {}", app.items[*i].id),
                Row::Needs { count, folded } => {
                    format!("{} needs me {count}", if *folded { "+" } else { "-" })
                }
                Row::Waiting { item, discussion } => {
                    format!("  {} {discussion}", app.items[*item].id)
                }
            })
            .collect()
    }

    fn review_of(key: &str, severities: &[Severity]) -> Review {
        Review {
            key: key.into(),
            head_sha: "h".into(),
            base_sha: "b".into(),
            created: Utc::now(),
            findings: severities
                .iter()
                .map(|s| Finding {
                    severity: *s,
                    file: None,
                    line: None,
                    title: String::new(),
                    body: String::new(),
                    inline: false,
                    selected: *s != Severity::Low,
                    posted: false,
                })
                .collect(),
        }
    }

    fn reviewed_ids(cmds: &[Command]) -> Vec<u64> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::RunReview(i) => Some(i.id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reviews_run_at_most_max_parallel_and_queue_the_rest() {
        let mut app = sample();
        app.max_parallel = 2;
        let items: Vec<Item> = app.items.clone();
        let mut started = Vec::new();
        for item in &items[..3] {
            started.extend(reviewed_ids(&app.start_review(item.clone())));
        }
        assert_eq!(started, [1405, 10]);
        assert!(
            app.start_review(items[0].clone()).is_empty(),
            "already running"
        );
        assert_eq!(app.jobs[&items[2].key()], Job::Queued);

        let next = app.review_finished(items[0].key(), Err("boom".into()));
        assert_eq!(reviewed_ids(&next), [11]);
        assert_eq!(app.jobs[&items[0].key()], Job::Failed("boom".into()));

        let done = review_of(&items[1].key(), &[Severity::High]);
        assert!(app.review_finished(items[1].key(), Ok(done)).is_empty());
        assert!(!app.jobs.contains_key(&items[1].key()));
        assert_eq!(app.reviews[&items[1].key()].findings.len(), 1);
        assert_eq!(
            reviewed_ids(&app.start_review(items[0].clone())),
            [1405],
            "a failed review can be retried"
        );
    }

    #[test]
    fn r_on_the_board_opens_findings_and_starts_a_review() {
        let mut app = sample();
        assert!(press(&mut app, "r").is_empty(), "a group row has no item");
        let cmds = press(&mut app, "jr");
        assert_eq!(reviewed_ids(&cmds), [11]);
        assert!(cmds.iter().any(|c| matches!(c, Command::LoadDetails(_))));
        assert_eq!(app.open.as_deref(), Some("ado-repo-11"));
        assert_eq!(app.view, View::Findings);
        assert_eq!(app.focus, Pane::Board);
        press(&mut app, "\n");
        assert_eq!(app.view, View::Details);

        let mut app = sample();
        press(&mut app, "j\nk");
        assert!(
            press(&mut app, "r").is_empty(),
            "a group row does not review the open item"
        );
    }

    fn failed(id: u64) -> Item {
        Item {
            status: Status::CiFailed,
            ..item(Source::GitLab, "parser", id, "pat", 30)
        }
    }

    fn diagnosed_ids(cmds: &[Command]) -> Vec<u64> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Diagnose(i) => Some(i.id),
                _ => None,
            })
            .collect()
    }

    fn select(app: &mut App, id: u64) {
        press(app, "gg");
        for _ in 0..10 {
            if app.selected_item().is_some_and(|i| i.id == id) {
                return;
            }
            press(app, "j");
        }
        panic!("item {id} is not on the board");
    }

    #[test]
    fn d_diagnoses_only_failed_ci_and_only_once_at_a_time() {
        let mut app = App::new(vec![
            failed(1405),
            item(Source::GitLab, "parser", 1406, "pat", 29),
        ]);
        select(&mut app, 1406);
        assert!(
            diagnosed_ids(&press(&mut app, "d")).is_empty(),
            "an item whose CI did not fail is not diagnosed"
        );
        select(&mut app, 1405);
        assert_eq!(diagnosed_ids(&press(&mut app, "d")), [1405]);
        assert!(
            diagnosed_ids(&press(&mut app, "d")).is_empty(),
            "a second press while the diagnosis runs starts nothing"
        );
        assert_eq!(app.view, View::Details);
    }

    #[test]
    fn a_failed_diagnosis_can_be_retried_and_a_finished_one_is_kept() {
        use crate::model::{Cause, Diagnosis};

        let item = failed(1405);
        let key = item.key();
        let mut app = App::new(vec![item]);
        select(&mut app, 1405);
        press(&mut app, "d");

        app.diagnosis_finished(key.clone(), Err("glab failed".into()));
        assert!(
            matches!(app.diagnosing.get(&key), Some(Job::Failed(e)) if e == "glab failed"),
            "the error is kept for the detail pane"
        );
        assert_eq!(
            diagnosed_ids(&press(&mut app, "d")),
            [1405],
            "a failed diagnosis can run again"
        );

        let diagnosis = Diagnosis {
            key: key.clone(),
            head_sha: "abc".into(),
            created: Utc::now(),
            jobs: vec!["unit".into()],
            cause: Cause::Trivial {
                summary: "Lint failed.".into(),
            },
        };
        app.diagnosis_finished(key.clone(), Ok(diagnosis.clone()));
        assert_eq!(app.diagnoses.get(&key), Some(&diagnosis));
        assert!(!app.diagnosing.contains_key(&key));
    }

    #[test]
    fn findings_keys_move_and_toggle_selection() {
        let mut app = sample();
        let key = "ado-repo-11".to_string();
        app.reviews.insert(
            key.clone(),
            review_of(
                &key,
                &[
                    Severity::High,
                    Severity::Medium,
                    Severity::Low,
                    Severity::Low,
                ],
            ),
        );
        press(&mut app, "jf");
        assert_eq!((app.focus, app.view), (Pane::Detail, View::Findings));
        let selected = |app: &App| -> Vec<bool> {
            app.reviews[&key]
                .findings
                .iter()
                .map(|f| f.selected)
                .collect()
        };

        let cmds = press(&mut app, "j ");
        assert!(matches!(cmds.as_slice(), [Command::SaveReview(r)] if !r.findings[1].selected));
        assert_eq!(selected(&app), [true, false, false, false]);
        press(&mut app, "3");
        assert_eq!(selected(&app), [true, false, true, true]);
        press(&mut app, "3");
        assert_eq!(selected(&app), [true, false, false, false]);
        press(&mut app, "a");
        assert_eq!(selected(&app), [true; 4]);
        press(&mut app, "a");
        assert_eq!(selected(&app), [false; 4]);

        press(&mut app, "jjjjj");
        assert_eq!(app.finding_cursor, 3);
        press(&mut app, "gg");
        assert_eq!(app.finding_cursor, 0);
        press(&mut app, "f");
        assert_eq!(app.view, View::Details);
        assert_eq!(
            app.reviews[&key].findings.len(),
            4,
            "toggling views keeps findings"
        );
    }

    fn post_picks_of(cmds: &[Command]) -> Option<Vec<usize>> {
        match cmds {
            [Command::PostFindings { picks, .. }] => Some(picks.clone()),
            _ => None,
        }
    }

    fn app_with_findings() -> (App, String) {
        let mut app = sample();
        let key = "ado-repo-11".to_string();
        let mut review = review_of(&key, &[Severity::High, Severity::Medium, Severity::Low]);
        review.findings[1].posted = true;
        app.reviews.insert(key.clone(), review);
        press(&mut app, "jf");
        (app, key)
    }

    #[test]
    fn p_asks_first_and_y_posts_only_checked_unposted_findings() {
        let (mut app, key) = app_with_findings();
        assert!(
            press(&mut app, "p").is_empty(),
            "nothing is posted before y"
        );
        assert_eq!(app.confirm_post.as_deref(), Some(key.as_str()));
        let cmds = press(&mut app, "y");
        assert_eq!(post_picks_of(&cmds), Some(vec![0]));
        assert_eq!(app.confirm_post, None);
        assert!(matches!(app.posting.get(&key), Some(Job::Running(_))));
        assert!(
            press(&mut app, "py").is_empty(),
            "a post already running is left alone"
        );
    }

    fn approved_key(cmds: &[Command]) -> Option<String> {
        cmds.iter().find_map(|c| match c {
            Command::Approve(item) => Some(item.key()),
            _ => None,
        })
    }

    #[test]
    fn shift_a_asks_first_and_only_y_approves_the_selected_item() {
        let mut app = sample();
        press(&mut app, "j");
        let key = app.selected_item().unwrap().key();
        assert!(
            press(&mut app, "A").is_empty(),
            "nothing is approved before y"
        );
        assert_eq!(app.confirm_approve.as_deref(), Some(key.as_str()));
        let cursor = app.cursor;
        assert_eq!(approved_key(&press(&mut app, "j")), None);
        assert_eq!(app.confirm_approve, None);
        assert_eq!(app.cursor, cursor, "the cancelling key does nothing else");
        assert_eq!(approved_key(&press(&mut app, "Aa")), None);
        assert_eq!(approved_key(&press(&mut app, "Ay")), Some(key.clone()));
        assert_eq!(app.approval, Some((key, Approval::Running)));
        assert!(
            press(&mut app, "A").is_empty() && app.confirm_approve.is_none(),
            "an approval already running is left alone"
        );
    }

    #[test]
    fn shift_a_on_a_group_row_asks_nothing() {
        let mut app = sample();
        assert!(app.selected_item().is_none());
        press(&mut app, "A");
        assert_eq!(app.confirm_approve, None);
    }

    fn finish_loading(app: &mut App) {
        for source in SOURCES {
            let items = app
                .items
                .iter()
                .filter(|i| i.source == source)
                .cloned()
                .collect();
            app.set_items(source, items);
        }
    }

    #[test]
    fn a_timed_refresh_waits_for_the_interval_and_for_the_last_refresh_to_finish() {
        let mut app = sample();
        app.refresh_interval = Some(Duration::from_secs(300));
        let start = Instant::now();
        app.start_refresh(start, false);
        assert_eq!(app.refresh_due_at(), None, "a refresh is still loading");
        assert!(
            app.refresh_if_due(start + Duration::from_secs(600))
                .is_empty()
        );

        finish_loading(&mut app);
        let due = start + Duration::from_secs(300);
        assert_eq!(app.refresh_due_at(), Some(due));
        assert!(app.refresh_if_due(due - Duration::from_secs(1)).is_empty());
        assert_eq!(app.refresh_if_due(due), vec![Command::Refresh]);
        assert_eq!(app.loading.len(), SOURCES.len());

        finish_loading(&mut app);
        assert_eq!(app.refresh_due_at(), Some(due + Duration::from_secs(300)));
    }

    #[test]
    fn a_manual_refresh_restarts_the_timer() {
        let mut app = sample();
        app.refresh_interval = Some(Duration::from_secs(300));
        let before = Instant::now();
        press(&mut app, "R");
        finish_loading(&mut app);
        assert!(app.refresh_due_at().unwrap() >= before + Duration::from_secs(300));
    }

    #[test]
    fn timed_refresh_is_off_without_an_interval() {
        let mut app = sample();
        app.start_refresh(Instant::now(), false);
        finish_loading(&mut app);
        assert_eq!(app.refresh_due_at(), None);
        assert!(
            app.refresh_if_due(Instant::now() + Duration::from_secs(86_400))
                .is_empty()
        );
    }

    #[test]
    fn a_timed_refresh_keeps_the_open_details_on_screen_while_they_reload() {
        let mut app = sample();
        app.refresh_interval = Some(Duration::from_secs(300));
        press(&mut app, "j\n");
        let key = app.open.clone().unwrap();
        app.set_details(
            key.clone(),
            DetailState::Loaded(Details {
                description: String::new(),
                comments: Vec::new(),
            }),
        );
        let start = Instant::now();
        app.start_refresh(start, false);
        finish_loading(&mut app);
        app.set_details(
            key.clone(),
            DetailState::Loaded(Details {
                description: String::new(),
                comments: Vec::new(),
            }),
        );

        let cmds = app.refresh_if_due(start + Duration::from_secs(300));
        assert!(matches!(
            app.details.get(&key),
            Some(DetailState::Loaded(_))
        ));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadDetails(i) if i.key() == key))
        );

        let cmds = app.refresh();
        assert!(matches!(app.details.get(&key), Some(DetailState::Loading)));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadDetails(i) if i.key() == key))
        );
    }

    #[test]
    fn a_finished_approval_refreshes_and_clears_on_the_next_key() {
        let mut app = sample();
        press(&mut app, "j");
        let key = app.selected_item().unwrap().key();
        press(&mut app, "Ay");
        let cmds = app.approve_finished(key.clone(), Ok(()));
        assert!(cmds.contains(&Command::Refresh));
        assert_eq!(app.approval, Some((key.clone(), Approval::Done)));
        press(&mut app, "j");
        assert_eq!(app.approval, None);
        press(&mut app, "kAy");
        let cmds = app.approve_finished(key.clone(), Err("denied".into()));
        assert!(cmds.is_empty());
        assert_eq!(app.approval, Some((key, Approval::Failed("denied".into()))));
    }

    #[test]
    fn any_key_but_y_cancels_a_post() {
        let (mut app, _) = app_with_findings();
        let cmds = press(&mut app, "pn");
        assert_eq!(post_picks_of(&cmds), None);
        assert_eq!(app.confirm_post, None);
        assert_eq!(
            app.finding_cursor, 0,
            "the cancelling key does nothing else"
        );
        press(&mut app, "aa");
        assert!(press(&mut app, "p").is_empty());
        assert_eq!(
            app.confirm_post, None,
            "nothing checked, nothing to confirm"
        );
    }

    #[test]
    fn a_stale_review_is_not_posted() {
        let (mut app, key) = app_with_findings();
        app.items[2].head_sha = Some("newer".into());
        assert!(press(&mut app, "py").is_empty());
        assert!(matches!(app.posting.get(&key), Some(Job::Failed(e)) if e.contains("stale")));
    }

    #[test]
    fn finished_posts_mark_findings_and_keep_the_first_error() {
        let (mut app, key) = app_with_findings();
        let created = app.reviews[&key].created;
        let outcome = vec![(0, Ok(())), (2, Err("az failed: 403".to_string()))];
        let cmds = app.post_finished(key.clone(), created, outcome);
        assert!(matches!(cmds.as_slice(), [Command::SaveReview(r)] if r.findings[0].posted));
        let posted: Vec<bool> = app.reviews[&key]
            .findings
            .iter()
            .map(|f| f.posted)
            .collect();
        assert_eq!(posted, [true, true, false]);
        assert_eq!(
            app.posting.get(&key),
            Some(&Job::Failed(
                "1 of 2 findings were not posted: az failed: 403".into()
            ))
        );

        let outcome = vec![(2, Ok(()))];
        let replaced = created + chrono::Duration::seconds(1);
        assert!(app.post_finished(key.clone(), replaced, outcome).is_empty());
        assert!(
            !app.reviews[&key].findings[2].posted,
            "a newer review is not touched"
        );
        assert!(!app.posting.contains_key(&key));
    }

    #[test]
    fn u_toggles_unresolved_comments_and_scrolls_to_top() {
        let mut app = sample();
        assert!(app.unresolved_only);
        app.detail_scroll = 7;
        press(&mut app, "u");
        assert!(!app.unresolved_only);
        assert_eq!(app.detail_scroll, 0);
        press(&mut app, "u");
        assert!(app.unresolved_only);
    }

    #[test]
    fn opening_another_item_hides_resolved_comments_again() {
        let mut app = sample();
        press(&mut app, "ggj\nu");
        assert!(!app.unresolved_only);
        press(&mut app, "\n");
        assert!(
            !app.unresolved_only,
            "reopening the same item keeps the choice"
        );
        press(&mut app, "hjj\n");
        assert_eq!(app.open.as_deref(), Some("ado-repo-12"));
        assert!(app.unresolved_only);
    }

    #[test]
    fn groups_ado_areas_first_then_gitlab_projects_newest_first() {
        let app = sample();
        assert_eq!(
            describe(&app),
            [
                "# Azure DevOps",
                "- API 1",
                "  11",
                "",
                "- Core Library 2",
                "  12",
                "  10",
                "",
                "",
                "# GitLab",
                "- parser 1",
                "  1405",
            ]
        );
        assert_eq!(app.cursor, 1, "cursor skips the section header");
    }

    #[test]
    fn j_and_k_skip_section_rows() {
        let mut app = sample();
        press(&mut app, "jjjjj");
        assert_eq!(
            app.rows[app.cursor],
            Row::Group {
                source: Source::GitLab,
                name: "parser".into(),
                count: 1,
                folded: false
            }
        );
        press(&mut app, "k");
        assert_eq!(app.selected_item().map(|i| i.id), Some(10));
        press(&mut app, "G");
        assert_eq!(app.selected_item().map(|i| i.id), Some(1405));
        press(&mut app, "gg");
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn za_folds_the_group_of_the_current_item_and_keeps_the_cursor_on_it() {
        let mut app = sample();
        press(&mut app, "jjj");
        assert_eq!(app.selected_item().map(|i| i.id), Some(12));
        press(&mut app, "za");
        assert_eq!(
            app.rows[app.cursor],
            Row::Group {
                source: Source::Ado,
                name: "Core Library".into(),
                count: 2,
                folded: true
            }
        );
        press(&mut app, "za");
        assert_eq!(describe(&app).len(), 12);
        press(&mut app, "zM");
        assert_eq!(
            describe(&app),
            [
                "# Azure DevOps",
                "+ API 1",
                "",
                "+ Core Library 2",
                "",
                "",
                "# GitLab",
                "+ parser 1"
            ]
        );
        press(&mut app, "zR");
        assert_eq!(describe(&app).len(), 12);
    }

    #[test]
    fn filter_narrows_rows_and_esc_clears_it() {
        let mut app = sample();
        press(&mut app, "/asmith\n");
        assert_eq!(
            describe(&app),
            ["# Azure DevOps", "- Core Library 1", "  12"]
        );
        assert_eq!(app.mode, Mode::Normal);
        press(&mut app, "\x1b");
        assert_eq!(describe(&app).len(), 12);
    }

    #[test]
    fn filter_matches_partial_and_prefixed_ids() {
        let filtered_ids = |query: &str| {
            let mut app = sample();
            for item in &mut app.items {
                item.title = "Untitled".into();
            }
            press(&mut app, &format!("/{query}\n"));
            let mut ids: Vec<u64> = app
                .rows
                .iter()
                .filter_map(|r| match r {
                    Row::Item(i) => Some(app.items[*i].id),
                    _ => None,
                })
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(filtered_ids("140"), [1405]);
        assert_eq!(filtered_ids("!1405"), [1405]);
        assert_eq!(filtered_ids("#12"), [12]);
        assert_eq!(filtered_ids("PR 11"), [11]);
        assert_eq!(filtered_ids("pr1"), [10, 11, 12, 1405]);
    }

    #[test]
    fn mine_shows_items_i_wrote_or_review() {
        let mut app = sample();
        app.me.insert(Source::Ado, "pat".into());
        app.me.insert(Source::GitLab, "pat".into());
        app.items[1].reviewers.push("Pat".into());
        press(&mut app, "m");
        let ids: Vec<u64> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(app.items[*i].id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [11, 10, 1405]);
        assert_eq!(app.mine_count(), 3);
    }

    #[test]
    fn w_shows_only_items_awaiting_review() {
        let mut app = sample();
        let statuses = [
            Status::Draft,
            Status::Approved,
            Status::CiRunning,
            Status::Conflicts,
        ];
        for (item, status) in app.items.iter_mut().zip(statuses) {
            item.status = status;
        }
        app.items.push(item(Source::Ado, "API", 13, "x", 4));
        app.rebuild();
        press(&mut app, "w");
        assert_eq!(item_ids(&app), [13, 11]);
        assert_eq!(app.review_count(), 2);
        assert_eq!(
            describe(&app),
            ["# Azure DevOps", "- API 2", "  13", "  11"]
        );
        press(&mut app, "w");
        assert_eq!(item_ids(&app).len(), 5);
    }

    fn item_ids(app: &App) -> Vec<u64> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                Row::Item(i) => Some(app.items[*i].id),
                _ => None,
            })
            .collect()
    }

    fn discussion(id: &str, logins: &[&str], hour: u32) -> Discussion {
        Discussion {
            id: id.into(),
            location: None,
            status: crate::model::ThreadStatus::Open,
            notes: logins
                .iter()
                .enumerate()
                .map(|(n, login)| crate::model::DiscussionNote {
                    login: login.to_string(),
                    author: login.to_string(),
                    date: Utc
                        .with_ymd_and_hms(2026, 11, 1, hour, n as u32, 0)
                        .unwrap(),
                    body: "text".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn threads_are_fetched_only_for_items_i_am_part_of() {
        let mut app = sample();
        assert!(
            app.load_discussions().is_empty(),
            "nothing is mine until me is known"
        );
        app.me.insert(Source::Ado, "pat".into());
        let cmds = app.load_discussions();
        assert!(matches!(cmds.as_slice(), [Command::LoadDiscussions(i)] if i.id == 11));
        assert!(app.load_discussions().is_empty(), "already loading");
        let mut posted = review_of("ado-repo-12", &[Severity::Low]);
        posted.findings[0].posted = true;
        app.reviews.insert("ado-repo-12".into(), posted);
        let cmds = app.load_discussions();
        assert!(matches!(cmds.as_slice(), [Command::LoadDiscussions(i)] if i.id == 12));
    }

    #[test]
    fn needs_me_lists_threads_answered_by_others_newest_first() {
        let mut app = sample();
        app.me.insert(Source::Ado, "pat".into());
        app.discussions_loaded(
            "ado-repo-11".into(),
            Ok(vec![
                discussion("a", &["pat", "bob"], 1),
                discussion("b", &["bob", "pat"], 2),
            ]),
        );
        app.discussions_loaded(
            "ado-repo-12".into(),
            Ok(vec![discussion("c", &["pat", "ann"], 3)]),
        );
        assert_eq!(
            describe(&app)[..9],
            [
                "- prs awaiting response 1",
                "  11",
                "",
                "",
                "- needs me 2",
                "  12 c",
                "  11 a",
                "",
                ""
            ]
        );
        let cmds = press(&mut app, "ggjjj\n");
        assert!(matches!(cmds.as_slice(), [Command::LoadDetails(i)] if i.id == 12));
        press(&mut app, "hggjj ");
        assert_eq!(describe(&app)[4..7], ["+ needs me 2", "", ""]);
        app.discussions_loaded("ado-repo-12".into(), Err("offline".into()));
        assert_eq!(
            describe(&app)[4],
            "+ needs me 2",
            "a failed fetch keeps the old threads"
        );
    }

    /// 12 by asmith, which I reviewed and asmith answered; 11 by me, which bob commented on.
    fn app_with_categories() -> App {
        let mut app = sample();
        app.me.insert(Source::Ado, "pat".into());
        app.discussions_loaded(
            "ado-repo-12".into(),
            Ok(vec![discussion("a", &["pat", "asmith"], 1)]),
        );
        app.discussions_loaded("ado-repo-11".into(), Ok(vec![discussion("b", &["bob"], 2)]));
        app
    }

    #[test]
    fn categories_head_the_board_and_take_their_items_out_of_the_groups() {
        let app = app_with_categories();
        assert_eq!(
            describe(&app),
            [
                "- reviews awaiting approval 1",
                "  12",
                "",
                "",
                "- prs awaiting response 1",
                "  11",
                "",
                "",
                "- needs me 1",
                "  12 a",
                "",
                "",
                "# Azure DevOps",
                "- Core Library 1",
                "  10",
                "",
                "",
                "# GitLab",
                "- parser 1",
                "  1405",
            ]
        );
    }

    #[test]
    fn an_approved_review_returns_to_its_group() {
        let mut app = app_with_categories();
        app.items.iter_mut().find(|i| i.id == 12).unwrap().status = Status::Approved;
        app.rebuild();
        let rows = describe(&app);
        assert!(!rows.contains(&"- reviews awaiting approval 1".to_string()));
        assert!(rows.contains(&"- Core Library 2".to_string()));
    }

    #[test]
    fn categories_fold_from_their_heading_or_an_item_and_follow_the_mine_filter() {
        let mut app = app_with_categories();
        press(&mut app, "ggj");
        assert_eq!(app.selected_item().map(|i| i.id), Some(12));
        press(&mut app, "za");
        assert_eq!(describe(&app)[..2], ["+ reviews awaiting approval 1", ""]);
        assert_eq!(
            app.rows[app.cursor],
            Row::Category {
                category: Category::AwaitingApproval,
                count: 1,
                folded: true
            },
            "the cursor moves to the folded heading"
        );
        press(&mut app, " ");
        assert_eq!(
            describe(&app)[..2],
            ["- reviews awaiting approval 1", "  12"]
        );
        press(&mut app, "zM");
        assert_eq!(describe(&app)[3], "+ prs awaiting response 1");
        press(&mut app, "zRm");
        assert_eq!(
            describe(&app)[0],
            "- prs awaiting response 1",
            "12 is neither mine nor reviewed by me as a listed reviewer"
        );
    }

    #[test]
    fn the_cursor_follows_an_item_into_a_category() {
        let mut app = sample();
        app.me.insert(Source::Ado, "pat".into());
        press(&mut app, "ggj");
        assert_eq!(app.selected_item().map(|i| i.id), Some(11));
        app.discussions_loaded("ado-repo-11".into(), Ok(vec![discussion("b", &["bob"], 2)]));
        assert_eq!(app.cursor, 1);
        assert_eq!(app.selected_item().map(|i| i.id), Some(11));
    }

    #[test]
    fn enter_loads_details_once() {
        let mut app = sample();
        let cmds = press(&mut app, "j\n");
        assert!(matches!(cmds.as_slice(), [Command::LoadDetails(i)] if i.id == 11));
        assert_eq!(app.open.as_deref(), Some("ado-repo-11"));
        assert!(press(&mut app, "\n").is_empty(), "already loading");
        let loaded = Details {
            description: String::new(),
            comments: Vec::new(),
        };
        app.set_details("ado-repo-11".into(), DetailState::Loaded(loaded));
        assert!(press(&mut app, "\n").is_empty(), "already loaded");
        app.set_details("ado-repo-11".into(), DetailState::Failed("x".into()));
        assert_eq!(press(&mut app, "\n").len(), 1, "a failed load is retried");
    }

    #[test]
    fn replacing_one_source_keeps_the_other_and_the_selection() {
        let mut app = sample();
        press(&mut app, "jjj");
        app.set_items(Source::GitLab, vec![item(Source::GitLab, "idl", 7, "x", 5)]);
        assert_eq!(app.selected_item().map(|i| i.id), Some(12));
        assert_eq!(app.count(Source::Ado), 3);
        assert_eq!(app.count(Source::GitLab), 1);
    }

    #[test]
    fn first_load_starts_at_the_top_whichever_source_arrives_first() {
        let mut app = App::default();
        app.set_items(Source::GitLab, vec![item(Source::GitLab, "idl", 7, "x", 5)]);
        app.set_items(Source::Ado, vec![item(Source::Ado, "API", 11, "pat", 2)]);
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn selection_survives_rows_shifting_above_it() {
        let mut app = sample();
        app.items
            .push(item(Source::GitLab, "parser", 1400, "x", 20));
        app.rebuild();
        press(&mut app, "Gk");
        assert_eq!(app.selected_item().map(|i| i.id), Some(1405));
        app.set_items(Source::Ado, vec![item(Source::Ado, "API", 11, "pat", 2)]);
        assert_eq!(app.selected_item().map(|i| i.id), Some(1405));
    }

    #[test]
    fn detail_focus_scrolls_instead_of_moving() {
        let mut app = sample();
        press(&mut app, "j\n\tjj");
        assert_eq!(app.focus, Pane::Detail);
        assert_eq!(app.detail_scroll, 2);
        assert_eq!(app.selected_item().map(|i| i.id), Some(11));
        press(&mut app, "\x1b");
        assert_eq!(app.focus, Pane::Board);
    }

    #[test]
    fn animates_only_while_a_spinner_shows() {
        let mut app = sample();
        assert!(!app.animating());
        for job in [Job::Queued, Job::Failed("x".into())] {
            app.jobs.insert("k".into(), job.clone());
            app.posting.insert("k".into(), job);
            assert!(!app.animating());
        }
        app.diagnosing
            .insert("k".into(), Job::Running("logs".into()));
        assert!(!app.animating(), "the CI block shows no spinner");

        app.jobs.insert("k".into(), Job::Running("cloning".into()));
        assert!(app.animating());
        app.jobs.clear();
        app.posting
            .insert("k".into(), Job::Running("posting".into()));
        assert!(app.animating());
        app.posting.clear();
        app.loading.insert(Source::Ado);
        assert!(app.animating());
    }
}
