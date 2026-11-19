//! UI state and key handling. Pure: no I/O, so it is unit-testable.

use crate::config::{Overrides, Panel, Settings};
use crate::model::{
    CheckState, Pipeline, PrChecks, Snapshot, Stale, Thresholds, Worker, WorkerState, classify,
};
use crate::options::{self, Category, Item, Target};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Active,
    NeedsYou,
}

impl Filter {
    pub fn next(self) -> Filter {
        match self {
            Filter::All => Filter::Active,
            Filter::Active => Filter::NeedsYou,
            Filter::NeedsYou => Filter::All,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Active => "active",
            Filter::NeedsYou => "needs you",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Up,
    Down,
    Top,
    Bottom,
    Toggle,
    Collapse,
    Filter,
    Refresh,
    Pause,
    Help,
    Quit,
    /// Open or close the options menu.
    Options,
    /// Show or hide a panel.
    Panel(Panel),
    /// In the options menu: change the selected row, up or down.
    Change {
        up: bool,
    },
    /// In the options menu: the next or previous category.
    Category {
        next: bool,
    },
}

/// The open options menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub category: Category,
    /// The selected row, by position.
    pub selected: usize,
    /// The selected row, by what it changes, so the selection stays on it
    /// when usage windows come and go between reads.
    focus: Option<Target>,
}

impl Menu {
    fn new(category: Category) -> Self {
        Menu {
            category,
            selected: 0,
            focus: None,
        }
    }

    /// The selected row of `items`, if there is one.
    pub fn index(&self, items: &[Item]) -> Option<usize> {
        if items.is_empty() {
            return None;
        }
        let by_focus = self
            .focus
            .as_ref()
            .and_then(|f| items.iter().position(|i| &i.target == f));
        Some(by_focus.unwrap_or(self.selected.min(items.len() - 1)))
    }

