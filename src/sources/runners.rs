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
//! following its newest worker and runner logs: read once from the end, then
//! only what they grew by, never the whole file. All of these are local
//! reads; no API call is needed.

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

/// The newest listener and worker logs of a runner's `_diag`. The runner
/// names its logs by their start time, so the newest name is the live log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiagLogs {
    pub runner: Option<PathBuf>,
    pub worker: Option<PathBuf>,
}

/// The newest `Runner_*.log` and `Worker_*.log` of `dir`, from one listing:
/// a busy runner's `_diag` holds thousands of files.
fn list_diag(dir: &Path) -> DiagLogs {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return DiagLogs::default();
    };
    let (mut runner, mut worker): (Option<String>, Option<String>) = (None, None);
    for name in entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
    {
        let slot = if name.starts_with("Runner_") {
            &mut runner
        } else if name.starts_with("Worker_") {
            &mut worker
        } else {
            continue;
        };
        if name.ends_with(".log") && slot.as_ref().is_none_or(|n| name > *n) {
            *slot = Some(name);
        }
    }
    DiagLogs {
        runner: runner.map(|n| dir.join(n)),
        worker: worker.map(|n| dir.join(n)),
    }
}

/// When the runner last wrote its listener log, its worker log and a
/// running step's output (`_diag/pages`). Only file times are read. Returns
/// the logs found, for [`LogTails::read`] to follow in the same turn: `_diag`
/// is listed once per turn, not once per question.
pub fn read_diag(r: &mut Runner) -> DiagLogs {
    let diag = r.dir.join("_diag");
    let logs = list_diag(&diag);
    r.runner_log_at = logs.runner.as_deref().and_then(mtime);
    r.worker_log_at = logs.worker.as_deref().and_then(mtime);
    r.step_output_at = std::fs::read_dir(diag.join("pages"))
        .ok()
        .and_then(|d| d.flatten().filter_map(|e| mtime(&e.path())).max());
    logs
}

/// The size of one block read when a log is first looked at, and the most
/// of its end read when that is enough to know the listener's or worker's
/// state.
pub const TAIL_BYTES: u64 = 64 * 1024;

/// The most of a log's end read, a [`TAIL_BYTES`] block at a time, to find
/// the line that says what a runner is doing (the step a worker started, the
/// listener's last console line) when it is further back than one block.
pub const SEARCH_BYTES: u64 = 2 * 1024 * 1024;

/// The most new bytes folded into the state of a log at once. A log that
/// grew more than this since it was last read (crowsnest was suspended) is
/// looked at afresh instead.
const FOLD_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// `bytes` as text, with control characters other than newlines and tabs
/// removed.
fn clean(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    text.retain(|c| !c.is_control() || c == '\n' || c == '\t');
    text
}

/// The bytes of the file at `path` from `from` to `to`.
fn read_range(path: &Path, from: u64, to: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.take(to.saturating_sub(from)).read_to_end(&mut buf)?;
    Ok(buf)
}

/// The whole lines at the end of the file at `path`: one [`TAIL_BYTES`]
/// block, and further blocks back while `enough` says the text does not yet
/// hold what is looked for, up to [`SEARCH_BYTES`] or the start of the file.
/// Returns the text and the offset just after it, where the next lines of
/// the log begin. A last line without its newline is not read yet: it may
/// be half written.
fn read_back(path: &Path, enough: fn(&str) -> bool) -> std::io::Result<(String, u64)> {
    let len = std::fs::metadata(path)?.len();
    let mut start = len;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let from = start.saturating_sub(TAIL_BYTES);
        let mut block = read_range(path, from, start)?;
        block.append(&mut buf);
        buf = block;
        start = from;
        let first = if start > 0 {
            buf.iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |i| i + 1)
        } else {
            0
        };
        let last = buf
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(first, |i| i + 1);
        let text = clean(&buf[first.min(last)..last]);
        if start == 0 || len - start >= SEARCH_BYTES || enough(&text) {
            return Ok((text, start + last as u64));
        }
    }
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

/// The text a line of a worker log gives for the step it started.
const STEP_LINE: &str = "Processing step: DisplayName='";

