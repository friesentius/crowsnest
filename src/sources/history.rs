//! Finished jobs of the repositories with a local runner: what each runner
//! ran lately and how long each job (and step) typically takes.
//!
//! A finished run never changes, so its jobs are read once and cached by run
//! id and attempt; later reads only list the latest completed runs and fetch
//! the jobs of runs not seen before.

use crate::model::{DoneJob, Job, RepoHistory, RunInfo, Typical};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Completed runs listed per read.
pub const LIST_RUNS: usize = 10;
/// Typical durations are the median of at most this many latest samples.
pub const SAMPLES: usize = 10;
/// Runs kept per repository.
const KEEP_RUNS: usize = 60;

/// One finished run and its jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRun {
    pub run: RunInfo,
    pub jobs: Vec<Job>,
}

/// The finished runs crowsnest has read for one repository.
#[derive(Debug, Clone, Default)]
pub struct RepoCache {
    runs: BTreeMap<(u64, u32), CachedRun>,
    /// Runs created before this may be missing: crowsnest was not looking
    /// while they finished. Runs from then on are all here.
    floor: Option<i64>,
    /// Changed since it was loaded or last saved.
    dirty: bool,
}

/// What a cache file holds. A file of another `version` is not read.
#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    #[serde(default)]
    floor: Option<i64>,
    runs: Vec<CachedRun>,
}

/// The format of the cache file; raise it when what is stored changes.
const FILE_VERSION: u32 = 1;

/// The file that keeps `repo`'s finished runs in `dir`, or `None` for a name
/// that is not a GitHub `owner/repo`. `+` cannot be in either part, so two
/// repositories never share a file.
pub fn cache_file(dir: &Path, repo: &str) -> Option<PathBuf> {
    let (owner, name) = repo.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name)).then(|| dir.join(format!("{owner}+{name}.json")))
}

impl RepoCache {
    /// The runs kept in `path`, or an empty cache when there is no file, it
    /// cannot be read or understood, or it is of another version: the runs
    /// are then read again, which is all a missing cache costs.
    pub fn load(path: &Path) -> RepoCache {
        let Ok(text) = std::fs::read_to_string(path) else {
            return RepoCache::default();
        };
        match serde_json::from_str::<CacheFile>(&text) {
            Ok(f) if f.version == FILE_VERSION => {
                let mut c = RepoCache {
                    floor: f.floor,
                    ..Default::default()
                };
                for r in f.runs {
                    c.insert(r.run, r.jobs);
                }
                c.dirty = false;
                c
            }
            _ => RepoCache::default(),
        }
    }

    /// Write the runs to `path` when they changed since the last load or
    /// save: to a temporary file first, so a crash never leaves half a file.
    pub fn save(&mut self, path: &Path) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = CacheFile {
            version: FILE_VERSION,
            floor: self.floor,
            runs: self.runs.values().cloned().collect(),
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(&file).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }

    /// Note that the latest listing of finished runs shares none with the
    /// cache, so runs that finished in between were never read: only what
    /// was created since the oldest listed run is known to be complete.
    pub fn gap_before(&mut self, listed: &[RunInfo]) {
        if self.runs.is_empty() || listed.iter().any(|r| self.has(r)) {
            return;
        }
        if let Some(since) = listed.iter().filter_map(|r| r.created_at).min()
            && self.floor.is_none_or(|f| f < since)
        {
            self.floor = Some(since);
            self.dirty = true;
        }
    }

    pub fn has(&self, run: &RunInfo) -> bool {
        self.runs.contains_key(&(run.id, run.attempt))
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Add a finished run, dropping the oldest beyond what is kept.
    pub fn insert(&mut self, run: RunInfo, jobs: Vec<Job>) {
        self.runs
            .insert((run.id, run.attempt), CachedRun { run, jobs });
        self.dirty = true;
        while self.runs.len() > KEEP_RUNS {
            self.runs.pop_first();
        }
    }

    /// The history these runs make, plus the jobs that already finished in
    /// runs still in progress (`live`).
    pub fn summary(&self, live: &[Job]) -> RepoHistory {
        let oldest = self.runs.values().filter_map(|r| r.run.created_at).min();
        let covered_since = oldest.into_iter().chain(self.floor).max();
        let mut jobs: Vec<&Job> = self.runs.values().flat_map(|r| r.jobs.iter()).collect();
        for j in live {
            if !jobs.iter().any(|c| c.id == j.id) {
                jobs.push(j);
            }
        }
        summarize(&jobs, covered_since)
    }
}

fn median(mut v: Vec<i64>) -> i64 {
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    }
}

