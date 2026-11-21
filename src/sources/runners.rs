//! Self-hosted GitHub Actions runners on this machine.
//!
//! A runner is a directory holding the `.runner` file that `config.sh`
//! writes. It is found under the configured folders by name, by being named
//! in the configured units, or from the directory of a running
//! `Runner.Listener` (see [`discover`]). It is up while a `Runner.Listener`
//! process runs from inside that directory, and busy while a `Runner.Worker`
//! does. Its systemd unit comes from the configuration, the cgroup of its
//! listener, or the `.service` file `svc.sh install` writes. How recently it
//! wrote its `_diag` logs comes from file times; what it last did comes from
//! the last 64 KiB of its newest worker and runner logs, never the whole
//! file. All of these are local reads; no API call is needed.

use crate::model::{LogTail, Runner, RunnerSearch, Service, UnitSource};
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

/// The folder names searched for runner directories when none are set.
pub const DEFAULT_NAME: &str = "actions-runner*";

/// Where to look for runners and which systemd unit runs each: the settings
/// of the `[runners]` config section and the `--runner-*` flags, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Find runners from the `Runner.Listener` processes that are running.
    pub processes: bool,
    /// Folders whose subfolders are searched for runner directories.
    pub roots: Vec<PathBuf>,
    /// Glob patterns a subfolder's name must match (`*` and `?`).
    pub names: Vec<String>,
    /// Runner directories and their units. A directory named here is a
    /// runner directory even when it is under no folder and has stopped.
    pub units: Vec<(PathBuf, String)>,
    /// The firstmate home: a folder searched by name never leads into it,
    /// unless the root searched is itself inside it.
    pub fm_home: Option<PathBuf>,
}

impl Default for Discovery {
    fn default() -> Self {
        Discovery {
            processes: true,
            roots: Vec::new(),
            names: vec![DEFAULT_NAME.into()],
            units: Vec::new(),
            fm_home: None,
        }
    }
}

/// Whether `name` matches `pattern`, where `*` matches any run of
/// characters and `?` any one.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and how much of `name` it has taken.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi += 1;
            }
            Some(c) if *c == '?' || *c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    pi = sp + 1;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// A running `Runner.Listener` process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    /// The runner directory it runs from: the nearest folder above its
    /// executable that holds `.runner`.
    pub dir: Option<PathBuf>,
}

/// The runner directory a process started from `exe` (or, when another
/// user's process hides its executable, from its first argument) belongs to:
/// the nearest folder above it that holds `.runner`. The executable is
/// `<dir>/bin/Runner.Listener` or `<dir>/bin.<version>/Runner.Listener`, so
/// the search goes up a few levels rather than taking the parent.
pub fn runner_dir_of(exe: Option<&Path>, argv0: Option<&Path>) -> Option<PathBuf> {
    [exe, argv0].into_iter().flatten().find_map(|path| {
        path.ancestors()
            .skip(1)
            .take(4)
            .find(|a| a.join(".runner").is_file())
            .map(Path::to_path_buf)
    })
}

/// The systemd unit a `/proc/<pid>/cgroup` file puts its process in: the
/// innermost `.service` of its cgroup path, as long as no `.scope` or
/// `.slice` lies below it. A listener started from a terminal sits in
/// `.../user@1000.service/app.slice/app-term.scope`, which belongs to the
/// user's manager and not to any runner unit, so it has none.
pub fn unit_from_cgroup(text: &str) -> Option<String> {
    let mut v2 = None;
    let mut v1 = None;
    for line in text.lines() {
        let mut f = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        match controllers {
            "" => v2 = v2.or(Some(path)),
            "name=systemd" => v1 = v1.or(Some(path)),
            _ => {}
        }
    }
    let path = v1.or(v2)?;
    for part in path.rsplit('/').filter(|p| !p.is_empty()) {
        if part.ends_with(".scope") || part.ends_with(".slice") {
            return None;
        }
        if part.ends_with(".service") {
            let manager = part.starts_with("user@") || part.starts_with("user-runtime-dir@");
            return (!manager && crate::exec::valid_unit(part)).then(|| part.to_string());
        }
    }
    None
}

/// The unit of process `pid` from `<proc_root>/<pid>/cgroup`. Only Linux has
/// that file; elsewhere, and for a process that has gone, there is none.
pub fn unit_of(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    unit_from_cgroup(&text)
}

