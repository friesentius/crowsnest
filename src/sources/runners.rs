//! Self-hosted GitHub Actions runners on this machine.
//!
//! A runner is a directory named `actions-runner*` holding the `.runner` file
//! that `config.sh` writes. It is busy while a `Runner.Worker` process runs
//! from inside that directory. Both are local reads; no API call is needed.

use crate::model::Runner;
use serde::Deserialize;
use std::path::{Path, PathBuf};
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
        busy: false,
        busy_secs: None,
    })
}

/// Find runner directories directly under each root.
pub fn discover(roots: &[PathBuf]) -> Vec<Runner> {
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

/// Mark runners busy from the process table.
pub struct ProcessScan {
    sys: Option<System>,
}

impl Default for ProcessScan {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessScan {
    pub fn new() -> Self {
        ProcessScan {
            sys: System::new().ok(),
        }
    }

    pub fn update(&mut self, runners: &mut [Runner]) {
        // Scanning the process table is the costliest local read; skip it
        // when there is nothing to match.
        if runners.is_empty() {
            return;
        }
        let Some(sys) = self.sys.as_mut() else { return };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
        );
        for r in runners.iter_mut() {
            r.busy = false;
            r.busy_secs = None;
        }
        for p in sys.processes().values() {
            if p.name().to_string_lossy() != "Runner.Worker" {
                continue;
            }
            let Some(exe) = p.exe() else { continue };
            if let Some(r) = runners.iter_mut().find(|r| exe.starts_with(&r.dir)) {
                r.busy = true;
                let secs = p.run_time() as i64;
                r.busy_secs = Some(r.busy_secs.map_or(secs, |s| s.max(secs)));
            }
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
        let found = discover(std::slice::from_ref(&root));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir, dir);
    }
}
