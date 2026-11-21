//! The only place crowsnest starts another program.
//!
//! Every command is one variant of [`ReadCommand`], an allowlist of exact,
//! read-only argument vectors. There is no way to run an arbitrary command,
//! and `tests/readonly.rs` checks the argument vectors against a deny list of
//! mutating verbs.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum ReadCommand {
    /// `<fm_home>/bin/fm-fleet-snapshot.sh --json`, firstmate's documented
    /// fleet contract. Its optional summary cache is redirected to
    /// crowsnest's own cache directory so firstmate state is never written.
    FleetSnapshot {
        fm_home: PathBuf,
        cache_dir: PathBuf,
    },
    /// `no-mistakes axi status`, run inside a task worktree.
    NoMistakesStatus { worktree: PathBuf },
    /// `gh api graphql --include -f query=<query>`; the query must be a read
    /// query. `--include` puts the response's status and headers (GitHub's
    /// rate-limit counters) before its body.
    GhGraphql { query: String },
    /// `gh api --method GET --include [-H 'If-None-Match: <etag>'] <path>`.
    /// With the ETag of an earlier answer, GitHub answers 304 when nothing
    /// changed, and that answer does not count against the rate limit.
    GhApiGet { path: String, etag: Option<String> },
    /// `quota-axi --no-credential-refresh --json --full [--provider a,b]`. Only
    /// `--full` carries each window's projected run-out time and its confidence.
    QuotaAxi { providers: Vec<String> },
    /// `systemctl [--user] show --property=... -- <unit>`: a runner's service.
    SystemctlShow { unit: String, user: bool },
}

/// The unit properties [`ReadCommand::SystemctlShow`] asks for.
pub const UNIT_PROPERTIES: &str = "LoadState,ActiveState,SubState,NRestarts";

/// An HTTP entity tag as GitHub sends it: `"<opaque>"` or `W/"<opaque>"`,
/// printable ASCII without quotes or backslashes inside.
pub fn valid_etag(etag: &str) -> bool {
    let inner = etag
        .strip_prefix("W/")
        .unwrap_or(etag)
        .strip_prefix('"')
        .and_then(|e| e.strip_suffix('"'));
    etag.len() <= 256
        && inner.is_some_and(|i| {
            i.chars()
                .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
        })
}

/// A systemd unit name: no options, paths or shell syntax.
pub fn valid_unit(unit: &str) -> bool {
    !unit.is_empty()
        && !unit.starts_with('-')
        && unit.len() <= 256
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@:\\".contains(c))
}

#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

#[derive(Debug)]
pub enum ExecError {
    Refused(String),
    NotFound(String),
    Spawn(String),
    TimedOut(Duration),
    Failed {
        code: Option<i32>,
        stderr: String,
        stdout: String,
    },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Refused(why) => write!(f, "refused: {why}"),
            ExecError::NotFound(prog) => write!(f, "{prog} not found"),
            ExecError::Spawn(e) => write!(f, "could not start: {e}"),
            ExecError::TimedOut(d) => write!(f, "timed out after {}s", d.as_secs()),
            ExecError::Failed { code, stderr, .. } => {
                let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                match code {
                    Some(c) => write!(f, "exit {c}: {first}"),
                    None => write!(f, "killed: {first}"),
                }
            }
        }
    }
}