/// Where Linux keeps its process files.
pub fn proc_root() -> &'static Path {
    Path::new("/proc")
}

/// A runner directory seen while its listener ran, with the unit that
/// listener was in.
pub type Remembered = (PathBuf, Option<String>);

/// What one discovery found.
#[derive(Debug, Default)]
pub struct Found {
    pub runners: Vec<Runner>,
    pub search: RunnerSearch,
    /// What could not be read: a root that cannot be listed, and a `.runner`
    /// file that cannot be read or parsed. Such a runner is left out of
    /// `runners`, so the failure has to be reported alongside it.
    pub errors: Vec<String>,
    /// The directories whose listeners ran, to be passed back as
    /// `remembered` so a listener that dies leaves its runner on screen.
    pub seen: Vec<Remembered>,
}

/// Find the runners: those under the `roots` whose folder name matches
/// `names`, those named in `units`, those whose `Runner.Listener` runs (when
/// `processes` is on), and, likewise, those `remembered` from an earlier
/// discovery of this run, in that order and without repeats.
///
/// A runner's unit comes from, in order: `units`; the cgroup of its listener
/// under `proc_root`; the `.service` file `svc.sh install` wrote in its
/// directory; the unit its listener was last seen in.
pub fn discover(
    d: &Discovery,
    listeners: &[Listener],
    remembered: &[Remembered],
    proc_root: &Path,
) -> Found {
    let (mut dirs, mut errors) = scan_roots(&d.roots, &d.names, d.fm_home.as_deref());
    dirs.extend(d.units.iter().map(|(dir, _)| dir.clone()));
    let mut live: Vec<Remembered> = Vec::new();
    let mut unlocated = 0;
    if d.processes {
        for l in listeners {
            match &l.dir {
                Some(dir) => live.push((dir.clone(), unit_of(proc_root, l.pid))),
                None => unlocated += 1,
            }
        }
    }
    dirs.extend(live.iter().map(|(dir, _)| dir.clone()));
    // What listeners were seen to run from belongs to finding them.
    let remembered: &[Remembered] = if d.processes { remembered } else { &[] };
    dirs.extend(remembered.iter().map(|(dir, _)| dir.clone()));
    let mut runners: Vec<Runner> = Vec::new();
    let mut visited: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if visited.iter().any(|v| same_dir(v, &dir)) {
            continue;
        }
        visited.push(dir.clone());
        let file = dir.join(".runner");
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            // A directory that was never configured is not a runner.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push(format!("{}: {}", io_reason(&e), file.display()));
                continue;
            }
        };
        let Some(mut r) = parse_runner_file(&dir, &text) else {
            errors.push(format!("not a runner file: {}", file.display()));
            continue;
        };
        let find = |list: &[Remembered]| {
            list.iter()
                .find(|(x, _)| same_dir(x, &r.dir))
                .and_then(|(_, u)| u.clone())
        };
        let (unit, source) = if let Some((_, u)) = d.units.iter().find(|(x, _)| same_dir(x, &r.dir))
        {
            (Some(u.clone()), Some(UnitSource::Configured))
        } else if let Some(u) = find(&live) {
            (Some(u), Some(UnitSource::Listener))
        } else if let Some(u) = std::fs::read_to_string(r.dir.join(".service"))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|u| crate::exec::valid_unit(u))
        {
            (Some(u), Some(UnitSource::Recorded))
        } else if let Some(u) = find(remembered) {
            (Some(u), Some(UnitSource::Listener))
        } else {
            (None, None)
        };
        r.unit = unit;
        r.unit_source = source;
        runners.push(r);
    }
    Found {
        search: RunnerSearch {
            processes: d.processes,
            listeners: if d.processes { listeners.len() } else { 0 },
            unlocated,
            roots: d.roots.clone(),
            names: d.names.clone(),
            pinned: d.units.len(),
        },
        seen: live,
        runners,
        errors,
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The subfolders of each root whose name matches one of `names`, sorted
/// within each root, and the roots that could not be listed. None is inside
/// `fm_home` unless its root is.
fn scan_roots(
    roots: &[PathBuf],
    names: &[String],
    fm_home: Option<&Path>,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for root in roots {
        let fence = fm_home.filter(|h| !crate::config::inside(root, h));
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) => {
                errors.push(format!("{}: {}", io_reason(&e), root.display()));
                continue;
            }
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    names.iter().any(|g| glob_match(g, &n))
                }) && p.is_dir()
                    && !fence.is_some_and(|h| crate::config::inside(p, h))
            })
            .collect();
        dirs.sort();
        out.extend(dirs);
    }
    (out, errors)
}