/// Fold the whole lines of `text`, from a `Worker_*.log`, into what is
/// known: the last line, the step last started, and whether a process it
/// started is still running.
pub fn fold_worker(t: &mut LogTail, text: &str) {
    for (at, level, component, msg) in text.lines().filter_map(diag_line) {
        t.last = Some((at, format!("[{level} {component}] {msg}")));
        if let Some(name) = msg.strip_prefix(STEP_LINE) {
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
}

/// What a worker log's text says: its last line, the step it last started,
/// and whether a process it started is still running.
pub fn parse_worker_tail(text: &str) -> LogTail {
    let mut t = LogTail::default();
    fold_worker(&mut t, text);
    t
}

/// What a console line of the listener says: `WRITE LINE: 2026-11-20
/// 18:28:06Z: Running job: Lint` -> `Running job: Lint`. Blank lines say
/// nothing.
fn console(msg: &str, kind: &str) -> Option<String> {
    let m = msg.strip_prefix(kind)?;
    let m = m.split_once("Z:").map_or(m, |(_, m)| m).trim();
    (!m.is_empty()).then(|| m.to_string())
}

/// A console error without the listener's own framing, which only repeats
/// what the label says: `Runner connect error: Error: Conflict. Retrying
/// until reconnected.` -> `Error: Conflict.`
fn console_error_text(m: &str) -> String {
    let m = m.strip_prefix("Runner connect error: ").unwrap_or(m);
    m.strip_suffix(" Retrying until reconnected.")
        .unwrap_or(m)
        .to_string()
}

/// Whether the text has an ordinary console line of the listener.
fn has_console_state(text: &str) -> bool {
    text.lines()
        .filter_map(diag_line)
        .filter(|(_, _, component, _)| *component == "Terminal")
        .filter_map(|(_, _, _, msg)| console(msg, "WRITE LINE: "))
        .any(|m| !m.starts_with("Current runner version"))
}

/// Listener components whose failures mean it cannot reach GitHub.
const CONNECTION: [&str; 4] = [
    "BrokerMessageListener",
    "BrokerServer",
    "GitHubActionsService",
    "MessageListener",
];

/// Whether a line shows the listener failing to reach GitHub: an error
/// from the connection, a failed attempt, a back-off, or a sleep before
/// retrying.
fn failure_line(level: &str, component: &str, msg: &str) -> bool {
    CONNECTION.contains(&component)
        && (level == "ERR"
            || msg.starts_with("Back off ")
            || (msg.starts_with("Attempt ") && msg.contains(" failed"))
            || (msg.starts_with("Sleeping for ") && msg.contains("before retrying")))
}

/// Note a failure line at `at`. Failure lines of one outage follow each
/// other within the listener's request timeouts; one after a longer quiet
/// stretch starts a new episode, so an error that ended long ago is not
/// revived by a routine back-off hours later.
fn note_failure(t: &mut LogTail, at: i64) {
    if t.failure_at
        .is_some_and(|f| at - f > crate::model::FAILURE_LIVE_SECS)
    {
        t.errors = 0;
        t.error_since = None;
        t.error = None;
        t.failure_since = None;
    }
    t.failure_since.get_or_insert(at);
    t.failure_at = Some(at);
}

/// Fold the whole lines of `text`, from a `Runner_*.log`, into what is
/// known: the last line, the last line the listener wrote to its console,
/// and the console errors and failing attempts after it.
pub fn fold_runner(t: &mut LogTail, text: &str) {
    for (at, level, component, msg) in text.lines().filter_map(diag_line) {
        t.last = Some((at, format!("[{level} {component}] {msg}")));
        if component == "Terminal" {
            if let Some(m) = console(msg, "WRITE LINE: ") {
                if m.starts_with("Current runner version") {
                    continue;
                }
                t.state = Some((at, m));
                t.errors = 0;
                t.error_since = None;
                t.error = None;
                t.failure_since = None;
                t.failure_at = None;
            } else if let Some(m) = console(msg, "WRITE ERROR: ") {
                note_failure(t, at);
                t.errors += 1;
                t.error_since.get_or_insert(at);
                t.error = Some((at, console_error_text(&m)));
            }
        } else if failure_line(level, component, msg) {
            note_failure(t, at);
        }
    }
}

/// What a runner log's text says: its last line, the last line the listener
/// wrote to its console, and the console errors after it.
pub fn parse_runner_tail(text: &str) -> LogTail {
    let mut t = LogTail::default();
    fold_runner(&mut t, text);
    t
}

/// A log being followed: how far it has been read, what its file looked
/// like then, and what its lines add up to.
#[derive(Debug)]
struct Followed {
    /// Just after the last whole line folded in.
    offset: u64,
    stamp: (u64, Option<std::time::SystemTime>),
    tail: LogTail,
}

/// Follows the newest worker and runner logs of each runner. A log is read
/// once from its end, searching back for the line that tells the state, and
/// after that only the bytes it grew by are folded into what is known, so a
/// line that scrolled out of the last 64 KiB is not forgotten. A log whose
/// size and time did not change is not read again.
#[derive(Debug, Default)]
pub struct LogTails {
    seen: BTreeMap<PathBuf, Followed>,
}

impl LogTails {
    /// Follow the logs `logs` that this turn's listing of `r`'s `_diag`
    /// found (see [`read_diag`]).
    pub fn read(&mut self, r: &mut Runner, logs: &DiagLogs) {
        r.worker_tail = self.tail(logs.worker.clone(), fold_worker, |text| {
            text.contains(STEP_LINE)
        });
        r.runner_tail = self.tail(logs.runner.clone(), fold_runner, has_console_state);
    }

    /// `fold` sums up the lines of the log at `path`; `enough` says whether
    /// a first look at its end has found what the sum needs.
    fn tail(
        &mut self,
        path: Option<PathBuf>,
        fold: fn(&mut LogTail, &str),
        enough: fn(&str) -> bool,
    ) -> Option<LogTail> {
        let path = path?;
        let meta = std::fs::metadata(&path).ok()?;
        let len = meta.len();
        let stamp = (len, meta.modified().ok());
        if let Some(f) = self.seen.get_mut(&path) {
            if f.stamp == stamp {
                return Some(f.tail.clone());
            }
            if len >= f.offset && len - f.offset <= FOLD_MAX_BYTES {
                let new = read_range(&path, f.offset, len).ok()?;
                // Whole lines only: the last one may be half written.
                let whole = new.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                fold(&mut f.tail, &clean(&new[..whole]));
                f.offset += whole as u64;
                f.stamp = stamp;
                return Some(f.tail.clone());
            }
            // Shorter than it was (replaced) or grown past reason: look
            // at it afresh.
        }
        let (text, offset) = read_back(&path, enough).ok()?;
        let mut tail = LogTail::default();
        fold(&mut tail, &text);
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
        self.seen.insert(
            path,
            Followed {
                offset,
                stamp,
                tail: tail.clone(),
            },
        );
        Some(tail)
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
    use crate::model::Trouble;

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

    #[test]
    fn the_diag_directory_is_listed_once_and_the_newest_of_each_kind_found() {
        let dir = std::env::temp_dir().join(format!("crowsnest-diag-once-{}", std::process::id()));
        let diag = dir.join("_diag");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&diag).unwrap();
        let line = "[2026-11-19 14:18:42Z INFO StepsRunner] hello\n";
        for f in [
            "Runner_20261119-090000-utc.log",
            "Runner_20261119-141834-utc.log",
            "Runner_20261119-141834-utc.log.1",
            "Worker_20261119-090000-utc.log",
            "Worker_20261119-100000-utc.log",
            "Other_20261120-000000-utc.log",
            "Worker_notes.txt",
        ] {
            std::fs::write(diag.join(f), line).unwrap();
        }
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let logs = read_diag(&mut r);
        assert_eq!(
            logs,
            DiagLogs {
                runner: Some(diag.join("Runner_20261119-141834-utc.log")),
                worker: Some(diag.join("Worker_20261119-100000-utc.log")),
            }
        );
        // The tails follow what that one listing found: a newer log that
        // appears afterwards waits for the next turn's listing.
        std::fs::write(diag.join("Worker_20261119-150000-utc.log"), "").unwrap();
        let mut tails = LogTails::default();
        tails.read(&mut r, &logs);
        assert!(r.worker_tail.as_ref().is_some_and(|t| t.last.is_some()));
        assert_eq!(
            read_diag(&mut r).worker,
            Some(diag.join("Worker_20261119-150000-utc.log"))
        );
        // No `_diag` at all finds no logs.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read_diag(&mut r), DiagLogs::default());
    }

    fn at(s: &str) -> i64 {
        crate::timefmt::parse_iso(s).unwrap()
    }

    /// Follow `r`'s logs as the collector does: list `_diag`, then read.
    fn read_tails(tails: &mut LogTails, r: &mut Runner) {
        let logs = list_diag(&r.dir.join("_diag"));
        tails.read(r, &logs);
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
        // The failing lines of one outage follow each other within minutes;
        // the second console error, 20 minutes after the first one's
        // failing lines, is a new outage. A routine back-off is not an
        // error.
        assert_eq!(t.errors, 1);
        assert_eq!(t.error_since, Some(at("2026-11-19T13:46:12Z")));
        assert_eq!(
            t.error,
            Some((
                at("2026-11-19T13:46:12Z"),
                "Error: Conflict.".into() // the listener's own framing is dropped
            ))
        );
        // A console line after them clears them.
        let back = parse_runner_tail(&format!(
            "{text}[2026-11-19 13:50:00Z INFO Terminal] WRITE LINE: 2026-11-19 13:50:00Z: Runner reconnected.\n"
        ));
        assert_eq!(back.state.as_ref().unwrap().1, "Runner reconnected.");
        assert_eq!(
            (back.errors, back.error_since, back.error.clone()),
            (0, None, None)
        );
        assert_eq!(back.trouble(at("2026-11-19T13:50:01Z")), None);
        // Blank console lines between errors change nothing.
        let blank = parse_runner_tail(&format!(
            "{text}[2026-11-19 13:47:00Z INFO Terminal] WRITE LINE: 2026-11-19 13:47:00Z: \n"
        ));
        assert_eq!(blank.errors, 1);
    }

    #[test]
    fn a_console_errors_message_is_given_without_the_listeners_framing() {
        let line = |m: &str| {
            format!("[2026-10-29 06:22:58Z ERR  Terminal] WRITE ERROR: 2026-10-29 06:22:58Z: {m}\n")
        };
        let err = |m: &str| parse_runner_tail(&line(m)).error.unwrap().1;
        assert_eq!(
            err(
                "Runner connect error: Resource temporarily unavailable (broker.actions.githubusercontent.com:443). Retrying until reconnected."
            ),
            "Resource temporarily unavailable (broker.actions.githubusercontent.com:443)."
        );
        assert_eq!(
            err("Runner connect error: Error: Conflict. Retrying until reconnected."),
            "Error: Conflict."
        );
        // Other errors are given as written.
        assert_eq!(
            err("Stop retry on SessionConflictException after retried for 240 seconds."),
            "Stop retry on SessionConflictException after retried for 240 seconds."
        );
        assert_eq!(
            parse_runner_tail(
                "[2026-10-03 21:09:55Z ERR  Terminal] WRITE ERROR: A session for this runner already exists.\n"
            )
            .error
            .unwrap()
            .1,
            "A session for this runner already exists."
        );
    }

    /// The lines of `text` up to the moment `hms` of 2026-10-29 (UTC).
    fn until(text: &str, hms: &str) -> String {
        let limit = format!("[2026-10-29 {hms}Z");
        let mut out = String::new();
        let mut open = true;
        for line in text.lines() {
            if line.starts_with("[2026-10-29 ") {
                open = line[..limit.len()] <= *limit.as_str();
            }
            if open {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// `text` with every timestamped line `secs` later, within the day.
    fn shifted(text: &str, secs: i64) -> String {
        let mut out = String::new();
        for line in text.lines() {
            match line.strip_prefix("[2026-10-29 ") {
                Some(rest) if rest.is_char_boundary(8) => {
                    let (hms, tail) = rest.split_at(8);
                    let p: Vec<i64> = hms.split(':').map(|n| n.parse().unwrap()).collect();
                    let s = p[0] * 3600 + p[1] * 60 + p[2] + secs;
                    out.push_str(&format!(
                        "[2026-10-29 {:02}:{:02}:{:02}{tail}\n",
                        s / 3600,
                        s / 60 % 60,
                        s % 60
                    ));
                }
                _ => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }

    const OUTAGE: &str = include_str!("../../tests/fixtures/runner/runner-outage.log");

    #[test]
    fn a_connect_error_is_live_while_attempts_keep_failing_and_over_after() {
        // REPLAY of a real log (glpi-relay, 2026-10-29): the listener wrote
        // its error at 06:22:58Z, its last failed attempt was at 06:27:18Z
        // and its token request worked at 06:27:34Z. It wrote nothing more
        // until its token refresh at 07:17:37Z.
        let refresh = "[2026-10-29 07:17:37Z INFO GitHubActionsService] AAD Correlation ID for this token request: Unknown\n";
        let log = format!("{OUTAGE}{refresh}");
        let moment = |hms: &str| at(&format!("2026-10-29T{hms}Z"));
        let since = moment("06:22:58");
        fn shown<'a>(t: &'a LogTail, hms: &str) -> Option<Trouble<'a>> {
            t.trouble(at(&format!("2026-10-29T{hms}Z")))
        }
        // Before the error: two failed attempts, 26 s apart, are routine.
        let before = parse_runner_tail(&until(&log, "06:22:30"));
        assert_eq!(before.error, None);
        assert_eq!(shown(&before, "06:22:30"), None);
        // In the outage: the error, with the listener's reason.
        let during = parse_runner_tail(&until(&log, "06:24:00"));
        let Some(Trouble::ConsoleError { since: s, msg }) = shown(&during, "06:24:00") else {
            panic!("{:?}", shown(&during, "06:24:00"));
        };
        assert_eq!(s, since);
        assert_eq!(
            msg,
            "Resource temporarily unavailable (broker.actions.githubusercontent.com:443)."
        );
        // The whole log: still in trouble 4 minutes after the last failed
        // attempt, though the runner has reconnected; over after 5.
        let all = parse_runner_tail(&log);
        assert_eq!(all.failure_at, Some(moment("06:27:18")));
        assert!(shown(&all, "06:31:00").is_some());
        assert_eq!(shown(&all, "06:32:30"), None);
        // 12 and 47 minutes after the reconnect, and when the refresh line
        // arrived: no error. (The old rule showed it until 07:17.)
        for hms in ["06:40:00", "07:15:00", "07:18:00"] {
            assert_eq!(shown(&all, hms), None, "{hms}");
        }
    }

    #[test]
    fn a_cached_log_still_clears_its_error_with_no_new_bytes() {
        // The idle listener writes nothing after it reconnects, so the log
        // is not read again; the error must still clear by the clock.
        let dir = scratch("idle-clears");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        std::fs::write(diag.join("Runner_20261026-165301-utc.log"), OUTAGE).unwrap();
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let mut tails = LogTails::default();
        let last = at("2026-10-29T06:27:18Z");
        read_tails(&mut tails, &mut r);
        assert!(r.runner_tail.as_ref().unwrap().trouble(last + 60).is_some());
        read_tails(&mut tails, &mut r);
        let again = r.runner_tail.as_ref().unwrap();
        assert!(again.trouble(last + 4 * 60).is_some());
        assert_eq!(again.trouble(last + 12 * 60), None);
        assert_eq!(again.trouble(last + 47 * 60), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_outage_that_goes_on_keeps_its_error_however_much_is_logged() {
        // SIMULATED from the real log: its failed-retry block (06:23:24Z to
        // 06:27:18Z, about 7.5 KB of stack traces) repeated every 108 s
        // models an outage that continues. Each block adds 7.5 KB, so the
        // error line leaves the last 64 KiB after nine of them.
        let block: String = OUTAGE
            .lines()
            .skip_while(|l| !l.starts_with("[2026-10-29 06:23:24Z"))
            .take_while(|l| !l.starts_with("[2026-10-29 06:27:34Z"))
            .map(|l| format!("{l}\n"))
            .collect();
        let since = at("2026-10-29T06:22:58Z");
        let mut log = until(OUTAGE, "06:26:55");
        let one = |n: i64| shifted(&block, 108 * n);
        for n in 1..=16 {
            log.push_str(&one(n));
        }
        assert!(log.len() as u64 > 2 * TAIL_BYTES, "{}", log.len());
        let last = at("2026-10-29T06:27:18Z") + 108 * 16;
        let expect = |t: &LogTail| {
            assert_eq!(t.error_since, Some(since));
            let live = t.trouble(last + 3);
            assert!(
                matches!(live, Some(Trouble::ConsoleError { since: s, .. }) if s == since),
                "{live:?}"
            );
            // 28 minutes after the error was written: still there.
            assert!(last + 3 - since > 28 * 60);
        };
        // Read whole.
        expect(&parse_runner_tail(&log));
        // First looked at by a crowsnest that starts mid-outage: the error
        // line is more than a block back, and is found by searching back.
        let dir = scratch("outage-goes-on");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        std::fs::write(diag.join("Runner_20261026-165301-utc.log"), &log).unwrap();
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let mut tails = LogTails::default();
        read_tails(&mut tails, &mut r);
        expect(r.runner_tail.as_ref().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
        // Followed as it grows, a block at a time.
        let dir = scratch("outage-followed");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        let path = diag.join("Runner_20261026-165301-utc.log");
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let mut tails = LogTails::default();
        std::fs::write(&path, until(OUTAGE, "06:26:55")).unwrap();
        read_tails(&mut tails, &mut r);
        for n in 1..=16 {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            std::io::Write::write_all(&mut f, one(n).as_bytes()).unwrap();
            read_tails(&mut tails, &mut r);
        }
        expect(r.runner_tail.as_ref().unwrap());
        assert_eq!(
            r.runner_tail.as_ref().unwrap(),
            &parse_runner_tail(&log),
            "following the log gives what reading it whole does"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_error_that_ended_long_ago_is_not_revived_by_a_routine_back_off() {
        let log = format!(
            "{OUTAGE}[2026-10-29 09:15:02Z WARN BrokerServer] Back off 5.017 seconds before next retry. 4 attempt left.\n"
        );
        let t = parse_runner_tail(&log);
        let now = at("2026-10-29T09:15:03Z");
        // One failed attempt is routine, and the old error is not shown.
        assert_eq!(t.trouble(now), None);
        assert_eq!((t.error_since, t.errors), (None, 0));
        assert_eq!(t.failure_since, Some(now - 1));
        // A second failed attempt half a minute later is still routine: the
        // third may work.
        let more = format!(
            "{log}[2026-10-29 09:15:30Z INFO BrokerMessageListener] Sleeping for 26.1 seconds before retrying.\n"
        );
        assert_eq!(parse_runner_tail(&more).trouble(now + 30), None);
        // Failing on for minutes is the listener retrying.
        let long = format!(
            "{more}[2026-10-29 09:17:30Z INFO BrokerMessageListener] Sleeping for 26.1 seconds before retrying.\n"
        );
        assert_eq!(
            parse_runner_tail(&long).trouble(now + 150),
            Some(Trouble::Retrying { since: now - 1 })
        );
    }

    #[test]
    fn a_routine_request_timeout_is_not_trouble() {
        // REPLAY of a real routine event (potful, 2026-11-03): a long poll
        // timed out after 100 s. The listener writes 7 failure lines for it
        // in one second (an ERR line per exception, and the back-off), then
        // carries on. Counting lines would call that trouble; the time the
        // failing spans does not.
        let block = include_str!("../../tests/fixtures/runner/runner-routine-timeout.log");
        let t = parse_runner_tail(block);
        let at = t.failure_at.unwrap();
        assert_eq!(t.failure_since, Some(at));
        assert_eq!(t.trouble(at + 1), None);
        // The same block twice, 100 s apart (two timeouts in a row), is
        // still routine; a third, 200 s in, is not.
        let two = format!("{block}{}", shifted_to(block, "2026-11-03", 100));
        assert_eq!(parse_runner_tail(&two).trouble(at + 101), None);
        let three = format!("{two}{}", shifted_to(block, "2026-11-03", 200));
        assert!(matches!(
            parse_runner_tail(&three).trouble(at + 201),
            Some(Trouble::Retrying { .. })
        ));
    }

    #[test]
    fn routine_timeouts_after_an_ordinary_console_line_are_not_retrying() {
        // Three timed-out requests 100 s apart span over 2 minutes. With no
        // console line in view the log was read mid-outage: retrying. After
        // an ordinary console line they are routine and show nothing.
        let block = include_str!("../../tests/fixtures/runner/runner-routine-timeout.log");
        let three = format!(
            "{block}{}{}",
            shifted_to(block, "2026-11-03", 100),
            shifted_to(block, "2026-11-03", 200)
        );
        let alone = parse_runner_tail(&three);
        let now = alone.failure_at.unwrap() + 1;
        assert!(matches!(alone.trouble(now), Some(Trouble::Retrying { .. })));
        let first = &block[1..20];
        let listening = format!("[{first}Z INFO Terminal] WRITE LINE: Listening for Jobs\n");
        let after = parse_runner_tail(&format!("{listening}{three}"));
        assert!(after.state.is_some());
        assert_eq!(after.failure_since, alone.failure_since);
        assert_eq!(after.trouble(now), None);
    }

    /// `text` with every `day` line `secs` later.
    fn shifted_to(text: &str, day: &str, secs: i64) -> String {
        let head = format!("[{day} ");
        let mut out = String::new();
        for line in text.lines() {
            match line.strip_prefix(&head) {
                Some(rest) if rest.is_char_boundary(8) => {
                    let (hms, tail) = rest.split_at(8);
                    let p: Vec<i64> = hms.split(':').map(|n| n.parse().unwrap()).collect();
                    let s = p[0] * 3600 + p[1] * 60 + p[2] + secs;
                    out.push_str(&format!(
                        "{head}{:02}:{:02}:{:02}{tail}\n",
                        s / 3600,
                        s / 60 % 60,
                        s % 60
                    ));
                }
                _ => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }

    #[test]
    fn failing_attempts_alone_say_the_listener_is_retrying() {
        // A log that begins mid-outage, with no console line in view.
        let failing: String = OUTAGE
            .lines()
            .skip_while(|l| !l.starts_with("[2026-10-29 06:23:24Z"))
            .take_while(|l| !l.starts_with("[2026-10-29 06:27:34Z"))
            .map(|l| format!("{l}\n"))
            .collect();
        let t = parse_runner_tail(&failing);
        assert_eq!(t.error, None);
        let first = at("2026-10-29T06:23:40Z");
        assert_eq!(t.failure_since, Some(first));
        assert_eq!(
            t.trouble(at("2026-10-29T06:28:00Z")),
            Some(Trouble::Retrying { since: first })
        );
        assert_eq!(t.trouble(at("2026-10-29T06:40:00Z")), None);
        // Other components' errors, and a lone failed attempt, are not it.
        let other = "[2026-10-29 06:23:24Z ERR  JobDispatcher] Job failed\n[2026-10-29 06:23:25Z ERR  JobDispatcher] Job failed\n";
        assert_eq!(parse_runner_tail(other).failure_at, None);
    }

    #[test]
    fn a_session_conflict_loop_reads_as_a_console_error() {
        // The real session-conflict messages (glpi-relay, 2026-10-03): the
        // listener prints a blank console line and then an error, every 30 s.
        let log = "\
[2026-10-03 21:09:55Z ERR  GitHubActionsService] POST request to https://broker.actions.githubusercontent.com/session failed. HTTP Status: Conflict
[2026-10-03 21:09:55Z ERR  BrokerMessageListener] Catch exception during create session.
[2026-10-03 21:09:55Z ERR  Terminal] WRITE ERROR: A session for this runner already exists.
[2026-10-03 21:09:55Z ERR  Terminal] WRITE ERROR: 2026-10-03 21:09:55Z: Runner connect error: Error: Conflict. Retrying until reconnected.
[2026-10-03 21:10:26Z INFO Terminal] WRITE LINE: 
[2026-10-03 21:10:26Z ERR  GitHubActionsService] POST request to https://broker.actions.githubusercontent.com/session failed. HTTP Status: Conflict
[2026-10-03 21:10:26Z ERR  Terminal] WRITE ERROR: A session for this runner already exists.
";
        let t = parse_runner_tail(log);
        assert_eq!(t.errors, 3);
        assert_eq!(t.error_since, Some(at("2026-10-03T21:09:55Z")));
        assert_eq!(
            t.error.as_ref().unwrap().1,
            "A session for this runner already exists."
        );
        assert!(matches!(
            t.trouble(at("2026-10-03T21:10:40Z")),
            Some(Trouble::ConsoleError { .. })
        ));
    }

    /// A worker log line of the kind a running step keeps writing.
    const WORKER_PAD: &str = "[2026-11-18 17:07:31Z INFO JobServerQueue] Try to append 1 batches web console lines for record '00000000-0000-4000-8000-000000000000', success rate: 1/1.\n";

    const WORKER_STEP: &str = include_str!("../../tests/fixtures/runner/worker-long-step.log");

    #[test]
    fn a_long_step_keeps_its_line_however_much_the_worker_logs() {
        // REPLAY of the shape of a real worker log (potful, 2026-11-18): the
        // step `Run actions/setup-node@v7` started at 17:06:15Z and its
        // process ran for 20 minutes, while the worker went on writing
        // JobServerQueue lines. The old reading lost the step line once 64
        // KiB had been written after it, 5 to 8 minutes in.
        let dir = scratch("long-step");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        let path = diag.join("Worker_20261118-170610-utc.log");
        std::fs::write(&path, WORKER_STEP).unwrap();
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let mut tails = LogTails::default();
        let step = Some((
            at("2026-11-18T17:06:15Z"),
            "Run actions/setup-node@v7".to_string(),
        ));
        read_tails(&mut tails, &mut r);
        let t = r.worker_tail.clone().unwrap();
        assert_eq!(t.step, step);
        assert!(t.process_running);
        // 200 KiB more, in pieces: 27 minutes of this job's logging.
        let kib = WORKER_PAD.repeat(1024 / WORKER_PAD.len() + 1);
        for _ in 0..200 {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            std::io::Write::write_all(&mut f, kib.as_bytes()).unwrap();
            read_tails(&mut tails, &mut r);
        }
        let t = r.worker_tail.clone().unwrap();
        assert_eq!(t.step, step, "the step line is kept");
        assert!(t.process_running, "so is its running process");
        // The same log first looked at after it grew by more than a block
        // (what `--once` does): found by searching back.
        let mut fresh = LogTails::default();
        let mut r2 = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        assert!(std::fs::metadata(&path).unwrap().len() > 3 * TAIL_BYTES);
        read_tails(&mut fresh, &mut r2);
        assert_eq!(r2.worker_tail, r.worker_tail);
        // A log that grew by more than a block between two reads.
        let mut grown = LogTails::default();
        let mut r3 = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        std::fs::write(&path, WORKER_STEP).unwrap();
        read_tails(&mut grown, &mut r3);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, WORKER_PAD.repeat(1500).as_bytes()).unwrap();
        read_tails(&mut grown, &mut r3);
        assert_eq!(r3.worker_tail.as_ref().unwrap().step, step);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_search_back_for_a_step_line_is_bounded() {
        let dir = scratch("search-bound");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        let path = diag.join("Worker_20261118-170610-utc.log");
        let mut text = WORKER_STEP.to_string();
        text.push_str(&WORKER_PAD.repeat((SEARCH_BYTES as usize) / WORKER_PAD.len() + 800));
        std::fs::write(&path, &text).unwrap();
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        read_tails(&mut LogTails::default(), &mut r);
        let t = r.worker_tail.unwrap();
        assert_eq!(t.step, None, "further back than the search goes");
        assert!(t.last.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_half_written_line_is_read_once_it_is_whole() {
        let dir = scratch("half-line");
        let diag = dir.join("_diag");
        std::fs::create_dir_all(&diag).unwrap();
        let path = diag.join("Worker_20261118-170610-utc.log");
        let line = "[2026-11-18 17:06:15Z INFO StepsRunner] Processing step: DisplayName='Run \u{1b}[31mtests'\n";
        let (head, rest) = line.split_at(70);
        std::fs::write(&path, format!("{WORKER_PAD}{head}")).unwrap();
        let mut r = Runner {
            dir: dir.clone(),
            ..Default::default()
        };
        let mut tails = LogTails::default();
        read_tails(&mut tails, &mut r);
        assert_eq!(r.worker_tail.as_ref().unwrap().step, None);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, rest.as_bytes()).unwrap();
        read_tails(&mut tails, &mut r);
        let step = r.worker_tail.take().unwrap().step.unwrap().1;
        assert_eq!(step, "Run [31mtests", "escape removed");
        // A log replaced by a shorter one is looked at afresh.
        std::fs::write(
            &path,
            "[2026-11-18 17:30:00Z INFO StepsRunner] Processing step: DisplayName='Test'\n",
        )
        .unwrap();
        read_tails(&mut tails, &mut r);
        assert_eq!(
            r.worker_tail.as_ref().unwrap().step.as_ref().unwrap().1,
            "Test"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_back_goes_a_block_at_a_time_and_stops_at_the_bound() {
        let dir = scratch("read-back");
        let path = dir.join("Worker_20261119-141834-utc.log");
        let line = "[2026-11-19 14:18:42Z INFO JobServerQueue] padding padding padding padding\n";
        let mut text = line.repeat(2000);
        text.push_str(
            "[2026-11-19 14:27:02Z INFO StepsRunner] Processing step: DisplayName='Run tests'\n",
        );
        std::fs::write(&path, &text).unwrap();
        assert!(text.len() as u64 > TAIL_BYTES);
        // Satisfied by the first block.
        let (tail, end) = read_back(&path, |t| t.contains("Run tests")).unwrap();
        assert!(tail.len() as u64 <= TAIL_BYTES);
        assert!(tail.starts_with("[2026-11-19 14:18:42Z"), "whole lines");
        assert!(tail.ends_with("DisplayName='Run tests'\n"));
        assert_eq!(end, text.len() as u64);
        // Not satisfied: it goes back to the start of this small file.
        let (all, _) = read_back(&path, |_| false).unwrap();
        assert_eq!(all, text);
        // A last line without its newline is left for later.
        std::fs::write(&path, format!("{line}{}", &line[..20])).unwrap();
        let (whole, end) = read_back(&path, |_| true).unwrap();
        assert_eq!((whole.as_str(), end), (line, line.len() as u64));
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
        read_tails(&mut tails, &mut r);
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
        read_tails(&mut tails, &mut r);
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
