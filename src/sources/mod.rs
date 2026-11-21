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
use crate::model::{Fleet, Job, Pipeline, PrChecks, QuotaWindow, RepoRunners, Runner, Service};
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

/// The jobs of a run, each carrying the run it belongs to. An unchanged
/// list is GitHub's 304, which costs nothing.
fn read_run_jobs(
    gh: &mut Client,
    repo: &str,
    run: &crate::model::RunInfo,
) -> Result<Vec<Job>, GhError> {
    let out = gh.get(&format!(
        "repos/{repo}/actions/runs/{}/jobs?per_page=50",
        run.id
    ))?;
    let mut jobs = gh::parse_jobs(repo, &out).map_err(GhError::Failed)?;
    for j in &mut jobs {
        j.run = Some(run.clone());
    }
    Ok(jobs)
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
}

/// Every job of the queued and running workflow runs of each repository
/// that has a local runner: queued, running, and already finished ones.
pub fn read_queue(gh: &mut Client, runners: &[Runner]) -> Result<Queue, GhError> {
    let mut q = Queue::default();
    for repo in runner_repos(runners) {
        let mut runs = Vec::new();
        let mut unread = 0;
        for status in ["queued", "in_progress"] {
            let out = gh.get(&format!(
                "repos/{repo}/actions/runs?status={status}&per_page={RUNS_PAGE}"
            ))?;
            let page = gh::parse_runs(&out).map_err(GhError::Failed)?;
            unread += gh::total_count(&out)
                .unwrap_or(0)
                .saturating_sub(page.len() as u64);
            runs.extend(page);
        }
        runs.sort_by_key(|r| r.id);
        runs.dedup_by_key(|r| r.id);
        for run in &runs {
            q.jobs.extend(read_run_jobs(gh, &repo, run)?);
        }
        if unread > 0 {
            q.unread.insert(repo, unread);
        }
    }
    Ok(q)
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
    let mut added = 0;
    for run in gh::parse_runs(&out).map_err(GhError::Failed)? {
        if cache.has(&run) {
            continue;
        }
        let jobs = read_run_jobs(gh, repo, &run)?;
        cache.insert(run, jobs);
        added += 1;
    }
    Ok(added)
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