/// An I/O error without the OS error number: "Permission denied".
fn io_reason(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// Unix modification time of a file.
fn mtime(p: &Path) -> Option<i64> {
    let t = std::fs::metadata(p).and_then(|m| m.modified()).ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// The newest `<prefix>*.log` in `dir`, by its name: the runner names its
/// logs by their start time, so the newest name is the live log.
fn newest_log(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix) && n.ends_with(".log"))
        .max()?;
    Some(dir.join(newest))
}

/// When the runner last wrote its listener log, its worker log and a
/// running step's output (`_diag/pages`). Only file times are read.
pub fn read_diag(r: &mut Runner) {
    let diag = r.dir.join("_diag");
    r.runner_log_at = newest_log(&diag, "Runner_").and_then(|p| mtime(&p));
    r.worker_log_at = newest_log(&diag, "Worker_").and_then(|p| mtime(&p));
    r.step_output_at = std::fs::read_dir(diag.join("pages"))
        .ok()
        .and_then(|d| d.flatten().filter_map(|e| mtime(&e.path())).max());
}

/// The most of a log's end that is read.
pub const TAIL_BYTES: u64 = 64 * 1024;

/// The last `max` bytes of the file at `path`, from its first whole line,
/// with control characters other than newlines and tabs removed. Only the
/// tail is read, never the whole file.
pub fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // The cut fell inside a line: drop that part.
        text = match text.find('\n') {
            Some(i) => text[i + 1..].to_string(),
            None => String::new(),
        };
    }
    text.retain(|c| !c.is_control() || c == '\n' || c == '\t');
    Ok(text)
}

/// A `_diag` log line: `[2026-11-20 18:28:07Z INFO StepsRunner] text` ->
/// (time, level, component, text). Continuation lines (stack traces) have
/// no such head and are skipped.
fn diag_line(line: &str) -> Option<(i64, &str, &str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (head, text) = rest.split_once("] ")?;
    let mut f = head.split_whitespace();
    let (day, time, level, component) = (f.next()?, f.next()?, f.next()?, f.next()?);
    let at = crate::timefmt::parse_iso(&format!("{day}T{time}"))?;
    Some((at, level, component, text.trim_end()))
}

/// What the tail of a `Worker_*.log` says: its last line, the step it last
/// started, and whether a process it started is still running.
pub fn parse_worker_tail(text: &str) -> LogTail {
    let mut t = LogTail::default();
    for (at, level, component, msg) in text.lines().filter_map(diag_line) {
        t.last = Some((at, format!("[{level} {component}] {msg}")));
        if let Some(name) = msg.strip_prefix("Processing step: DisplayName='") {
            let name = name.strip_suffix('\'').unwrap_or(name);
            t.step = Some((at, name.to_string()));
            t.process_running = false;
        } else if component == "ProcessInvokerWrapper" {
            if msg.starts_with("Starting process:") {
                t.process_running = true;
            } else if msg.starts_with("Finished process") {
                t.process_running = false;
            }
        }
    }
    t
}

/// What the tail of a `Runner_*.log` says: its last line, the last line
/// the listener wrote to its console, and the console errors after it.
pub fn parse_runner_tail(text: &str) -> LogTail {
    let mut t = LogTail::default();
    for (at, level, component, msg) in text.lines().filter_map(diag_line) {
        t.last = Some((at, format!("[{level} {component}] {msg}")));
        if component != "Terminal" {
            continue;
        }
        // `WRITE LINE: 2026-11-20 18:28:06Z: Running job: Lint`
        let console = |kind: &str| {
            let m = msg.strip_prefix(kind)?;
            let m = m.split_once("Z: ").map_or(m, |(_, m)| m).trim();
            (!m.is_empty()).then(|| m.to_string())
        };
        if let Some(m) = console("WRITE LINE: ") {
            if m.starts_with("Current runner version") {
                continue;
            }
            t.state = Some((at, m));
            t.errors = 0;
            t.error_since = None;
            t.error = None;
        } else if let Some(m) = console("WRITE ERROR: ") {
            t.errors += 1;
            t.error_since.get_or_insert(at);
            t.error = Some((at, m));
        }
    }
    t
}

