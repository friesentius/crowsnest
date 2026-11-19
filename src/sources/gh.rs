//! GitHub, through the user's own `gh` login: pull-request check rollups (one
//! GraphQL read query for every PR on screen) and the Actions queue of
//! repositories with a self-hosted runner. Read requests only.

use crate::model::{Check, CheckState, Job, PrChecks};
use crate::timefmt;
use serde_json::Value;

/// `https://github.com/<owner>/<repo>/pull/<n>` -> (owner, repo, n).
pub fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next()? != "pull" {
        return None;
    }
    let n = parts.next()?.parse().ok()?;
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (safe(owner) && safe(repo)).then(|| (owner.to_string(), repo.to_string(), n))
}

/// One read query fetching the latest commit's check rollup of every PR.
pub fn checks_query(urls: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let Some((owner, repo, n)) = parse_pr_url(url) else {
            continue;
        };
        parts.push(format!(
            "p{i}: repository(owner: \"{owner}\", name: \"{repo}\") {{ pullRequest(number: {n}) {{ number url isDraft state commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first: 60) {{ nodes {{ __typename ... on CheckRun {{ name status conclusion startedAt }} ... on StatusContext {{ context state createdAt }} }} }} }} }} }} }} }} }}"
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("query {{ {} }}", parts.join(" ")))
    }
}

fn check_state(status: &str, conclusion: &str) -> CheckState {
    match (status, conclusion) {
        (_, "SUCCESS") => CheckState::Pass,
        (_, "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => {
            CheckState::Fail
        }
        (_, "SKIPPED" | "NEUTRAL" | "STALE") => CheckState::Skipped,
        ("IN_PROGRESS", _) => CheckState::Running,
        _ => CheckState::Queued,
    }
}

fn status_context_state(state: &str) -> CheckState {
    match state {
        "SUCCESS" => CheckState::Pass,
        "FAILURE" | "ERROR" => CheckState::Fail,
        _ => CheckState::Running,
    }
}

/// Parse the GraphQL response of [`checks_query`].
pub fn parse_checks(json: &str) -> Result<Vec<PrChecks>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid GraphQL JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(Value::as_object)
        .ok_or("GraphQL response has no data")?;
    let mut out = Vec::new();
    for pr in data.values() {
        let Some(pr) = pr.get("pullRequest").filter(|p| !p.is_null()) else {
            continue;
        };
        let mut checks = Vec::new();
        let nodes = pr
            .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for n in nodes {
            let s = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let check = if s("__typename") == "StatusContext" {
                Check {
                    name: s("context"),
                    state: status_context_state(&s("state")),
                    started_at: timefmt::parse_iso(&s("createdAt")),
                }
            } else {
                Check {
                    name: s("name"),
                    state: check_state(&s("status"), &s("conclusion")),
                    started_at: timefmt::parse_iso(&s("startedAt")),
                }
            };
            checks.push(check);
        }
        out.push(PrChecks {
            url: pr
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            number: pr.get("number").and_then(Value::as_u64).unwrap_or(0),
            draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
            state: pr
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            checks,
        });
    }
    Ok(out)
}

/// Run ids from `repos/<r>/actions/runs?status=...`.
pub fn parse_run_ids(json: &str) -> Result<Vec<u64>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid runs JSON: {e}"))?;
    Ok(v.get("workflow_runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|r| r.get("id").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default())
}

/// Queued and running jobs from `repos/<r>/actions/runs/<id>/jobs`.
pub fn parse_jobs(repo: &str, json: &str) -> Result<Vec<Job>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid jobs JSON: {e}"))?;
    let mut out = Vec::new();
    for j in v
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let s = |k: &str| j.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let status = s("status");
        if status != "queued"
            && status != "in_progress"
            && status != "waiting"
            && status != "pending"
        {
            continue;
        }
        out.push(Job {
            repo: repo.to_string(),
            name: s("name"),
            status: if status == "in_progress" {
                status
            } else {
                "queued".into()
            },
            runner: Some(s("runner_name")).filter(|r| !r.is_empty()),
            started_at: if s("status") == "in_progress" {
                timefmt::parse_iso(&s("started_at"))
            } else {
                None
            },
            branch: s("head_branch"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_urls() {
        assert_eq!(
            parse_pr_url("https://github.com/acme/pantry/pull/122"),
            Some(("acme".into(), "pantry".into(), 122))
        );
        assert_eq!(
            parse_pr_url("https://gitlab.com/acme/pantry/-/merge_requests/3"),
            None
        );
        assert_eq!(parse_pr_url("https://github.com/a\"b/c/pull/1"), None);
    }

    #[test]
    fn query_is_a_read_query() {
        let q = checks_query(&["https://github.com/acme/pantry/pull/122".into()]).unwrap();
        assert!(q.starts_with("query {"));
        assert!(q.contains("pullRequest(number: 122)"));
        assert!(checks_query(&["not a url".into()]).is_none());
    }

    #[test]
    fn parses_check_rollup() {
        let prs = parse_checks(include_str!("../../tests/fixtures/gh-checks.json")).unwrap();
        assert_eq!(prs.len(), 1);
        let pr = &prs[0];
        assert_eq!(pr.number, 122);
        assert!(!pr.draft);
        assert_eq!(pr.checks.len(), 4);
        assert_eq!(pr.count(CheckState::Pass), 3);
        assert_eq!(pr.count(CheckState::Running), 1);
        assert_eq!(pr.checks[0].name, "Lint & typecheck");
        assert!(pr.checks[1].started_at.is_some());
    }

    #[test]
    fn parses_queue() {
        assert_eq!(
            parse_run_ids(include_str!("../../tests/fixtures/gh-runs.json")).unwrap(),
            vec![44_537_668_424]
        );
        let fixture = include_str!("../../tests/fixtures/gh-jobs.json");
        let jobs = parse_jobs("acme/pantry", fixture).unwrap();
        assert_eq!(jobs.len(), 1, "completed jobs are not shown");
        assert_eq!(jobs[0].name, "Unit + e2e tests");
        assert_eq!(jobs[0].status, "in_progress");
        assert_eq!(jobs[0].runner.as_deref(), Some("pantry-local-runner"));
        assert!(jobs[0].started_at.is_some());
        // The same run a few minutes earlier, with jobs still waiting.
        let earlier = fixture.replace("\"status\": \"completed\"", "\"status\": \"queued\"");
        let jobs = parse_jobs("acme/pantry", &earlier).unwrap();
        assert_eq!(jobs.iter().filter(|j| j.status == "queued").count(), 3);
        assert!(
            jobs.iter()
                .filter(|j| j.status == "queued")
                .all(|j| j.started_at.is_none())
        );
    }
}
