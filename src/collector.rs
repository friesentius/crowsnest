//! Background refresh. Each source runs on its own thread at its own cadence
//! and writes into one shared [`Snapshot`]; the UI only reads it.
//!
//! | tier     | default cadence                                              |
//! |----------|--------------------------------------------------------------|
//! | host     | 2 s                                                          |
//! | runners  | 5 s (process scan, `_diag` file times), directories re-listed every 60 s and whenever a `Runner.Listener` starts or stops, systemd units every 30 s |
//! | pipelines| 15 s per live no-mistakes task                               |
//! | fleet    | 60 s                                                         |
//! | github   | 60 s while a PR, runner or queued job is active, otherwise 5 min |
//! | ↳ history| completed runs every 10 min, or when a watched run finishes; each run's jobs once |
//! | ↳ runners API | 5 min per repository                                    |
//! | quota    | 5 min                                                        |
//!
//! A tier never overlaps itself, keeps its last good data on failure,
//! records the failure so the UI can show it, and backs off while its reads
//! keep failing outright (doubling its interval, up to 5 min). A read that
//! succeeded with errors for some items is shown as an error but keeps the
//! normal cadence.

use crate::model::{
    Fleet, Freshness, Job, PrChecks, RepoHistory, RepoRunners, Runner, RunnerSearch, Service,
    Snapshot,
};
use crate::sources::{
    self,
    history::RepoCache,
    host::{self, HostReader},
    runners::{self, Discovery},
};
use crate::timefmt::now_epoch;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Config {
    pub fm_home: PathBuf,
    /// Crowsnest's own cache directory (never inside the firstmate home).
    pub cache_dir: PathBuf,
    /// Where to look for runners and their units.
    pub runners: Discovery,
    pub disk_paths: Vec<PathBuf>,
    pub fleet_interval: Duration,
    pub pipeline_interval: Duration,
    pub github: bool,
    pub quota: bool,
    pub quota_providers: Vec<String>,
}

pub type Shared = Arc<Mutex<Snapshot>>;

/// Handle the UI uses to steer refreshing (never firstmate).
pub struct Handle {
    wakers: Vec<Sender<()>>,
    paused: Arc<AtomicBool>,
    intervals: Arc<Intervals>,
    discovery: Arc<Mutex<Discovery>>,
    runners_waker: Sender<()>,
}

/// The cadences the options menu can change while crowsnest runs, in
/// milliseconds.
#[derive(Debug, Default)]
struct Intervals {
    fleet: AtomicU64,
    pipeline: AtomicU64,
}

impl Intervals {
    fn get(v: &AtomicU64) -> Duration {
        Duration::from_millis(v.load(Ordering::Relaxed))
    }

