//! The `gh api` transport. Every read is made with `--include`, so each
//! answer carries GitHub's rate-limit counters. A list asked for again is
//! asked with the ETag of its last answer: when nothing changed GitHub
//! answers 304, which does not count against the budget, and the last body
//! is reused. A refusal for rate limit is not a failure: reads of that
//! budget pause until the time GitHub gives, and nothing is asked meanwhile.
//!
//! REST and GraphQL are separate budgets of the same `gh` login, shared with
//! every other program using it.

use crate::exec::{ExecError, ReadCommand};
use crate::model::{GhBudget, RateLimit};
use crate::timefmt::now_epoch;
use std::collections::BTreeMap;
use std::time::Duration;

/// One HTTP answer as `gh api --include` prints it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub status: u16,
    /// Header names in lower case.
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn number(&self, name: &str) -> Option<i64> {
        self.header(name).and_then(|v| v.trim().parse().ok())
    }

    /// The rate-limit counters, and which budget they are for.
    pub fn rate_limit(&self) -> Option<(String, RateLimit)> {
        let rl = RateLimit {
            limit: self.number("x-ratelimit-limit")?.max(0) as u64,
            remaining: self.number("x-ratelimit-remaining")?.max(0) as u64,
            reset_at: self.number("x-ratelimit-reset")?,
            ..Default::default()
        };
        let resource = self.header("x-ratelimit-resource").unwrap_or("core");
        Some((resource.to_string(), rl))
    }

    /// Until when GitHub refuses this budget, when the answer is a refusal
    /// for rate limit: a primary limit (no requests left), a secondary limit
    /// (`retry-after`, or GitHub's message), or GraphQL's `RATE_LIMITED`.
    pub fn limited_until(&self, now: i64) -> Option<i64> {
        let retry_after = self.number("retry-after").map(|s| now + s.max(1));
        let reset = self.number("x-ratelimit-reset").filter(|r| *r > now);
        let spent = self.number("x-ratelimit-remaining") == Some(0);
        let refused = matches!(self.status, 403 | 429);
        let says = || {
            let b = self.body.to_ascii_lowercase();
            b.contains("rate limit") || b.contains("\"rate_limited\"")
        };
        let graphql_limited = self.status == 200 && self.body.contains("\"RATE_LIMITED\"");
        if !(refused && (spent || retry_after.is_some() || says()) || graphql_limited) {
            return None;
        }
        // Secondary limits carry retry-after or nothing: wait a minute then.
        Some(
            retry_after
                .or(if spent || graphql_limited {
                    reset
                } else {
                    None
                })
                .unwrap_or(now + 60),
        )
    }
}

/// Parse `gh api --include` output. Output without a status line (an older
/// `gh`, or a test double) is a 200 whose body is the whole text.
pub fn parse_response(text: &str) -> Response {
    let Some(rest) = text.strip_prefix("HTTP/") else {
        return Response {
            status: 200,
            body: text.to_string(),
            ..Default::default()
        };
    };
    let (head, body) = match rest.find("\r\n\r\n") {
        Some(i) => (&rest[..i], &rest[i + 4..]),
        None => match rest.find("\n\n") {
            Some(i) => (&rest[..i], &rest[i + 2..]),
            None => (rest, ""),
        },
    };
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Response {
        status,
        headers,
        body: body.to_string(),
    }
}

/// Why a read gave nothing new.
#[derive(Debug, Clone, PartialEq)]
pub enum GhError {
    /// GitHub refused for rate limit; nothing of this budget is asked
    /// until then. Not a failure: the last data stays and is not retried.
    Paused {
        until: i64,
    },
    Failed(String),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GhError::Paused { until } => write!(
                f,
                "gh: rate limited until {}",
                crate::timefmt::clock(*until)
            ),
            GhError::Failed(e) => f.write_str(e),
        }
    }
}

/// What a GraphQL read returned: its body, and the failure `gh` reported
/// when it exited with an error but still printed an answer.
pub type GraphqlAnswer = (String, Option<String>);

/// Runs `gh` for one source, remembering ETags and the budget between reads.
#[derive(Debug, Default)]
pub struct Client {
    /// Path -> when it was last asked for (a counter), and the ETag and
    /// body of its last 200 answer.
    etags: BTreeMap<String, (u64, String, String)>,
    asked: u64,
    /// The counter value at which the current turn's reads began (see
    /// [`Client::begin_turn`]): entries asked since are never evicted.
    turn_start: Option<u64>,
    pub budget: GhBudget,
    /// Reads that counted against the REST budget (not 304s) since
    /// [`Client::take_calls`] was last asked.
    calls: u32,
    /// Stands in for `gh` in tests.
    #[cfg(test)]
    fake: Option<Fake>,
}

