//! Runner lanes: what each local runner is doing, what waits for it, when
//! that should start, and whether the runner looks stuck. Pure: everything
//! is derived from a [`Snapshot`] and its `now`, like [`crate::model::classify`]
//! derives a worker's state, so a verdict never depends on when it is drawn.
//!
//! | verdict          | rule                                                         |
//! |------------------|--------------------------------------------------------------|
//! | `DOWN`           | no `Runner.Listener` for the directory, or GitHub says offline |
//! | `STALLED`        | busy, no step output (else worker log) for the silent limit, and the job's processes idle on CPU over that window (for a job run in a container, whose CPU is not visible, silence alone); or GitHub says a job is in progress here and no `Runner.Worker` runs |
//! | `NOT PICKING UP` | a job queued past the pickup limit while the runner is idle  |
//! | `SLOW`           | still producing output, but past the overdue factor times its typical duration |
//! | moving           | busy otherwise                                               |
//! | idle             | nothing running                                              |

use crate::model::{
    GhRunner, Job, JobStep, RepoHistory, Runner, RunnerThresholds, Snapshot, Typical,
};

/// Fewer samples than this give no typical duration: elapsed time only,
/// never SLOW.
pub const MIN_SAMPLES: usize = 3;

/// How long GitHub may lag behind the process table before a job it still
/// calls in progress, with no worker running, counts as stalled.
pub const GH_LAG_SECS: i64 = 60;

/// The window the busy share is measured over.
pub const BUSY_WINDOW_SECS: i64 = 6 * 3600;

/// Most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Down,
    Stalled,
    NotPickingUp,
    Slow,
    Moving,
    Idle,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Down => "DOWN",
            Verdict::Stalled => "STALLED",
            Verdict::NotPickingUp => "NOT PICKING UP",
            Verdict::Slow => "SLOW",
            Verdict::Moving => "moving",
            Verdict::Idle => "idle",
        }
    }

    /// Needs someone to act: counted as stalled in the panel title.
    pub fn is_stuck(self) -> bool {
        matches!(
            self,
            Verdict::Down | Verdict::Stalled | Verdict::NotPickingUp
        )
    }
}

/// Why the verdict is what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    None,
    /// No `Runner.Listener` process runs from the runner's directory.
    NoListener,
    /// GitHub reports the runner offline.
    Offline,
    /// The job's progress signal has not advanced for this many seconds.
    Silent(i64),
    /// GitHub says a job runs here, but no `Runner.Worker` does.
    NoWorker,
    /// A job has waited this long while the runner sat idle.
    Unpicked(i64),
    /// Running past the overdue factor of its typical duration.
    Overdue,
}

/// Whether GitHub lists the runner online.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Online {
    Yes,
    No,
    /// The token may not read the repository's runners (admin read).
    Denied,
    /// Not in GitHub's list of the repository's runners.
    Unregistered,
    /// Not read (yet, or GitHub is off).
    Unknown,
}

/// Which local file says the job is still moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// The running step's output (`_diag/pages`).
    StepOutput,
    /// The worker's log (`_diag/Worker_*.log`), when there is no step output.
    WorkerLog,
    /// Neither file was written since the job started.
    None,
}

/// What the running job's processes did over the silent window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCpu {
    /// Used less CPU than the idle limit: silence means stuck.
    Idle(u64),
    /// Used this many CPU milliseconds: silence only means quiet.
    Busy(u64),
    /// The steps run in a container (or the process table is unreadable),
    /// so their CPU cannot be seen: silence alone decides.
    Unseen,
    /// Fewer than a minute of samples so far: no judgement yet.
    Measuring,
}

/// Seconds of CPU samples needed before a silent job is judged.
pub const MIN_CPU_SAMPLE_SECS: i64 = 60;

/// The job a runner is running.
#[derive(Debug, Clone)]
pub struct Current<'a> {
    /// GitHub's record of it, once read.
    pub job: Option<&'a Job>,
    pub elapsed: i64,
    /// Typical duration, when there are enough samples.
    pub typical: Option<i64>,
    pub step: Option<&'a JobStep>,
    pub step_elapsed: Option<i64>,
    pub step_typical: Option<i64>,
}

impl Current<'_> {
    /// Short name: `Fresh-clone setup` for `Fresh-clone setup (README...)`.
    pub fn name(&self) -> Option<&str> {
        self.job.map(|j| short_name(&j.name))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    /// Waiting its turn behind a job that is moving.
    Turn,
    /// The runner is idle: it should start now.
    Next,
    /// Queued past the pickup limit while the runner sits idle.
    NotPickingUp,
    /// Cannot start: the runner is down or stalled.
    Blocked,
    /// GitHub holds it (an approval or a concurrency group).
    Held,
}

/// Why a runner's queue is not known: its repository's queue was never read,
/// or the last attempt to read it failed (its jobs are those of the last
/// good read, if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueGap<'a> {
    Never,
    Failed(&'a str),
}

#[derive(Debug, Clone)]
pub struct Queued<'a> {
    pub job: &'a Job,
    pub waited: i64,
    pub typical: Option<i64>,
    pub state: QueueState,
}