    fn set(v: &AtomicU64, d: Duration) {
        v.store(
            d.as_millis().try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
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

    /// Change where runners are looked for; the runner tier re-lists them
    /// now.
    pub fn set_discovery(&self, d: Discovery) {
        *self.discovery.lock().unwrap() = d;
        let _ = self.runners_waker.send(());
    }

    /// Change the fleet and pipeline cadences; each tier picks the new one up
    /// on its next check.
    pub fn set_intervals(&self, fleet: Duration, pipeline: Duration) {
        Intervals::set(&self.intervals.fleet, fleet);
        Intervals::set(&self.intervals.pipeline, pipeline);
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
        Ok(j) => {
            s.jobs = j.into_iter().filter(|j| j.status != "completed").collect();
            s.jobs_read_at = Some(now_epoch());
        }
        Err(e) => failed.push(e),
    }
    Read::from_errors(failed, partial)
}

/// How often the completed runs of a repository are listed.
const HISTORY_EVERY: Duration = Duration::from_secs(600);
/// How often GitHub's runner list of a repository is read.
const RUNNERS_API_EVERY: Duration = Duration::from_secs(300);
/// How often a runner's systemd unit is read.
const SERVICE_EVERY: Duration = Duration::from_secs(30);

/// The gh tier's own memory between reads: the finished runs it has read,
/// and when it last read each slow-moving part. Failures here never fail the
/// tier: they are kept per repository and shown where they apply.
#[derive(Default)]
struct GhState {
    cache: BTreeMap<String, RepoCache>,
    history_at: BTreeMap<String, Instant>,
    history_error: BTreeMap<String, String>,
    runners_at: BTreeMap<String, Instant>,
    gh_runners: BTreeMap<String, RepoRunners>,
    /// Runs that were queued or running at the last read.
    active: BTreeSet<u64>,
}

impl GhState {
    /// Read what is due for the repositories of `runner_list`, given the
    /// queue just read. Returns the history and runner lists to show.
    fn read(
        &mut self,
        runner_list: &[Runner],
        jobs: &Result<Vec<Job>, String>,
    ) -> (BTreeMap<String, RepoHistory>, BTreeMap<String, RepoRunners>) {
        let repos = sources::runner_repos(runner_list);
        // A run that left the queue has finished: read its jobs now rather
        // than at the next listing.
        let mut finished = false;
        if let Ok(jobs) = jobs {
            let active: BTreeSet<u64> = jobs
                .iter()
                .filter(|j| j.status != "completed")
                .map(|j| j.run_id)
                .collect();
            finished = self.active.difference(&active).next().is_some();
            self.active = active;
        }
        for repo in &repos {
            let due = |at: Option<&Instant>, every| at.is_none_or(|t| t.elapsed() >= every);
            if finished || due(self.history_at.get(repo), HISTORY_EVERY) {
                self.history_at.insert(repo.clone(), Instant::now());
                let cache = self.cache.entry(repo.clone()).or_default();
                match sources::read_history(repo, cache) {
                    Ok(_) => self.history_error.remove(repo),
                    Err(e) => self.history_error.insert(repo.clone(), e),
                };
            }
            if due(self.runners_at.get(repo), RUNNERS_API_EVERY) {
                self.runners_at.insert(repo.clone(), Instant::now());
                self.gh_runners
                    .insert(repo.clone(), sources::read_gh_runners(repo));
            }
        }
        let live: &[Job] = jobs.as_deref().unwrap_or(&[]);
        let history = repos
            .iter()
            .map(|repo| {
                let finished: Vec<Job> = live
                    .iter()
                    .filter(|j| &j.repo == repo && j.status == "completed")
                    .cloned()
                    .collect();
                let mut h = self
                    .cache
                    .get(repo)
                    .map(|c| c.summary(&finished))
                    .unwrap_or_default();
                h.error = self.history_error.get(repo).cloned();
                (repo.clone(), h)
            })
            .collect();
        let gh_runners = self
            .gh_runners
            .iter()
            .filter(|(repo, _)| repos.contains(repo))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        (history, gh_runners)
    }
}

/// The runner tier's memory of systemd units: which scope each unit lives
/// in, and its last reading.
#[derive(Default)]
struct Services {
    read_at: Option<Instant>,
    /// Unit name -> found in the user's own systemd (`--user`).
    user: BTreeMap<String, bool>,
    state: BTreeMap<String, Service>,
}

impl Services {
    fn update(&mut self, list: &mut [Runner]) {
        // A unit that was not there at the last read (a runner just found) is
        // read now rather than at the next turn.
        let new_unit = list
            .iter()
            .filter_map(|r| r.unit.as_ref())
            .any(|u| !self.state.contains_key(u));
        if new_unit || self.read_at.is_none_or(|t| t.elapsed() >= SERVICE_EVERY) {
            self.read_at = Some(Instant::now());
            self.state.clear();
            for unit in list.iter().filter_map(|r| r.unit.clone()) {
                if self.state.contains_key(&unit) {
                    continue;
                }
                // A unit lives in the user's systemd or the system's; the
                // scope it was last found in is asked first, and the other
                // when that one does not have it or cannot be asked.
                let first = self.user.get(&unit).copied().unwrap_or(true);
                let mut failed = None;
                let mut found = None;
                for user in [first, !first] {
                    match sources::read_service(&unit, user) {
                        Some(svc) if svc.error.is_none() => {
                            found = Some((svc, user));
                            break;
                        }
                        Some(svc) => failed = failed.or(Some(svc)),
                        None => {}
                    }
                }
                let svc = match found {
                    Some((svc, user)) => {
                        self.user.insert(unit.clone(), user);
                        svc
                    }
                    None => failed.unwrap_or_else(|| Service {
                        error: Some("not-found".into()),
                        ..Default::default()
                    }),
                };
                self.state.insert(unit, svc);
            }
        }
        for r in list {
            r.service = r.unit.as_ref().and_then(|u| self.state.get(u).cloned());
        }
    }
}

/// How often the runner folders are listed again, besides when a listener
/// starts or stops.
const RELIST_EVERY: Duration = Duration::from_secs(60);

/// The runner tier's memory between its turns.
struct RunnerTier {
    disc: Discovery,
    scan: runners::ProcessScan,
    services: Services,
    /// The runners as last listed, with the process table's state on them.
    list: Vec<Runner>,
    search: RunnerSearch,
    /// What the last listing could not read.
    errors: Vec<String>,
    listed: Option<Instant>,
    /// The runner directories whose listener ran at any time this run, with
    /// the unit it was in, so one that dies still shows, as DOWN, with its
    /// unit. Kept only while runners are found from processes.
    memory: BTreeMap<PathBuf, Option<String>>,
    listener_pids: Vec<u32>,
}

impl RunnerTier {
    fn new(disc: Discovery) -> Self {
        RunnerTier {
            disc,
            scan: runners::ProcessScan::new(),
            services: Services::default(),
            list: Vec::new(),
            search: RunnerSearch::default(),
            errors: Vec::new(),
            listed: None,
            memory: BTreeMap::new(),
            listener_pids: Vec::new(),
        }
    }

    /// One turn: list the runners afresh when that is due, the discovery
    /// settings changed (`wanted`) or a listener started or stopped; then
    /// mark each up or busy from the process table, read its logs' times and
    /// its unit.
    fn tick(&mut self, wanted: &Discovery, proc_root: &std::path::Path) {
        let mut relist = self.listed.is_none_or(|t| t.elapsed() > RELIST_EVERY);
        if *wanted != self.disc {
            self.disc.clone_from(wanted);
            relist = true;
        }
        if !self.disc.processes {
            self.memory.clear();
        }
        // Reading the process table is the costliest local read: skip it
        // when there is nothing to find or to match.
        let mut read = false;
        let mut listeners = Vec::new();
        if self.disc.processes || !self.list.is_empty() {
            self.scan.refresh();
            read = true;
            if self.disc.processes {
                listeners = self.scan.listeners();
            }
        }
        let pids: Vec<u32> = listeners.iter().map(|l| l.pid).collect();
        if pids != self.listener_pids {
            self.listener_pids = pids;
            relist = true;
        }
        if relist {
            let remembered: Vec<_> = self.memory.clone().into_iter().collect();
            let found = runners::discover(&self.disc, &listeners, &remembered, proc_root);
            for (dir, unit) in found.seen {
                let old = self.memory.entry(dir).or_default();
                if unit.is_some() {
                    *old = unit;
                }
            }
            self.list = found.runners;
            self.search = found.search;
            self.errors = found.errors;
            self.listed = Some(Instant::now());
        }
        // Runners just listed have not been matched against a table read
        // before the listing: without one they would all read as DOWN.
        if !read && !self.list.is_empty() {
            self.scan.refresh();
        }
        self.scan.apply(&mut self.list, now_epoch());
        for r in &mut self.list {
            runners::read_diag(r);
        }
        self.services.update(&mut self.list);
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
    let intervals = Arc::new(Intervals::default());
    Intervals::set(&intervals.fleet, cfg.fleet_interval);
    Intervals::set(&intervals.pipeline, cfg.pipeline_interval);
    let mut wakers = Vec::new();
    let mut channel = || {
        let (tx, rx) = mpsc::channel();
        wakers.push(tx.clone());
        (tx, rx)
    };
    let (_, host_rx) = channel();
    let (runners_tx, runners_rx) = channel();
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
                let read = reader.read();
                shared.lock().unwrap().host = Some(read.host);
                item_read(read.errors, host::PARTS)
            },
        );
    }

