//! Crowsnest's own view of the fleet. Sources fill these types; the UI only
//! reads them, so a different collector can replace the sources without
//! touching the UI.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What a row needs from the reader, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// The worker has an open decision for the captain.
    Captain,
    Blocked,
    Failed,
    /// Working or paused with no sign of life past the stale threshold.
    Stale,
    /// The worker's no-mistakes run is parked at a gate.
    Gate,
    Working,
    Paused,
    Done,
    Unknown,
    Idle,
}

impl WorkerState {
    pub fn word(self) -> &'static str {
        match self {
            WorkerState::Captain => "CAPTAIN",
            WorkerState::Blocked => "BLOCKED",
            WorkerState::Failed => "FAILED",
            WorkerState::Stale => "STALE",
            WorkerState::Gate => "GATE",
            WorkerState::Working => "WORKING",
            WorkerState::Paused => "PAUSED",
            WorkerState::Done => "DONE",
            WorkerState::Unknown => "UNKNOWN",
            WorkerState::Idle => "IDLE",
        }
    }

    /// Rows that need nobody: hidden by the "active" filter.
    pub fn is_quiet(self) -> bool {
        matches!(
            self,
            WorkerState::Done | WorkerState::Idle | WorkerState::Unknown
        )
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LastEvent {
    pub state: String,
    pub note: String,
    /// Age when the fleet snapshot was taken.
    pub age_secs: Option<i64>,
    /// Unix time of the event, when the snapshot's own time is known. The
    /// age on screen is measured from this, so it keeps counting between
    /// snapshots instead of freezing at the last read.
    pub at: Option<i64>,
}

impl LastEvent {
    /// Seconds since the event, as of `now`.
    pub fn age(&self, now: i64) -> Option<i64> {
        match self.at {
            Some(at) => Some((now - at).max(0)),
            None => self.age_secs,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Worker {
    pub id: String,
    /// `ship`, `scout`, `secondmate`, or a child's kind.
    pub kind: String,
    pub project: String,
    pub harness: String,
    pub mode: String,
    pub title: Option<String>,
    /// Firstmate's current state word (`working`, `parked`, `done`, ...).
    pub fm_state: String,
    /// Where firstmate observed it (`run-step`, `pane`, `status-log`, ...).
    pub source: String,
    pub detail: String,
    pub branch: Option<String>,
    pub worktree: Option<PathBuf>,
    pub pr_url: Option<String>,
    pub open_decisions: Vec<String>,
    pub blocked_event: bool,
    pub last_event: Option<LastEvent>,
    /// `until <time>` declared by the worker's latest status line.
    pub wait_until: Option<i64>,
    /// The secondmate this row belongs to, for a secondmate's child.
    pub parent: Option<String>,
    pub endpoint_exists: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HeldCall {
    pub id: String,
    pub title: String,
    pub age_days: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Fleet {
    pub generated: Option<i64>,
    pub workers: Vec<Worker>,
    /// Backlog rows waiting on the captain (`captain_actionable`).
    pub held: Vec<HeldCall>,
    /// The first queued backlog row that is not a captain call.
    pub next: Option<HeldCall>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Completed,
    Running,
    Fixing,
    Pending,
    Skipped,
    Failed,
    /// Parked at an approval gate.
    Awaiting,
    Other,
}

impl StepStatus {
    pub fn parse(s: &str) -> StepStatus {
        match s {
            "completed" | "passed" | "success" => StepStatus::Completed,
            "running" => StepStatus::Running,
            "fixing" => StepStatus::Fixing,
            "pending" => StepStatus::Pending,
            "skipped" => StepStatus::Skipped,
            "failed" | "error" | "cancelled" => StepStatus::Failed,
            s if s.starts_with("awaiting") || s == "fix_review" || s == "parked" => {
                StepStatus::Awaiting
            }
            _ => StepStatus::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub name: String,
    pub status: StepStatus,
    pub findings: u32,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActiveStep {
    pub step: String,
    pub status: String,
    pub active_secs: Option<i64>,
    pub last_activity: String,
    /// Seconds since the step last reported activity, when known.
    pub last_activity_secs: Option<i64>,
    pub round: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Pipeline {
    pub run_id: String,
    pub branch: String,
    /// Run status (`running`, `completed`, `failed`, `cancelled`, ...).
    pub status: String,
    pub head: String,
    pub pr_url: Option<String>,
    /// `none`, `1 awaiting`, `2 auto-fix`, ...
    pub findings: String,
    pub steps: Vec<Step>,
    pub active: Option<ActiveStep>,
    pub outcome: Option<String>,
    pub error: Option<String>,
    /// The gate step the run is parked at, when no-mistakes names it.
    pub gate: Option<String>,
    /// How long the run had been parked waiting on the agent when it was read
    /// (`awaiting_agent: parked 1h17m`).
    pub parked_secs: Option<i64>,
    /// Unix time this status was read. Durations in it count on from here.
    pub read_at: Option<i64>,
}

impl Pipeline {
    /// The run is still moving on its own. no-mistakes leaves a run's status
    /// at `running` while it waits at a gate, so a parked run is not live.
    pub fn is_live(&self) -> bool {
        matches!(self.status.as_str(), "running" | "fixing" | "pending") && !self.is_parked()
    }

    pub fn is_parked(&self) -> bool {
        self.parked_secs.is_some()
            || self.status.starts_with("awaiting")
            || self.status == "fix_review"
            || self.steps.iter().any(|s| s.status == StepStatus::Awaiting)
    }

    /// Seconds between reading this status and `now`.
    fn since_read(&self, now: i64) -> i64 {
        self.read_at.map_or(0, |at| (now - at).max(0))
    }

    /// The name of the gate the run is parked at.
    pub fn gate_step(&self) -> Option<&str> {
        self.gate.as_deref().filter(|g| !g.is_empty()).or_else(|| {
            self.steps
                .iter()
                .find(|s| s.status == StepStatus::Awaiting)
                .map(|s| s.name.as_str())
        })
    }

    /// How long the run has been parked waiting on the agent, as of `now`.
    pub fn parked_for(&self, now: i64) -> Option<i64> {
        self.parked_secs.map(|s| s + self.since_read(now))
    }

    /// How long the active step has been running, as of `now`.
    pub fn active_for(&self, now: i64) -> Option<i64> {
        let a = self.active.as_ref()?;
        a.active_secs.map(|s| s + self.since_read(now))
    }

    /// How long ago the active step last reported activity, as of `now`.
    pub fn quiet_for(&self, now: i64) -> Option<i64> {
        let a = self.active.as_ref()?;
        a.last_activity_secs.map(|s| s + self.since_read(now))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pass,
    Fail,
    Running,
    Queued,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    pub started_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PrChecks {
    pub url: String,
    pub number: u64,
    pub draft: bool,
    pub state: String,
    pub checks: Vec<Check>,
}

impl PrChecks {
    pub fn count(&self, state: CheckState) -> usize {
        self.checks.iter().filter(|c| c.state == state).count()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Runner {
    pub name: String,
    pub dir: PathBuf,
    /// `owner/repo` when the runner is registered to one repository.
    pub repo: Option<String>,
    pub busy: bool,
    pub busy_secs: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Job {
    pub repo: String,
    pub name: String,
    /// `queued` or `in_progress`.
    pub status: String,
    pub runner: Option<String>,
    pub started_at: Option<i64>,
    pub branch: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Disk {
    pub mount: String,
    pub used_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Host {
    pub name: String,
    pub load: [f64; 3],
    pub cpus: usize,
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub disks: Vec<Disk>,
    /// Linux pressure stall `some avg10` for memory and cpu, in percent.
    pub psi_mem: Option<f64>,
    pub psi_cpu: Option<f64>,
}

impl Host {
    pub fn mem_used_pct(&self) -> f64 {
        pct(
            self.mem_total.saturating_sub(self.mem_available),
            self.mem_total,
        )
    }
    pub fn swap_used_pct(&self) -> f64 {
        pct(
            self.swap_total.saturating_sub(self.swap_free),
            self.swap_total,
        )
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct QuotaWindow {
    pub provider: String,
    /// quota-axi's stable window id (`five_hour`, `premium_interactions`).
    pub id: String,
    pub label: String,
    pub pct_used: f64,
    pub resets_at: Option<i64>,
    pub pace: Option<String>,
}

/// How fresh one source is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Freshness {
    /// Unix time of the last successful read.
    pub ok_at: Option<i64>,
    pub error: Option<String>,
}

/// Everything the UI shows, as of `now`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub now: i64,
    pub fleet: Option<Fleet>,
    pub pipelines: BTreeMap<String, Pipeline>,
    pub prs: BTreeMap<String, PrChecks>,
    pub runners: Vec<Runner>,
    pub jobs: Vec<Job>,
    pub host: Option<Host>,
    pub quota: Vec<QuotaWindow>,
    pub fresh: BTreeMap<String, Freshness>,
}

/// The limits that turn a quiet row into one that needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// A working task with no status line or pipeline activity this long is stale.
    pub stale_secs: i64,
    /// A pipeline gate parked on the agent this long needs attention.
    pub parked_secs: i64,
    /// How long after a declared wait ends before the worker is flagged.
    pub wait_grace_secs: i64,
}

impl Thresholds {
    pub const DEFAULT_PARKED_SECS: i64 = 10 * 60;
    pub const DEFAULT_WAIT_GRACE_SECS: i64 = 5 * 60;

    pub fn new(stale_secs: i64) -> Self {
        Thresholds {
            stale_secs,
            parked_secs: Self::DEFAULT_PARKED_SECS,
            wait_grace_secs: Self::DEFAULT_WAIT_GRACE_SECS,
        }
    }
}

/// Why a row needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleKind {
    /// No sign of life past the stale threshold.
    Quiet,
    /// A pipeline gate has waited on the worker past the parked threshold.
    Parked,
    /// The worker's declared wait ended and nothing followed.
    WaitPassed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub kind: StaleKind,
    pub reason: String,
}

/// Decide a row's state from firstmate's state, its pipeline, and time.
pub fn derive_state(w: &Worker, pipe: Option<&Pipeline>, now: i64, th: &Thresholds) -> WorkerState {
    classify(w, pipe, now, th).0
}

/// A row's state, and why it needs attention when that state is `Stale`.
pub fn classify(
    w: &Worker,
    pipe: Option<&Pipeline>,
    now: i64,
    th: &Thresholds,
) -> (WorkerState, Option<Stale>) {
    // A parked gate stays a quiet GATE row until it has waited too long, or
    // its worker's declared wait has passed.
    let gate = || match stale_for(w, pipe, now, th, false) {
        Some(s) => (WorkerState::Stale, Some(s)),
        None => (WorkerState::Gate, None),
    };
    if !w.open_decisions.is_empty() {
        return (WorkerState::Captain, None);
    }
    if w.blocked_event || w.fm_state == "blocked" {
        return (WorkerState::Blocked, None);
    }
    match w.fm_state.as_str() {
        "failed" => return (WorkerState::Failed, None),
        "parked" => return gate(),
        "done" => return (WorkerState::Done, None),
        "no_active_work" | "idle" => return (WorkerState::Idle, None),
        _ => {}
    }
    if pipe.is_some_and(|p| p.status == "failed") {
        return (WorkerState::Failed, None);
    }
    if pipe.is_some_and(Pipeline::is_parked) {
        return gate();
    }
    let active = matches!(w.fm_state.as_str(), "working" | "paused");
    if !active {
        return (WorkerState::Unknown, None);
    }
    if let Some(s) = stale_for(w, pipe, now, th, true) {
        return (WorkerState::Stale, Some(s));
    }
    if w.fm_state == "paused" {
        (WorkerState::Paused, None)
    } else {
        (WorkerState::Working, None)
    }
}

/// Why a worker looks stuck, if it does.
///
/// A run parked at a gate waiting on the worker needs attention once it has
/// waited past the parked threshold, whatever else the worker looks like: the
/// gate is waiting for an answer whether or not the harness is busy.
/// Otherwise a worker is alive when firstmate sees its harness busy, or when
/// its no-mistakes run is moving. A live run keeps the worker alive even past
/// its declared wait, since the pipeline is the work. Otherwise a worker is
/// stuck when its declared wait passed more than the grace period ago, or,
/// with `quiet` and no declared wait, when its last status line is older than
/// the stale threshold.
pub fn stale_for(
    w: &Worker,
    pipe: Option<&Pipeline>,
    now: i64,
    th: &Thresholds,
    quiet: bool,
) -> Option<Stale> {
    use crate::timefmt::dur;
    if let Some(p) = pipe
        && let Some(parked) = p.parked_for(now)
        && parked >= th.parked_secs
    {
        let gate = p
            .gate_step()
            .map_or("gate".to_string(), |g| format!("{g} gate"));
        return Some(Stale {
            kind: StaleKind::Parked,
            reason: format!(
                "parked at {gate} for {}, waiting on the worker",
                dur(parked)
            ),
        });
    }
    if w.source == "pane" && w.detail.contains("busy") {
        return None;
    }
    if pipe.is_some_and(Pipeline::is_live) {
        return None;
    }
    if let Some(until) = w.wait_until {
        if now < until + th.wait_grace_secs {
            return None;
        }
        return Some(Stale {
            kind: StaleKind::WaitPassed,
            reason: format!("declared wait ended {} ago", dur(now - until)),
        });
    }
    if !quiet {
        return None;
    }
    let age = w.last_event.as_ref().and_then(|e| e.age(now))?;
    (age > th.stale_secs).then(|| Stale {
        kind: StaleKind::Quiet,
        reason: format!("no status line or pipeline activity for {}", dur(age)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th() -> Thresholds {
        Thresholds::new(1800)
    }

    fn worker(state: &str) -> Worker {
        Worker {
            id: "t".into(),
            fm_state: state.into(),
            source: "status-log".into(),
            last_event: Some(LastEvent {
                state: "working".into(),
                note: String::new(),
                age_secs: Some(60),
                at: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn captain_beats_everything() {
        let mut w = worker("working");
        w.open_decisions = vec!["board-review".into()];
        assert_eq!(derive_state(&w, None, 0, &th()), WorkerState::Captain);
    }

    #[test]
    fn stale_after_threshold_without_pipeline() {
        let mut w = worker("working");
        assert_eq!(derive_state(&w, None, 0, &th()), WorkerState::Working);
        w.last_event.as_mut().unwrap().age_secs = Some(2900);
        assert_eq!(derive_state(&w, None, 0, &th()), WorkerState::Stale);
        let live = Pipeline {
            status: "running".into(),
            ..Default::default()
        };
        assert_eq!(
            derive_state(&w, Some(&live), 0, &th()),
            WorkerState::Working
        );
    }

    #[test]
    fn failed_pipeline_is_failed() {
        let failed = Pipeline {
            status: "failed".into(),
            ..Default::default()
        };
        for state in ["working", "paused"] {
            let mut w = worker(state);
            assert_eq!(
                derive_state(&w, Some(&failed), 0, &th()),
                WorkerState::Failed
            );
            w.last_event.as_mut().unwrap().age_secs = Some(5000);
            assert_eq!(
                derive_state(&w, Some(&failed), 0, &th()),
                WorkerState::Failed
            );
        }
        // A cancelled run was stopped on purpose, not a failure.
        let cancelled = Pipeline {
            status: "cancelled".into(),
            ..Default::default()
        };
        assert_eq!(
            derive_state(&worker("working"), Some(&cancelled), 0, &th()),
            WorkerState::Working
        );
    }

    #[test]
    fn declared_waits() {
        let mut w = worker("paused");
        w.last_event.as_mut().unwrap().age_secs = Some(5000);
        w.wait_until = Some(1000);
        assert_eq!(derive_state(&w, None, 900, &th()), WorkerState::Paused);
        assert_eq!(derive_state(&w, None, 1200, &th()), WorkerState::Paused);
        assert_eq!(derive_state(&w, None, 1400, &th()), WorkerState::Stale);
    }

    #[test]
    fn busy_pane_is_alive() {
        let mut w = worker("working");
        w.source = "pane".into();
        w.detail = "harness busy (claude-hook)".into();
        w.last_event.as_mut().unwrap().age_secs = Some(99_999);
        assert_eq!(derive_state(&w, None, 0, &th()), WorkerState::Working);
    }

    #[test]
    fn simple_mappings() {
        assert_eq!(
            derive_state(&worker("done"), None, 0, &Thresholds::new(1)),
            WorkerState::Done
        );
        assert_eq!(
            derive_state(&worker("parked"), None, 0, &Thresholds::new(1)),
            WorkerState::Gate
        );
        assert_eq!(
            derive_state(&worker("blocked"), None, 0, &Thresholds::new(1)),
            WorkerState::Blocked
        );
        assert_eq!(
            derive_state(&worker("no_active_work"), None, 0, &Thresholds::new(1)),
            WorkerState::Idle
        );
        assert_eq!(
            derive_state(&worker("unknown"), None, 0, &Thresholds::new(1)),
            WorkerState::Unknown
        );
    }

    /// A run no-mistakes reports as `running` while it waits at its review gate.
    fn parked(secs: i64) -> Pipeline {
        Pipeline {
            status: "running".into(),
            gate: Some("review".into()),
            parked_secs: Some(secs),
            read_at: Some(1000),
            steps: vec![Step {
                name: "review".into(),
                status: StepStatus::Awaiting,
                findings: 1,
                duration_ms: 0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_parked_run_is_not_live() {
        assert!(!parked(5).is_live());
        assert!(parked(5).is_parked());
    }

    #[test]
    fn a_gate_parked_past_the_threshold_needs_attention() {
        let w = worker("working");
        let t = Thresholds::new(1800);
        // Under the threshold it stays a quiet GATE row.
        let (state, why) = classify(&w, Some(&parked(120)), 1000, &t);
        assert_eq!((state, why), (WorkerState::Gate, None));
        // Parked time keeps counting after the read: 120s + 500s is past 10m.
        let (state, why) = classify(&w, Some(&parked(120)), 1500, &t);
        assert_eq!(state, WorkerState::Stale);
        let why = why.unwrap();
        assert_eq!(why.kind, StaleKind::Parked);
        assert_eq!(
            why.reason,
            "parked at review gate for 10m, waiting on the worker"
        );
        // The threshold is configurable.
        let patient = Thresholds {
            parked_secs: 3600,
            ..t
        };
        assert_eq!(
            derive_state(&w, Some(&parked(4000 - 1000)), 1000, &patient),
            WorkerState::Gate
        );
        assert_eq!(
            derive_state(&w, Some(&parked(4600 - 1000)), 1000 + 1000, &patient),
            WorkerState::Stale
        );
    }

    #[test]
    fn firstmate_parking_the_worker_does_not_hide_a_stalled_gate() {
        let w = worker("parked");
        assert_eq!(
            derive_state(&w, Some(&parked(4620)), 1000, &th()),
            WorkerState::Stale
        );
        // Parked with no pipeline reading is the worker waiting on someone else.
        assert_eq!(derive_state(&w, None, 1000, &th()), WorkerState::Gate);
    }

    #[test]
    fn a_busy_pane_does_not_hide_a_stalled_gate() {
        let mut w = worker("working");
        w.source = "pane".into();
        w.detail = "harness busy (claude-hook)".into();
        assert_eq!(
            derive_state(&w, Some(&parked(4620)), 1000, &th()),
            WorkerState::Stale
        );
        // Under the threshold it is just a gate, and with no gate it is working.
        assert_eq!(
            derive_state(&w, Some(&parked(60)), 1000, &th()),
            WorkerState::Gate
        );
        assert_eq!(derive_state(&w, None, 0, &th()), WorkerState::Working);
    }

    #[test]
    fn a_live_pipeline_keeps_a_worker_alive_past_its_declared_wait() {
        let mut w = worker("working");
        w.wait_until = Some(1000);
        let live = Pipeline {
            status: "running".into(),
            ..Default::default()
        };
        // Twelve minutes past the wait, but the pipeline is still moving.
        assert_eq!(
            derive_state(&w, Some(&live), 1000 + 720, &th()),
            WorkerState::Working
        );
        // Once it stops moving, the passed wait counts.
        assert_eq!(
            derive_state(&w, None, 1000 + 720, &th()),
            WorkerState::Stale
        );
    }

    #[test]
    fn a_passed_declared_wait_is_flagged_on_a_gate_row_too() {
        let mut w = worker("parked");
        w.wait_until = Some(1000);
        let t = Thresholds::new(1800);
        assert_eq!(
            derive_state(&w, Some(&parked(30)), 1200, &t),
            WorkerState::Gate
        );
        let (state, why) = classify(&w, Some(&parked(30)), 1000 + 301, &t);
        assert_eq!(state, WorkerState::Stale);
        assert_eq!(why.unwrap().kind, StaleKind::WaitPassed);
        // Without a gate the same wait flags a paused worker.
        let mut p = worker("paused");
        p.wait_until = Some(1000);
        let (state, why) = classify(&p, None, 1000 + 301, &t);
        assert_eq!(state, WorkerState::Stale);
        assert_eq!(why.unwrap().reason, "declared wait ended 5m ago");
        let lenient = Thresholds {
            wait_grace_secs: 3600,
            ..t
        };
        assert_eq!(
            derive_state(&p, None, 1000 + 301, &lenient),
            WorkerState::Paused
        );
    }

    #[test]
    fn event_age_keeps_counting_after_the_snapshot() {
        let e = LastEvent {
            age_secs: Some(60),
            at: Some(940),
            ..Default::default()
        };
        assert_eq!(e.age(1000), Some(60));
        assert_eq!(e.age(1100), Some(160));
        let legacy = LastEvent {
            age_secs: Some(60),
            ..Default::default()
        };
        assert_eq!(legacy.age(5000), Some(60));
    }
}
