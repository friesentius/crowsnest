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
    pub budget: GhBudget,
    /// Reads that counted against the REST budget (not 304s) since
    /// [`Client::take_calls`] was last asked.
    calls: u32,
}

/// How many ETag answers are kept. Job lists of active runs are added as
/// runs come and go; the path asked for longest ago is dropped past this.
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
    /// Reads made against the REST budget since the last call, reset to 0.
    pub fn take_calls(&mut self) -> u32 {
        std::mem::take(&mut self.calls)
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
        if slot.paused_until.is_none_or(|u| u <= now) {
            slot.paused_at = Some(now);
        }
        slot.paused_until = Some(until);
        Some(until)
    }

    /// `gh api --method GET <path>`, asked with the last answer's ETag.
    pub fn get(&mut self, path: &str) -> Result<String, GhError> {
        self.get_at(path, true, now_epoch(), |cmd| cmd.run())
    }

    /// `gh api --method GET <path>` for an answer that is read only once
    /// (a finished run's jobs never change): no ETag is kept for it.
    pub fn get_once(&mut self, path: &str) -> Result<String, GhError> {
        self.get_at(path, false, now_epoch(), |cmd| cmd.run())
    }

    /// `gh api --method GET` for each `(path, keep)` of `reads`, at most
    /// [`PARALLEL`] at a time, answered in the order asked.
    pub fn get_many(&mut self, reads: &[(String, bool)]) -> Vec<Result<String, GhError>> {
        self.get_many_at(reads, now_epoch(), |cmd| cmd.run())
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
                    Some(out) => self.answer(path, *keep, now, out),
                    None => Err(GhError::Paused {
                        until: self.paused(false, now).unwrap_or(refused_until),
                    }),
                }
            })
            .collect()
    }

    fn get_at<F>(&mut self, path: &str, keep: bool, now: i64, run: F) -> Result<String, GhError>
    where
        F: FnOnce(&ReadCommand) -> Result<crate::exec::Output, ExecError>,
    {
        let cmd = self.request(path, now)?;
        let out = run(&cmd);
        self.answer(path, keep, now, out)
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

    /// Take in the answer to the GET of `path`.
    fn answer(
        &mut self,
        path: &str,
        keep: bool,
        now: i64,
        out: Result<crate::exec::Output, ExecError>,
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
            return match self.etags.get(path) {
                Some((_, _, body)) => Ok(body.clone()),
                None => Err(GhError::Failed(
                    "gh: HTTP 304 without a cached answer".into(),
                )),
            };
        }
        self.calls += 1;
        if failure.is_some() || !(200..300).contains(&r.status) {
            return Err(GhError::Failed(http_error(&r)));
        }
        match r.header("etag") {
            Some(etag) if keep && crate::exec::valid_etag(etag) => {
                if self.etags.len() >= MAX_ETAGS && !self.etags.contains_key(path) {
                    let oldest = self
                        .etags
                        .iter()
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
        self.graphql_at(query, now_epoch(), |cmd| cmd.run())
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
            ok(first)
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
            failed(refused, "gh: API rate limit exceeded (HTTP 403)")
        });
        assert_eq!(e, Err(GhError::Paused { until: NOW + 1200 }));
        assert_eq!(c.budget.core.paused_until, Some(NOW + 1200));
        assert_eq!(c.budget.core.paused_at, Some(NOW));
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