    fn select(&mut self, i: usize, items: &[Item]) {
        self.selected = i;
        self.focus = items.get(i).map(|it| it.target.clone());
    }
}

/// What the options menu shows about the config file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigStatus {
    /// Where settings are saved, as shown to the reader.
    pub path: String,
    /// Entries of the file that were ignored, and why.
    pub problems: Vec<String>,
    /// When settings were last saved, and the error if that failed.
    pub saved: Option<(i64, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct App {
    pub selected: Option<String>,
    pub expanded: BTreeSet<String>,
    pub filter: Filter,
    pub help: bool,
    pub quit: bool,
    pub paused: bool,
    /// The settings as the config file has them; the options menu edits these.
    pub settings: Settings,
    /// Settings given on the command line, which win over the file.
    pub overrides: Overrides,
    /// The thresholds in effect: the file's, with the overrides on top.
    pub thresholds: Thresholds,
    pub menu: Option<Menu>,
    pub config: ConfigStatus,
    /// A fleet read this old is overdue, unless refresh is paused.
    pub read_overdue_secs: i64,
    changes: BTreeMap<String, Seen>,
    /// A fleet reading has been observed: later new rows are changes.
    seeded: bool,
}

/// A fleet read older than this is overdue, at the default fleet interval.
pub const DEFAULT_READ_OVERDUE_SECS: i64 = 180;

/// What [`App::observe`] last saw of one worker.
#[derive(Debug, Clone)]
struct Seen {
    fingerprint: Fingerprint,
    /// When the row last changed: the time crowsnest watched it change, or,
    /// for a row it has only just met, its last status line.
    changed_at: Option<i64>,
}

impl Default for App {
    fn default() -> Self {
        App::new(Settings::default(), Overrides::default())
    }
}

impl App {
    pub fn new(settings: Settings, overrides: Overrides) -> Self {
        let thresholds = overrides.apply(&settings).thresholds();
        App {
            selected: None,
            expanded: BTreeSet::new(),
            filter: Filter::All,
            help: false,
            quit: false,
            paused: false,
            settings,
            overrides,
            thresholds,
            menu: None,
            config: ConfigStatus::default(),
            read_overdue_secs: DEFAULT_READ_OVERDUE_SECS,
            changes: BTreeMap::new(),
            seeded: false,
        }
    }

    /// Note which rows changed since the last call. Call it with every
    /// snapshot that is about to be drawn, so each change is dated by the
    /// moment it was first on screen.
    pub fn observe(&mut self, snap: &Snapshot) {
        // A failed fleet reading leaves the rows as they were.
        if snap.fleet.is_none() {
            return;
        }
        let rows = all_rows(snap, &self.thresholds);
        // On the first reading rows are only being met, not changing.
        let known = self.seeded;
        let mut seen = BTreeMap::new();
        for r in &rows {
            let fingerprint = fingerprint(r);
            let id = &r.worker.id;
            let entry = match self.changes.remove(id) {
                Some(mut s) => {
                    match s.fingerprint.update(fingerprint) {
                        Update::Same => {}
                        Update::Met => s.changed_at = s.changed_at.max(met_at(r, snap.now)),
                        Update::Changed => s.changed_at = Some(snap.now),
                    }
                    s
                }
                None => Seen {
                    fingerprint,
                    changed_at: if known {
                        Some(snap.now)
                    } else {
                        met_at(r, snap.now)
                    },
                },
            };
            seen.insert(id.clone(), entry);
        }
        self.changes = seen;
        self.seeded = true;
    }

    /// Seconds since this worker's row last changed, as of `now`.
    pub fn changed_ago(&self, id: &str, now: i64) -> Option<i64> {
        let at = self.changes.get(id)?.changed_at?;
        Some((now - at).max(0))
    }

    /// Seconds since any row last changed, as of `now`.
    pub fn last_change_ago(&self, now: i64) -> Option<i64> {
        self.changes
            .values()
            .filter_map(|s| s.changed_at)
            .max()
            .map(|at| (now - at).max(0))
    }

    /// The settings in effect: the file's, with the overrides on top.
    pub fn effective(&self) -> Settings {
        self.overrides.apply(&self.settings)
    }

    /// The rows of the options menu's current category.
    pub fn menu_items(&self, snap: &Snapshot) -> Vec<Item> {
        self.menu.as_ref().map_or_else(Vec::new, |m| {
            options::items(m.category, &self.settings, &self.overrides, &snap.quota)
        })
    }

    /// Apply a key action to the screen drawn from `snap`. Returns true when
    /// the action asks the collector to refresh. A change to the settings
    /// shows in [`App::settings`]; saving them is the caller's job.
    pub fn apply(&mut self, action: Action, snap: &Snapshot) -> bool {
        if self.help && !matches!(action, Action::Quit) {
            self.help = false;
            return false;
        }
        match action {
            Action::Quit => self.quit = true,
            Action::Panel(p) => {
                options::change(&Target::Panel(p), &mut self.settings, &self.overrides, true);
            }
            Action::Options if self.menu.is_some() => self.menu = None,
            Action::Options => self.menu = Some(Menu::new(Category::Panels)),
            _ if self.menu.is_some() => self.apply_menu(action, snap),
            _ => {
                let rows = visible_rows(snap, self);
                let ids: Vec<String> = rows.iter().map(|r| r.worker.id.clone()).collect();
                let idx = self.selected_index(&rows);
                return self.apply_roster(action, &ids, idx);
            }
        }
        self.thresholds = self.effective().thresholds();
        false
    }

    fn apply_menu(&mut self, action: Action, snap: &Snapshot) {
        let Some(mut menu) = self.menu.take() else {
            return;
        };
        let items = options::items(menu.category, &self.settings, &self.overrides, &snap.quota);
        let idx = menu.index(&items).unwrap_or(0);
        let last = items.len().saturating_sub(1);
        match action {
            Action::Up => menu.select(idx.saturating_sub(1), &items),
            Action::Down => menu.select((idx + 1).min(last), &items),
            Action::Top => menu.select(0, &items),
            Action::Bottom => menu.select(last, &items),
            Action::Change { up } => {
                if let Some(item) = items.get(idx) {
                    options::change(&item.target, &mut self.settings, &self.overrides, up);
                    menu.select(idx, &items);
                }
            }
            Action::Category { next } => menu = Menu::new(menu.category.cycle(next)),
            _ => {}
        }
        self.menu = Some(menu);
    }

    fn apply_roster(&mut self, action: Action, ids: &[String], idx: Option<usize>) -> bool {
        let pick = |i: usize| ids.get(i).cloned();
        match action {
            Action::Up => self.selected = pick(idx.map_or(0, |i| i.saturating_sub(1))),
            Action::Down => {
                let last = ids.len().saturating_sub(1);
                self.selected = pick(idx.map_or(0, |i| (i + 1).min(last)));
            }
            Action::Top => self.selected = pick(0),
            Action::Bottom => self.selected = pick(ids.len().saturating_sub(1)),
            Action::Toggle => {
                if let Some(id) = idx.and_then(pick) {
                    if !self.expanded.remove(&id) {
                        self.expanded.insert(id.clone());
                    }
                    self.selected = Some(id);
                }
            }
            Action::Collapse => {
                if let Some(id) = &self.selected {
                    self.expanded.remove(id);
                }
            }
            Action::Filter => self.filter = self.filter.next(),
            Action::Refresh => return true,
            Action::Pause => self.paused = !self.paused,
            Action::Help => self.help = true,
            // Handled by `apply`, or only meaningful in the options menu.
            Action::Quit
            | Action::Options
            | Action::Panel(_)
            | Action::Change { .. }
            | Action::Category { .. } => {}
        }
        false
    }

    /// Index of the selected row, defaulting to the first row.
    pub fn selected_index(&self, rows: &[Row]) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        Some(
            self.selected
                .as_ref()
                .and_then(|id| rows.iter().position(|r| &r.worker.id == id))
                .unwrap_or(0),
        )
    }
}

/// One worker as the roster shows it.
#[derive(Debug, Clone)]
pub struct Row<'a> {
    pub worker: &'a Worker,
    pub state: WorkerState,
    pub pipeline: Option<&'a Pipeline>,
    pub pr: Option<&'a PrChecks>,
    pub stale: Option<Stale>,
}