/// A fully resolved invocation, exposed for the allowlist tests.
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl ReadCommand {
    pub fn invocation(&self) -> Result<Invocation, ExecError> {
        let quiet_env = vec![
            ("NO_COLOR".to_string(), "1".to_string()),
            ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
            ("GH_NO_UPDATE_NOTIFIER".to_string(), "1".to_string()),
            ("NO_MISTAKES_NO_UPDATE_CHECK".to_string(), "1".to_string()),
        ];
        let inv = match self {
            ReadCommand::FleetSnapshot { fm_home, cache_dir } => {
                let script = fm_home.join("bin").join("fm-fleet-snapshot.sh");
                let mut env = quiet_env;
                env.push(("FM_HOME".into(), fm_home.display().to_string()));
                env.push((
                    "FM_SNAPSHOT_CACHE_DIR".into(),
                    cache_dir.display().to_string(),
                ));
                Invocation {
                    program: script.display().to_string(),
                    args: vec!["--json".into()],
                    cwd: Some(fm_home.clone()),
                    env,
                    timeout: Duration::from_secs(30),
                }
            }
            ReadCommand::NoMistakesStatus { worktree } => Invocation {
                program: "no-mistakes".into(),
                args: vec!["axi".into(), "status".into()],
                cwd: Some(worktree.clone()),
                env: quiet_env,
                timeout: Duration::from_secs(8),
            },
            ReadCommand::GhGraphql { query } => {
                let head = query.trim_start();
                if !(head.starts_with("query") || head.starts_with('{'))
                    || query.contains("mutation")
                {
                    return Err(ExecError::Refused(
                        "only GraphQL read queries are allowed".into(),
                    ));
                }
                Invocation {
                    program: "gh".into(),
                    args: vec![
                        "api".into(),
                        "graphql".into(),
                        "--include".into(),
                        "-f".into(),
                        format!("query={query}"),
                    ],
                    cwd: None,
                    env: quiet_env,
                    timeout: Duration::from_secs(20),
                }
            }
            ReadCommand::GhApiGet { path, etag } => {
                let ok = path.starts_with("repos/")
                    && path
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "/-_.?=&".contains(c));
                if !ok {
                    return Err(ExecError::Refused(format!("unexpected API path {path}")));
                }
                let mut args: Vec<String> = vec![
                    "api".into(),
                    "--method".into(),
                    "GET".into(),
                    "--include".into(),
                ];
                if let Some(etag) = etag {
                    if !valid_etag(etag) {
                        return Err(ExecError::Refused(format!("unexpected ETag {etag}")));
                    }
                    args.push("-H".into());
                    args.push(format!("If-None-Match: {etag}"));
                }
                args.push(path.clone());
                Invocation {
                    program: "gh".into(),
                    args,
                    cwd: None,
                    env: quiet_env,
                    timeout: Duration::from_secs(20),
                }
            }
            ReadCommand::QuotaAxi { providers } => {
                let mut args = vec![
                    "--no-credential-refresh".to_string(),
                    "--json".to_string(),
                    "--full".to_string(),
                ];
                if !providers.is_empty() {
                    if providers.iter().any(|p| {
                        p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    }) {
                        return Err(ExecError::Refused("unexpected quota provider name".into()));
                    }
                    args.push("--provider".into());
                    args.push(providers.join(","));
                }
                Invocation {
                    program: "quota-axi".into(),
                    args,
                    cwd: None,
                    env: quiet_env,
                    timeout: Duration::from_secs(20),
                }
            }
            ReadCommand::SystemctlShow { unit, user } => {
                if !valid_unit(unit) {
                    return Err(ExecError::Refused(format!("unexpected unit name {unit}")));
                }
                let mut args = Vec::new();
                if *user {
                    args.push("--user".to_string());
                }
                args.extend([
                    "show".to_string(),
                    format!("--property={UNIT_PROPERTIES}"),
                    "--".to_string(),
                    unit.clone(),
                ]);
                let mut env = quiet_env;
                env.push(("SYSTEMD_PAGER".into(), String::new()));
                Invocation {
                    program: "systemctl".into(),
                    args,
                    cwd: None,
                    env,
                    timeout: Duration::from_secs(5),
                }
            }
        };
        Ok(inv)
    }

    pub fn run(&self) -> Result<Output, ExecError> {
        let inv = self.invocation()?;
        run_invocation(&inv)
    }
}

