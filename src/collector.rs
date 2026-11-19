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
//! records the failure so the UI can show it, and backs off while it keeps
//! failing (doubling its interval, up to 5 min).

use crate::model::Snapshot;
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

fn mark(shared: &Shared, tier: &str, result: Result<(), String>) {
    let mut s = shared.lock().unwrap();
    let f = s.fresh.entry(tier.to_string()).or_default();
    match result {
        Ok(()) => {
            f.ok_at = Some(now_epoch());
            f.error = None;
        }
        Err(e) => f.error = Some(e),
    }
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
/// run when woken explicitly. `work` returns whether it actually read its
/// source; while the tier's last read failed, the interval backs off.
fn spawn_tier<F, I>(
    tier: &'static str,
    shared: Shared,
    rx: Receiver<()>,
    paused: Arc<AtomicBool>,
    interval: I,
    mut work: F,
) where
    F: FnMut(bool) -> bool + Send + 'static,
    I: Fn() -> Duration + Send + 'static,
{
    thread::spawn(move || {
        let mut forced = true;
        let mut last = Instant::now();
        let mut failures = 0;
        loop {
            let due = last.elapsed() >= backoff(interval(), failures);
            if forced || (!paused.load(Ordering::Relaxed) && due) {
                if work(forced) {
                    let failed = shared
                        .lock()
                        .unwrap()
                        .fresh
                        .get(tier)
                        .is_some_and(|f| f.error.is_some());
                    failures = if failed { failures + 1 } else { 0 };
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

    // Host.
    {
        let (shared, changed) = (shared.clone(), changed.clone());
        let mut reader = HostReader::new(cfg.disk_paths.clone());
        spawn_tier(
            "host",
            shared.clone(),
            host_rx,
            paused.clone(),
            || Duration::from_secs(2),
            move |_| {
                let host = reader.read();
                shared.lock().unwrap().host = Some(host);
                mark(&shared, "host", Ok(()));
                let _ = changed.send(());
                true
            },
        );
    }

    // Runners: directories every minute, processes every 5 s.
    {
        let (shared, changed, roots) = (shared.clone(), changed.clone(), cfg.runner_roots.clone());
        let mut scan = runners::ProcessScan::new();
        let mut list = Vec::new();
        let mut listed: Option<Instant> = None;
        spawn_tier(
            "runners",
            shared.clone(),
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
                mark(&shared, "runners", Ok(()));
                let _ = changed.send(());
                true
            },
        );
    }

    // Fleet.
    {
        let (shared, changed, cfg2) = (shared.clone(), changed.clone(), cfg.clone());
        let ledger = cfg.ledger();
        let mut last_run: Option<Instant> = None;
        let mut ledger_seen = mtime(&ledger);
        let interval = cfg.fleet_interval;
        let min_gap = cfg.fleet_min_gap;
        let paused2 = paused.clone();
        spawn_tier(
            "fleet",
            shared.clone(),
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
                    return false;
                }
                ledger_seen = ledger_now;
                last_run = Some(Instant::now());
                let result = sources::read_fleet(&cfg2.fm_home, &cfg2.cache_dir);
                let ok = match result {
                    Ok(fleet) => {
                        shared.lock().unwrap().fleet = Some(fleet);
                        // New tasks: read their pipelines and PRs now.
                        let _ = pipe_tx.send(());
                        let _ = gh_tx.send(());
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                mark(&shared, "fleet", ok);
                let _ = changed.send(());
                true
            },
        );
    }

    // Pipelines.
    {
        let (shared, changed) = (shared.clone(), changed.clone());
        let interval = cfg.pipeline_interval;
        spawn_tier(
            "nm",
            shared.clone(),
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
                shared.lock().unwrap().pipelines = found;
                mark(
                    &shared,
                    "nm",
                    if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(errors.join("; "))
                    },
                );
                let _ = changed.send(());
                true
            },
        );
    }

    // GitHub.
    if cfg.github {
        let (shared, changed, shared2) = (shared.clone(), changed.clone(), shared.clone());
        let interval = move || {
            let s = shared2.lock().unwrap();
            let active = s.runners.iter().any(|r| r.busy)
                || s.prs.values().any(|p| p.state == "OPEN")
                || s.fleet
                    .as_ref()
                    .is_some_and(|f| f.workers.iter().any(|w| w.pr_url.is_some()));
            Duration::from_secs(if active { 60 } else { 300 })
        };
        spawn_tier(
            "gh",
            shared.clone(),
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
                let mut errors = Vec::new();
                match sources::read_pr_checks(&urls) {
                    Ok((prs, pr_errors)) => {
                        shared.lock().unwrap().prs =
                            prs.into_iter().map(|p| (p.url.clone(), p)).collect();
                        errors.extend(pr_errors);
                    }
                    Err(e) => errors.push(e),
                }
                match sources::read_queue(&runner_list) {
                    Ok(jobs) => shared.lock().unwrap().jobs = jobs,
                    Err(e) => errors.push(e),
                }
                mark(
                    &shared,
                    "gh",
                    if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(errors.join("; "))
                    },
                );
                let _ = changed.send(());
                true
            },
        );
    }

    // Agent usage windows.
    if cfg.quota {
        let (shared, changed, providers) =
            (shared.clone(), changed.clone(), cfg.quota_providers.clone());
        spawn_tier(
            "quota",
            shared.clone(),
            quota_rx,
            paused.clone(),
            || Duration::from_secs(300),
            move |_| {
                let r = sources::read_quota(&providers).map(|q| {
                    shared.lock().unwrap().quota = q;
                });
                mark(&shared, "quota", r);
                let _ = changed.send(());
                true
            },
        );
    }

    (shared, Handle { wakers, paused })
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
    let note = |s: &mut Snapshot, tier: &str, r: Result<(), String>| {
        let f = s.fresh.entry(tier.into()).or_default();
        match r {
            Ok(()) => f.ok_at = Some(now_epoch()),
            Err(e) => f.error = Some(e),
        }
    };
    s.host = Some(HostReader::new(cfg.disk_paths.clone()).read());
    note(&mut s, "host", Ok(()));
    let mut list = runners::discover(&cfg.runner_roots);
    runners::ProcessScan::new().update(&mut list);
    s.runners = list;
    note(&mut s, "runners", Ok(()));
    let r = sources::read_fleet(&cfg.fm_home, &cfg.cache_dir).map(|f| s.fleet = Some(f));
    note(&mut s, "fleet", r);
    let mut errors = Vec::new();
    if let Some(f) = s.fleet.clone() {
        for (id, dir) in sources::pipeline_targets(&f) {
            match sources::read_pipeline(&dir) {
                Ok(Some(p)) => {
                    s.pipelines.insert(id, p);
                }
                Ok(None) => {}
                Err(e) => errors.push(format!("{id}: {e}")),
            }
        }
    }
    note(
        &mut s,
        "nm",
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        },
    );
    if cfg.github {
        let urls = sources::pr_urls(s.fleet.as_ref(), &s.pipelines);
        let mut errors = Vec::new();
        match sources::read_pr_checks(&urls) {
            Ok((prs, pr_errors)) => {
                s.prs = prs.into_iter().map(|p| (p.url.clone(), p)).collect();
                errors.extend(pr_errors);
            }
            Err(e) => errors.push(e),
        }
        match sources::read_queue(&s.runners) {
            Ok(j) => s.jobs = j,
            Err(e) => errors.push(e),
        }
        note(
            &mut s,
            "gh",
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            },
        );
    }
    if cfg.quota {
        let r = sources::read_quota(&cfg.quota_providers).map(|q| s.quota = q);
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
}