/// A stand-in for running `gh`, so a source's reads can be tested whole.
#[cfg(test)]
pub type FakeRun = dyn Fn(&ReadCommand) -> Result<crate::exec::Output, ExecError> + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
pub struct Fake(pub std::sync::Arc<FakeRun>);

#[cfg(test)]
impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fake")
    }
}

/// How many ETag answers are kept. Job lists of active runs are added as
/// runs come and go; past this the path asked for longest ago is dropped,
/// but never one asked in the current turn: a turn that asks for more paths
/// than this keeps them all, so that none of its 304s loses its body.
const MAX_ETAGS: usize = 400;

/// How many `gh` reads [`Client::get_many`] runs at once.
pub const PARALLEL: usize = 8;

/// `f` of every item, at most [`PARALLEL`] at a time, in the items' order.
fn bounded<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<R>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..PARALLEL.min(items.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    *slots[i].lock().unwrap() = Some(f(item));
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().expect("every item is run"))
        .collect()
}

impl Client {
    /// A client whose reads are answered by `fake` instead of `gh`.
    #[cfg(test)]
    pub fn with_fake(fake: Fake) -> Client {
        Client {
            fake: Some(fake),
            ..Default::default()
        }
    }

    /// Runs one command: `gh`, or the stand-in of a test.
    fn runner(
        &self,
    ) -> impl Fn(&ReadCommand) -> Result<crate::exec::Output, ExecError> + Sync + use<> {
        #[cfg(test)]
        let fake = self.fake.clone();
        move |cmd: &ReadCommand| {
            #[cfg(test)]
            if let Some(f) = &fake {
                return (f.0)(cmd);
            }
            cmd.run()
        }
    }

    /// Reads made against the REST budget since the last call, reset to 0.
    pub fn take_calls(&mut self) -> u32 {
        std::mem::take(&mut self.calls)
    }

    /// Start a turn of reads (the queue, history and runner lists read
    /// together). Every path asked until the next turn is kept, however many
    /// there are; the answers of paths that went unasked for a whole turn
    /// are dropped once the store is past its size.
    pub fn begin_turn(&mut self) {
        if self.etags.len() > MAX_ETAGS
            && let Some(last) = self.turn_start
        {
            self.etags.retain(|_, (at, _, _)| *at >= last);
        }
        self.turn_start = Some(self.asked + 1);
    }

    fn paused(&self, graphql: bool, now: i64) -> Option<i64> {
        let rl = if graphql {
            &self.budget.graphql
        } else {
            &self.budget.core
        };
        rl.paused_until.filter(|u| *u > now)
    }

    /// Note the counters an answer carried, and a refusal for rate limit.
    fn observe(&mut self, r: &Response, graphql: bool, now: i64) -> Option<i64> {
        if let Some((resource, rl)) = r.rate_limit() {
            let slot = match resource.as_str() {
                "graphql" => &mut self.budget.graphql,
                "core" => &mut self.budget.core,
                _ if graphql => &mut self.budget.graphql,
                _ => &mut self.budget.core,
            };
            slot.limit = rl.limit;
            slot.remaining = rl.remaining;
            slot.reset_at = rl.reset_at;
            slot.read_at = Some(now);
        }
        let until = r.limited_until(now)?;
        let slot = if graphql {
            &mut self.budget.graphql
        } else {
            &mut self.budget.core
        };
        slot.paused_until = Some(until);
        Some(until)
    }

    /// `gh api --method GET <path>`, asked with the last answer's ETag.
    pub fn get(&mut self, path: &str) -> Result<String, GhError> {
        let run = self.runner();
        self.get_at(path, true, now_epoch(), run)
    }

    /// `gh api --method GET <path>` for an answer that is read only once
    /// (a finished run's jobs never change): no ETag is kept for it.
    pub fn get_once(&mut self, path: &str) -> Result<String, GhError> {
        let run = self.runner();
        self.get_at(path, false, now_epoch(), run)
    }

    /// `gh api --method GET` for each `(path, keep)` of `reads`, at most
    /// [`PARALLEL`] at a time, answered in the order asked.
    pub fn get_many(&mut self, reads: &[(String, bool)]) -> Vec<Result<String, GhError>> {
        let run = self.runner();
        self.get_many_at(reads, now_epoch(), run)
    }

