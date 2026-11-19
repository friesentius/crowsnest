//! Self-hosted GitHub Actions runners on this machine.
//!
//! A runner is a directory named `actions-runner*` holding the `.runner` file
//! that `config.sh` writes. It is up while a `Runner.Listener` process runs
//! from inside that directory, and busy while a `Runner.Worker` does. How
//! recently it wrote its `_diag` logs comes from file times alone; no log is
//! read. All of these are local reads; no API call is needed.

use crate::model::{Runner, Service};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RunnerFile {
    agent_name: String,
    git_hub_url: String,
}

/// `https://github.com/<owner>/<repo>` -> `owner/repo`; org-level runners
/// (`https://github.com/<org>`) have no single repository.
pub fn repo_from_url(url: &str) -> Option<String> {
    let rest = url
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Parse a `.runner` file (UTF-8, often with a byte-order mark).
pub fn parse_runner_file(dir: &Path, text: &str) -> Option<Runner> {
    let text = text.trim_start_matches('\u{feff}');
    let f: RunnerFile = serde_json::from_str(text).ok()?;
    let name = if f.agent_name.is_empty() {
        dir.file_name()?.to_string_lossy().into_owned()
    } else {
        f.agent_name
    };
    Some(Runner {
        name,
        dir: dir.to_path_buf(),
        repo: repo_from_url(&f.git_hub_url),
        ..Default::default()
    })
}

/// Find runner directories directly under each root. `units` names the
/// systemd unit of a runner directory; otherwise the unit is the one the
/// runner's own `svc.sh install` recorded in its `.service` file.
pub fn discover(roots: &[PathBuf], units: &[(PathBuf, String)]) -> Vec<Runner> {
    let mut out = discover_dirs(roots);
    for r in &mut out {
        r.unit = units
            .iter()
            .find(|(d, _)| same_dir(d, &r.dir))
            .map(|(_, u)| u.clone())
            .or_else(|| {
                std::fs::read_to_string(r.dir.join(".service"))
                    .ok()
                    .map(|t| t.trim().to_string())
                    .filter(|u| crate::exec::valid_unit(u))
            });
    }
    out
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

fn discover_dirs(roots: &[PathBuf]) -> Vec<Runner> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("actions-runner"))
            })
            .collect();
        dirs.sort();
        for dir in dirs {
            if let Ok(text) = std::fs::read_to_string(dir.join(".runner"))
                && let Some(r) = parse_runner_file(&dir, &text)
            {
                out.push(r);
            }
        }
    }
    out
}

/// Unix modification time of a file.
fn mtime(p: &Path) -> Option<i64> {
    let t = std::fs::metadata(p).and_then(|m| m.modified()).ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// The newest `<prefix>*.log` in `dir`, by its name: the runner names its
/// logs by their start time, so the newest name is the live log.
fn newest_log(dir: &Path, prefix: &str) -> Option<i64> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix) && n.ends_with(".log"))
        .max()?;
    mtime(&dir.join(newest))
}

/// When the runner last wrote its listener log, its worker log and a
/// running step's output (`_diag/pages`). Only file times are read.
pub fn read_diag(r: &mut Runner) {
    let diag = r.dir.join("_diag");
    r.runner_log_at = newest_log(&diag, "Runner_");
    r.worker_log_at = newest_log(&diag, "Worker_");
    r.step_output_at = std::fs::read_dir(diag.join("pages"))
        .ok()
        .and_then(|d| d.flatten().filter_map(|e| mtime(&e.path())).max());
}

/// Parse `systemctl show --property=...` output. `None` when systemd has no
/// such unit.
pub fn parse_unit(text: &str) -> Option<Service> {
    let props: BTreeMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
    if props.get("LoadState") == Some(&"not-found") {
        return None;
    }
    Some(Service {
        active: props.get("ActiveState").unwrap_or(&"").to_string(),
        sub: props.get("SubState").unwrap_or(&"").to_string(),
        restarts: props.get("NRestarts").and_then(|n| n.parse().ok()),
        error: None,
    })
}

/// Samples of a running job's process tree CPU time.
#[derive(Default)]
struct JobCpu {
    worker: Option<sysinfo::Pid>,
    samples: Vec<(i64, u64)>,
}

/// Keep a sample at most every this many seconds...
const CPU_SAMPLE_SECS: i64 = 15;
/// ...for this long.
const CPU_KEEP_SECS: i64 = 2 * 3600;