#[derive(Debug, Clone)]
pub struct Lane<'a> {
    pub runner: &'a Runner,
    pub gh: Option<&'a GhRunner>,
    pub online: Online,
    pub verdict: Verdict,
    pub why: Why,
    pub current: Option<Current<'a>>,
    /// Oldest first.
    pub queue: Vec<Queued<'a>>,
    /// Why `queue` may be missing jobs: it was not read, or not lately.
    pub queue_gap: Option<QueueGap<'a>>,
    /// The job's progress signal and its age in seconds.
    pub signal: Signal,
    pub signal_age: Option<i64>,
    /// The job's CPU over the silent window, once it has been silent that long.
    pub job_cpu: Option<JobCpu>,
    /// When the next queued job should start, and the queue drain.
    pub next_start: Option<i64>,
    pub drains_at: Option<i64>,
    /// When the runner last finished a job.
    pub last_ran: Option<i64>,
    pub history: Option<&'a RepoHistory>,
}

impl Lane<'_> {
    /// The longest wait in its queue.
    pub fn longest_wait(&self) -> Option<i64> {
        self.queue.iter().map(|q| q.waited).max()
    }
}

/// `Fresh-clone setup` for `Fresh-clone setup (README, as written)`.
pub fn short_name(name: &str) -> &str {
    match name.find(" (") {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// `Fresh-clone` for `Fresh-clone setup (README...)`, `Lint` for `Lint & typecheck`.
pub fn tiny_name(name: &str) -> &str {
    short_name(name).split_whitespace().next().unwrap_or(name)
}

/// `setup-node` for `Run actions/setup-node@v4`, `npm ci` for `Run npm ci`;
/// other step names stay as they are.
pub fn step_name(name: &str) -> &str {
    let n = name.strip_prefix("Run ").unwrap_or(name);
    let n = n.split('@').next().unwrap_or(n);
    if !n.contains(' ') {
        n.rsplit('/').next().unwrap_or(n)
    } else {
        n
    }
}

fn typical(h: Option<&RepoHistory>, job: &Job, step: Option<&str>) -> Option<i64> {
    h?.typical(&job.workflow, &job.name, step)
        .filter(|t| t.samples >= MIN_SAMPLES)
        .map(|t: &Typical| t.secs)
}

fn labels_fit(job: &Job, gh: Option<&GhRunner>) -> bool {
    match gh {
        Some(g) if !g.labels.is_empty() => job
            .labels
            .iter()
            .all(|l| g.labels.iter().any(|r| r.eq_ignore_ascii_case(l))),
        _ => true,
    }
}

fn gh_runner<'a>(snap: &'a Snapshot, r: &Runner) -> (Option<&'a GhRunner>, Online) {
    let Some(list) = r.repo.as_ref().and_then(|repo| snap.gh_runners.get(repo)) else {
        return (None, Online::Unknown);
    };
    if list.denied {
        return (None, Online::Denied);
    }
    if list.read_at.is_none() || (list.runners.is_empty() && list.error.is_some()) {
        return (None, Online::Unknown);
    }
    match list.runners.iter().find(|g| g.name == r.name) {
        Some(g) if g.status == "online" => (Some(g), Online::Yes),
        Some(g) => (Some(g), Online::No),
        None => (None, Online::Unregistered),
    }
}

/// One lane per local runner, in discovery order.
pub fn lanes<'a>(snap: &'a Snapshot, th: &RunnerThresholds) -> Vec<Lane<'a>> {
    let now = snap.now;
    let ghs: Vec<(Option<&GhRunner>, Online)> =
        snap.runners.iter().map(|r| gh_runner(snap, r)).collect();
    // Each queued self-hosted job belongs to the first runner of its
    // repository whose labels fit it.
    let mut queues: Vec<Vec<&Job>> = vec![Vec::new(); snap.runners.len()];
    for j in snap
        .jobs
        .iter()
        .filter(|j| j.is_queued() && j.is_self_hosted())
    {
        if let Some(i) = snap.runners.iter().enumerate().position(|(i, r)| {
            r.repo.as_deref() == Some(j.repo.as_str()) && labels_fit(j, ghs[i].0)
        }) {
            queues[i].push(j);
        }
    }
    snap.runners
        .iter()
        .zip(ghs)
        .zip(queues)
        .map(|((r, (gh, online)), mut queue)| {
            queue.sort_by_key(|j| (j.created_at, j.id));
            lane(snap, th, now, r, gh, online, queue)
        })
        .collect()
}

/// Why the queue of `r`'s repository is not known, if it is not.
fn queue_gap<'a>(snap: &'a Snapshot, r: &Runner) -> Option<QueueGap<'a>> {
    match r.repo.as_ref().and_then(|repo| snap.queue_reads.get(repo)) {
        Some(q) => match (&q.error, q.read_at) {
            (Some(e), _) => Some(QueueGap::Failed(e)),
            (None, None) => Some(QueueGap::Never),
            (None, Some(_)) => None,
        },
        None => snap.jobs_read_at.is_none().then_some(QueueGap::Never),
    }
}