    fn get_many_at<F>(
        &mut self,
        reads: &[(String, bool)],
        now: i64,
        run: F,
    ) -> Vec<Result<String, GhError>>
    where
        F: Fn(&ReadCommand) -> Result<crate::exec::Output, ExecError> + Sync,
    {
        use std::sync::atomic::{AtomicI64, Ordering};
        let cmds: Vec<_> = reads.iter().map(|(p, _)| self.request(p, now)).collect();
        let asked: Vec<&ReadCommand> = cmds.iter().filter_map(|c| c.as_ref().ok()).collect();
        let refused_until = AtomicI64::new(0);
        let outs = bounded(&asked, |cmd| {
            if refused_until.load(Ordering::SeqCst) != 0 {
                return None;
            }
            let out = run(cmd);
            let stdout = match &out {
                Ok(o) => Some(&o.stdout),
                Err(ExecError::Failed { stdout, .. }) => Some(stdout),
                Err(_) => None,
            };
            if let Some(until) = stdout.and_then(|o| parse_response(o).limited_until(now)) {
                refused_until.store(until, Ordering::SeqCst);
            }
            Some(out)
        });
        let refused_until = refused_until.into_inner();
        let mut outs = outs.into_iter();
        reads
            .iter()
            .zip(cmds)
            .map(|((path, keep), cmd)| {
                cmd?;
                match outs.next().expect("one answer per command run") {
                    Some(out) => self.answer(path, *keep, now, out, &run, false),
                    None => Err(GhError::Paused {
                        until: self.paused(false, now).unwrap_or(refused_until),
                    }),
                }
            })
            .collect()
    }

    fn get_at<F>(&mut self, path: &str, keep: bool, now: i64, run: F) -> Result<String, GhError>
    where
        F: Fn(&ReadCommand) -> Result<crate::exec::Output, ExecError>,
    {
        let cmd = self.request(path, now)?;
        let out = run(&cmd);
        self.answer(path, keep, now, out, &run, false)
    }

    /// The command for one GET, asked with the last answer's ETag.
    fn request(&mut self, path: &str, now: i64) -> Result<ReadCommand, GhError> {
        if let Some(until) = self.paused(false, now) {
            return Err(GhError::Paused { until });
        }
        self.asked += 1;
        let asked = self.asked;
        Ok(ReadCommand::GhApiGet {
            path: path.to_string(),
            etag: self.etags.get_mut(path).map(|(at, e, _)| {
                *at = asked;
                e.clone()
            }),
        })
    }

    /// Take in the answer to the GET of `path`. A 304 whose body is no
    /// longer stored is asked again, once, without the ETag (`run` asks).
    fn answer(
        &mut self,
        path: &str,
        keep: bool,
        now: i64,
        out: Result<crate::exec::Output, ExecError>,
        run: &dyn Fn(&ReadCommand) -> Result<crate::exec::Output, ExecError>,
        reasked: bool,
    ) -> Result<String, GhError> {
        let (out, failure) = match out {
            Ok(o) => (o.stdout, None),
            Err(ExecError::Failed { stdout, .. }) if stdout.starts_with("HTTP/") => {
                (stdout, Some(()))
            }
            Err(e) => return Err(GhError::Failed(format!("gh: {e}"))),
        };
        let r = parse_response(&out);
        if let Some(until) = self.observe(&r, false, now) {
            return Err(GhError::Paused { until });
        }
        if r.status == 304 {
            if let Some((_, _, body)) = self.etags.get(path) {
                return Ok(body.clone());
            }
            // Nothing changed, but the last answer is gone: ask for it again
            // without the ETag, unless that has been done or GitHub refuses.
            if reasked {
                return Err(GhError::Failed(
                    "gh: HTTP 304 without a cached answer".into(),
                ));
            }
            if let Some(until) = self.paused(false, now) {
                return Err(GhError::Paused { until });
            }
            let again = run(&ReadCommand::GhApiGet {
                path: path.to_string(),
                etag: None,
            });
            return self.answer(path, keep, now, again, run, true);
        }
        self.calls += 1;
        if failure.is_some() || !(200..300).contains(&r.status) {
            return Err(GhError::Failed(http_error(&r)));
        }
        match r.header("etag") {
            Some(etag) if keep && crate::exec::valid_etag(etag) => {
                if self.etags.len() >= MAX_ETAGS && !self.etags.contains_key(path) {
                    // Not one asked in this turn: its 304 may be waiting.
                    let turn = self.turn_start;
                    let oldest = self
                        .etags
                        .iter()
                        .filter(|(_, (at, _, _))| turn.is_none_or(|t| *at < t))
                        .min_by_key(|(_, (at, _, _))| *at)
                        .map(|(k, _)| k.clone());
                    if let Some(k) = oldest {
                        self.etags.remove(&k);
                    }
                }
                self.etags.insert(
                    path.to_string(),
                    (self.asked, etag.to_string(), r.body.clone()),
                );
            }
            _ => {
                self.etags.remove(path);
            }
        }
        Ok(r.body)
    }