impl JobCpu {
    fn add(&mut self, now: i64, cpu_ms: u64) {
        let n = self.samples.len();
        // Within the spacing, the newest sample just moves forward, so the
        // latest reading is always kept.
        if n > 1 && now - self.samples[n - 2].0 < CPU_SAMPLE_SECS {
            self.samples[n - 1] = (now, cpu_ms);
        } else {
            self.samples.push((now, cpu_ms));
        }
        let cut = now - CPU_KEEP_SECS;
        let old = self.samples.iter().take_while(|(t, _)| *t < cut).count();
        self.samples.drain(..old.saturating_sub(1));
    }
}

/// Mark runners up and busy from the process table.
pub struct ProcessScan {
    sys: Option<System>,
    jobs: BTreeMap<PathBuf, JobCpu>,
    /// When each runner directory was first seen without a worker, in its
    /// current idle stretch. Kept here so it survives re-discovery.
    idle_since: BTreeMap<PathBuf, i64>,
}

impl Default for ProcessScan {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a process started from `exe` (or, when another user's process
/// hides its executable, from its first argument) lives in `dir`.
fn runs_from(exe: Option<&Path>, argv0: Option<&Path>, dir: &Path) -> bool {
    exe.or(argv0).is_some_and(|p| p.starts_with(dir))
}

impl ProcessScan {
    pub fn new() -> Self {
        ProcessScan {
            sys: System::new().ok(),
            jobs: BTreeMap::new(),
            idle_since: BTreeMap::new(),
        }
    }

    pub fn update(&mut self, runners: &mut [Runner]) {
        self.update_at(runners, crate::timefmt::now_epoch());
    }

    pub fn update_at(&mut self, runners: &mut [Runner], now: i64) {
        // Scanning the process table is the costliest local read; skip it
        // when there is nothing to match.
        if runners.is_empty() {
            return;
        }
        let Some(sys) = self.sys.as_mut() else { return };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_cpu(),
        );
        for r in runners.iter_mut() {
            r.busy = false;
            r.busy_secs = None;
            r.listener_secs = None;
            r.scanned_at = Some(now);
        }
        let dirs: Vec<Vec<PathBuf>> = runners
            .iter()
            .map(|r| {
                let mut v = vec![r.dir.clone()];
                if let Ok(c) = r.dir.canonicalize()
                    && c != r.dir
                {
                    v.push(c);
                }
                v
            })
            .collect();
        let mut workers: Vec<(usize, sysinfo::Pid)> = Vec::new();
        for (pid, p) in sys.processes() {
            let name = p.name().to_string_lossy();
            let worker = name == "Runner.Worker";
            if !worker && name != "Runner.Listener" {
                continue;
            }
            let argv0 = p.cmd().first().map(Path::new);
            let Some(i) = dirs
                .iter()
                .position(|ds| ds.iter().any(|d| runs_from(p.exe(), argv0, d)))
            else {
                continue;
            };
            let r = &mut runners[i];
            let secs = p.run_time() as i64;
            if worker {
                r.busy = true;
                r.busy_secs = Some(r.busy_secs.map_or(secs, |s| s.max(secs)));
                workers.push((i, *pid));
            } else {
                r.listener_secs = Some(r.listener_secs.map_or(secs, |s| s.max(secs)));
            }
        }
        // The CPU the running job's processes have used: everything below
        // its Runner.Worker, not the worker itself, whose own heartbeats and
        // log flushing say nothing about the step. A job run with `docker`
        // executes in a container, outside this tree, so its CPU cannot be
        // seen here.
        let mut children: BTreeMap<sysinfo::Pid, Vec<sysinfo::Pid>> = BTreeMap::new();
        for (pid, p) in sys.processes() {
            if let Some(parent) = p.parent() {
                children.entry(parent).or_default().push(*pid);
            }
        }
        let mut seen_jobs = Vec::new();
        for (i, worker) in workers {
            let (mut cpu_ms, mut container) = (0u64, false);
            let mut stack: Vec<sysinfo::Pid> = children
                .get(&worker)
                .into_iter()
                .flatten()
                .copied()
                .collect();
            while let Some(pid) = stack.pop() {
                if let Some(p) = sys.process(pid) {
                    cpu_ms += p.accumulated_cpu_time();
                    let n = p.name().to_string_lossy();
                    container |= n == "docker" || n == "podman";
                }
                stack.extend(children.get(&pid).into_iter().flatten().copied());
            }
            let r = &mut runners[i];
            let job = self.jobs.entry(r.dir.clone()).or_default();
            if job.worker != Some(worker) {
                *job = JobCpu {
                    worker: Some(worker),
                    ..Default::default()
                };
            }
            job.add(now, cpu_ms);
            r.job_cpu = job.samples.clone();
            r.job_in_container = container;
            seen_jobs.push(r.dir.clone());
        }
        self.jobs.retain(|d, _| seen_jobs.contains(d));
        for r in runners.iter_mut() {
            if !r.busy {
                r.job_cpu.clear();
                r.job_in_container = false;
            }
            if r.busy {
                self.idle_since.remove(&r.dir);
            } else {
                self.idle_since.entry(r.dir.clone()).or_insert(now);
            }
            r.idle_since = self.idle_since.get(&r.dir).copied();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runner_file_with_bom() {
        let text = include_str!("../../tests/fixtures/runner/.runner");
        let r = parse_runner_file(Path::new("/srv/actions-runner-pantry"), text).unwrap();
        assert_eq!(r.name, "pantry-local-runner");
        assert_eq!(r.repo.as_deref(), Some("acme/pantry"));
        assert!(!r.busy);
    }

    #[test]
    fn org_runners_have_no_repo() {
        assert_eq!(repo_from_url("https://github.com/acme"), None);
        assert_eq!(
            repo_from_url("https://github.com/acme/pantry/"),
            Some("acme/pantry".into())
        );
    }

    #[test]
    fn discovers_runner_directories() {
        let root = std::env::temp_dir().join(format!("crowsnest-runners-{}", std::process::id()));
        let dir = root.join("actions-runner-pantry");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(root.join("unrelated")).unwrap();
        std::fs::write(
            dir.join(".runner"),
            include_str!("../../tests/fixtures/runner/.runner"),
        )
        .unwrap();
        std::fs::write(
            dir.join(".service"),
            "actions.runner.acme-pantry.pantry-local-runner.service\n",
        )
        .unwrap();
        let found = discover(std::slice::from_ref(&root), &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir, dir);
        assert_eq!(
            found[0].unit.as_deref(),
            Some("actions.runner.acme-pantry.pantry-local-runner.service")
        );
        // A configured unit wins over the recorded one.
        let found = discover(
            std::slice::from_ref(&root),
            &[(dir.clone(), "actions-runner-pantry.service".into())],
        );
        assert_eq!(
            found[0].unit.as_deref(),
            Some("actions-runner-pantry.service")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn diag_times_come_from_the_newest_logs() {
        let dir = std::env::temp_dir().join(format!("crowsnest-diag-{}", std::process::id()));
        let diag = dir.join("_diag");
        std::fs::create_dir_all(diag.join("pages")).unwrap();
        for f in [
            "Runner_20261119-090000-utc.log",
            "Worker_20261119-090000-utc.log",
            "Worker_20261119-141834-utc.log",
            "pages/a_1.log",
        ] {
            std::fs::write(diag.join(f), "x").unwrap();
        }
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        read_diag(&mut r);
        std::fs::remove_dir_all(&dir).unwrap();
        let now = crate::timefmt::now_epoch();
        for t in [r.runner_log_at, r.worker_log_at, r.step_output_at] {
            assert!(t.is_some_and(|t| (now - t).abs() < 60), "{t:?}");
        }
        // No `_diag` at all reads as nothing known.
        read_diag(&mut r);
        assert_eq!(
            (r.runner_log_at, r.worker_log_at, r.step_output_at),
            (None, None, None)
        );
    }

    #[test]
    fn parses_systemctl_show() {
        let s = parse_unit("LoadState=loaded\nActiveState=active\nSubState=running\nNRestarts=2\n")
            .unwrap();
        assert_eq!(
            (s.active.as_str(), s.sub.as_str(), s.restarts),
            ("active", "running", Some(2))
        );
        assert!(parse_unit("LoadState=not-found\nActiveState=inactive\n").is_none());
    }

    #[test]
    fn processes_match_by_executable_or_first_argument() {
        let dir = Path::new("/srv/actions-runner-pantry");
        let exe = Path::new("/srv/actions-runner-pantry/bin.2.338.2/Runner.Listener");
        assert!(runs_from(Some(exe), None, dir));
        assert!(runs_from(None, Some(exe), dir));
        assert!(!runs_from(
            Some(Path::new("/srv/actions-runner-pantry2/bin/Runner.Worker")),
            None,
            dir
        ));
        assert!(!runs_from(None, None, dir));
    }
}
