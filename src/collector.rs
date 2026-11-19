//! Background refresh. Each source runs on its own thread at its own cadence
//! and writes into one shared [`Snapshot`]; the UI only reads it.
//!
//! | tier     | default cadence                                              |
//! |----------|--------------------------------------------------------------|
//! | host     | 2 s                                                          |
//! | runners  | 5 s (process scan), directories re-listed every 60 s         |
//! | pipelines| 15 s per live no-mistakes task                               |
//! | fleet    | 60 s, or 20 s after firstmate's fleet ledger changes         |
//! | github   | 60 s while a PR or runner is active, otherwise 5 min         |
//! | quota    | 5 min                                                        |
//!
//! A tier never overlaps itself, keeps its last good data on failure,
//! records the failure so the UI can show it, and backs off while its reads
//! keep failing outright (doubling its interval, up to 5 min). A read that
//! succeeded with errors for some items is shown as an error but keeps the
//! normal cadence.

use crate::model::{Fleet, Freshness, Job, PrChecks, Snapshot};
use crate::sources::{self, host::HostReader, runners};
use crate::timefmt::now_epoch;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Config {
    pub fm_home: PathBuf,
    /// Crowsnest's own cache directory (never inside the firstmate home).
    pub cache_dir: PathBuf,
    pub runner_roots: Vec<PathBuf>,
    pub disk_paths: Vec<PathBuf>,
    pub fleet_interval: Duration,
    pub fleet_min_gap: Duration,
    pub pipeline_interval: Duration,
    pub github: bool,
    pub quota: bool,
    pub quota_providers: Vec<String>,
}

impl Config {
    /// Firstmate's documented, opt-in activity ledger. Its modification time
    /// is a cheap "something happened" signal when a home has it enabled.
    fn ledger(&self) -> PathBuf {
        self.fm_home.join("state").join("fleet-ledger.jsonl")
    }
}

pub type Shared = Arc<Mutex<Snapshot>>;

/// Handle the UI uses to steer refreshing (never firstmate).
pub struct Handle {
    wakers: Vec<Sender<()>>,
    paused: Arc<AtomicBool>,
}