fn lane<'a>(
    snap: &'a Snapshot,
    th: &RunnerThresholds,
    now: i64,
    r: &'a Runner,
    gh: Option<&'a GhRunner>,
    online: Online,
    queue: Vec<&'a Job>,
) -> Lane<'a> {
    let history = r.repo.as_ref().and_then(|repo| snap.history.get(repo));
    let gh_job = snap
        .jobs
        .iter()
        .filter(|j| {
            j.status == "in_progress"
                && j.runner.as_deref() == Some(r.name.as_str())
                && r.repo.as_deref().is_none_or(|repo| repo == j.repo)
        })
        .max_by_key(|j| j.started_at);
    let busy_since = r.busy_since();
    let current = if r.busy || gh_job.is_some() {
        let started = gh_job.and_then(|j| j.started_at).or(busy_since);
        let step = gh_job.and_then(Job::current_step);
        Some(Current {
            job: gh_job,
            elapsed: started.map_or(0, |s| (now - s).max(0)),
            typical: gh_job.and_then(|j| typical(history, j, None)),
            step,
            step_elapsed: step
                .filter(|s| s.status == "in_progress")
                .and_then(|s| s.started_at)
                .map(|s| (now - s).max(0)),
            step_typical: gh_job
                .zip(step)
                .and_then(|(j, s)| typical(history, j, Some(&s.name))),
        })
    } else {
        None
    };

    // The newest sign the job is moving, never older than the job itself:
    // a leftover file from the previous job must not count as silence. The
    // job starts when GitHub says it did, else when its worker started.
    let job_start = gh_job.and_then(|j| j.started_at).or(busy_since);
    let (signal, signal_age) = if r.busy {
        let (kind, at) = match (r.step_output_at, r.worker_log_at) {
            (Some(t), _) => (Signal::StepOutput, Some(t)),
            (None, Some(t)) => (Signal::WorkerLog, Some(t)),
            (None, None) => (Signal::None, None),
        };
        let fresh = at.filter(|t| job_start.is_none_or(|b| *t >= b));
        let kind = if fresh.is_some() { kind } else { Signal::None };
        let last = fresh.or(job_start);
        (kind, last.map(|t| (now - t).max(0)))
    } else {
        (Signal::None, None)
    };

    // A queue that failed to read says nothing about a job being picked up.
    let queue_gap = queue_gap(snap, r);
    let jobs_read = match queue_gap {
        Some(_) => i64::MIN,
        None => r
            .repo
            .as_ref()
            .and_then(|repo| snap.queue_reads.get(repo))
            .and_then(|q| q.read_at)
            .or(snap.jobs_read_at)
            .unwrap_or(i64::MIN),
    };
    // Measured from when the runner went idle too: a job that waited an hour
    // behind a busy runner is only late once the runner has been free.
    let unpicked = |j: &Job| -> Option<i64> {
        if j.status != "queued" || r.busy || gh_job.is_some() {
            return None;
        }
        let since = j.created_at?.max(r.idle_since.unwrap_or(i64::MIN));
        (jobs_read >= since + th.pickup_secs).then(|| now - j.created_at.unwrap_or(since))
    };

    // Silence alone is not stuck: a quiet step (a test suite, a long build)
    // writes nothing for minutes while its processes work.
    let silent = r.busy && signal_age.is_some_and(|a| a > th.silent_secs);
    let job_cpu = silent.then(|| {
        if r.scanned_at.is_none() || r.job_in_container {
            return JobCpu::Unseen;
        }
        match r.job_cpu_since(now - th.silent_secs) {
            Some((used, covered)) if covered >= MIN_CPU_SAMPLE_SECS => {
                // Scale a shorter sample up to the whole window.
                let over_window =
                    used as f64 * th.silent_secs as f64 / covered.min(th.silent_secs) as f64;
                if over_window < th.idle_cpu_ms as f64 {
                    JobCpu::Idle(used)
                } else {
                    JobCpu::Busy(used)
                }
            }
            _ => JobCpu::Measuring,
        }
    });

    let (verdict, why) = if r.scanned_at.is_some() && r.listener_secs.is_none() {
        (Verdict::Down, Why::NoListener)
    } else if online == Online::No {
        (Verdict::Down, Why::Offline)
    } else if matches!(job_cpu, Some(JobCpu::Idle(_) | JobCpu::Unseen)) {
        (Verdict::Stalled, Why::Silent(signal_age.unwrap_or(0)))
    } else if !r.busy
        && gh_job.is_some()
        && r.scanned_at.is_some()
        && r.idle_since.is_some_and(|t| jobs_read >= t + GH_LAG_SECS)
    {
        (Verdict::Stalled, Why::NoWorker)
    } else if let Some(waited) = queue.iter().filter_map(|j| unpicked(j)).max() {
        (Verdict::NotPickingUp, Why::Unpicked(waited))
    } else if r.busy
        && current.as_ref().is_some_and(|c| {
            c.typical
                .is_some_and(|t| c.elapsed as f64 > th.overdue_factor * t as f64)
        })
    {
        (Verdict::Slow, Why::Overdue)
    } else if current.is_some() {
        (Verdict::Moving, Why::None)
    } else {
        (Verdict::Idle, Why::None)
    };

    let queue: Vec<Queued> = queue
        .into_iter()
        .map(|j| {
            let state = if j.status == "waiting" {
                QueueState::Held
            } else if matches!(verdict, Verdict::Down | Verdict::Stalled) {
                QueueState::Blocked
            } else if unpicked(j).is_some() {
                QueueState::NotPickingUp
            } else if current.is_some() {
                QueueState::Turn
            } else {
                QueueState::Next
            };
            Queued {
                job: j,
                waited: j.created_at.map_or(0, |c| (now - c).max(0)),
                typical: typical(history, j, None),
                state,
            }
        })
        .collect();

    // When the running job should end: its typical duration, at once when
    // it is already past that but not yet slow, unknown once it is slow.
    let next_start = match verdict {
        Verdict::Moving => current.as_ref().and_then(|c| {
            c.typical
                .filter(|_| c.job.is_some())
                .map(|t| now + (t - c.elapsed).max(0))
        }),
        Verdict::Idle | Verdict::NotPickingUp => Some(now),
        _ => None,
    }
    .filter(|_| !queue.is_empty());
    let drains_at = next_start.and_then(|start| {
        queue
            .iter()
            .filter(|q| q.state != QueueState::Held)
            .try_fold(start, |at, q| q.typical.map(|t| at + t))
    });

    let last_ran = history.and_then(|h| {
        h.jobs
            .iter()
            .filter(|j| j.runner == r.name)
            .filter_map(|j| j.completed_at)
            .max()
    });

    Lane {
        runner: r,
        gh,
        online,
        verdict,
        why,
        current,
        queue,
        queue_gap,
        signal,
        signal_age,
        job_cpu,
        next_start,
        drains_at,
        last_ran,
        history,
    }
}