impl Row<'_> {
    pub fn pr_url(&self) -> Option<&str> {
        self.worker
            .pr_url
            .as_deref()
            .or_else(|| self.pipeline.and_then(|p| p.pr_url.as_deref()))
    }
}

/// When a row crowsnest has only just met last did something: its last status
/// line, the moment its pipeline's active step started, or the moment it
/// parked at a gate, whichever is latest.
fn met_at(r: &Row, now: i64) -> Option<i64> {
    let status = r
        .worker
        .last_event
        .as_ref()
        .and_then(|e| e.age(now))
        .map(|a| now - a);
    let since = |d: Option<i64>| d.map(|a| now - a);
    let (active, parked) = r.pipeline.map_or((None, None), |p| {
        (since(p.active_for(now)), since(p.parked_for(now)))
    });
    status.max(active).max(parked)
}

/// Everything the readings say about a row that, when it differs between two
/// of them, means the row changed. Durations are left out: they change on
/// every reading. The pipeline and PR come from their own sources, which may
/// not have read them yet, so each is only compared between readings that
/// have it.
#[derive(Debug, Clone)]
struct Fingerprint {
    worker: String,
    pipeline: Option<String>,
    pr: Option<String>,
}

enum Update {
    Same,
    /// Only a part not read before turned up.
    Met,
    Changed,
}

impl Fingerprint {
    /// Take in a newer reading of the same row and say how it differs.
    fn update(&mut self, new: Fingerprint) -> Update {
        let mut update = Update::Same;
        if self.worker != new.worker {
            self.worker = new.worker;
            update = Update::Changed;
        }
        // The pipeline tier keeps a task's last reading when a read fails, so
        // a pipeline that goes is gone; a PR whose read failed just drops out.
        for (old, new, gone_is_change) in [
            (&mut self.pipeline, new.pipeline, true),
            (&mut self.pr, new.pr, false),
        ] {
            match (old.as_ref(), new) {
                (Some(_), None) if gone_is_change => {
                    *old = None;
                    update = Update::Changed;
                }
                (_, None) => {}
                (Some(o), Some(n)) if *o == n => {}
                (Some(_), Some(n)) => {
                    *old = Some(n);
                    update = Update::Changed;
                }
                (None, Some(n)) => {
                    *old = Some(n);
                    if matches!(update, Update::Same) {
                        update = Update::Met;
                    }
                }
            }
        }
        update
    }
}

fn fingerprint(r: &Row) -> Fingerprint {
    use std::fmt::Write;
    let w = r.worker;
    // The derived state is left out too: a row crossing a time threshold is
    // not new data, and its own tag says how long it has been so.
    let mut worker = format!("{}|{}|{:?}", w.fm_state, w.blocked_event, w.open_decisions);
    if let Some(e) = &w.last_event {
        let _ = write!(worker, "|{}:{}", e.state, e.note);
    }
    let _ = write!(worker, "|{:?}", w.wait_until);
    let pipeline = r.pipeline.map(|p| {
        let mut f = format!("{}|{}|{:?}|{}", p.status, p.findings, p.gate, p.is_parked());
        for s in &p.steps {
            let _ = write!(f, "|{}{:?}{}", s.name, s.status, s.findings);
        }
        if let Some(a) = &p.active {
            let _ = write!(f, "|{}{}{}", a.step, a.status, a.round);
        }
        f
    });
    let pr = r.pr.map(|pr| {
        let mut f = format!("{}{}", pr.state, pr.draft);
        for state in [
            CheckState::Pass,
            CheckState::Fail,
            CheckState::Running,
            CheckState::Queued,
            CheckState::Skipped,
        ] {
            let _ = write!(f, "{}", pr.count(state));
        }
        f
    });
    Fingerprint {
        worker,
        pipeline,
        pr,
    }
}

