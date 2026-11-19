//! UI state and key handling. Pure: no I/O, so it is unit-testable.

use crate::model::{Pipeline, PrChecks, Snapshot, Worker, WorkerState, derive_state, stale_reason};
use std::collections::BTreeSet;

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
    pub stale_secs: i64,
}

impl App {
    pub fn new(stale_secs: i64) -> Self {
        App {
            selected: None,
            expanded: BTreeSet::new(),
            filter: Filter::All,
            help: false,
            quit: false,
            paused: false,
            stale_secs,
        }
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
    pub stale: Option<String>,
}

impl Row<'_> {
    pub fn pr_url(&self) -> Option<&str> {
        self.worker
            .pr_url
            .as_deref()
            .or_else(|| self.pipeline.and_then(|p| p.pr_url.as_deref()))
    }
}

/// Every worker with its derived state, most urgent first.
pub fn all_rows<'a>(snap: &'a Snapshot, stale_secs: i64) -> Vec<Row<'a>> {
    let Some(fleet) = &snap.fleet else {
        return Vec::new();
    };
    let mut rows: Vec<Row> = fleet
        .workers
        .iter()
        .map(|w| {
            let pipeline = snap.pipelines.get(&w.id);
            let state = derive_state(w, pipeline, snap.now, stale_secs);
            let pr_url = w
                .pr_url
                .as_deref()
                .or_else(|| pipeline.and_then(|p| p.pr_url.as_deref()));
            Row {
                worker: w,
                state,
                pipeline,
                pr: pr_url.and_then(|u| snap.prs.get(u)),
                stale: if state == WorkerState::Stale {
                    stale_reason(w, pipeline, snap.now, stale_secs)
                } else {
                    None
                },
            }
        })
        .collect();
    rows.sort_by_key(|r| r.state);
    rows
}

pub fn visible_rows<'a>(snap: &'a Snapshot, app: &App) -> Vec<Row<'a>> {
    all_rows(snap, app.stale_secs)
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
        let ids: Vec<&str> = all_rows(&s, 1800)
            .iter()
            .map(|r| r.worker.id.as_str())
            .collect();
        assert_eq!(ids, ["asks", "b-work", "a-done", "idle"]);
    }

    #[test]
    fn navigation_filter_and_expand() {
        let s = snap();
        let mut app = App::new(1800);
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