/// Build the history of finished jobs that ran on self-hosted runners.
pub fn summarize(jobs: &[&Job], covered_since: Option<i64>) -> RepoHistory {
    let mut done: Vec<&Job> = jobs
        .iter()
        .copied()
        .filter(|j| j.status == "completed" && j.is_self_hosted() && j.runner.is_some())
        .collect();
    done.sort_by_key(|j| std::cmp::Reverse((j.started_at, j.id)));

    // Typical durations come from successful jobs only: a failure or a
    // cancel stops early and says nothing about how long a pass takes.
    let mut runs: BTreeMap<(String, String), Vec<i64>> = BTreeMap::new();
    let mut steps: BTreeMap<(String, String, String), Vec<i64>> = BTreeMap::new();
    for j in done
        .iter()
        .filter(|j| j.conclusion.as_deref() == Some("success"))
    {
        let Some(secs) = j.started_at.zip(j.completed_at).map(|(s, e)| e - s) else {
            continue;
        };
        let key = (j.workflow.clone(), j.name.clone());
        let samples = runs.entry(key).or_default();
        if samples.len() < SAMPLES {
            samples.push(secs.max(0));
        } else {
            continue;
        }
        for s in &j.steps {
            if s.conclusion.as_deref() != Some("success") {
                continue;
            }
            if let (Some(a), Some(b)) = (s.started_at, s.completed_at) {
                steps
                    .entry((j.workflow.clone(), j.name.clone(), s.name.clone()))
                    .or_default()
                    .push((b - a).max(0));
            }
        }
    }
    let mut typical: Vec<Typical> = runs
        .into_iter()
        .map(|((workflow, job), v)| Typical {
            workflow,
            job,
            step: None,
            samples: v.len(),
            secs: median(v),
        })
        .collect();
    typical.extend(steps.into_iter().map(|((workflow, job, step), v)| Typical {
        workflow,
        job,
        step: Some(step),
        samples: v.len(),
        secs: median(v),
    }));
    RepoHistory {
        jobs: done
            .iter()
            .map(|j| DoneJob {
                runner: j.runner.clone().unwrap_or_default(),
                workflow: j.workflow.clone(),
                name: j.name.clone(),
                label: j.label(),
                conclusion: j.conclusion.clone().unwrap_or_default(),
                created_at: j.created_at,
                started_at: j.started_at,
                completed_at: j.completed_at,
            })
            .collect(),
        typical,
        covered_since,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::gh;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// The four finished runs of the recorded history.
    pub fn cache() -> RepoCache {
        let mut c = RepoCache::default();
        for run in gh::parse_runs(&fixture("gh-runs-completed.json")).unwrap() {
            let mut jobs =
                gh::parse_jobs("acme/pantry", &fixture(&format!("gh-jobs-{}.json", run.id)))
                    .unwrap();
            for j in &mut jobs {
                j.run = Some(run.clone());
            }
            c.insert(run, jobs);
        }
        c
    }

    #[test]
    fn medians() {
        assert_eq!(median(vec![3, 1, 2]), 2);
        assert_eq!(median(vec![4, 1, 2, 3]), 2);
        assert_eq!(median(vec![7]), 7);
    }

    #[test]
    fn typical_durations_come_from_successful_runs() {
        let h = cache().summary(&[]);
        // Unit + e2e passed three times (40m24s, 40m, 33m18s) and failed once.
        let unit = h.typical("CI", "Unit + e2e tests", None).unwrap();
        assert_eq!((unit.samples, unit.secs), (3, 40 * 60));
        let lint = h.typical("CI", "Lint & typecheck", None).unwrap();
        assert_eq!(lint.samples, 4);
        assert_eq!(lint.secs, (12 * 60 + 18 + 13 * 60 + 18) / 2);
        // Steps too: setup-node took 620s, 630s and 610s in the passing runs.
        let step = h
            .typical("CI", "Unit + e2e tests", Some("Run actions/setup-node@v4"))
            .unwrap();
        assert_eq!((step.samples, step.secs), (3, 620));
    }

    #[test]
    fn hosted_jobs_are_not_runner_history() {
        let h = cache().summary(&[]);
        assert!(h.jobs.iter().all(|j| j.runner == "pantry-local-runner"));
        assert!(h.typical("CI", "Dependency review", None).is_none());
        assert_eq!(h.jobs.len(), 16);
        // Newest first, labelled with the PR or the pushed branch.
        assert_eq!(h.jobs[0].name, "Unit + e2e tests");
        assert_eq!(h.jobs[0].label, "#121");
        assert_eq!(h.jobs.last().unwrap().label, "dev push");
        assert_eq!(
            h.covered_since,
            crate::timefmt::parse_iso("2026-11-19T09:02:10Z")
        );
    }

    #[test]
    fn jobs_finished_in_a_live_run_count_once() {
        let live = gh::parse_jobs("acme/pantry", &fixture("gh-jobs.json")).unwrap();
        let c = cache();
        let h = c.summary(&live);
        // Lint, Fresh-clone and Docker finished in the live run; Unit runs on.
        assert_eq!(h.jobs.len(), 19);
        assert_eq!(
            h.typical("CI", "Lint & typecheck", None).unwrap().samples,
            5
        );
        let again = c.summary(&[live.clone(), live].concat());
        assert_eq!(again.jobs.len(), 19);
    }

    #[test]
    fn a_rerun_is_a_new_run_and_old_runs_are_dropped() {
        let mut c = cache();
        let mut run = gh::parse_runs(&fixture("gh-runs-completed.json")).unwrap()[0].clone();
        assert!(c.has(&run));
        run.attempt = 2;
        assert!(!c.has(&run));
        for i in 0..KEEP_RUNS as u64 {
            let mut r = run.clone();
            r.id = 50_000_000_000 + i;
            c.insert(r, Vec::new());
        }
        assert_eq!(c.len(), KEEP_RUNS);
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("crowsnest-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_saved_cache_is_read_back_whole_and_written_only_when_it_changed() {
        let dir = scratch("roundtrip");
        let file = cache_file(&dir, "acme/pantry").unwrap();
        let mut c = cache();
        c.save(&file).unwrap();
        let back = RepoCache::load(&file);
        assert_eq!(back.len(), 4);
        let (a, b) = (c.summary(&[]), back.summary(&[]));
        assert_eq!(a.jobs.len(), b.jobs.len());
        assert_eq!(a.typical, b.typical);
        assert_eq!(a.covered_since, b.covered_since);
        assert_eq!(
            a.jobs
                .iter()
                .map(|j| (&j.name, j.started_at))
                .collect::<Vec<_>>(),
            b.jobs
                .iter()
                .map(|j| (&j.name, j.started_at))
                .collect::<Vec<_>>()
        );
        // Unchanged since the last save: nothing is written.
        std::fs::remove_file(&file).unwrap();
        c.save(&file).unwrap();
        assert!(!file.exists());
        // A loaded cache has nothing to write either.
        let mut loaded = RepoCache::load(&dir.join("none.json"));
        loaded.save(&file).unwrap();
        assert!(!file.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_damaged_or_foreign_file_is_an_empty_cache() {
        let dir = scratch("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a+b.json");
        assert!(RepoCache::load(&file).is_empty());
        std::fs::write(&file, "{ not json").unwrap();
        assert!(RepoCache::load(&file).is_empty());
        std::fs::write(&file, r#"{"version":999,"runs":[]}"#).unwrap();
        assert!(RepoCache::load(&file).is_empty());
        // The right version with a run that lacks fields still loads.
        std::fs::write(
            &file,
            r#"{"version":1,"runs":[{"run":{"id":5},"jobs":[]}]}"#,
        )
        .unwrap();
        assert_eq!(RepoCache::load(&file).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_repository_name_is_a_safe_file_name_or_none() {
        let d = Path::new("/c");
        assert_eq!(
            cache_file(d, "acme/pantry"),
            Some(PathBuf::from("/c/acme+pantry.json"))
        );
        assert_ne!(cache_file(d, "a-b/c"), cache_file(d, "a/b-c"));
        for bad in ["", "acme", "a/b/c", "../x", "a/..", "a/b c", "/b", "a/"] {
            assert_eq!(cache_file(d, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn runs_missed_while_crowsnest_was_off_do_not_count_as_covered() {
        let mut c = cache();
        let all = gh::parse_runs(&fixture("gh-runs-completed.json")).unwrap();
        assert_eq!(
            c.summary(&[]).covered_since,
            crate::timefmt::parse_iso("2026-11-19T09:02:10Z")
        );
        // A listing that overlaps the cache leaves nothing in doubt.
        c.gap_before(&all[..2]);
        assert_eq!(
            c.summary(&[]).covered_since,
            crate::timefmt::parse_iso("2026-11-19T09:02:10Z")
        );
        // Ten newer runs, none cached: what finished in between is unknown.
        let newer: Vec<RunInfo> = (0..3)
            .map(|i| RunInfo {
                id: 90 + i,
                attempt: 1,
                created_at: crate::timefmt::parse_iso("2026-11-20T08:00:00Z").map(|t| t + i as i64),
                ..Default::default()
            })
            .collect();
        c.gap_before(&newer);
        let since = crate::timefmt::parse_iso("2026-11-20T08:00:00Z");
        assert_eq!(c.summary(&[]).covered_since, since);
        // The older jobs are still history, only the claim of coverage moved.
        assert_eq!(c.summary(&[]).jobs.len(), 16);
        // It survives a save and a load.
        let dir = scratch("gap");
        let file = dir.join("x+y.json");
        c.save(&file).unwrap();
        assert_eq!(RepoCache::load(&file).summary(&[]).covered_since, since);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