/// Reads the tails of each runner's newest worker and runner logs, again
/// only when a file's size or time changed.
#[derive(Debug, Default)]
pub struct LogTails {
    seen: BTreeMap<PathBuf, ((u64, Option<std::time::SystemTime>), LogTail)>,
}

impl LogTails {
    pub fn read(&mut self, r: &mut Runner) {
        let diag = r.dir.join("_diag");
        r.worker_tail = self.tail(newest_log(&diag, "Worker_"), parse_worker_tail);
        r.runner_tail = self.tail(newest_log(&diag, "Runner_"), parse_runner_tail);
    }

    fn tail(&mut self, path: Option<PathBuf>, parse: fn(&str) -> LogTail) -> Option<LogTail> {
        let path = path?;
        let meta = std::fs::metadata(&path).ok()?;
        let key = (meta.len(), meta.modified().ok());
        if let Some((k, t)) = self.seen.get(&path)
            && *k == key
        {
            return Some(t.clone());
        }
        let t = parse(&read_tail(&path, TAIL_BYTES).ok()?);
        // A newer log of the same kind replaces the older one.
        let kind = |p: &Path| {
            let name = p.file_name()?.to_str()?;
            Some((
                p.parent()?.to_path_buf(),
                name.split('_').next()?.to_string(),
            ))
        };
        let this = kind(&path);
        self.seen.retain(|p, _| kind(p) != this);
        self.seen.insert(path, (key, t.clone()));
        Some(t)
    }

    /// Forget logs that are no longer any runner's newest.
    pub fn keep_only(&mut self, runners: &[Runner]) {
        self.seen
            .retain(|p, _| runners.iter().any(|r| p.starts_with(&r.dir)));
    }
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

    /// Whether the process table cannot be read at all, so no runner can be
    /// told up or busy.
    pub fn unavailable(&self) -> bool {
        self.sys.is_none()
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
        self.refresh();
        self.apply(runners, now);
    }