/// What the runner panel's title says about the fleet of runners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fleet {
    /// This many runners are down, stalled or not picking up.
    Stuck(usize),
    /// This many runners are slow.
    Slow(usize),
    /// Something runs and nothing is wrong.
    Moving,
    /// Nothing runs.
    Quiet,
}

pub fn fleet(lanes: &[Lane]) -> Fleet {
    let n = |f: fn(Verdict) -> bool| lanes.iter().filter(|l| f(l.verdict)).count();
    let stuck = n(Verdict::is_stuck);
    let slow = n(|v| v == Verdict::Slow);
    if stuck > 0 {
        Fleet::Stuck(stuck)
    } else if slow > 0 {
        Fleet::Slow(slow)
    } else if lanes.iter().any(|l| l.current.is_some()) {
        Fleet::Moving
    } else {
        Fleet::Quiet
    }
}

/// The runner's recent load, from the history crowsnest holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    /// Share of the window the runner was busy, in percent.
    pub busy_pct: Option<f64>,
    /// Start of that window: six hours ago, or the oldest run read.
    pub busy_from: i64,
    /// Jobs started since `since`.
    pub jobs: usize,
    /// Local midnight, or the oldest run read when that is later.
    pub since: i64,
    pub today: bool,
    pub wait_median: Option<i64>,
    pub wait_max: Option<i64>,
}