    /// One GraphQL read query. An answer `gh` reported as failed is still
    /// returned with the failure, so the parts that resolved can be shown.
    pub fn graphql(&mut self, query: String) -> Result<GraphqlAnswer, GhError> {
        let run = self.runner();
        self.graphql_at(query, now_epoch(), run)
    }

    fn graphql_at<F>(&mut self, query: String, now: i64, run: F) -> Result<GraphqlAnswer, GhError>
    where
        F: FnOnce(&ReadCommand) -> Result<crate::exec::Output, ExecError>,
    {
        if let Some(until) = self.paused(true, now) {
            return Err(GhError::Paused { until });
        }
        let (out, failure) = match run(&ReadCommand::GhGraphql { query }) {
            Ok(o) => (o.stdout, None),
            Err(e) => match &e {
                ExecError::Failed { stdout, .. } => (stdout.clone(), Some(format!("gh: {e}"))),
                _ => return Err(GhError::Failed(format!("gh: {e}"))),
            },
        };
        let r = parse_response(&out);
        if let Some(until) = self.observe(&r, true, now) {
            return Err(GhError::Paused { until });
        }
        Ok((r.body, failure))
    }
}

/// The gh tier's cadence: `base`, stretched when the REST budget `core`
/// would not last until its reset at `calls` counted reads a turn. Half of
/// what is left is kept for the other programs using the same `gh` login.
pub fn cadence(core: &RateLimit, base: Duration, calls: u32, now: i64) -> Duration {
    let left = core.reset_at - now;
    if core.limit == 0 || left <= 0 || calls == 0 {
        return base;
    }
    let turns = (core.remaining / 2) / u64::from(calls);
    if turns == 0 {
        return base.max(Duration::from_secs(left as u64));
    }
    base.max(Duration::from_secs(left as u64 / turns))
}

