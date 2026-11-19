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
    pub age_secs: Option<i64>,
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
}

impl Pipeline {
    /// The run is still moving on its own.
    pub fn is_live(&self) -> bool {
        matches!(self.status.as_str(), "running" | "fixing" | "pending")
    }

    pub fn is_parked(&self) -> bool {
        self.status.starts_with("awaiting")
            || self.status == "fix_review"
            || self.steps.iter().any(|s| s.status == StepStatus::Awaiting)
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
    pub label: String,
    pub pct_remaining: f64,
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

/// Decide a row's state from firstmate's state, its pipeline, and time.
pub fn derive_state(w: &Worker, pipe: Option<&Pipeline>, now: i64, stale_secs: i64) -> WorkerState {
    if !w.open_decisions.is_empty() {
        return WorkerState::Captain;
    }
    if w.blocked_event || w.fm_state == "blocked" {
        return WorkerState::Blocked;
    }
    match w.fm_state.as_str() {
        "failed" => return WorkerState::Failed,
        "parked" => return WorkerState::Gate,
        "done" => return WorkerState::Done,
        "no_active_work" | "idle" => return WorkerState::Idle,
        _ => {}
    }
    if pipe.is_some_and(|p| p.status == "failed") {
        return WorkerState::Failed;
    }
    if let Some(p) = pipe
        && p.is_parked()
    {
        return WorkerState::Gate;
    }
    let active = matches!(w.fm_state.as_str(), "working" | "paused");
    if !active {
        return WorkerState::Unknown;
    }
    if stale_reason(w, pipe, now, stale_secs).is_some() {
        return WorkerState::Stale;
    }
    if w.fm_state == "paused" {
        WorkerState::Paused
    } else {
        WorkerState::Working
    }
}

/// Why an active worker looks stuck, if it does.
///
/// A worker is alive when firstmate sees its harness busy, when its
/// no-mistakes run is moving, or when it declared a wait that has not passed.
/// Otherwise it is stale once its last status line is older than the threshold
/// or its declared wait passed more than five minutes ago.
pub fn stale_reason(
    w: &Worker,
    pipe: Option<&Pipeline>,
    now: i64,
    stale_secs: i64,
) -> Option<String> {
    if w.source == "pane" && w.detail.contains("busy") {
        return None;
    }
    if pipe.is_some_and(Pipeline::is_live) {
        return None;
    }
    if let Some(until) = w.wait_until {
        if now < until + 300 {
            return None;
        }
        return Some(format!(
            "declared wait ended {} ago",
            crate::timefmt::dur(now - until)
        ));
    }
    let age = w.last_event.as_ref().and_then(|e| e.age_secs)?;
    if age > stale_secs {
        Some(format!(
            "no status line or pipeline activity for {}",
            crate::timefmt::dur(age)
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(state: &str) -> Worker {
        Worker {
            id: "t".into(),
            fm_state: state.into(),
            source: "status-log".into(),
            last_event: Some(LastEvent {
                state: "working".into(),
                note: String::new(),
                age_secs: Some(60),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn captain_beats_everything() {
        let mut w = worker("working");
        w.open_decisions = vec!["board-review".into()];
        assert_eq!(derive_state(&w, None, 0, 1800), WorkerState::Captain);
    }

    #[test]
    fn stale_after_threshold_without_pipeline() {
        let mut w = worker("working");
        assert_eq!(derive_state(&w, None, 0, 1800), WorkerState::Working);
        w.last_event.as_mut().unwrap().age_secs = Some(2900);
        assert_eq!(derive_state(&w, None, 0, 1800), WorkerState::Stale);
        let live = Pipeline {
            status: "running".into(),
            ..Default::default()
        };
        assert_eq!(derive_state(&w, Some(&live), 0, 1800), WorkerState::Working);
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
                derive_state(&w, Some(&failed), 0, 1800),
                WorkerState::Failed
            );
            w.last_event.as_mut().unwrap().age_secs = Some(5000);
            assert_eq!(
                derive_state(&w, Some(&failed), 0, 1800),
                WorkerState::Failed
            );
        }
        // A cancelled run was stopped on purpose, not a failure.
        let cancelled = Pipeline {
            status: "cancelled".into(),
            ..Default::default()
        };
        assert_eq!(
            derive_state(&worker("working"), Some(&cancelled), 0, 1800),
            WorkerState::Working
        );
    }

    #[test]
    fn declared_waits() {
        let mut w = worker("paused");
        w.last_event.as_mut().unwrap().age_secs = Some(5000);
        w.wait_until = Some(1000);
        assert_eq!(derive_state(&w, None, 900, 1800), WorkerState::Paused);
        assert_eq!(derive_state(&w, None, 1200, 1800), WorkerState::Paused);
        assert_eq!(derive_state(&w, None, 1400, 1800), WorkerState::Stale);
    }

    #[test]
    fn busy_pane_is_alive() {
        let mut w = worker("working");
        w.source = "pane".into();
        w.detail = "harness busy (claude-hook)".into();
        w.last_event.as_mut().unwrap().age_secs = Some(99_999);
        assert_eq!(derive_state(&w, None, 0, 1800), WorkerState::Working);
    }

    #[test]
    fn simple_mappings() {
        assert_eq!(derive_state(&worker("done"), None, 0, 1), WorkerState::Done);
        assert_eq!(
            derive_state(&worker("parked"), None, 0, 1),
            WorkerState::Gate
        );
        assert_eq!(
            derive_state(&worker("blocked"), None, 0, 1),
            WorkerState::Blocked
        );
        assert_eq!(
            derive_state(&worker("no_active_work"), None, 0, 1),
            WorkerState::Idle
        );
        assert_eq!(
            derive_state(&worker("unknown"), None, 0, 1),
            WorkerState::Unknown
        );
    }
}