    // Runners: directories every minute and when a listener starts or stops,
    // processes every 5 s.
    let discovery = Arc::new(Mutex::new(cfg.runners.clone()));
    {
        let (shared, discovery) = (shared.clone(), discovery.clone());
        let mut tier = RunnerTier::new(cfg.runners.clone());
        let gh_wake = gh_tx.clone();
        let mut repos_seen: Vec<String> = Vec::new();
        spawn_tier(
            "runners",
            shared.clone(),
            changed.clone(),
            runners_rx,
            paused.clone(),
            || Duration::from_secs(5),
            move |_| {
                let wanted = discovery.lock().unwrap().clone();
                tier.tick(&wanted, runners::proc_root());
                {
                    let mut s = shared.lock().unwrap();
                    s.runners = tier.list.clone();
                    s.runner_search = Some(tier.search.clone());
                }
                // Runners (or their repositories) turned up: read their queue
                // now, not at the gh tier's next turn a minute away.
                let repos = sources::runner_repos(&tier.list);
                if repos != repos_seen {
                    repos_seen = repos;
                    let _ = gh_wake.send(());
                }
                runners_read(&tier.list, &tier.errors, &tier.scan)
            },
        );
    }

    // Fleet.
    {
        let (shared, cfg2) = (shared.clone(), cfg.clone());
        let mut last_run: Option<Instant> = None;
        let intervals = intervals.clone();
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
                let due = match since {
                    None => true,
                    Some(e) if forced => e >= Duration::from_secs(3),
                    Some(_) if paused2.load(Ordering::Relaxed) => false,
                    Some(e) => e >= Intervals::get(&intervals.fleet),
                };
                if !due {
                    return Read::Skipped;
                }
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
        let intervals = intervals.clone();
        spawn_tier(
            "nm",
            shared.clone(),
            changed.clone(),
            pipe_rx,
            paused.clone(),
            move || Intervals::get(&intervals.pipeline),
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
                item_read(errors, total)
            },
        );
    }

    // GitHub.
    if cfg.github {
        let (shared, shared2) = (shared.clone(), shared.clone());
        let mut state = GhState::default();
        let changed_gh = changed.clone();
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
                // Show the checks and the queue first: the first history
                // read fetches several runs' jobs and takes a while.
                let live = jobs.clone();
                let read = apply_gh(&mut shared.lock().unwrap(), prs, jobs);
                let _ = changed_gh.send(());
                let (history, gh_runners) = state.read(&runner_list, &live);
                let mut s = shared.lock().unwrap();
                s.history = history;
                s.gh_runners = gh_runners;
                read
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

    let handle = Handle {
        wakers,
        paused,
        intervals,
        discovery,
        runners_waker: runners_tx,
    };
    (shared, handle)
}

