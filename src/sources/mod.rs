//! Data collection. Each source turns one read-only command or file into
//! crowsnest's model types. Nothing here knows about the UI, so these
//! functions can back another front end, or be replaced by a single
//! firstmate-provided collector, without touching `ui`.

pub mod fleet;
pub mod gh;
pub mod ghapi;
pub mod history;
pub mod host;
pub mod nm;
pub mod quota;
pub mod runners;

use crate::exec::ReadCommand;
use crate::model::{
    Fleet, Job, Pipeline, PrChecks, QueueRead, QuotaWindow, RepoRunners, Runner, Service,
};
use ghapi::{Client, GhError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Read the fleet through firstmate's documented snapshot command.
pub fn read_fleet(fm_home: &Path, cache_dir: &Path) -> Result<Fleet, String> {
    let out = ReadCommand::FleetSnapshot {
        fm_home: fm_home.to_path_buf(),
        cache_dir: cache_dir.to_path_buf(),
    }
    .run()
    .map_err(|e| format!("fleet snapshot: {e}"))?;
    fleet::parse(&out.stdout)
}

/// Worktrees whose no-mistakes pipeline should be read, keyed by task id.
pub fn pipeline_targets(fleet: &Fleet) -> Vec<(String, PathBuf)> {
    fleet
        .workers
        .iter()
        .filter(|w| w.mode == "no-mistakes" && w.parent.is_none())
        .filter_map(|w| Some((w.id.clone(), w.worktree.clone()?)))
        .filter(|(_, p)| p.is_dir())
        .collect()
}

/// Read one task's pipeline. `Ok(None)` means the worktree has no run.
pub fn read_pipeline(worktree: &Path) -> Result<Option<Pipeline>, String> {
    let out = ReadCommand::NoMistakesStatus {
        worktree: worktree.to_path_buf(),
    }
    .run()
    .map_err(|e| format!("no-mistakes: {e}"))?;
    Ok(nm::parse(&out.stdout).map(|mut p| {
        p.read_at = Some(crate::timefmt::now_epoch());
        p
    }))
}

/// PR URLs shown on screen, from the fleet and from pipelines.
pub fn pr_urls(fleet: Option<&Fleet>, pipelines: &BTreeMap<String, Pipeline>) -> Vec<String> {
    let mut urls: Vec<String> = fleet
        .into_iter()
        // An idle secondmate's PR is history from its last report.
        .flat_map(|f| {
            f.workers
                .iter()
                .filter(|w| w.fm_state != "no_active_work")
                .filter_map(|w| w.pr_url.clone())
        })
        .chain(pipelines.values().filter_map(|p| p.pr_url.clone()))
        .filter(|u| gh::parse_pr_url(u).is_some())
        .collect();
    urls.sort();
    urls.dedup();
    urls
}

/// Check rollups of the PRs that resolved, keyed by the input URL, plus one
/// error per PR GitHub could not resolve. `Err` means nothing resolved.
pub fn read_pr_checks(
    gh: &mut Client,
    urls: &[String],
) -> Result<(Vec<PrChecks>, Vec<String>), GhError> {
    let Some(query) = gh::checks_query(urls) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let (body, failure) = gh.graphql(query)?;
    match gh::parse_checks(&body, urls) {
        Ok((prs, mut errors)) => {
            if let Some(f) = failure
                && errors.is_empty()
            {
                errors.push(f);
            }
            Ok((prs, errors))
        }
        Err(e) => Err(GhError::Failed(failure.unwrap_or(e))),
    }
}

/// The repositories local runners are registered to.
pub fn runner_repos(runners: &[Runner]) -> Vec<String> {
    let mut repos: Vec<String> = runners.iter().filter_map(|r| r.repo.clone()).collect();
    repos.sort();
    repos.dedup();
    repos
}

/// The jobs of `runs` of their repositories, each carrying the run it
/// belongs to, read in parallel. An unchanged list is GitHub's 304, which
/// costs nothing; a finished run's jobs never change and keep no ETag.
fn read_runs_jobs(
    gh: &mut Client,
    runs: &[(String, crate::model::RunInfo)],
    finished: bool,
) -> Vec<Result<Vec<Job>, GhError>> {
    let reads: Vec<(String, bool)> = runs
        .iter()
        .map(|(repo, run)| {
            (
                format!("repos/{repo}/actions/runs/{}/jobs?per_page=50", run.id),
                !finished,
            )
        })
        .collect();
    gh.get_many(&reads)
        .into_iter()
        .zip(runs)
        .map(|(out, (repo, run))| {
            let mut jobs = gh::parse_jobs(repo, &out?).map_err(GhError::Failed)?;
            for j in &mut jobs {
                j.run = Some(run.clone());
            }
            Ok(jobs)
        })
        .collect()
}

/// How many queued, and how many running, workflow runs of a repository
/// are read: one page of each, GitHub's largest. Unchanged lists and job
/// lists are 304s, which cost nothing, so reading them all stays cheap.
pub const RUNS_PAGE: usize = 100;

/// What the queue read found.
#[derive(Debug, Default)]
pub struct Queue {
    pub jobs: Vec<Job>,
    /// Active runs per repository beyond the page that were not read.
    pub unread: BTreeMap<String, u64>,
    /// How each repository's queue was read: when it was last read in full,
    /// and what failed this turn. A repository that failed keeps the jobs
    /// of its last good read in `jobs`.
    pub reads: BTreeMap<String, QueueRead>,
    /// When a repository's queue was read in full this turn (also when
    /// there was none to read), else `None`.
    pub read_at: Option<i64>,
}

/// What the last good read of one repository's queue found, kept so that a
/// failing read does not blank it.
#[derive(Debug, Clone, Default)]
pub struct KeptQueue {
    pub jobs: Vec<Job>,
    pub unread: u64,
    pub read_at: Option<i64>,
}

/// The run states the queue lists.
const ACTIVE: [&str; 2] = ["queued", "in_progress"];

/// What reading one repository's queued and running runs found.
type RunList = Result<(Vec<crate::model::RunInfo>, u64), GhError>;

/// Each repository's queued and running runs, or why they were not read.
/// The lists of every repository and state are asked together, 8 at a time,
/// not one `gh` process after another.
fn read_run_lists(gh: &mut Client, repos: &[String]) -> Vec<RunList> {
    let reads: Vec<(String, bool)> = repos
        .iter()
        .flat_map(|repo| {
            ACTIVE.map(|status| {
                (
                    format!("repos/{repo}/actions/runs?status={status}&per_page={RUNS_PAGE}"),
                    true,
                )
            })
        })
        .collect();
    let mut answers = gh.get_many(&reads).into_iter();
    repos
        .iter()
        .map(|_| {
            let outs: Vec<_> = answers.by_ref().take(ACTIVE.len()).collect();
            let mut runs = Vec::new();
            let mut unread = 0;
            for out in outs {
                let out = out?;
                let page = gh::parse_runs(&out).map_err(GhError::Failed)?;
                unread += gh::total_count(&out)
                    .unwrap_or(0)
                    .saturating_sub(page.len() as u64);
                runs.extend(page);
            }
            runs.sort_by_key(|r| r.id);
            runs.dedup_by_key(|r| r.id);
            Ok((runs, unread))
        })
        .collect()
}

/// Every job of the queued and running workflow runs of each repository
/// that has a local runner: queued, running, and already finished ones.
///
/// Each repository is read on its own. One that cannot be read (a deleted
/// or private repository, an organisation that needs SSO, a failed job
/// list) keeps what `kept` holds of its last good read, or of the runs whose
/// jobs did answer, and is reported in `reads` with its error: it never
/// blanks the queue of the others. `Err` only when GitHub refused every
/// repository for rate limit.
pub fn read_queue(
    gh: &mut Client,
    runners: &[Runner],
    kept: &mut BTreeMap<String, KeptQueue>,
) -> Result<Queue, GhError> {
    let repos = runner_repos(runners);
    kept.retain(|repo, _| repos.contains(repo));
    let lists = read_run_lists(gh, &repos);
    // The jobs of every repository's runs, 8 at a time.
    let active: Vec<(String, crate::model::RunInfo)> = repos
        .iter()
        .zip(&lists)
        .filter_map(|(repo, l)| l.as_ref().ok().map(|(runs, _)| (repo, runs)))
        .flat_map(|(repo, runs)| runs.iter().map(|r| (repo.clone(), r.clone())))
        .collect();
    let mut jobs = read_runs_jobs(gh, &active, false).into_iter();
    let now = crate::timefmt::now_epoch();
    let mut q = Queue::default();
    let mut paused = Vec::new();
    for (repo, list) in repos.iter().zip(lists) {
        let old = kept.get(repo).cloned().unwrap_or_default();
        let n = list.as_ref().map_or(0, |(runs, _)| runs.len());
        let answers: Vec<_> = jobs.by_ref().take(n).collect();
        let mine = merge_repo(&old, list, answers, now);
        if mine.whole {
            q.read_at = Some(now);
        }
        paused.extend(mine.paused);
        if mine.unread > 0 {
            q.unread.insert(repo.clone(), mine.unread);
        }
        kept.insert(
            repo.clone(),
            KeptQueue {
                jobs: mine.jobs.clone(),
                unread: mine.unread,
                read_at: mine.read.read_at,
            },
        );
        q.jobs.extend(mine.jobs);
        q.reads.insert(repo.clone(), mine.read);
    }
    if repos.is_empty() {
        q.read_at = Some(now);
    }
    // Nothing was read because GitHub refused: the last data stays as it is.
    if q.read_at.is_none() && !paused.is_empty() && q.reads.values().all(|r| r.error.is_none()) {
        return Err(GhError::Paused {
            until: paused.into_iter().max().unwrap_or(now),
        });
    }
    Ok(q)
}

/// What one repository's read came to.
struct Merged {
    jobs: Vec<Job>,
    unread: u64,
    read: QueueRead,
    /// Everything answered: the queue was read in full.
    whole: bool,
    /// Where GitHub refused for rate limit, until when.
    paused: Vec<i64>,
}

/// One repository's queue from this turn's answers and its last good read:
/// the run lists, then the jobs of each run listed. What did not answer
/// (the lists, or one run's jobs) is taken from `old`, so a failing read
/// never blanks what was known; the error is kept for the screen. The
/// repository counts as read in full at `now` only when everything did
/// answer.
fn merge_repo(
    old: &KeptQueue,
    list: Result<(Vec<crate::model::RunInfo>, u64), GhError>,
    jobs: Vec<Result<Vec<Job>, GhError>>,
    now: i64,
) -> Merged {
    let mut m = Merged {
        jobs: Vec::new(),
        unread: old.unread,
        read: QueueRead {
            read_at: old.read_at,
            error: None,
        },
        whole: false,
        paused: Vec::new(),
    };
    let note = |m: &mut Merged, e: GhError| match e {
        GhError::Failed(e) => {
            m.read.error.get_or_insert(e);
        }
        GhError::Paused { until } => m.paused.push(until),
    };
    match list {
        Ok((runs, unread)) => {
            m.unread = unread;
            m.whole = true;
            for (run, answer) in runs.iter().zip(jobs) {
                match answer {
                    Ok(j) => m.jobs.extend(j),
                    Err(e) => {
                        m.whole = false;
                        m.jobs
                            .extend(old.jobs.iter().filter(|j| j.run_id == run.id).cloned());
                        note(&mut m, e);
                    }
                }
            }
            if m.whole {
                m.read.read_at = Some(now);
            }
        }
        Err(e) => {
            m.jobs = old.jobs.clone();
            note(&mut m, e);
        }
    }
    m
}

/// Add the latest completed runs of `repo` that `cache` has not read yet.
/// Returns how many runs were added.
pub fn read_history(
    gh: &mut Client,
    repo: &str,
    cache: &mut history::RepoCache,
) -> Result<usize, GhError> {
    let out = gh.get(&format!(
        "repos/{repo}/actions/runs?status=completed&per_page={}",
        history::LIST_RUNS
    ))?;
    let listed = gh::parse_runs(&out).map_err(GhError::Failed)?;
    cache.gap_before(&listed);
    let new: Vec<_> = listed
        .into_iter()
        .filter(|run| !cache.has(run))
        .map(|run| (repo.to_string(), run))
        .collect();
    let mut added = 0;
    let mut failure = None;
    for ((_, run), jobs) in new.iter().zip(read_runs_jobs(gh, &new, true)) {
        match jobs {
            Ok(jobs) => {
                cache.insert(run.clone(), jobs);
                added += 1;
            }
            Err(e) => {
                failure.get_or_insert(e);
            }
        }
    }
    failure.map_or(Ok(added), Err)
}

/// GitHub's runner list for `repo`. Reading it needs repository admin
/// rights; without them the answer is `denied`, not an error. `Err` means
/// the read paused for rate limit, so the last list should stay.
pub fn read_gh_runners(gh: &mut Client, repo: &str) -> Result<RepoRunners, i64> {
    let mut r = RepoRunners {
        read_at: Some(crate::timefmt::now_epoch()),
        ..Default::default()
    };
    match gh.get(&format!("repos/{repo}/actions/runners?per_page=100")) {
        Ok(out) => match gh::parse_runners(&out) {
            Ok(list) => r.runners = list,
            Err(e) => r.error = Some(e),
        },
        Err(GhError::Paused { until }) => return Err(until),
        Err(e) if ghapi::denied(&e) => r.denied = true,
        Err(e) => r.error = Some(e.to_string()),
    }
    Ok(r)
}

/// The state of a runner's systemd unit, or `None` when systemd has no such
/// unit in that scope.
pub fn read_service(unit: &str, user: bool) -> Option<Service> {
    match (ReadCommand::SystemctlShow {
        unit: unit.to_string(),
        user,
    })
    .run()
    {
        Ok(out) => runners::parse_unit(&out.stdout),
        Err(e) => Some(Service {
            error: Some(format!("systemctl: {e}")),
            ..Default::default()
        }),
    }
}

pub fn read_quota(providers: &[String]) -> Result<Vec<QuotaWindow>, String> {
    let out = ReadCommand::QuotaAxi {
        providers: providers.to_vec(),
    }
    .run()
    .map_err(|e| format!("quota-axi: {e}"))?;
    quota::parse(&out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ExecError, Output};
    use crate::model::RunInfo;
    use ghapi::Fake;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn answer(body: &str) -> Result<Output, ExecError> {
        Ok(Output {
            stdout: body.to_string(),
            stderr: String::new(),
            success: true,
        })
    }

    fn path(cmd: &crate::exec::ReadCommand) -> String {
        match cmd {
            crate::exec::ReadCommand::GhApiGet { path, .. } => path.clone(),
            other => panic!("not a read: {other:?}"),
        }
    }

    fn runner_of(repo: &str) -> Runner {
        Runner {
            repo: Some(repo.into()),
            ..Default::default()
        }
    }

    /// A stand-in for `gh` where every read waits for `peers` reads to be
    /// under way at once (or gives up after 2 s), and then answers with
    /// `body`. Returns it and a counter of reads that met all their peers.
    fn rendezvous(
        peers: usize,
        body: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (Fake, Arc<AtomicUsize>) {
        let (arrived, met) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let met2 = met.clone();
        let fake = Fake(Arc::new(move |cmd| {
            let p = path(cmd);
            arrived.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            while arrived.load(Ordering::SeqCst) < peers && start.elapsed() < Duration::from_secs(2)
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            if arrived.load(Ordering::SeqCst) >= peers {
                met2.fetch_add(1, Ordering::SeqCst);
            }
            answer(&body(&p))
        }));
        (fake, met)
    }

    #[test]
    fn the_queue_lists_of_every_repository_are_read_together() {
        // 4 repositories, a queued and a running list each: 8 reads that
        // can only all meet if they run at once, not one after another.
        let (fake, met) = rendezvous(8, |_| r#"{"total_count":0,"workflow_runs":[]}"#.into());
        let mut gh = Client::with_fake(fake);
        let runners: Vec<Runner> = ["a/a", "b/b", "c/c", "d/d"].map(runner_of).into();
        let q = read_queue(&mut gh, &runners, &mut BTreeMap::new()).unwrap();
        assert_eq!(
            met.load(Ordering::SeqCst),
            8,
            "all 8 lists were in flight at once"
        );
        assert!(q.jobs.is_empty());
        assert_eq!(q.reads.len(), 4);
        assert!(q.reads.values().all(|r| r.error.is_none()));
    }

    #[test]
    fn a_failing_list_does_not_misplace_the_answers_of_the_others() {
        let fake = Fake(Arc::new(move |cmd| {
            let p = path(cmd);
            if p.starts_with("repos/a/a/") && p.contains("status=queued") {
                return answer("HTTP/2.0 404 Not Found\r\n\r\n{\"message\":\"Not Found\"}");
            }
            if p.contains("status=queued") && p.starts_with("repos/b/b/") {
                return answer(
                    r#"{"total_count":1,"workflow_runs":[{"id":7,"name":"CI","status":"queued"}]}"#,
                );
            }
            if p.contains("/runs/7/jobs") {
                return answer(
                    r#"{"total_count":1,"jobs":[{"id":70,"run_id":7,"name":"Unit","status":"queued","labels":["self-hosted"]}]}"#,
                );
            }
            answer(r#"{"total_count":0,"workflow_runs":[]}"#)
        }));
        let mut gh = Client::with_fake(fake);
        let runners = [runner_of("a/a"), runner_of("b/b")];
        let q = read_queue(&mut gh, &runners, &mut BTreeMap::new()).unwrap();
        assert_eq!(q.jobs.len(), 1);
        assert_eq!(q.jobs[0].name, "Unit");
        assert!(q.reads["a/a"].error.is_some(), "{:?}", q.reads);
        assert!(q.reads["b/b"].error.is_none(), "{:?}", q.reads);
    }

    fn run(id: u64) -> RunInfo {
        RunInfo {
            id,
            ..Default::default()
        }
    }

    fn job(run_id: u64, name: &str) -> Job {
        Job {
            run_id,
            name: name.into(),
            status: "queued".into(),
            ..Default::default()
        }
    }

    fn names(m: &Merged) -> Vec<&str> {
        m.jobs.iter().map(|j| j.name.as_str()).collect()
    }

    fn kept() -> KeptQueue {
        KeptQueue {
            jobs: vec![job(1, "old a"), job(2, "old b")],
            unread: 4,
            read_at: Some(100),
        }
    }

    #[test]
    fn a_repository_read_in_full_replaces_what_was_kept() {
        let m = merge_repo(
            &kept(),
            Ok((vec![run(2), run(3)], 0)),
            vec![Ok(vec![job(2, "new b")]), Ok(vec![job(3, "new c")])],
            200,
        );
        assert_eq!(names(&m), ["new b", "new c"]);
        assert_eq!(
            (m.read.read_at, m.read.error, m.unread),
            (Some(200), None, 0)
        );
        assert!(m.whole);
    }

    #[test]
    fn an_unreadable_repository_keeps_its_last_queue_and_reports_why() {
        let m = merge_repo(
            &kept(),
            Err(GhError::Failed("gh: HTTP 404: Not Found".into())),
            vec![],
            200,
        );
        assert_eq!(names(&m), ["old a", "old b"]);
        assert_eq!(m.read.error.as_deref(), Some("gh: HTTP 404: Not Found"));
        assert_eq!(m.read.read_at, Some(100), "the last good read, not now");
        assert_eq!(m.unread, 4);
        assert!(!m.whole);
        // One never read has nothing to keep.
        let none = merge_repo(
            &KeptQueue::default(),
            Err(GhError::Failed("gh: HTTP 404: Not Found".into())),
            vec![],
            200,
        );
        assert!(none.jobs.is_empty());
        assert_eq!(none.read.read_at, None);
    }

    #[test]
    fn a_run_whose_jobs_failed_keeps_its_old_jobs_and_the_rest_are_fresh() {
        let m = merge_repo(
            &kept(),
            Ok((vec![run(1), run(2)], 0)),
            vec![
                Err(GhError::Failed("gh: HTTP 502".into())),
                Ok(vec![job(2, "new b")]),
            ],
            200,
        );
        assert_eq!(names(&m), ["old a", "new b"]);
        assert_eq!(m.read.error.as_deref(), Some("gh: HTTP 502"));
        assert_eq!(m.read.read_at, Some(100), "not read in full");
        assert!(!m.whole);
    }

    #[test]
    fn a_refusal_for_rate_limit_keeps_the_queue_without_an_error() {
        let m = merge_repo(&kept(), Err(GhError::Paused { until: 900 }), vec![], 200);
        assert_eq!(names(&m), ["old a", "old b"]);
        assert_eq!((m.read.error, m.paused), (None, vec![900]));
        let m = merge_repo(
            &kept(),
            Ok((vec![run(1)], 0)),
            vec![Err(GhError::Paused { until: 900 })],
            200,
        );
        assert_eq!(names(&m), ["old a"]);
        assert_eq!((m.read.error, m.paused, m.whole), (None, vec![900], false));
    }
}