fn run_invocation(inv: &Invocation) -> Result<Output, ExecError> {
    let mut cmd = Command::new(&inv.program);
    cmd.args(&inv.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(dir) = &inv.cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in &inv.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ExecError::NotFound(inv.program.clone())
        } else {
            ExecError::Spawn(e.to_string())
        }
    })?;
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf.truncate(64 * 1024);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > inv.timeout => {
                // Kill the whole process group: snapshot scripts start helpers.
                let pgid = child.id() as libc::pid_t;
                // SAFETY: signalling our own child's process group.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(ExecError::TimedOut(inv.timeout));
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(ExecError::Spawn(e.to_string())),
        }
    };
    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    if !status.success() {
        return Err(ExecError::Failed {
            code: status.code(),
            stderr: if stderr.trim().is_empty() {
                stdout.clone()
            } else {
                stderr
            },
            stdout,
        });
    }
    Ok(Output {
        stdout,
        stderr,
        success: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_mutations_are_refused() {
        let q = ReadCommand::GhGraphql {
            query: "mutation { mergePullRequest(input: {}) { clientMutationId } }".into(),
        };
        assert!(matches!(q.invocation(), Err(ExecError::Refused(_))));
        let q = ReadCommand::GhGraphql {
            query: "query { viewer { login } }".into(),
        };
        assert!(q.invocation().is_ok());
    }

    #[test]
    fn api_paths_are_constrained() {
        for bad in [
            "user/repos",
            "repos/a/b; rm -rf /",
            "repos/a/b --method POST",
        ] {
            let c = ReadCommand::GhApiGet {
                path: bad.into(),
                etag: None,
            };
            assert!(
                matches!(c.invocation(), Err(ExecError::Refused(_))),
                "{bad}"
            );
        }
        let c = ReadCommand::GhApiGet {
            path: "repos/o/r/actions/runs?status=queued&per_page=10".into(),
            etag: None,
        };
        let inv = c.invocation().unwrap();
        assert_eq!(
            inv.args,
            [
                "api",
                "--method",
                "GET",
                "--include",
                "repos/o/r/actions/runs?status=queued&per_page=10"
            ]
        );
    }

    #[test]
    fn etags_are_constrained() {
        let get = |etag: &str| ReadCommand::GhApiGet {
            path: "repos/o/r/actions/runs/1/jobs?per_page=50".into(),
            etag: Some(etag.into()),
        };
        let inv = get("W/\"36458da1f62c\"").invocation().unwrap();
        assert_eq!(
            inv.args[4..],
            [
                "-H",
                "If-None-Match: W/\"36458da1f62c\"",
                "repos/o/r/actions/runs/1/jobs?per_page=50"
            ]
        );
        assert!(get("\"abc\"").invocation().is_ok());
        for bad in [
            "",
            "abc",
            "\"a\" -X POST",
            "\"a\nb\"",
            "W/abc",
            "\"a\"b\"",
            "\"a\\\\b\"",
        ] {
            assert!(
                matches!(get(bad).invocation(), Err(ExecError::Refused(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unit_names_are_constrained() {
        for bad in ["", "--now", "a b", "a;b", "../x", "a\nb"] {
            let c = ReadCommand::SystemctlShow {
                unit: bad.into(),
                user: true,
            };
            assert!(
                matches!(c.invocation(), Err(ExecError::Refused(_))),
                "{bad:?}"
            );
        }
        let c = ReadCommand::SystemctlShow {
            unit: "actions.runner.acme-pantry.pantry-local-runner.service".into(),
            user: false,
        };
        let inv = c.invocation().unwrap();
        assert_eq!(inv.args[0], "show");
        assert_eq!(
            inv.args.last().unwrap(),
            "actions.runner.acme-pantry.pantry-local-runner.service"
        );
    }

    #[test]
    fn missing_program_is_reported() {
        let inv = Invocation {
            program: "crowsnest-definitely-missing-binary".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            timeout: Duration::from_secs(1),
        };
        assert!(matches!(run_invocation(&inv), Err(ExecError::NotFound(_))));
    }

    #[test]
    fn timeouts_kill_the_child() {
        let inv = Invocation {
            program: "sleep".into(),
            args: vec!["5".into()],
            cwd: None,
            env: vec![],
            timeout: Duration::from_millis(200),
        };
        let t = Instant::now();
        assert!(matches!(run_invocation(&inv), Err(ExecError::TimedOut(_))));
        assert!(t.elapsed() < Duration::from_secs(3));
    }
}