/// How long the gh tier really waits from `now` for its next read. The
/// tier compares the time since its last read with [`cadence`] judged afresh
/// every half second, and the time to the reset shrinks as the wait goes on,
/// so a wait of `w` ends at the first `w` with `cadence(now + w) <= w`: sooner
/// than the cadence `now` gave (by a tenth at 9 turns, by half at 0 or 1).
pub fn wait(core: &RateLimit, base: Duration, calls: u32, now: i64) -> Duration {
    let (mut lo, mut hi) = (0, cadence(core, base, calls, now).as_secs());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if cadence(core, base, calls, now + mid as i64) <= Duration::from_secs(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Duration::from_secs(hi)
}

/// A failed answer in a line: `gh: HTTP 404: Not Found`.
fn http_error(r: &Response) -> String {
    let msg = serde_json::from_str::<serde_json::Value>(&r.body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if msg.is_empty() {
        format!("gh: HTTP {}", r.status)
    } else {
        format!("gh: HTTP {}: {msg}", r.status)
    }
}

/// Whether a failed read was GitHub saying the token may not read it.
pub fn denied(e: &GhError) -> bool {
    matches!(e, GhError::Failed(m) if m.contains("HTTP 403") || m.contains("HTTP 404"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Output;

    const NOW: i64 = 1_795_232_000;

    fn answer(status: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut s = format!("HTTP/2.0 {status}\r\n");
        for (k, v) in headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str("\r\n");
        s.push_str(body);
        s
    }

    fn ok(stdout: String) -> Result<Output, ExecError> {
        Ok(Output {
            stdout,
            stderr: String::new(),
            success: true,
        })
    }

    fn failed(stdout: String, stderr: &str) -> Result<Output, ExecError> {
        Err(ExecError::Failed {
            code: Some(1),
            stderr: stderr.into(),
            stdout,
        })
    }

    fn counters(remaining: &str) -> Vec<(&'static str, String)> {
        vec![
            ("X-Ratelimit-Limit", "5000".into()),
            ("X-Ratelimit-Remaining", remaining.into()),
            ("X-Ratelimit-Reset", (NOW + 1200).to_string()),
            ("X-Ratelimit-Resource", "core".into()),
        ]
    }

    fn with<'a>(
        extra: &[(&'a str, &'a str)],
        base: &'a [(&'static str, String)],
    ) -> Vec<(&'a str, &'a str)> {
        base.iter()
            .map(|(k, v)| (*k, v.as_str()))
            .chain(extra.iter().copied())
            .collect()
    }

    #[test]
    fn parses_status_headers_and_body() {
        let r = parse_response(&answer(
            "200 OK",
            &[("Etag", "W/\"abc\""), ("X-Ratelimit-Remaining", "4724")],
            "{\"total_count\":0}",
        ));
        assert_eq!(r.status, 200);
        assert_eq!(r.header("etag"), Some("W/\"abc\""));
        assert_eq!(r.body, "{\"total_count\":0}");
        let plain = parse_response("{\"a\": 1}");
        assert_eq!((plain.status, plain.body.as_str()), (200, "{\"a\": 1}"));
    }

    #[test]
    fn an_unchanged_answer_reuses_the_last_body_and_costs_nothing() {
        let mut c = Client::default();
        let h = counters("4724");
        let first = answer(
            "200 OK",
            &with(&[("Etag", "W/\"v1\"")], &h),
            "{\"jobs\":[1]}",
        );
        let body = c.get_at("repos/o/r/actions/runs/1/jobs", true, NOW, |cmd| {
            assert!(matches!(cmd, ReadCommand::GhApiGet { etag: None, .. }));
            ok(first.clone())
        });
        assert_eq!(body.unwrap(), "{\"jobs\":[1]}");
        assert_eq!(c.take_calls(), 1);
        let again = c.get_at("repos/o/r/actions/runs/1/jobs", true, NOW + 60, |cmd| {
            let ReadCommand::GhApiGet { etag, .. } = cmd else {
                panic!()
            };
            assert_eq!(
                etag.as_deref(),
                Some("W/\"v1\""),
                "the ETag goes back as received"
            );
            // gh exits 1 on a 304, with the answer's head on stdout.
            failed(
                answer("304 Not Modified", &with(&[], &h), ""),
                "gh: HTTP 304",
            )
        });
        assert_eq!(again.unwrap(), "{\"jobs\":[1]}");
        assert_eq!(c.take_calls(), 0, "a 304 is not counted");
        assert_eq!(c.budget.core.remaining, 4724);
    }

    #[test]
    fn one_shot_answers_keep_no_etag_and_the_oldest_path_goes_first() {
        let mut c = Client::default();
        let h = counters("4000");
        let tagged = |tag: &str| answer("200 OK", &with(&[("Etag", tag)], &h), "{}");
        c.get_at("repos/o/r/actions/runs/9/jobs", false, NOW, |_| {
            ok(tagged("\"a\""))
        })
        .unwrap();
        assert!(c.etags.is_empty(), "a finished run's jobs are read once");
        for i in 0..MAX_ETAGS {
            let path = format!("repos/o/r/actions/runs/{i}/jobs");
            c.get_at(&path, true, NOW, |_| ok(tagged("\"a\""))).unwrap();
        }
        // Path 0 is asked again (a 304), so path 1 is now the oldest.
        c.get_at("repos/o/r/actions/runs/0/jobs", true, NOW, |_| {
            failed(
                answer("304 Not Modified", &with(&[], &h), ""),
                "gh: HTTP 304",
            )
        })
        .unwrap();
        c.get_at("repos/o/r/actions/runs/new/jobs", true, NOW, |_| {
            ok(tagged("\"b\""))
        })
        .unwrap();
        assert_eq!(c.etags.len(), MAX_ETAGS);
        assert!(c.etags.contains_key("repos/o/r/actions/runs/0/jobs"));
        assert!(!c.etags.contains_key("repos/o/r/actions/runs/1/jobs"));
    }

    /// The job-list paths of a repository's runs.
    fn jobs_of(repo: &str, n: usize) -> Vec<(String, bool)> {
        (0..n)
            .map(|i| {
                (
                    format!("repos/o/{repo}/actions/runs/{i}/jobs?per_page=50"),
                    true,
                )
            })
            .collect()
    }

    /// A GitHub where every list answers 200 with its ETag the first time
    /// and 304 when asked again with that ETag.
    fn github(h: &[(&'static str, String)]) -> impl Fn(&ReadCommand) -> Result<Output, ExecError> {
        let h = h.to_vec();
        move |cmd| {
            let ReadCommand::GhApiGet { path, etag } = cmd else {
                panic!()
            };
            let tag = format!("\"{path}\"");
            if etag.as_deref() == Some(tag.as_str()) {
                return failed(
                    answer("304 Not Modified", &with(&[], &h), ""),
                    "gh: HTTP 304",
                );
            }
            ok(answer(
                "200 OK",
                &with(&[("Etag", tag.as_str())], &h),
                &format!("{{\"path\": \"{path}\"}}"),
            ))
        }
    }

    #[test]
    fn the_wait_to_the_next_read_is_judged_against_the_clock_as_it_runs() {
        let core = |remaining: u64| RateLimit {
            limit: 5000,
            remaining,
            reset_at: NOW + 3600,
            ..Default::default()
        };
        let base = Duration::from_secs(60);
        // 100 left keeps half for others: 5 turns of 10 reads. The cadence
        // now says 720 s, but the tier fires as soon as the time since the
        // last read reaches the time to the reset over 5: 600 s.
        let c = core(100);
        assert_eq!(cadence(&c, base, 10, NOW), Duration::from_secs(720));
        assert_eq!(wait(&c, base, 10, NOW), Duration::from_secs(600));
        // Not even one turn left: the time to the reset, which the tier
        // reaches at the half.
        let c = core(10);
        assert_eq!(cadence(&c, base, 10, NOW), Duration::from_secs(3600));
        assert_eq!(wait(&c, base, 10, NOW), Duration::from_secs(1800));
        // Plenty left: the base.
        let c = core(4000);
        assert_eq!(wait(&c, base, 10, NOW), base);
        // And the wait is what `cadence` then says.
        for remaining in [10, 40, 100, 300, 900] {
            let c = core(remaining);
            let w = wait(&c, base, 10, NOW);
            assert!(cadence(&c, base, 10, NOW + w.as_secs() as i64) <= w);
            let before = w.as_secs().saturating_sub(1) as i64;
            if w > base {
                assert!(cadence(&c, base, 10, NOW + before) > Duration::from_secs(before as u64));
            }
        }
    }

    #[test]
    fn a_turn_never_evicts_what_it_asked_and_keeps_every_304_whole() {
        // UNIT, the review's scenario: 400 cached job lists of repository b,
        // then a turn where repository a, which sorts first, has 30 new runs
        // and b is unchanged. The new answers used to evict b's entries that
        // the same batch had asked and not yet taken in: 30 reads failed.
        let mut c = Client::default();
        let h = counters("4000");
        let gh = github(&h);
        let b = jobs_of("b", MAX_ETAGS);
        c.begin_turn();
        assert!(c.get_many_at(&b, NOW, &gh).iter().all(Result::is_ok));
        assert_eq!(c.etags.len(), MAX_ETAGS);
        c.begin_turn();
        let turn: Vec<_> = jobs_of("a", 30).into_iter().chain(b.clone()).collect();
        let answers = c.get_many_at(&turn, NOW + 60, &gh);
        assert_eq!(
            answers.iter().filter(|a| a.is_err()).count(),
            0,
            "{:?}",
            answers.iter().find(|a| a.is_err())
        );
        for ((path, _), a) in turn.iter().zip(&answers) {
            assert_eq!(a.as_ref().unwrap(), &format!("{{\"path\": \"{path}\"}}"));
        }
        // The store grew to hold the whole turn, and every 304 of b was free:
        // 400 reads of the first turn and the 30 new ones were counted.
        assert_eq!(c.etags.len(), MAX_ETAGS + 30);
        assert_eq!(c.take_calls(), (MAX_ETAGS + 30) as u32);
    }

    #[test]
    fn paths_unasked_for_a_whole_turn_are_dropped_once_the_store_is_past_its_size() {
        let mut c = Client::default();
        let h = counters("4000");
        let gh = github(&h);
        c.begin_turn();
        c.get_many_at(&jobs_of("b", MAX_ETAGS + 30), NOW, &gh);
        assert_eq!(c.etags.len(), MAX_ETAGS + 30);
        // A turn that asks for none of them: each new path takes the place
        // of an old one, down to the size.
        c.begin_turn();
        c.get_many_at(&jobs_of("c", 5), NOW, &gh);
        assert_eq!(c.etags.len(), MAX_ETAGS + 30);
        // Once a whole turn has passed without them, the rest are dropped.
        c.begin_turn();
        c.get_many_at(&jobs_of("c", 5), NOW, &gh);
        assert_eq!(c.etags.len(), 5);
    }

    #[test]
    fn a_304_whose_body_is_gone_is_asked_again_without_its_etag() {
        let mut c = Client::default();
        let h = counters("4000");
        let asked = std::cell::RefCell::new(Vec::new());
        let body = c.get_at("repos/o/r/actions/runs/1/jobs", true, NOW, |cmd| {
            let ReadCommand::GhApiGet { etag, .. } = cmd else {
                panic!()
            };
            asked.borrow_mut().push(etag.clone());
            if asked.borrow().len() == 1 {
                // GitHub says nothing changed to a path nothing is stored for.
                failed(
                    answer("304 Not Modified", &with(&[], &h), ""),
                    "gh: HTTP 304",
                )
            } else {
                ok(answer(
                    "200 OK",
                    &with(&[("Etag", "W/\"v2\"")], &h),
                    "{\"jobs\":[2]}",
                ))
            }
        });
        assert_eq!(body.unwrap(), "{\"jobs\":[2]}");
        assert_eq!(asked.borrow().len(), 2, "one more ask, no more");
        assert_eq!(asked.borrow()[1], None, "without the ETag");
        assert_eq!(c.take_calls(), 1, "the re-ask is a counted read");
        assert!(c.etags.contains_key("repos/o/r/actions/runs/1/jobs"));
        // A second 304 for the re-ask is a failure, not a loop.
        let mut c = Client::default();
        let n = std::cell::Cell::new(0);
        let e = c.get_at("repos/o/r/actions/runs/1/jobs", true, NOW, |_| {
            n.set(n.get() + 1);
            failed(
                answer("304 Not Modified", &with(&[], &h), ""),
                "gh: HTTP 304",
            )
        });
        assert_eq!(n.get(), 2);
        assert!(matches!(e, Err(GhError::Failed(m)) if m.contains("304")));
    }

    #[test]
    fn many_reads_run_in_parallel_at_most_eight_at_a_time_in_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;
        let mut c = Client::default();
        let reads: Vec<(String, bool)> = (0..20)
            .map(|i| (format!("repos/o/r/actions/runs/{i}/jobs"), true))
            .collect();
        let (running, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let h = counters("4000");
        let answers = c.get_many_at(&reads, NOW, |cmd| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            // Hold each read until eight have been seen at once (or give up),
            // so a ninth would be counted if one ever started.
            let deadline = Instant::now() + Duration::from_secs(2);
            while most.load(Ordering::SeqCst) < 8 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(20));
            running.fetch_sub(1, Ordering::SeqCst);
            let ReadCommand::GhApiGet { path, .. } = cmd else {
                panic!()
            };
            ok(answer("200 OK", &with(&[("Etag", "\"a\"")], &h), path))
        });
        assert_eq!(most.load(Ordering::SeqCst), 8);
        let bodies: Vec<String> = answers.into_iter().map(Result::unwrap).collect();
        let paths: Vec<String> = reads.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(bodies, paths, "answers come back in the order asked");
        assert_eq!(c.take_calls(), 20);
        assert_eq!(c.etags.len(), 20);
        // While paused nothing is run.
        c.budget.core.paused_until = Some(NOW + 60);
        let paused = c.get_many_at(&reads, NOW, |_| panic!("asked GitHub while paused"));
        assert!(
            paused
                .iter()
                .all(|r| *r == Err(GhError::Paused { until: NOW + 60 }))
        );
    }

    #[test]
    fn a_rate_limit_refusal_in_a_batch_stops_the_reads_not_yet_sent() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut c = Client::default();
        let reads: Vec<(String, bool)> = (0..40)
            .map(|i| (format!("repos/o/r/actions/runs/{i}/jobs"), true))
            .collect();
        let sent = AtomicUsize::new(0);
        let refused = std::sync::atomic::AtomicBool::new(false);
        let (fine, spent) = (counters("1"), counters("0"));
        let answers = c.get_many_at(&reads, NOW, |cmd| {
            let n = sent.fetch_add(1, Ordering::SeqCst);
            let ReadCommand::GhApiGet { path, .. } = cmd else {
                panic!()
            };
            if path == "repos/o/r/actions/runs/0/jobs" {
                refused.store(true, Ordering::SeqCst);
                return failed(
                    answer(
                        "403 Forbidden",
                        &with(&[], &spent),
                        "{\"message\":\"API rate limit exceeded for user ID 1.\"}",
                    ),
                    "gh: API rate limit exceeded (HTTP 403)",
                );
            }
            // The other reads already in flight finish after the refusal.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !refused.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(20 + n as u64));
            ok(answer("200 OK", &with(&[], &fine), "{}"))
        });
        let sent = sent.load(Ordering::SeqCst);
        assert!(sent <= PARALLEL, "{sent} reads sent after a refusal");
        assert_eq!(answers[0], Err(GhError::Paused { until: NOW + 1200 }));
        let unsent = &answers[PARALLEL..];
        assert!(
            unsent
                .iter()
                .all(|r| *r == Err(GhError::Paused { until: NOW + 1200 })),
            "{unsent:?}"
        );
        assert_eq!(c.budget.core.paused_until, Some(NOW + 1200));
    }

    #[test]
    fn a_rate_limit_refusal_pauses_reads_until_the_reset() {
        let mut c = Client::default();
        let h = counters("0");
        let refused = answer(
            "403 Forbidden",
            &with(&[], &h),
            "{\"message\":\"API rate limit exceeded for user ID 1.\"}",
        );
        let e = c.get_at("repos/o/r/actions/runs?status=queued", true, NOW, |_| {
            failed(refused.clone(), "gh: API rate limit exceeded (HTTP 403)")
        });
        assert_eq!(e, Err(GhError::Paused { until: NOW + 1200 }));
        assert_eq!(c.budget.core.paused_until, Some(NOW + 1200));
        // Nothing is asked while paused.
        let e = c.get_at(
            "repos/o/r/actions/runs?status=queued",
            true,
            NOW + 600,
            |_| panic!("asked GitHub while paused"),
        );
        assert_eq!(e, Err(GhError::Paused { until: NOW + 1200 }));
        // GraphQL is a separate budget.
        let q = c.graphql_at("query { viewer { login } }".into(), NOW + 600, |_| {
            ok(answer("200 OK", &[], "{\"data\":{}}"))
        });
        assert!(q.is_ok());
        // After the reset reads resume.
        let fine = c.get_at(
            "repos/o/r/actions/runs?status=queued",
            true,
            NOW + 1201,
            |_| ok(answer("200 OK", &with(&[], &counters("4999")), "{}")),
        );
        assert_eq!(fine.unwrap(), "{}");
        assert_eq!(c.budget.core.remaining, 4999);
    }

    #[test]
    fn a_secondary_limit_waits_for_retry_after() {
        let r = parse_response(&answer(
            "403 Forbidden",
            &[("Retry-After", "90")],
            "{\"message\":\"You have exceeded a secondary rate limit.\"}",
        ));
        assert_eq!(r.limited_until(NOW), Some(NOW + 90));
        let r = parse_response(&answer(
            "429 Too Many Requests",
            &[],
            "{\"message\":\"You have exceeded a secondary rate limit.\"}",
        ));
        assert_eq!(r.limited_until(NOW), Some(NOW + 60));
    }

    #[test]
    fn a_403_for_rights_is_not_a_rate_limit() {
        let mut c = Client::default();
        let h = counters("4800");
        let e = c.get_at("repos/o/r/actions/runners?per_page=100", true, NOW, |_| {
            failed(
                answer(
                    "403 Forbidden",
                    &with(&[], &h),
                    "{\"message\":\"Must have admin rights to Repository.\"}",
                ),
                "gh: Must have admin rights to Repository. (HTTP 403)",
            )
        });
        let e = e.unwrap_err();
        assert_eq!(
            e,
            GhError::Failed("gh: HTTP 403: Must have admin rights to Repository.".into())
        );
        assert!(denied(&e));
        assert!(c.budget.core.paused_until.is_none());
    }

    #[test]
    fn a_graphql_rate_limit_pauses_graphql_only() {
        let mut c = Client::default();
        let head = [
            ("X-Ratelimit-Limit", "5000"),
            ("X-Ratelimit-Remaining", "0"),
            ("X-Ratelimit-Reset", "1795233500"),
            ("X-Ratelimit-Resource", "graphql"),
        ];
        let e = c.graphql_at("query { viewer { login } }".into(), NOW, |_| {
            ok(answer(
                "200 OK",
                &head,
                "{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"API rate limit exceeded\"}]}",
            ))
        });
        assert_eq!(
            e,
            Err(GhError::Paused {
                until: 1_795_233_500
            })
        );
        assert!(c.budget.core.paused_until.is_none());
        assert_eq!(c.budget.graphql.remaining, 0);
    }

    #[test]
    fn a_graphql_answer_with_errors_is_kept_with_its_failure() {
        let mut c = Client::default();
        let (body, failure) = c
            .graphql_at("query { x }".into(), NOW, |_| {
                failed(
                    answer("200 OK", &[], "{\"data\":{\"p0\":null},\"errors\":[]}"),
                    "gh: Could not resolve",
                )
            })
            .unwrap();
        assert!(body.starts_with("{\"data\""));
        assert!(failure.unwrap().contains("Could not resolve"));
    }

    #[test]
    fn the_cadence_stretches_only_when_the_budget_would_run_out() {
        let s = Duration::from_secs;
        let mut core = RateLimit {
            limit: 5000,
            remaining: 4700,
            reset_at: NOW + 3600,
            ..Default::default()
        };
        assert_eq!(cadence(&core, s(60), 10, NOW), s(60));
        core.remaining = 400;
        // 200 to spend at 10 a turn: 20 turns in the hour, one every 3 min.
        assert_eq!(cadence(&core, s(60), 10, NOW), s(180));
        core.remaining = 10;
        assert_eq!(cadence(&core, s(60), 10, NOW), s(3600));
        assert_eq!(
            cadence(&core, s(60), 0, NOW),
            s(60),
            "nothing counted, nothing to stretch"
        );
        assert_eq!(cadence(&RateLimit::default(), s(60), 10, NOW), s(60));
    }
}
