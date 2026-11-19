//! Data collection. Each source turns one read-only command or file into
//! crowsnest's model types. Nothing here knows about the UI, so these
//! functions can back another front end, or be replaced by a single
//! firstmate-provided collector, without touching `ui`.

pub mod fleet;
pub mod gh;
pub mod host;
pub mod nm;
pub mod quota;
pub mod runners;

use crate::exec::ReadCommand;
use crate::model::{Fleet, Job, Pipeline, PrChecks, QuotaWindow, Runner};
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
    Ok(nm::parse(&out.stdout))
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

pub fn read_pr_checks(urls: &[String]) -> Result<Vec<PrChecks>, String> {
    let Some(query) = gh::checks_query(urls) else {
        return Ok(Vec::new());
    };
    let out = ReadCommand::GhGraphql { query }
        .run()
        .map_err(|e| format!("gh: {e}"))?;
    gh::parse_checks(&out.stdout)
}

/// Queued and running jobs for each repository that has a local runner.
pub fn read_queue(runners: &[Runner]) -> Result<Vec<Job>, String> {
    let mut repos: Vec<&str> = runners.iter().filter_map(|r| r.repo.as_deref()).collect();
    repos.sort();
    repos.dedup();
    let mut jobs = Vec::new();
    for repo in repos {
        let mut ids = Vec::new();
        for status in ["queued", "in_progress"] {
            let out = ReadCommand::GhApiGet {
                path: format!("repos/{repo}/actions/runs?status={status}&per_page=5"),
            }
            .run()
            .map_err(|e| format!("gh: {e}"))?;
            ids.extend(gh::parse_run_ids(&out.stdout)?);
        }
        ids.sort();
        ids.dedup();
        for id in ids.into_iter().take(5) {
            let out = ReadCommand::GhApiGet {
                path: format!("repos/{repo}/actions/runs/{id}/jobs?per_page=50"),
            }
            .run()
            .map_err(|e| format!("gh: {e}"))?;
            jobs.extend(gh::parse_jobs(repo, &out.stdout)?);
        }
    }
    Ok(jobs)
}

pub fn read_quota(providers: &[String]) -> Result<Vec<QuotaWindow>, String> {
    let out = ReadCommand::QuotaAxi {
        providers: providers.to_vec(),
    }
    .run()
    .map_err(|e| format!("quota-axi: {e}"))?;
    quota::parse(&out.stdout)
}