    /// Read the process table afresh.
    pub fn refresh(&mut self) {
        let Some(sys) = self.sys.as_mut() else { return };
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_cpu(),
        );
    }

    /// The `Runner.Listener` processes in the table as last read, with the
    /// runner directory each runs from.
    pub fn listeners(&self) -> Vec<Listener> {
        let Some(sys) = self.sys.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<Listener> = sys
            .processes()
            .iter()
            .filter(|(_, p)| p.name().to_string_lossy() == "Runner.Listener")
            .map(|(pid, p)| Listener {
                pid: pid.as_u32(),
                dir: runner_dir_of(p.exe(), p.cmd().first().map(Path::new)),
            })
            .collect();
        out.sort_by_key(|l| l.pid);
        out
    }

    /// Mark `runners` up and busy from the process table as last read.
    pub fn apply(&mut self, runners: &mut [Runner], now: i64) {
        let Some(sys) = self.sys.as_mut() else {
            for r in runners.iter_mut() {
                r.scanned_at = None;
            }
            return;
        };
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

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crowsnest-runners-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A runner directory `name` under `root`, as `config.sh` leaves it, with
    /// the listener binary where the runner puts it.
    fn runner_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("bin.2.338.2")).unwrap();
        std::fs::write(dir.join("bin.2.338.2").join("Runner.Listener"), "").unwrap();
        std::fs::write(
            dir.join(".runner"),
            include_str!("../../tests/fixtures/runner/.runner"),
        )
        .unwrap();
        dir
    }

    fn fixture_proc() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proc")
    }

    fn only_roots(roots: &[PathBuf]) -> Discovery {
        Discovery {
            processes: false,
            roots: roots.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn discovers_runner_directories() {
        let root = scratch("dirs");
        let dir = runner_dir(&root, "actions-runner-pantry");
        std::fs::create_dir_all(root.join("unrelated")).unwrap();
        std::fs::write(
            dir.join(".service"),
            "actions.runner.acme-pantry.pantry-local-runner.service\n",
        )
        .unwrap();
        let none = Path::new("/nonexistent");
        let found = discover(&only_roots(std::slice::from_ref(&root)), &[], &[], none);
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        assert_eq!(found.runners.len(), 1);
        let r = &found.runners[0];
        assert_eq!(r.dir, dir);
        assert_eq!(
            r.unit.as_deref(),
            Some("actions.runner.acme-pantry.pantry-local-runner.service")
        );
        assert_eq!(r.unit_source, Some(UnitSource::Recorded));
        // A configured unit wins over the recorded one.
        let d = Discovery {
            units: vec![(dir.clone(), "actions-runner-pantry.service".into())],
            ..only_roots(std::slice::from_ref(&root))
        };
        let found = discover(&d, &[], &[], none);
        assert_eq!(found.runners.len(), 1, "named twice, listed once");
        assert_eq!(
            found.runners[0].unit.as_deref(),
            Some("actions-runner-pantry.service")
        );
        assert_eq!(found.runners[0].unit_source, Some(UnitSource::Configured));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn names_choose_the_folders_searched() {
        let root = scratch("names");
        runner_dir(&root, "actions-runner-a");
        runner_dir(&root, "gh-runner-b");
        runner_dir(&root, "other");
        let found = |names: &[&str]| -> Vec<String> {
            let d = Discovery {
                names: names.iter().map(|n| n.to_string()).collect(),
                ..only_roots(std::slice::from_ref(&root))
            };
            discover(&d, &[], &[], Path::new("/nonexistent"))
                .runners
                .iter()
                .map(|r| r.dir.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(found(&["actions-runner*"]), ["actions-runner-a"]);
        assert_eq!(
            found(&["actions-runner*", "gh-runner-?"]),
            ["actions-runner-a", "gh-runner-b"]
        );
        assert_eq!(found(&["*"]).len(), 3);
        assert!(found(&["nothing*"]).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn globs_match_stars_and_question_marks() {
        for (p, n, want) in [
            ("actions-runner*", "actions-runner", true),
            ("actions-runner*", "actions-runner-app", true),
            ("actions-runner*", "my-actions-runner", false),
            ("*runner*", "my-actions-runner-2", true),
            ("gh-?", "gh-1", true),
            ("gh-?", "gh-12", false),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("*", "", true),
            ("", "", true),
            ("", "x", false),
        ] {
            assert_eq!(glob_match(p, n), want, "{p} vs {n}");
        }
    }

    #[test]
    fn the_runner_dir_is_found_above_the_listener_executable() {
        let root = scratch("above");
        let dir = runner_dir(&root, "ci-box");
        // The real executable sits in a versioned folder; the first argument
        // of the command line says `bin`.
        let exe = dir.join("bin.2.338.2").join("Runner.Listener");
        let argv0 = dir.join("bin").join("Runner.Listener");
        assert_eq!(runner_dir_of(Some(&exe), None), Some(dir.clone()));
        assert_eq!(
            runner_dir_of(None, Some(&argv0)),
            Some(dir.clone()),
            "another user's process hides its executable"
        );
        assert_eq!(runner_dir_of(Some(Path::new("/usr/bin/x")), None), None);
        assert_eq!(runner_dir_of(None, None), None);
        // A `.runner` far above the executable is not its runner.
        let deep = dir.join("a").join("b").join("c").join("d").join("e");
        assert_eq!(
            runner_dir_of(Some(&deep.join("Runner.Listener")), None),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn units_come_from_cgroup_strings() {
        let unit = |t: &str| unit_from_cgroup(t);
        assert_eq!(
            unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/actions-runner-potful.service\n").as_deref(),
            Some("actions-runner-potful.service"),
            "a hand-written user unit"
        );
        assert_eq!(
            unit("0::/system.slice/actions.runner.acme-pantry.pantry-local-runner.service")
                .as_deref(),
            Some("actions.runner.acme-pantry.pantry-local-runner.service"),
            "a svc.sh system unit"
        );
        assert_eq!(
            unit("0::/system.slice/foo.service/payload\n").as_deref(),
            Some("foo.service"),
            "a delegated cgroup below the service"
        );
        assert_eq!(
            unit("12:pids:/x.slice/app.scope\n1:name=systemd:/system.slice/b.service\n0::/system.slice/b.service\n").as_deref(),
            Some("b.service"),
            "cgroup v1 reads the systemd hierarchy"
        );
        // Started from a terminal: the user's manager is not a runner unit.
        assert_eq!(
            unit("0::/user.slice/user-1000.slice/session-199.scope\n"),
            None
        );
        assert_eq!(
            unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-term.scope\n"),
            None
        );
        assert_eq!(
            unit("0::/user.slice/user-1000.slice/user@1000.service\n"),
            None
        );
        assert_eq!(unit("0::/\n"), None);
        assert_eq!(unit(""), None);
        assert_eq!(unit("garbage"), None);
        assert_eq!(
            unit("0::/system.slice/-rf.service"),
            None,
            "never an option for systemctl"
        );
    }

    #[test]
    fn units_are_read_from_a_proc_tree() {
        let proc = fixture_proc();
        assert_eq!(
            unit_of(&proc, 4242).as_deref(),
            Some("actions-runner-potful.service")
        );
        assert_eq!(
            unit_of(&proc, 4243).as_deref(),
            Some("actions.runner.acme-pantry.pantry-local-runner.service")
        );
        assert_eq!(unit_of(&proc, 4244), None, "a terminal session");
        assert_eq!(
            unit_of(&proc, 4245).as_deref(),
            Some("actions.runner.acme-pantry.build.service")
        );
        assert_eq!(unit_of(&proc, 4246), None, "a terminal app");
        assert_eq!(unit_of(&proc, 1), None, "a process that is not there");
        assert_eq!(unit_of(Path::new("/nonexistent"), 4242), None, "no /proc");
    }

    #[test]
    fn running_listeners_find_runners_anywhere() {
        let root = scratch("procs");
        let dir = runner_dir(&root, "ci-box");
        let listener = |pid, dir: &Path| Listener {
            pid,
            dir: runner_dir_of(Some(&dir.join("bin.2.338.2").join("Runner.Listener")), None),
        };
        let proc = fixture_proc();
        let d = Discovery::default();
        let l = [listener(4242, &dir)];
        let found = discover(&d, &l, &[], &proc);
        assert_eq!(found.runners.len(), 1, "no folder was searched");
        let r = &found.runners[0];
        assert_eq!(r.dir, dir);
        assert_eq!(r.unit.as_deref(), Some("actions-runner-potful.service"));
        assert_eq!(r.unit_source, Some(UnitSource::Listener));
        assert_eq!(found.seen, [(dir.clone(), r.unit.clone())]);
        assert_eq!(found.search.listeners, 1);

        // A listener that is not under systemd leaves the unit unknown.
        let found = discover(&d, &[listener(4244, &dir)], &[], &proc);
        assert_eq!(found.runners[0].unit, None);

        // With processes off, nothing is found from them, nor from what
        // they were remembered to run from.
        let off = Discovery {
            processes: false,
            ..Default::default()
        };
        let found = discover(&off, &l, &[(dir.clone(), None)], &proc);
        assert!(found.runners.is_empty());
        assert_eq!((found.search.processes, found.search.listeners), (false, 0));

        // A listener with no runner directory above it is counted, not shown.
        let lost = Listener { pid: 9, dir: None };
        let found = discover(&d, &[lost], &[], &proc);
        assert!(found.runners.is_empty());
        assert_eq!((found.search.listeners, found.search.unlocated), (1, 1));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_unit_comes_from_the_most_specific_source() {
        let root = scratch("precedence");
        let dir = runner_dir(&root, "actions-runner-ci");
        std::fs::write(dir.join(".service"), "recorded.service\n").unwrap();
        let proc = fixture_proc();
        let l = [Listener {
            pid: 4242,
            dir: Some(dir.clone()),
        }];
        let found = |units: Vec<(PathBuf, String)>, l: &[Listener], mem: &[Remembered]| {
            let d = Discovery {
                units,
                ..only_roots(std::slice::from_ref(&root))
            };
            let f = discover(
                &Discovery {
                    processes: true,
                    ..d
                },
                l,
                mem,
                &proc,
            );
            assert_eq!(f.runners.len(), 1, "one runner however it was found");
            (f.runners[0].unit.clone(), f.runners[0].unit_source)
        };
        let configured = vec![(dir.clone(), "configured.service".to_string())];
        let remembered = [(dir.clone(), Some("old.service".to_string()))];
        let s = |u: &str| Some(u.to_string());
        assert_eq!(
            found(configured, &l, &remembered),
            (s("configured.service"), Some(UnitSource::Configured))
        );
        assert_eq!(
            found(vec![], &l, &remembered),
            (
                s("actions-runner-potful.service"),
                Some(UnitSource::Listener)
            )
        );
        assert_eq!(
            found(vec![], &[], &remembered),
            (s("recorded.service"), Some(UnitSource::Recorded)),
            "the file svc.sh wrote beats what an old listener was in"
        );
        std::fs::remove_file(dir.join(".service")).unwrap();
        assert_eq!(
            found(vec![], &[], &remembered),
            (s("old.service"), Some(UnitSource::Listener)),
            "a listener that has died leaves its runner and unit on screen"
        );
        assert_eq!(found(vec![], &[], &[]), (None, None));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_unit_directory_is_a_runner_even_when_stopped() {
        let root = scratch("pinned");
        let dir = runner_dir(&root, "my-ci");
        let d = Discovery {
            units: vec![
                (dir.clone(), "my-ci.service".into()),
                (root.join("gone"), "gone.service".into()),
            ],
            ..only_roots(&[])
        };
        let found = discover(&d, &[], &[], Path::new("/nonexistent"));
        assert_eq!(found.runners.len(), 1, "a missing directory is skipped");
        assert_eq!(found.runners[0].dir, dir);
        assert_eq!(found.search.pinned, 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discovery_reports_what_it_cannot_read() {
        let root = std::env::temp_dir().join(format!("crowsnest-broken-{}", std::process::id()));
        let broken = root.join("actions-runner-broken");
        let fresh = root.join("actions-runner-unconfigured");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(broken.join(".runner"), "{not json").unwrap();
        std::fs::write(
            root.join("actions-runner-linux-x64-2.321.0.tar.gz"),
            "tarball",
        )
        .unwrap();
        let missing = root.join("gone");
        let found = discover(
            &only_roots(&[root.clone(), missing.clone()]),
            &[],
            &[],
            Path::new("/nonexistent"),
        );
        assert!(found.runners.is_empty());
        let errors = found.errors;
        assert_eq!(errors.len(), 2, "{errors:?}");
        // Every root is listed before any runner file is read.
        assert_eq!(
            errors[0],
            format!("No such file or directory: {}", missing.display())
        );
        assert!(errors[1].starts_with("not a runner file: "));
        assert!(errors[1].ends_with("actions-runner-broken/.runner"));
        // A broken directory reached from every source is reported once.
        let d = Discovery {
            processes: true,
            units: vec![(broken.clone(), "broken.service".into())],
            ..only_roots(std::slice::from_ref(&root))
        };
        let found = discover(
            &d,
            &[Listener {
                pid: 4244,
                dir: Some(broken.clone()),
            }],
            &[(broken.clone(), None)],
            Path::new("/nonexistent"),
        );
        assert_eq!(found.errors.len(), 1, "{:?}", found.errors);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_search_by_name_never_enters_the_firstmate_home() {
        let root = scratch("fmhome");
        let fm = runner_dir(&root, "firstmate");
        runner_dir(&root, "actions-runner-a");
        let d = Discovery {
            names: vec!["*".into()],
            fm_home: Some(fm.clone()),
            ..only_roots(std::slice::from_ref(&root))
        };
        let found = discover(&d, &[], &[], Path::new("/nonexistent"));
        let dirs: Vec<_> = found.runners.iter().map(|r| r.dir.clone()).collect();
        assert_eq!(dirs, [root.join("actions-runner-a")]);
        // A root inside the firstmate home, as only a flag can give, is searched.
        let inner = runner_dir(&fm, "actions-runner-b");
        let d = Discovery {
            roots: vec![fm.clone()],
            ..d
        };
        let found = discover(&d, &[], &[], Path::new("/nonexistent"));
        let dirs: Vec<_> = found.runners.iter().map(|r| r.dir.clone()).collect();
        assert_eq!(dirs, [inner]);
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

    fn at(s: &str) -> i64 {
        crate::timefmt::parse_iso(s).unwrap()
    }

    #[test]
    fn the_worker_tail_names_its_step_and_a_running_process() {
        let t = parse_worker_tail(include_str!("../../tests/fixtures/runner/worker-tail.log"));
        assert_eq!(
            t.step,
            Some((
                at("2026-11-19T14:18:42Z"),
                "Run actions/setup-node@v4".into()
            ))
        );
        assert!(t.process_running, "setup-node's process has not finished");
        assert_eq!(
            t.last,
            Some((
                at("2026-11-19T14:27:02Z"),
                "[INFO JobServerQueue] Try to upload 2 log files or attachments, success rate: 2/2."
                    .into()
            ))
        );
        // The step before finished its process.
        let done = parse_worker_tail(
            &include_str!("../../tests/fixtures/runner/worker-tail.log")
                .lines()
                .take(6)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(done.step.unwrap().1, "Run actions/checkout@v4");
        assert!(!done.process_running);
        assert_eq!(parse_worker_tail("not a log\n"), LogTail::default());
    }

    #[test]
    fn the_runner_tail_keeps_the_console_state_and_errors_after_it() {
        let text = include_str!("../../tests/fixtures/runner/runner-tail.log");
        let t = parse_runner_tail(text);
        assert_eq!(
            t.state,
            Some((
                at("2026-11-19T13:18:40Z"),
                "Job Lint & typecheck completed with result: Succeeded".into()
            ))
        );
        // A routine back-off is not an error; the console's errors are.
        assert_eq!(t.errors, 2);
        assert_eq!(t.error_since, Some(at("2026-11-19T13:26:03Z")));
        assert_eq!(
            t.error,
            Some((
                at("2026-11-19T13:46:12Z"),
                "Runner connect error: Error: Conflict. Retrying until reconnected.".into()
            ))
        );
        // A console line after them clears them.
        let back = parse_runner_tail(&format!(
            "{text}[2026-11-19 13:50:00Z INFO Terminal] WRITE LINE: 2026-11-19 13:50:00Z: Runner reconnected.\n"
        ));
        assert_eq!(back.state.unwrap().1, "Runner reconnected.");
        assert_eq!((back.errors, back.error_since, back.error), (0, None, None));
    }

    #[test]
    fn only_the_tail_of_a_log_is_read() {
        let dir = scratch("tail");
        let path = dir.join("Worker_20261119-141834-utc.log");
        let line = "[2026-11-19 14:18:42Z INFO JobServerQueue] padding padding padding padding\n";
        let mut text = line.repeat(2000);
        text.push_str("[2026-11-19 14:27:02Z INFO StepsRunner] Processing step: DisplayName='Run \u{1b}[31mtests'\n");
        std::fs::write(&path, &text).unwrap();
        assert!(text.len() as u64 > TAIL_BYTES);
        let tail = read_tail(&path, TAIL_BYTES).unwrap();
        assert!(tail.len() as u64 <= TAIL_BYTES);
        assert!(
            tail.starts_with("[2026-11-19 14:18:42Z"),
            "starts at a whole line"
        );
        assert!(
            tail.ends_with("DisplayName='Run [31mtests'\n"),
            "escape removed: {:?}",
            &tail[tail.len() - 60..]
        );
        let short = read_tail(&path, 10).unwrap();
        assert_eq!(short, "", "no whole line fits");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_tails_are_read_again_only_when_the_newest_log_changes() {
        let dir = scratch("tails");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        let worker = diag.join("Worker_20261119-090000-utc.log");
        std::fs::write(
            &worker,
            "[2026-11-19 09:00:01Z INFO StepsRunner] Processing step: DisplayName='Build'\n",
        )
        .unwrap();
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let mut tails = LogTails::default();
        tails.read(&mut r);
        assert_eq!(
            r.worker_tail.as_ref().unwrap().step.as_ref().unwrap().1,
            "Build"
        );
        assert_eq!(r.runner_tail, None, "no runner log yet");
        // A newer worker log (the next job) replaces the old one.
        std::fs::write(
            diag.join("Worker_20261119-100000-utc.log"),
            "[2026-11-19 10:00:01Z INFO StepsRunner] Processing step: DisplayName='Test'\n",
        )
        .unwrap();
        tails.read(&mut r);
        assert_eq!(
            r.worker_tail.as_ref().unwrap().step.as_ref().unwrap().1,
            "Test"
        );
        assert_eq!(tails.seen.len(), 1, "the older log is forgotten");
        tails.keep_only(&[]);
        assert!(tails.seen.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
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