/// A tier read in `total` parts (a task's pipeline, a part of the host): it
/// fails outright only when every part failed, and is partial when some did.
fn item_read(errors: Vec<String>, total: usize) -> Read {
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

/// Whether gh has anything live to watch: a busy runner, a queued job (so a
/// runner that does not pick it up is seen in time), or a PR on screen.
fn gh_active(s: &Snapshot) -> bool {
    s.runners.iter().any(|r| r.busy)
        || s.jobs.iter().any(Job::is_queued)
        || !sources::pr_urls(s.fleet.as_ref(), &s.pipelines).is_empty()
}

/// What the runner tier read: a root or `.runner` file that could not be
/// read, or a process table that cannot be scanned, is an error. With
/// nothing discovered the read failed outright; otherwise it is partial.
fn runners_read(list: &[Runner], discovery: &[String], scan: &runners::ProcessScan) -> Read {
    let mut errors = discovery.to_vec();
    if scan.unavailable() && !list.is_empty() {
        errors.push("process table unavailable: runners cannot be told up or busy".into());
    }
    if errors.is_empty() {
        Read::Ok
    } else if list.is_empty() {
        Read::Failed(errors.join("; "))
    } else {
        Read::Partial(errors.join("; "))
    }
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
    let read = HostReader::new(cfg.disk_paths.clone()).read();
    s.host = Some(read.host);
    note(&mut s, "host", item_read(read.errors, host::PARTS));
    let mut scan = runners::ProcessScan::new();
    let listeners = if cfg.runners.processes {
        scan.refresh();
        scan.listeners()
    } else {
        Vec::new()
    };
    let found = runners::discover(&cfg.runners, &listeners, &[], runners::proc_root());
    let mut list = found.runners;
    let list_errors = found.errors;
    s.runner_search = Some(found.search);
    if !list.is_empty() && !cfg.runners.processes {
        scan.refresh();
    }
    scan.apply(&mut list, now_epoch());
    for r in &mut list {
        runners::read_diag(r);
    }
    Services::default().update(&mut list);
    let r = runners_read(&list, &list_errors, &scan);
    s.runners = list;
    note(&mut s, "runners", r);
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
    note(&mut s, "nm", item_read(errors, total));
    if cfg.github {
        let urls = sources::pr_urls(s.fleet.as_ref(), &s.pipelines);
        let prs = sources::read_pr_checks(&urls);
        let jobs = sources::read_queue(&s.runners);
        let (history, gh_runners) = GhState::default().read(&s.runners, &jobs);
        s.history = history;
        s.gh_runners = gh_runners;
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
    fn a_queued_job_keeps_gh_on_its_fast_cadence() {
        let mut s = Snapshot::default();
        assert!(!gh_active(&s));
        // An idle runner with a job queued for it must be re-read soon, or a
        // runner that never picks the job up is seen minutes late.
        s.jobs.push(Job {
            status: "queued".into(),
            ..Default::default()
        });
        assert!(gh_active(&s));
    }

    #[test]
    fn the_queue_read_keeps_only_unfinished_jobs() {
        let mut s = Snapshot::default();
        let job = |status: &str| Job {
            status: status.into(),
            ..Default::default()
        };
        let r = apply_gh(
            &mut s,
            Ok((vec![], vec![])),
            Ok(vec![job("completed"), job("in_progress"), job("queued")]),
        );
        assert_eq!(r, Read::Ok);
        assert_eq!(s.jobs.len(), 2);
        assert!(s.jobs_read_at.is_some());
    }

    #[test]
    fn pipelines_fail_outright_only_when_every_task_failed() {
        let e = || vec!["a: no-mistakes: timed out".to_string()];
        assert!(matches!(item_read(e(), 1), Read::Failed(_)));
        assert!(matches!(item_read(e(), 2), Read::Partial(_)));
        assert_eq!(item_read(vec![], 2), Read::Ok);
        assert_eq!(item_read(vec![], 0), Read::Ok);
    }

    #[test]
    fn the_host_fails_outright_only_when_every_part_failed() {
        let parts = |n: usize| (0..n).map(|i| format!("part {i}")).collect::<Vec<_>>();
        assert_eq!(item_read(vec![], host::PARTS), Read::Ok);
        assert!(matches!(item_read(parts(1), host::PARTS), Read::Partial(_)));
        assert!(matches!(
            item_read(parts(host::PARTS), host::PARTS),
            Read::Failed(_)
        ));
    }

    #[test]
    fn the_runners_tier_reports_what_discovery_could_not_read() {
        let scan = runners::ProcessScan::new();
        let one = [Runner::default()];
        let err = ["Permission denied: /srv".to_string()];
        assert_eq!(runners_read(&one, &[], &scan), Read::Ok);
        assert_eq!(runners_read(&[], &[], &scan), Read::Ok);
        // Some found: partial, so the cadence stays.
        assert_eq!(
            runners_read(&one, &err, &scan),
            Read::Partial("Permission denied: /srv".into())
        );
        // Nothing found and something unreadable: the read failed.
        assert_eq!(
            runners_read(&[], &err, &scan),
            Read::Failed("Permission denied: /srv".into())
        );
    }

    #[test]
    fn a_failed_read_is_recorded_and_a_good_one_clears_it() {
        let mut f = Freshness::default();
        Read::Failed("boom".into()).record(&mut f);
        assert_eq!(f.error.as_deref(), Some("boom"));
        assert_eq!(f.ok_at, None);
        Read::Ok.record(&mut f);
        assert_eq!(f.error, None);
        assert!(f.ok_at.is_some());
    }

    /// A running process that `sysinfo` sees as a `Runner.Listener` from
    /// `dir/bin`: a copy of `sleep` under that name. Killed on drop.
    #[cfg(target_os = "linux")]
    struct FakeListener(std::process::Child);

    #[cfg(target_os = "linux")]
    impl FakeListener {
        fn start(dir: &std::path::Path) -> Self {
            let sleep = ["/bin/sleep", "/usr/bin/sleep"]
                .into_iter()
                .find(|p| std::path::Path::new(p).is_file())
                .expect("sleep");
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let exe = bin.join("Runner.Listener");
            std::fs::copy(sleep, &exe).unwrap();
            FakeListener(std::process::Command::new(exe).arg("60").spawn().unwrap())
        }

        fn stop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for FakeListener {
        fn drop(&mut self) {
            self.stop();
        }
    }

    #[cfg(target_os = "linux")]
    fn runner_folder(root: &std::path::Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".runner"),
            include_str!("../tests/fixtures/runner/.runner"),
        )
        .unwrap();
        dir
    }

    #[cfg(target_os = "linux")]
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crowsnest-tier-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runners_listed_this_turn_are_matched_against_a_fresh_process_table() {
        // With processes off the table is not read for finding runners, and
        // the first turn has none to match yet: the runner found by its
        // folder must still read as up, not DOWN.
        let root = scratch("fresh");
        let dir = runner_folder(&root, "actions-runner-live");
        let _listener = FakeListener::start(&dir);
        let disc = Discovery {
            processes: false,
            roots: vec![root.clone()],
            ..Default::default()
        };
        let mut tier = RunnerTier::new(disc.clone());
        tier.tick(&disc, std::path::Path::new("/nonexistent"));
        assert_eq!(tier.list.len(), 1);
        assert!(
            tier.list[0].listener_secs.is_some(),
            "a running listener is not DOWN"
        );
        assert!(!tier.search.processes);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_runner_whose_listener_dies_stays_until_processes_are_switched_off() {
        let root = scratch("memory");
        // Under no searched folder: found only through its listener.
        let dir = runner_folder(&root, "somewhere-else");
        let on = Discovery {
            processes: true,
            roots: vec![],
            ..Default::default()
        };
        let mut listener = FakeListener::start(&dir);
        let mut tier = RunnerTier::new(on.clone());
        let proc = std::path::Path::new("/nonexistent");
        tier.tick(&on, proc);
        let ours = |tier: &RunnerTier| {
            tier.list
                .iter()
                .find(|r| r.dir == dir)
                .map(|r| r.listener_secs.is_some())
        };
        assert_eq!(ours(&tier), Some(true), "found from its listener");
        assert!(tier.search.listeners >= 1);

        listener.stop();
        tier.tick(&on, proc);
        assert_eq!(
            ours(&tier),
            Some(false),
            "still listed after its listener died, now with none running"
        );

        let off = Discovery {
            processes: false,
            ..on
        };
        tier.tick(&off, proc);
        assert_eq!(ours(&tier), None, "gone once processes are switched off");
        assert!(tier.memory.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