/// Every worker with its derived state, most urgent first.
pub fn all_rows<'a>(snap: &'a Snapshot, th: &Thresholds) -> Vec<Row<'a>> {
    let Some(fleet) = &snap.fleet else {
        return Vec::new();
    };
    let mut rows: Vec<Row> = fleet
        .workers
        .iter()
        .map(|w| {
            let pipeline = snap.pipelines.get(&w.id);
            let (state, stale) = classify(w, pipeline, snap.now, th);
            let pr_url = w
                .pr_url
                .as_deref()
                .or_else(|| pipeline.and_then(|p| p.pr_url.as_deref()));
            Row {
                worker: w,
                state,
                pipeline,
                pr: pr_url.and_then(|u| snap.prs.get(u)),
                stale,
            }
        })
        .collect();
    rows.sort_by_key(|r| r.state);
    rows
}

pub fn visible_rows<'a>(snap: &'a Snapshot, app: &App) -> Vec<Row<'a>> {
    all_rows(snap, &app.thresholds)
        .into_iter()
        .filter(|r| match app.filter {
            Filter::All => true,
            Filter::Active => !r.state.is_quiet(),
            Filter::NeedsYou => r.state == WorkerState::Captain,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Setting;
    use crate::model::{Fleet, QuotaWindow};

    fn snap() -> Snapshot {
        let w = |id: &str, state: &str| Worker {
            id: id.into(),
            fm_state: state.into(),
            ..Default::default()
        };
        let mut asking = w("asks", "working");
        asking.open_decisions = vec!["k".into()];
        Snapshot {
            fleet: Some(Fleet {
                workers: vec![
                    w("a-done", "done"),
                    w("b-work", "working"),
                    asking,
                    w("idle", "no_active_work"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn captain_rows_sort_first() {
        let s = snap();
        let ids: Vec<&str> = all_rows(&s, &Thresholds::new(1800))
            .iter()
            .map(|r| r.worker.id.as_str())
            .collect();
        assert_eq!(ids, ["asks", "b-work", "a-done", "idle"]);
    }

    #[test]
    fn crossing_a_time_threshold_is_not_a_change() {
        let mut s = snap();
        s.now = 1_000_000;
        // A parked run, read just now: 9m on the gate, the threshold is 10m.
        s.pipelines.insert(
            "b-work".into(),
            Pipeline {
                status: "running".into(),
                parked_secs: Some(9 * 60),
                read_at: Some(s.now),
                ..Default::default()
            },
        );
        let mut app = App::default();
        app.observe(&s);
        let state = |s: &Snapshot| {
            all_rows(s, &Thresholds::new(1800))
                .iter()
                .find(|r| r.worker.id == "b-work")
                .unwrap()
                .state
        };
        assert_eq!(state(&s), WorkerState::Gate);
        let met = app.changed_ago("b-work", s.now);
        // Two minutes on, with no new reading, the gate is parked past 10m.
        s.now += 120;
        app.observe(&s);
        assert_eq!(state(&s), WorkerState::Stale);
        assert_eq!(
            app.changed_ago("b-work", s.now),
            met.map(|m| m + 120),
            "the row's clock keeps counting"
        );
        // A real change, the gate answered, does reset it.
        s.pipelines.get_mut("b-work").unwrap().parked_secs = None;
        s.pipelines.get_mut("b-work").unwrap().status = "completed".into();
        app.observe(&s);
        assert_eq!(app.changed_ago("b-work", s.now), Some(0));
    }

    #[test]
    fn a_pipeline_read_after_the_fleet_is_met_not_changed() {
        let mut s = snap();
        s.now = 1_000_000;
        let mut app = App::default();
        // The fleet is read first; the pipeline tier has not read yet.
        app.observe(&s);
        // Five seconds on it reads a run parked at its gate for 1h17m.
        s.now += 5;
        s.pipelines.insert(
            "b-work".into(),
            Pipeline {
                status: "running".into(),
                parked_secs: Some(77 * 60),
                read_at: Some(s.now),
                ..Default::default()
            },
        );
        app.observe(&s);
        assert_eq!(app.changed_ago("b-work", s.now), Some(77 * 60));
        assert_eq!(app.changed_ago("a-done", s.now), None);
        // A later read that finds the same run is no change.
        s.now += 15;
        app.observe(&s);
        assert_eq!(app.changed_ago("b-work", s.now), Some(77 * 60 + 15));
        // The run going away is.
        s.pipelines.remove("b-work");
        app.observe(&s);
        assert_eq!(app.changed_ago("b-work", s.now), Some(0));
    }

    #[test]
    fn navigation_filter_and_expand() {
        let s = snap();
        let mut app = App::default();
        app.apply(Action::Down, &s);
        assert_eq!(app.selected.as_deref(), Some("b-work"));
        app.apply(Action::Toggle, &s);
        assert!(app.expanded.contains("b-work"));
        app.apply(Action::Toggle, &s);
        assert!(!app.expanded.contains("b-work"));
        app.apply(Action::Bottom, &s);
        assert_eq!(app.selected.as_deref(), Some("idle"));
        app.apply(Action::Filter, &s);
        assert_eq!(visible_rows(&s, &app).len(), 2);
        app.apply(Action::Filter, &s);
        assert_eq!(visible_rows(&s, &app).len(), 1);
        assert!(app.apply(Action::Refresh, &s));
        app.apply(Action::Help, &s);
        assert!(app.help);
        app.apply(Action::Down, &s);
        assert!(!app.help, "any key closes help");
        app.apply(Action::Quit, &s);
        assert!(app.quit);
    }

    #[test]
    fn panel_hotkeys_toggle_from_the_dashboard_and_the_menu() {
        let s = snap();
        let mut app = App::default();
        app.apply(Action::Panel(Panel::Runners), &s);
        assert!(!app.settings.shown(Panel::Runners));
        app.apply(Action::Options, &s);
        app.apply(Action::Panel(Panel::Runners), &s);
        assert!(
            app.settings.shown(Panel::Runners),
            "1-5 work in the menu too"
        );
        assert!(app.menu.is_some(), "and leave it open");
        app.apply(Action::Options, &s);
        app.apply(Action::Help, &s);
        app.apply(Action::Panel(Panel::Host), &s);
        assert!(!app.help);
        assert!(
            app.settings.shown(Panel::Host),
            "a key that closes help does nothing else"
        );
    }

    #[test]
    fn menu_keys_select_and_change_rows() {
        let mut s = snap();
        s.quota = vec![
            QuotaWindow {
                provider: "claude".into(),
                id: "five_hour".into(),
                label: "session".into(),
                ..Default::default()
            },
            QuotaWindow {
                provider: "copilot".into(),
                id: "premium_interactions".into(),
                label: "premium interactions".into(),
                pct_used: 100.0,
                ..Default::default()
            },
        ];
        let mut overrides = Overrides::default();
        overrides.set(Setting::ParkedMinutes, 3);
        let mut app = App::new(Settings::default(), overrides);
        assert_eq!(app.thresholds.parked_secs, 180, "the flag is in effect");
        app.apply(Action::Options, &s);
        // Down to runners, and hide it.
        app.apply(Action::Down, &s);
        app.apply(Action::Change { up: false }, &s);
        assert!(!app.settings.shown(Panel::Runners));
        // Roster keys mean nothing here.
        app.apply(Action::Filter, &s);
        assert_eq!(app.filter, Filter::All);
        // Usage: the last row is copilot's premium window.
        app.apply(Action::Category { next: true }, &s);
        app.apply(Action::Bottom, &s);
        app.apply(Action::Change { up: true }, &s);
        assert!(app.settings.usage_hidden("copilot", "premium_interactions"));
        // A new read drops claude: the selection stays on the same window.
        s.quota.remove(0);
        app.apply(Action::Up, &s);
        let items = app.menu_items(&s);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(items[menu.index(&items).unwrap()].label, "copilot");
        // Thresholds: stale steps up, the pinned parked row does not change.
        app.apply(Action::Category { next: true }, &s);
        app.apply(Action::Change { up: true }, &s);
        assert_eq!(app.settings.stale_minutes, 45);
        assert_eq!(app.thresholds.stale_secs, 45 * 60, "applied at once");
        app.apply(Action::Down, &s);
        app.apply(Action::Change { up: true }, &s);
        assert_eq!(app.settings.parked_minutes, 10);
        assert_eq!(app.thresholds.parked_secs, 180);
        // Back around to panels, then close.
        app.apply(Action::Category { next: false }, &s);
        app.apply(Action::Category { next: false }, &s);
        assert_eq!(app.menu.as_ref().unwrap().category, Category::Panels);
        app.apply(Action::Options, &s);
        assert!(app.menu.is_none());
    }
}