pub fn stats(lane: &Lane, now: i64) -> Option<Stats> {
    let h = lane.history?;
    let covered = h.covered_since.unwrap_or(now);
    let mine: Vec<_> = h
        .jobs
        .iter()
        .filter(|j| j.runner == lane.runner.name)
        .collect();
    let busy_from = (now - BUSY_WINDOW_SECS).max(covered);
    let mut spans: Vec<(i64, i64)> = mine
        .iter()
        .filter_map(|j| Some((j.started_at?, j.completed_at?)))
        .collect();
    if let Some(c) = &lane.current {
        spans.push((now - c.elapsed, now));
    }
    spans.sort_unstable();
    let (mut busy, mut reach) = (0, busy_from);
    for (a, b) in spans {
        let (a, b) = (a.max(reach), b.min(now));
        if b > a {
            busy += b - a;
            reach = b;
        }
    }
    let window = now - busy_from;
    let midnight = crate::timefmt::start_of_day(now);
    let since = midnight.max(covered);
    let today: Vec<_> = mine
        .iter()
        .filter(|j| j.started_at.is_some_and(|s| s >= since))
        .collect();
    let mut waits: Vec<i64> = today.iter().filter_map(|j| j.waited_secs()).collect();
    waits.sort_unstable();
    let current_today = lane
        .current
        .as_ref()
        .is_some_and(|c| now - c.elapsed >= since);
    Some(Stats {
        busy_pct: (window >= 600).then(|| busy as f64 * 100.0 / window as f64),
        busy_from,
        jobs: today.len() + usize::from(current_today),
        since,
        today: since == midnight,
        wait_median: (!waits.is_empty()).then(|| {
            let n = waits.len();
            if n % 2 == 1 {
                waits[n / 2]
            } else {
                (waits[n / 2 - 1] + waits[n / 2]) / 2
            }
        }),
        wait_max: waits.last().copied(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{QueueRead, RepoRunners, RunInfo};

    const NOW: i64 = 1_000_000;
    const MIN: i64 = 60;

    fn runner() -> Runner {
        Runner {
            name: "pantry-local-runner".into(),
            dir: "/srv/actions-runner-pantry".into(),
            repo: Some("acme/pantry".into()),
            scanned_at: Some(NOW),
            listener_secs: Some(16 * 86_400),
            idle_since: Some(NOW - 3600),
            ..Default::default()
        }
    }

    fn busy(r: &mut Runner, secs: i64) {
        r.busy = true;
        r.busy_secs = Some(secs);
        r.idle_since = None;
        r.worker_log_at = Some(NOW - 4);
        r.step_output_at = Some(NOW - 1);
        // Half a CPU-second over the last half hour: an idle job.
        r.job_cpu = vec![
            (NOW - 30 * MIN, 9_000),
            (NOW - 15 * MIN, 9_200),
            (NOW, 9_500),
        ];
    }

    fn job(name: &str, status: &str, created_ago: i64) -> Job {
        Job {
            repo: "acme/pantry".into(),
            name: name.into(),
            workflow: "CI".into(),
            status: status.into(),
            id: created_ago as u64,
            created_at: Some(NOW - created_ago),
            labels: vec!["self-hosted".into(), "pantry".into()],
            run: Some(RunInfo {
                number: 312,
                event: "pull_request".into(),
                pr: Some(123),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn running(name: &str, elapsed: i64) -> Job {
        let mut j = job(name, "in_progress", 2 * 3600);
        j.runner = Some("pantry-local-runner".into());
        j.started_at = Some(NOW - elapsed);
        j
    }

    fn typ(job: &str, secs: i64, samples: usize) -> Typical {
        Typical {
            workflow: "CI".into(),
            job: job.into(),
            step: None,
            secs,
            samples,
        }
    }

    /// A busy runner 7m into Fresh-clone (typically 12m) with Lint queued
    /// for 1h54m behind it.
    fn snap() -> Snapshot {
        let mut r = runner();
        busy(&mut r, 7 * MIN);
        let mut s = Snapshot {
            now: NOW,
            runners: vec![r],
            jobs: vec![
                running("Fresh-clone setup (README, as written)", 7 * MIN),
                job("Lint & typecheck", "queued", 114 * MIN),
            ],
            jobs_read_at: Some(NOW - 10),
            ..Default::default()
        };
        s.history.insert(
            "acme/pantry".into(),
            RepoHistory {
                typical: vec![
                    typ("Fresh-clone setup (README, as written)", 12 * MIN, 5),
                    typ("Lint & typecheck", 12 * MIN, 5),
                ],
                ..Default::default()
            },
        );
        s
    }

    fn one(s: &Snapshot) -> Lane<'_> {
        lanes(s, &RunnerThresholds::default()).remove(0)
    }

    #[test]
    fn busy_and_moving_with_a_queue_waiting_its_turn() {
        let s = snap();
        let l = one(&s);
        assert_eq!((l.verdict, l.why), (Verdict::Moving, Why::None));
        let c = l.current.as_ref().unwrap();
        assert_eq!((c.elapsed, c.typical), (7 * MIN, Some(12 * MIN)));
        assert_eq!(c.name(), Some("Fresh-clone setup"));
        assert_eq!((l.signal, l.signal_age), (Signal::StepOutput, Some(1)));
        assert_eq!(l.queue.len(), 1);
        assert_eq!(l.queue[0].state, QueueState::Turn);
        assert_eq!(l.queue[0].waited, 114 * MIN);
        // Lint starts when Fresh-clone's typical 12m is up, 5m from now, and
        // runs its own typical 12m.
        assert_eq!(l.next_start, Some(NOW + 5 * MIN));
        assert_eq!(l.drains_at, Some(NOW + 17 * MIN));
        assert_eq!(fleet(&[l]), Fleet::Moving);
    }

    #[test]
    fn estimates_add_up_the_jobs_ahead() {
        let mut s = snap();
        s.jobs.push(job("Docker dev workflow", "queued", 100 * MIN));
        s.jobs.push(job("Unit + e2e tests", "queued", 110 * MIN));
        let h = s.history.get_mut("acme/pantry").unwrap();
        h.typical.push(typ("Docker dev workflow", MIN, 4));
        h.typical.push(typ("Unit + e2e tests", 40 * MIN, 3));
        let l = one(&s);
        let names: Vec<&str> = l.queue.iter().map(|q| q.job.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Lint & typecheck",
                "Unit + e2e tests",
                "Docker dev workflow"
            ],
            "oldest first"
        );
        assert_eq!(l.next_start, Some(NOW + 5 * MIN));
        assert_eq!(l.drains_at, Some(NOW + (5 + 12 + 40 + 1) * MIN));
        // One job ahead without a typical duration leaves the drain unknown.
        s.history.get_mut("acme/pantry").unwrap().typical.pop();
        let l = one(&s);
        assert_eq!(l.next_start, Some(NOW + 5 * MIN));
        assert_eq!(l.drains_at, None);
    }

    #[test]
    fn a_job_just_past_its_typical_time_ends_any_moment() {
        let mut s = snap();
        s.jobs[0].started_at = Some(NOW - 15 * MIN);
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::Moving);
        assert_eq!(l.next_start, Some(NOW));
    }

    #[test]
    fn too_few_samples_mean_elapsed_only_never_slow() {
        let mut s = snap();
        s.jobs[0].started_at = Some(NOW - 90 * MIN);
        s.history.get_mut("acme/pantry").unwrap().typical[0].samples = 2;
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::Moving);
        assert_eq!(l.current.as_ref().unwrap().typical, None);
        assert_eq!((l.next_start, l.drains_at), (None, None));
    }

    #[test]
    fn overdue_but_still_producing_output_is_slow() {
        let mut s = snap();
        s.jobs[0].started_at = Some(NOW - 25 * MIN);
        let l = one(&s);
        assert_eq!((l.verdict, l.why), (Verdict::Slow, Why::Overdue));
        assert_eq!(l.queue[0].state, QueueState::Turn);
        assert_eq!(l.next_start, None, "no end can be promised");
        assert_eq!(fleet(&[l]), Fleet::Slow(1));
        // The factor is configurable.
        let th = RunnerThresholds {
            overdue_factor: 3.0,
            ..Default::default()
        };
        assert_eq!(lanes(&s, &th)[0].verdict, Verdict::Moving);
    }

    #[test]
    fn silent_past_the_limit_is_stalled() {
        let mut s = snap();
        s.jobs[0].started_at = Some(NOW - 24 * MIN);
        s.runners[0].busy_secs = Some(24 * MIN);
        s.jobs[0].started_at = Some(NOW - 24 * MIN);
        s.runners[0].step_output_at = Some(NOW - 14 * MIN);
        let l = one(&s);
        assert_eq!(
            (l.verdict, l.why),
            (Verdict::Stalled, Why::Silent(14 * MIN))
        );
        assert_eq!(l.queue[0].state, QueueState::Blocked);
        assert_eq!((l.next_start, l.drains_at), (None, None));
        assert_eq!(fleet(&[l]), Fleet::Stuck(1));
        // Nine minutes of silence is under the default limit, and the limit
        // is configurable.
        s.runners[0].step_output_at = Some(NOW - 9 * MIN);
        assert_eq!(one(&s).verdict, Verdict::Moving);
        let th = RunnerThresholds {
            silent_secs: 5 * MIN,
            ..Default::default()
        };
        assert_eq!(lanes(&s, &th)[0].verdict, Verdict::Stalled);
    }

    #[test]
    fn a_silent_job_whose_processes_work_is_quiet_not_stalled() {
        let mut s = snap();
        s.jobs[0].started_at = Some(NOW - 24 * MIN);
        s.runners[0].busy_secs = Some(24 * MIN);
        s.jobs[0].started_at = Some(NOW - 24 * MIN);
        s.runners[0].step_output_at = Some(NOW - 14 * MIN);
        // A browser test suite: minutes of CPU, nothing printed.
        s.runners[0].job_cpu = vec![
            (NOW - 20 * MIN, 60_000),
            (NOW - 5 * MIN, 200_000),
            (NOW, 260_000),
        ];
        let l = one(&s);
        assert_eq!((l.verdict, l.why), (Verdict::Moving, Why::None));
        // 200s used since NOW - 10m: the sample at or before the window start
        // is NOW - 20m, and its increases count.
        assert_eq!(l.job_cpu, Some(JobCpu::Busy(200_000)));
        assert_eq!(l.queue[0].state, QueueState::Turn);
        // The idle limit is configurable.
        let th = RunnerThresholds {
            idle_cpu_ms: 500_000,
            ..Default::default()
        };
        assert_eq!(lanes(&s, &th)[0].verdict, Verdict::Stalled);
    }

    #[test]
    fn a_container_job_falls_back_to_silence_alone() {
        let mut s = snap();
        s.runners[0].busy_secs = Some(24 * MIN);
        s.jobs[0].started_at = Some(NOW - 24 * MIN);
        s.runners[0].step_output_at = Some(NOW - 14 * MIN);
        s.runners[0].job_cpu = vec![(NOW - 20 * MIN, 0), (NOW, 900_000)];
        s.runners[0].job_in_container = true;
        let l = one(&s);
        assert_eq!(
            (l.verdict, l.job_cpu),
            (Verdict::Stalled, Some(JobCpu::Unseen))
        );
    }

    #[test]
    fn a_silent_job_is_not_judged_before_a_minute_of_cpu_samples() {
        let mut s = snap();
        s.runners[0].busy_secs = Some(24 * MIN);
        s.jobs[0].started_at = Some(NOW - 24 * MIN);
        s.runners[0].step_output_at = Some(NOW - 14 * MIN);
        s.runners[0].job_cpu = vec![(NOW - 30, 100), (NOW, 100)];
        let l = one(&s);
        assert_eq!(
            (l.verdict, l.job_cpu),
            (Verdict::Moving, Some(JobCpu::Measuring))
        );
        // Two idle minutes are scaled to the window: still idle.
        s.runners[0].job_cpu = vec![(NOW - 2 * MIN, 100), (NOW, 300)];
        assert_eq!(one(&s).verdict, Verdict::Stalled);
        // Two minutes at half a core are busy.
        s.runners[0].job_cpu = vec![(NOW - 2 * MIN, 0), (NOW, 60_000)];
        assert_eq!(one(&s).verdict, Verdict::Moving);
    }

    #[test]
    fn cpu_of_exited_processes_is_not_negative() {
        let r = Runner {
            job_cpu: vec![(0, 5_000), (15, 9_000), (30, 2_000), (45, 2_500)],
            ..Default::default()
        };
        // 4s, then a process exited (no change counted), then 0.5s.
        assert_eq!(r.job_cpu_since(0), Some((4_500, 45)));
        assert_eq!(r.job_cpu_since(20), Some((500, 25)));
        assert_eq!(Runner::default().job_cpu_since(0), None);
    }

    #[test]
    fn a_leftover_page_from_the_last_job_is_not_silence() {
        let mut s = snap();
        // The job started 2m ago; the newest page is from the job before.
        s.runners[0].busy_secs = Some(2 * MIN);
        s.jobs[0].started_at = Some(NOW - 2 * MIN);
        s.runners[0].step_output_at = Some(NOW - 30 * MIN);
        s.runners[0].worker_log_at = Some(NOW - 30 * MIN);
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::Moving);
        assert_eq!((l.signal, l.signal_age), (Signal::None, Some(2 * MIN)));
        // Twelve minutes in with nothing written since it started: stalled.
        s.runners[0].busy_secs = Some(12 * MIN);
        s.jobs[0].started_at = Some(NOW - 12 * MIN);
        assert_eq!(one(&s).verdict, Verdict::Stalled);
    }

    #[test]
    fn without_step_output_the_worker_log_is_the_signal() {
        let mut s = snap();
        s.runners[0].step_output_at = None;
        s.runners[0].worker_log_at = Some(NOW - 4);
        let l = one(&s);
        assert_eq!((l.signal, l.signal_age), (Signal::WorkerLog, Some(4)));
        s.runners[0].worker_log_at = Some(NOW - 11 * MIN);
        s.runners[0].busy_secs = Some(20 * MIN);
        s.jobs[0].started_at = Some(NOW - 20 * MIN);
        assert_eq!(one(&s).verdict, Verdict::Stalled);
    }

    #[test]
    fn github_running_a_job_no_worker_runs_is_stalled_once_github_caught_up() {
        let mut s = snap();
        let r = &mut s.runners[0];
        r.busy = false;
        r.busy_secs = None;
        r.idle_since = Some(NOW - 90);
        // GitHub was read 30s after the worker went: it may not know yet.
        s.jobs_read_at = Some(NOW - 60);
        assert_eq!(one(&s).verdict, Verdict::Moving);
        // Read again well after: still in progress at GitHub, so stalled.
        s.jobs_read_at = Some(NOW - 10);
        let l = one(&s);
        assert_eq!((l.verdict, l.why), (Verdict::Stalled, Why::NoWorker));
        // Without a readable process table nothing is claimed.
        s.runners[0].scanned_at = None;
        assert_eq!(one(&s).verdict, Verdict::Moving);
    }

    #[test]
    fn queued_past_the_limit_while_idle_is_not_picking_up() {
        let mut s = snap();
        s.jobs.remove(0);
        let r = &mut s.runners[0];
        r.busy = false;
        r.busy_secs = None;
        // Lint was created 1h54m ago, but the runner only went idle 3m ago.
        r.idle_since = Some(NOW - 3 * MIN);
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::Idle);
        assert_eq!(l.queue[0].state, QueueState::Next);
        assert_eq!(l.next_start, Some(NOW));
        assert_eq!(l.drains_at, Some(NOW + 12 * MIN));
        // Idle for 6m, and GitHub still had it queued at the last read.
        s.runners[0].idle_since = Some(NOW - 6 * MIN);
        let l = one(&s);
        assert_eq!(
            (l.verdict, l.why),
            (Verdict::NotPickingUp, Why::Unpicked(114 * MIN))
        );
        assert_eq!(l.queue[0].state, QueueState::NotPickingUp);
        assert_eq!(fleet(&[l]), Fleet::Stuck(1));
        // A read from before the limit passed proves nothing yet.
        s.jobs_read_at = Some(NOW - 2 * MIN);
        assert_eq!(one(&s).verdict, Verdict::Idle);
        // The limit is configurable.
        s.jobs_read_at = Some(NOW);
        let th = RunnerThresholds {
            pickup_secs: 10 * MIN,
            ..Default::default()
        };
        assert_eq!(lanes(&s, &th)[0].verdict, Verdict::Idle);
    }

    #[test]
    fn a_queue_that_failed_to_read_is_unknown_and_never_not_picking_up() {
        let mut s = snap();
        s.jobs.remove(0);
        let r = &mut s.runners[0];
        r.busy = false;
        r.busy_secs = None;
        r.idle_since = Some(NOW - 6 * MIN);
        // Read in full: the job queued for nearly two hours is not picked up.
        let repo = r.repo.clone().unwrap();
        s.queue_reads.insert(
            repo.clone(),
            QueueRead {
                read_at: Some(NOW - 10),
                error: None,
            },
        );
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::NotPickingUp);
        assert_eq!(l.queue_gap, None);
        // The repository's last read failed: its jobs are from an older read,
        // which proves nothing about the runner now.
        s.queue_reads.insert(
            repo.clone(),
            QueueRead {
                read_at: Some(NOW - 10),
                error: Some("gh: HTTP 502".into()),
            },
        );
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::Idle);
        assert_eq!(l.queue_gap, Some(QueueGap::Failed("gh: HTTP 502")));
        // Another repository failing says nothing about this one.
        s.queue_reads.clear();
        s.queue_reads.insert(
            "acme/gone".into(),
            QueueRead {
                read_at: None,
                error: Some("gh: HTTP 404".into()),
            },
        );
        s.queue_reads.insert(
            repo,
            QueueRead {
                read_at: Some(NOW - 10),
                error: None,
            },
        );
        assert_eq!(one(&s).verdict, Verdict::NotPickingUp);
        // Never read: unknown too.
        s.queue_reads.clear();
        s.jobs_read_at = None;
        assert_eq!(one(&s).queue_gap, Some(QueueGap::Never));
    }

    #[test]
    fn jobs_held_by_github_or_meant_for_other_runners_never_alarm() {
        let mut s = snap();
        s.jobs.remove(0);
        s.runners[0].busy = false;
        s.runners[0].idle_since = Some(NOW - 3600);
        s.jobs[0].status = "waiting".into();
        let mut hosted = job("Dependency review", "queued", 3600);
        hosted.labels = vec!["ubuntu-latest".into()];
        s.jobs.push(hosted);
        let mut other = job("Arm build", "queued", 3600);
        other.labels = vec!["self-hosted".into(), "arm64".into()];
        s.jobs.push(other);
        s.gh_runners.insert(
            "acme/pantry".into(),
            RepoRunners {
                read_at: Some(NOW),
                runners: vec![GhRunner {
                    name: "pantry-local-runner".into(),
                    status: "online".into(),
                    labels: vec!["self-hosted".into(), "Linux".into(), "pantry".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let l = one(&s);
        assert_eq!(l.verdict, Verdict::Idle);
        assert_eq!(l.queue.len(), 1, "only the held Lint is this runner's");
        assert_eq!(l.queue[0].state, QueueState::Held);
        assert_eq!(l.online, Online::Yes);
    }

    #[test]
    fn offline_or_without_a_listener_is_down() {
        let mut s = snap();
        s.runners[0].listener_secs = None;
        let l = one(&s);
        assert_eq!((l.verdict, l.why), (Verdict::Down, Why::NoListener));
        assert_eq!(l.queue[0].state, QueueState::Blocked);
        // An unreadable process table says nothing about the listener.
        s.runners[0].scanned_at = None;
        assert_eq!(one(&s).verdict, Verdict::Moving);
        s.gh_runners.insert(
            "acme/pantry".into(),
            RepoRunners {
                read_at: Some(NOW),
                runners: vec![GhRunner {
                    name: "pantry-local-runner".into(),
                    status: "offline".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let l = one(&s);
        assert_eq!(
            (l.verdict, l.why, l.online),
            (Verdict::Down, Why::Offline, Online::No)
        );
        // Without admin read the runner list is denied: nothing is claimed.
        s.gh_runners.get_mut("acme/pantry").unwrap().denied = true;
        let l = one(&s);
        assert_eq!((l.verdict, l.online), (Verdict::Moving, Online::Denied));
    }

    #[test]
    fn the_title_counts_the_worst_first() {
        let mut s = snap();
        let mut idle = runner();
        idle.name = "bookshop-runner".into();
        idle.repo = Some("acme/bookshop".into());
        s.runners.push(idle);
        let mut down = runner();
        down.name = "millwork-runner".into();
        down.repo = Some("acme/millwork".into());
        down.listener_secs = None;
        s.runners.push(down);
        let ls = lanes(&s, &RunnerThresholds::default());
        assert_eq!(fleet(&ls), Fleet::Stuck(1));
        assert_eq!(fleet(&ls[..2]), Fleet::Moving);
        assert_eq!(fleet(&ls[1..2]), Fleet::Quiet);
    }

    #[test]
    fn names() {
        assert_eq!(
            short_name("Fresh-clone setup (README, as written)"),
            "Fresh-clone setup"
        );
        assert_eq!(
            tiny_name("Fresh-clone setup (README, as written)"),
            "Fresh-clone"
        );
        assert_eq!(tiny_name("Lint & typecheck"), "Lint");
        assert_eq!(step_name("Run actions/setup-node@v4"), "setup-node");
        assert_eq!(step_name("Run npm ci"), "npm ci");
        assert_eq!(step_name("Set up job"), "Set up job");
    }

    #[test]
    fn busy_share_and_waits_over_the_history_held() {
        use crate::model::DoneJob;
        let mut s = snap();
        let done = |start_ago: i64, ran: i64, waited: i64| DoneJob {
            runner: "pantry-local-runner".into(),
            created_at: Some(NOW - start_ago - waited),
            started_at: Some(NOW - start_ago),
            completed_at: Some(NOW - start_ago + ran),
            ..Default::default()
        };
        let h = s.history.get_mut("acme/pantry").unwrap();
        // Three hours read: two 30m jobs, plus the current 7m one.
        h.covered_since = Some(NOW - 3 * 3600);
        h.jobs = vec![
            done(60 * MIN, 30 * MIN, 10 * MIN),
            done(150 * MIN, 30 * MIN, 50 * MIN),
        ];
        let l = one(&s);
        let st = stats(&l, NOW).unwrap();
        assert_eq!(st.busy_from, NOW - 3 * 3600);
        let pct = st.busy_pct.unwrap();
        assert!((pct - 67.0 * 100.0 / 180.0).abs() < 0.01, "{pct}");
        assert_eq!(
            (st.wait_median, st.wait_max),
            (Some(30 * MIN), Some(50 * MIN))
        );
    }
}
