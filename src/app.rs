//! UI state and key handling. Pure: no I/O, so it is unit-testable.

use crate::model::{
    CheckState, Pipeline, PrChecks, Snapshot, Stale, Thresholds, Worker, WorkerState, classify,
};
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
}

#[derive(Debug, Clone)]
pub struct App {
    pub selected: Option<String>,
    pub expanded: BTreeSet<String>,
    pub filter: Filter,
    pub help: bool,
    pub quit: bool,
    pub paused: bool,
    pub thresholds: Thresholds,
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

impl App {
    pub fn new(thresholds: Thresholds) -> Self {
        App {
            selected: None,
            expanded: BTreeSet::new(),
            filter: Filter::All,
            help: false,
            quit: false,
            paused: false,
            thresholds,
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

    /// Apply a key action against the rows currently on screen. Returns true
    /// when the action asks the collector to refresh.
    pub fn apply(&mut self, action: Action, rows: &[Row]) -> bool {
        if self.help && !matches!(action, Action::Quit) {
            self.help = false;
            return false;
        }
        let idx = self.selected_index(rows);
        let pick = |i: usize| rows.get(i).map(|r| r.worker.id.clone());
        match action {
            Action::Up => self.selected = pick(idx.map_or(0, |i| i.saturating_sub(1))),
            Action::Down => {
                let last = rows.len().saturating_sub(1);
                self.selected = pick(idx.map_or(0, |i| (i + 1).min(last)));
            }
            Action::Top => self.selected = pick(0),
            Action::Bottom => self.selected = pick(rows.len().saturating_sub(1)),
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
            Action::Quit => self.quit = true,
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
    use crate::model::Fleet;

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
        let mut app = App::new(Thresholds::new(1800));
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
        let mut app = App::new(Thresholds::new(1800));
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
        let mut app = App::new(Thresholds::new(1800));
        let rows = visible_rows(&s, &app);
        app.apply(Action::Down, &rows);
        assert_eq!(app.selected.as_deref(), Some("b-work"));
        app.apply(Action::Toggle, &rows);
        assert!(app.expanded.contains("b-work"));
        app.apply(Action::Toggle, &rows);
        assert!(!app.expanded.contains("b-work"));
        app.apply(Action::Bottom, &rows);
        assert_eq!(app.selected.as_deref(), Some("idle"));
        app.apply(Action::Filter, &rows);
        assert_eq!(visible_rows(&s, &app).len(), 2);
        app.apply(Action::Filter, &rows);
        assert_eq!(visible_rows(&s, &app).len(), 1);
        assert!(app.apply(Action::Refresh, &rows));
        app.apply(Action::Help, &rows);
        assert!(app.help);
        app.apply(Action::Down, &rows);
        assert!(!app.help, "any key closes help");
        app.apply(Action::Quit, &rows);
        assert!(app.quit);
    }
}