impl Handle {
    /// Refresh every tier now.
    pub fn refresh(&self) {
        for w in &self.wakers {
            let _ = w.send(());
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// What one attempt of a tier did.
#[derive(Debug, Clone, PartialEq)]
enum Read {
    /// Not due; nothing was read.
    Skipped,
    Ok,
    /// Read, but some items failed. Shown as an error, never backed off.
    Partial(String),
    /// Nothing usable was read.
    Failed(String),
}

impl Read {
    /// `Failed` when any outright failure happened, else `Partial` when some
    /// items failed, else `Ok`.
    fn from_errors(failed: Vec<String>, partial: Vec<String>) -> Read {
        if !failed.is_empty() {
            Read::Failed(
                failed
                    .into_iter()
                    .chain(partial)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        } else if !partial.is_empty() {
            Read::Partial(partial.join("; "))
        } else {
            Read::Ok
        }
    }

    fn record(self, f: &mut Freshness) {
        match self {
            Read::Skipped => {}
            Read::Ok => {
                f.ok_at = Some(now_epoch());
                f.error = None;
            }
            Read::Partial(e) => {
                f.ok_at = Some(now_epoch());
                f.error = Some(e);
            }
            Read::Failed(e) => f.error = Some(e),
        }
    }

    /// Consecutive outright failures after this attempt.
    fn failures(&self, before: u32) -> u32 {
        match self {
            Read::Skipped => before,
            Read::Failed(_) => before + 1,
            Read::Ok | Read::Partial(_) => 0,
        }
    }
}

/// Store a gh read in `s`, keeping the last good data of a part that failed.
fn apply_gh(
    s: &mut Snapshot,
    prs: Result<(Vec<PrChecks>, Vec<String>), String>,
    jobs: Result<Vec<Job>, String>,
) -> Read {
    let (mut failed, mut partial) = (Vec::new(), Vec::new());
    match prs {
        Ok((prs, errors)) => {
            s.prs = prs.into_iter().map(|p| (p.url.clone(), p)).collect();
            partial = errors;
        }
        Err(e) => failed.push(e),
    }
    match jobs {
        Ok(j) => s.jobs = j,
        Err(e) => failed.push(e),
    }
    Read::from_errors(failed, partial)
}

/// Longest wait between attempts of a failing tier.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// `interval`, doubled for each consecutive failure, up to [`MAX_BACKOFF`]
/// (or `interval` itself when that is longer).
fn backoff(interval: Duration, failures: u32) -> Duration {
    let cap = interval.max(MAX_BACKOFF);
    interval.saturating_mul(1 << failures.min(16)).min(cap)
}

/// Run `work` every `interval()`, or at once when woken. Paused tiers only
/// run when woken explicitly. Each read is recorded and announced on
/// `changed`; while reads keep failing outright, the interval backs off.
fn spawn_tier<F, I>(
    tier: &'static str,
    shared: Shared,
    changed: Sender<()>,
    rx: Receiver<()>,
    paused: Arc<AtomicBool>,
    interval: I,
    mut work: F,
) where
    F: FnMut(bool) -> Read + Send + 'static,
    I: Fn() -> Duration + Send + 'static,
{
    thread::spawn(move || {
        let mut forced = true;
        let mut last = Instant::now();
        let mut failures = 0;
        loop {
            let due = last.elapsed() >= backoff(interval(), failures);
            if forced || (!paused.load(Ordering::Relaxed) && due) {
                let read = work(forced);
                if read != Read::Skipped {
                    failures = read.failures(failures);
                    read.record(shared.lock().unwrap().fresh.entry(tier.into()).or_default());
                    let _ = changed.send(());
                }
                last = Instant::now();
            }
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(()) => forced = true,
                Err(RecvTimeoutError::Timeout) => forced = false,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Start every tier. `changed` receives a message after each update.
pub fn start(cfg: Config, changed: Sender<()>) -> (Shared, Handle) {
    let shared: Shared = Arc::new(Mutex::new(Snapshot::default()));
    let paused = Arc::new(AtomicBool::new(false));
    let mut wakers = Vec::new();
    let mut channel = || {
        let (tx, rx) = mpsc::channel();
        wakers.push(tx.clone());
        (tx, rx)
    };
    let (_, host_rx) = channel();
    let (_, runners_rx) = channel();
    let (_, fleet_rx) = channel();
    let (pipe_tx, pipe_rx) = channel();
    let (gh_tx, gh_rx) = channel();
    let (_, quota_rx) = channel();
    // The PR URLs the gh tier last set out to read.
    let gh_read: Arc<Mutex<Vec<String>>> = Arc::default();

    // Host.
    {
        let shared = shared.clone();
        let mut reader = HostReader::new(cfg.disk_paths.clone());
        spawn_tier(
            "host",
            shared.clone(),
            changed.clone(),
            host_rx,
            paused.clone(),
            || Duration::from_secs(2),
            move |_| {
                let host = reader.read();
                shared.lock().unwrap().host = Some(host);
                Read::Ok
            },
        );
    }

    // Runners: directories every minute, processes every 5 s.
    {
        let (shared, roots) = (shared.clone(), cfg.runner_roots.clone());
        let mut scan = runners::ProcessScan::new();
        let mut list = Vec::new();
        let mut listed: Option<Instant> = None;
        spawn_tier(
            "runners",
            shared.clone(),
            changed.clone(),
            runners_rx,
            paused.clone(),
            || Duration::from_secs(5),
            move |_| {
                if listed.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                    list = runners::discover(&roots);
                    listed = Some(Instant::now());
                }
                scan.update(&mut list);
                shared.lock().unwrap().runners = list.clone();
                Read::Ok
            },
        );
    }

    // Fleet.
    {
        let (shared, cfg2) = (shared.clone(), cfg.clone());
        let ledger = cfg.ledger();
        let mut last_run: Option<Instant> = None;
        let mut ledger_seen = mtime(&ledger);
        let interval = cfg.fleet_interval;
        let min_gap = cfg.fleet_min_gap;
        let paused2 = paused.clone();
        let (gh_read, gh_tx) = (gh_read.clone(), gh_tx.clone());
        spawn_tier(
            "fleet",
            shared.clone(),
            changed.clone(),
            fleet_rx,
            paused.clone(),
            || Duration::from_secs(2),
            move |forced| {
                let since = last_run.map(|t| t.elapsed());
                let ledger_now = mtime(&ledger);
                let ledger_moved = ledger_now != ledger_seen;
                let due = match since {
                    None => true,
                    Some(e) if forced => e >= Duration::from_secs(3),
                    Some(_) if paused2.load(Ordering::Relaxed) => false,
                    Some(e) => e >= interval || (ledger_moved && e >= min_gap),
                };
                if !due {
                    return Read::Skipped;
                }
                ledger_seen = ledger_now;
                last_run = Some(Instant::now());
                match sources::read_fleet(&cfg2.fm_home, &cfg2.cache_dir) {
                    Ok(fleet) => {
                        let mut s = shared.lock().unwrap();
                        let prev = s.fleet.replace(fleet);
                        // New tasks or PRs: read them now. Otherwise the
                        // tiers' own timers keep their single cadence.
                        if pipelines_changed(prev.as_ref(), s.fleet.as_ref()) {
                            let _ = pipe_tx.send(());
                        }
                        if has_new_prs(&s, &gh_read.lock().unwrap()) {
                            let _ = gh_tx.send(());
                        }
                        Read::Ok
                    }
                    Err(e) => Read::Failed(e),
                }
            },
        );
    }

    // Pipelines.
    {
        let (shared, gh_read) = (shared.clone(), gh_read.clone());
        let interval = cfg.pipeline_interval;
        spawn_tier(
            "nm",
            shared.clone(),
            changed.clone(),
            pipe_rx,
            paused.clone(),
            move || interval,
            move |_| {
                let targets = {
                    let s = shared.lock().unwrap();
                    s.fleet
                        .as_ref()
                        .map(sources::pipeline_targets)
                        .unwrap_or_default()
                };
                let mut errors = Vec::new();
                let mut found = std::collections::BTreeMap::new();
                let total = targets.len();
                for (id, dir) in targets {
                    match sources::read_pipeline(&dir) {
                        Ok(Some(p)) => {
                            found.insert(id, p);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            // Keep the previous reading for this task.
                            if let Some(prev) = shared.lock().unwrap().pipelines.get(&id).cloned() {
                                found.insert(id.clone(), prev);
                            }
                            errors.push(format!("{id}: {e}"));
                        }
                    }
                }
                let mut s = shared.lock().unwrap();
                s.pipelines = found;
                if has_new_prs(&s, &gh_read.lock().unwrap()) {
                    let _ = gh_tx.send(());
                }
                nm_read(errors, total)
            },
        );
    }

    // GitHub.
    if cfg.github {
        let (shared, shared2) = (shared.clone(), shared.clone());
        let interval = move || {
            Duration::from_secs(if gh_active(&shared2.lock().unwrap()) {
                60
            } else {
                300
            })
        };
        spawn_tier(
            "gh",
            shared.clone(),
            changed.clone(),
            gh_rx,
            paused.clone(),
            interval,
            move |_| {
                let (urls, runner_list) = {
                    let s = shared.lock().unwrap();
                    (
                        sources::pr_urls(s.fleet.as_ref(), &s.pipelines),
                        s.runners.clone(),
                    )
                };
                gh_read.lock().unwrap().clone_from(&urls);
                let prs = sources::read_pr_checks(&urls);
                let jobs = sources::read_queue(&runner_list);
                apply_gh(&mut shared.lock().unwrap(), prs, jobs)
            },
        );
    }

    // Agent usage windows.
    if cfg.quota {
        let (shared, providers) = (shared.clone(), cfg.quota_providers.clone());
        spawn_tier(
            "quota",
            shared.clone(),
            changed.clone(),
            quota_rx,
            paused.clone(),
            || Duration::from_secs(300),
            move |_| match sources::read_quota(&providers) {
                Ok(q) => {
                    shared.lock().unwrap().quota = q;
                    Read::Ok
                }
                Err(e) => Read::Failed(e),
            },
        );
    }

    (shared, Handle { wakers, paused })
}

/// Pipelines are read one task at a time: the read fails outright only when
/// every task failed.
fn nm_read(errors: Vec<String>, total: usize) -> Read {
    if errors.len() == total {
        Read::from_errors(errors, Vec::new())
    } else {
        Read::from_errors(Vec::new(), errors)
    }
}

/// Whether a new fleet reading changed the pipeline tier's tasks, so it is
/// woken only when it has something new to read.
fn pipelines_changed(before: Option<&Fleet>, after: Option<&Fleet>) -> bool {
    let targets = |f: Option<&Fleet>| f.map(sources::pipeline_targets).unwrap_or_default();
    targets(before) != targets(after)
}

/// Whether `s` shows a PR, from the fleet or a pipeline, that the gh tier
/// has not set out to read yet (`read`).
fn has_new_prs(s: &Snapshot, read: &[String]) -> bool {
    sources::pr_urls(s.fleet.as_ref(), &s.pipelines)
        .iter()
        .any(|u| !read.contains(u))
}

/// Whether gh has anything live to watch: a busy runner or a PR on screen.
fn gh_active(s: &Snapshot) -> bool {
    s.runners.iter().any(|r| r.busy) || !sources::pr_urls(s.fleet.as_ref(), &s.pipelines).is_empty()
}

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Collect every tier once, in order, for `--once`.
pub fn collect_once(cfg: &Config) -> Snapshot {
    let mut s = Snapshot {
        now: now_epoch(),
        ..Default::default()
    };
    let note = |s: &mut Snapshot, tier: &str, r: Read| {
        r.record(s.fresh.entry(tier.into()).or_default());
    };
    s.host = Some(HostReader::new(cfg.disk_paths.clone()).read());
    note(&mut s, "host", Read::Ok);
    let mut list = runners::discover(&cfg.runner_roots);
    runners::ProcessScan::new().update(&mut list);
    s.runners = list;
    note(&mut s, "runners", Read::Ok);
    let r = match sources::read_fleet(&cfg.fm_home, &cfg.cache_dir) {
        Ok(f) => {
            s.fleet = Some(f);
            Read::Ok
        }
        Err(e) => Read::Failed(e),
    };
    note(&mut s, "fleet", r);
    let mut errors = Vec::new();
    let targets = s
        .fleet
        .as_ref()
        .map(sources::pipeline_targets)
        .unwrap_or_default();
    let total = targets.len();
    for (id, dir) in targets {
        match sources::read_pipeline(&dir) {
            Ok(Some(p)) => {
                s.pipelines.insert(id, p);
            }
            Ok(None) => {}
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    note(&mut s, "nm", nm_read(errors, total));
    if cfg.github {
        let urls = sources::pr_urls(s.fleet.as_ref(), &s.pipelines);
        let prs = sources::read_pr_checks(&urls);
        let jobs = sources::read_queue(&s.runners);
        let r = apply_gh(&mut s, prs, jobs);
        note(&mut s, "gh", r);
    }
    if cfg.quota {
        let r = match sources::read_quota(&cfg.quota_providers) {
            Ok(q) => {
                s.quota = q;
                Read::Ok
            }
            Err(e) => Read::Failed(e),
        };
        note(&mut s, "quota", r);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failing_tiers_back_off_to_five_minutes() {
        let s = Duration::from_secs;
        assert_eq!(backoff(s(15), 0), s(15));
        assert_eq!(backoff(s(15), 1), s(30));
        assert_eq!(backoff(s(15), 3), s(120));
        assert_eq!(backoff(s(15), 5), s(300));
        assert_eq!(backoff(s(2), 40), s(300));
        // A slow tier is never sped up by the cap.
        assert_eq!(backoff(s(600), 2), s(600));
    }

    fn pr(url: &str) -> PrChecks {
        PrChecks {
            url: url.into(),
            number: 122,
            state: "OPEN".into(),
            ..Default::default()
        }
    }

    #[test]
    fn partial_gh_errors_are_shown_but_never_back_off() {
        let url = "https://github.com/acme/pantry/pull/122";
        let gone = "gh: https://github.com/acme/gone/pull/7: Could not resolve".to_string();
        let mut s = Snapshot::default();
        let mut failures = 0;
        for _ in 0..3 {
            let r = apply_gh(&mut s, Ok((vec![pr(url)], vec![gone.clone()])), Ok(vec![]));
            assert_eq!(r, Read::Partial(gone.clone()));
            failures = r.failures(failures);
            r.record(s.fresh.entry("gh".into()).or_default());
        }
        assert_eq!(failures, 0);
        assert_eq!(
            backoff(Duration::from_secs(60), failures),
            Duration::from_secs(60)
        );
        assert!(s.prs.contains_key(url));
        let f = &s.fresh["gh"];
        assert_eq!(f.error.as_deref(), Some(gone.as_str()));
        assert!(f.ok_at.is_some(), "a partial read is still fresh");
    }

    #[test]
    fn outright_gh_failures_back_off_and_keep_the_last_reading() {
        let url = "https://github.com/acme/pantry/pull/122";
        let mut s = Snapshot::default();
        assert_eq!(
            apply_gh(&mut s, Ok((vec![pr(url)], vec![])), Ok(vec![])),
            Read::Ok
        );
        let mut failures = 0;
        for _ in 0..3 {
            let r = apply_gh(
                &mut s,
                Err("gh: exit 1: bad credentials".into()),
                Ok(vec![]),
            );
            assert!(matches!(r, Read::Failed(_)));
            failures = r.failures(failures);
            r.record(s.fresh.entry("gh".into()).or_default());
        }
        assert_eq!(failures, 3);
        assert_eq!(
            backoff(Duration::from_secs(60), failures),
            Duration::from_secs(300)
        );
        assert!(
            s.prs.contains_key(url),
            "the last good checks stay on screen"
        );
        assert!(s.fresh["gh"].ok_at.is_none());
        // A failed queue read is an outright failure too.
        let r = apply_gh(&mut s, Ok((vec![], vec![])), Err("gh: timed out".into()));
        assert_eq!(r, Read::Failed("gh: timed out".into()));
    }

    fn worker(id: &str, pr: Option<&str>) -> crate::model::Worker {
        crate::model::Worker {
            id: id.into(),
            fm_state: "working".into(),
            mode: "no-mistakes".into(),
            worktree: Some(std::env::temp_dir()),
            pr_url: pr.map(Into::into),
            ..Default::default()
        }
    }

    fn fleet(workers: Vec<crate::model::Worker>) -> Fleet {
        Fleet {
            workers,
            ..Default::default()
        }
    }

    #[test]
    fn an_unchanged_fleet_does_not_wake_the_pipelines() {
        let a = fleet(vec![worker("a", None)]);
        assert!(pipelines_changed(None, Some(&a)));
        assert!(!pipelines_changed(Some(&a), Some(&a.clone())));
        let b = fleet(vec![worker("a", None), worker("b", None)]);
        assert!(pipelines_changed(Some(&a), Some(&b)));
    }

    #[test]
    fn gh_is_woken_only_for_prs_it_has_not_read() {
        let pr = "https://github.com/acme/pantry/pull/122";
        let pr2 = "https://github.com/acme/pantry/pull/123";
        let mut s = Snapshot {
            fleet: Some(fleet(vec![worker("a", Some(pr))])),
            ..Default::default()
        };
        assert!(has_new_prs(&s, &[]));
        let mut read = sources::pr_urls(s.fleet.as_ref(), &s.pipelines);
        // A routine re-read leaves gh on its own timer.
        assert!(!has_new_prs(&s, &read));
        // The pipeline tier finds a PR that firstmate has not recorded yet.
        s.fleet = Some(fleet(vec![worker("a", Some(pr)), worker("b", None)]));
        s.pipelines.insert(
            "b".into(),
            crate::model::Pipeline {
                pr_url: Some(pr2.into()),
                ..Default::default()
            },
        );
        assert!(has_new_prs(&s, &read));
        read = sources::pr_urls(s.fleet.as_ref(), &s.pipelines);
        assert!(!has_new_prs(&s, &read));
        // A PR leaving the screen needs no read.
        s.pipelines.clear();
        assert!(!has_new_prs(&s, &read));
    }

    #[test]
    fn a_pipeline_pr_keeps_gh_on_its_fast_cadence() {
        let mut s = Snapshot {
            fleet: Some(fleet(vec![worker("b", None)])),
            ..Default::default()
        };
        assert!(!gh_active(&s));
        s.pipelines.insert(
            "b".into(),
            crate::model::Pipeline {
                pr_url: Some("https://github.com/acme/pantry/pull/122".into()),
                ..Default::default()
            },
        );
        assert!(gh_active(&s));
        // An idle secondmate's old PR is not read, so it is not activity.
        s.pipelines.clear();
        let mut idle = worker("c", Some("https://github.com/acme/pantry/pull/9"));
        idle.fm_state = "no_active_work".into();
        s.fleet = Some(fleet(vec![idle]));
        assert!(!gh_active(&s));
    }

    #[test]
    fn pipelines_fail_outright_only_when_every_task_failed() {
        let e = || vec!["a: no-mistakes: timed out".to_string()];
        assert!(matches!(nm_read(e(), 1), Read::Failed(_)));
        assert!(matches!(nm_read(e(), 2), Read::Partial(_)));
        assert_eq!(nm_read(vec![], 2), Read::Ok);
        assert_eq!(nm_read(vec![], 0), Read::Ok);
    }
}
