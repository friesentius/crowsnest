//! GitHub, through the user's own `gh` login: pull-request check rollups (one
//! GraphQL read query for every PR on screen) and the Actions queue of
//! repositories with a self-hosted runner. Read requests only.

use crate::model::{Check, CheckState, GhRunner, Job, JobStep, PrChecks, RunInfo};
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

/// Parse the GraphQL response of [`checks_query`] for the same `urls`.
///
/// Each PR is keyed by the URL it was asked for, so callers can look it up
/// by the URL they hold. PRs GitHub could not resolve are skipped and come
/// back as one error each, so one bad PR never hides the others.
pub fn parse_checks(json: &str, urls: &[String]) -> Result<(Vec<PrChecks>, Vec<String>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid GraphQL JSON: {e}"))?;
    let url_of = |alias: &str| {
        alias
            .strip_prefix('p')
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| urls.get(i))
    };
    let errors: Vec<String> = v
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
            match e
                .pointer("/path/0")
                .and_then(Value::as_str)
                .and_then(url_of)
            {
                Some(url) => format!("gh: {url}: {msg}"),
                None => format!("gh: {msg}"),
            }
        })
        .collect();
    let Some(data) = v.get("data").and_then(Value::as_object) else {
        return Err(errors
            .into_iter()
            .next()
            .unwrap_or_else(|| "GraphQL response has no data".into()));
    };
    let mut out = Vec::new();
    for (alias, pr) in data {
        let Some(url) = url_of(alias) else {
            continue;
        };
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
            url: url.clone(),
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
    Ok((out, errors))
}

/// The runs in `repos/<r>/actions/runs?...`.
pub fn parse_runs(json: &str) -> Result<Vec<RunInfo>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid runs JSON: {e}"))?;
    let mut out = Vec::new();
    for r in v
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = r.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let event = s("event");
        // Only a pull request event is for the PR it lists.
        let pr = r
            .pointer("/pull_requests/0")
            .filter(|_| event.starts_with("pull_request"));
        out.push(RunInfo {
            id,
            attempt: r.get("run_attempt").and_then(Value::as_u64).unwrap_or(1) as u32,
            workflow: s("name"),
            number: r.get("run_number").and_then(Value::as_u64).unwrap_or(0),
            event,
            title: s("display_title"),
            branch: s("head_branch"),
            sha: s("head_sha"),
            pr: pr.and_then(|p| p.get("number")).and_then(Value::as_u64),
            base: pr
                .and_then(|p| p.pointer("/base/ref"))
                .and_then(Value::as_str)
                .map(str::to_string),
            created_at: timefmt::parse_iso(&s("created_at")),
        });
    }
    Ok(out)
}

/// Every job of `repos/<r>/actions/runs/<id>/jobs`, with its steps.
pub fn parse_jobs(repo: &str, json: &str) -> Result<Vec<Job>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid jobs JSON: {e}"))?;
    let time = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .and_then(timefmt::parse_iso)
    };
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = Vec::new();
    for j in v
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let status = match text(j, "status").as_str() {
            "in_progress" => "in_progress",
            "completed" => "completed",
            "waiting" | "pending" => "waiting",
            _ => "queued",
        }
        .to_string();
        let steps = j
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|s| JobStep {
                number: s.get("number").and_then(Value::as_u64).unwrap_or(0) as u32,
                name: text(s, "name"),
                status: text(s, "status"),
                conclusion: Some(text(s, "conclusion")).filter(|c| !c.is_empty()),
                started_at: time(s, "started_at"),
                completed_at: time(s, "completed_at"),
            })
            .collect();
        out.push(Job {
            repo: repo.to_string(),
            name: text(j, "name"),
            runner: Some(text(j, "runner_name")).filter(|r| !r.is_empty()),
            // A queued job carries the time it was created as `started_at`.
            started_at: if status == "in_progress" || status == "completed" {
                time(j, "started_at")
            } else {
                None
            },
            completed_at: if status == "completed" {
                time(j, "completed_at")
            } else {
                None
            },
            status,
            branch: text(j, "head_branch"),
            id: j.get("id").and_then(Value::as_u64).unwrap_or(0),
            run_id: j.get("run_id").and_then(Value::as_u64).unwrap_or(0),
            workflow: text(j, "workflow_name"),
            created_at: time(j, "created_at"),
            conclusion: Some(text(j, "conclusion")).filter(|c| !c.is_empty()),
            labels: j
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect(),
            steps,
            run: None,
        });
    }
    Ok(out)
}

/// `repos/<r>/actions/runners`.
pub fn parse_runners(json: &str) -> Result<Vec<GhRunner>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("invalid runners JSON: {e}"))?;
    let runners = v
        .get("runners")
        .and_then(Value::as_array)
        .ok_or("runners JSON has no runners")?;
    Ok(runners
        .iter()
        .map(|r| {
            let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            GhRunner {
                name: s("name"),
                status: s("status"),
                busy: r.get("busy").and_then(Value::as_bool).unwrap_or(false),
                version: s("version"),
                labels: r
                    .get("labels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect(),
            }
        })
        .collect())
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
        let urls = ["https://github.com/acme/pantry/pull/122".to_string()];
        let (prs, errors) =
            parse_checks(include_str!("../../tests/fixtures/gh-checks.json"), &urls).unwrap();
        assert!(errors.is_empty());
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
    fn checks_are_keyed_by_the_url_asked_for() {
        // GitHub answers with its canonical URL after a rename or with a
        // trailing path; rows look PRs up by the URL firstmate recorded.
        let asked = "https://github.com/Acme/Pantry/pull/122/files".to_string();
        let (prs, _) = parse_checks(
            include_str!("../../tests/fixtures/gh-checks.json"),
            std::slice::from_ref(&asked),
        )
        .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].url, asked);
    }

    #[test]
    fn one_unresolvable_pr_keeps_the_others() {
        let urls = [
            "https://github.com/acme/pantry/pull/122".to_string(),
            "https://github.com/acme/gone/pull/7".to_string(),
            "https://github.com/acme/pantry/pull/999".to_string(),
        ];
        let ok: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/gh-checks.json")).unwrap();
        let json = serde_json::json!({
            "data": {
                "p0": ok["data"]["p0"],
                "p1": null,
                "p2": { "pullRequest": null },
            },
            "errors": [
                {
                    "type": "NOT_FOUND",
                    "path": ["p1"],
                    "message": "Could not resolve to a Repository with the name 'acme/gone'."
                },
                {
                    "type": "NOT_FOUND",
                    "path": ["p2", "pullRequest"],
                    "message": "Could not resolve to a PullRequest with the number of 999."
                }
            ]
        })
        .to_string();
        let (prs, errors) = parse_checks(&json, &urls).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].url, urls[0]);
        assert_eq!(prs[0].checks.len(), 4);
        assert_eq!(
            errors,
            [
                "gh: https://github.com/acme/gone/pull/7: Could not resolve to a Repository with the name 'acme/gone'.",
                "gh: https://github.com/acme/pantry/pull/999: Could not resolve to a PullRequest with the number of 999.",
            ]
        );
        // Without any data the first error is the failure.
        let json = r#"{"errors":[{"message":"Bad credentials"}]}"#;
        assert_eq!(
            parse_checks(json, &urls).unwrap_err(),
            "gh: Bad credentials"
        );
    }

    #[test]
    fn parses_queue() {
        let runs = parse_runs(include_str!("../../tests/fixtures/gh-runs.json")).unwrap();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.id, 44_537_668_424);
        assert_eq!((run.workflow.as_str(), run.number), ("CI", 309));
        assert_eq!(run.pr, Some(122));
        assert_eq!(run.base.as_deref(), Some("dev"));
        assert_eq!(run.label(), "#122");
        let fixture = include_str!("../../tests/fixtures/gh-jobs.json");
        let jobs = parse_jobs("acme/pantry", fixture).unwrap();
        assert_eq!(jobs.len(), 4);
        let running: Vec<&Job> = jobs.iter().filter(|j| j.status == "in_progress").collect();
        assert_eq!(running.len(), 1);
        let unit = running[0];
        assert_eq!(unit.name, "Unit + e2e tests");
        assert_eq!(unit.runner.as_deref(), Some("pantry-local-runner"));
        assert!(unit.started_at.is_some());
        assert_eq!(unit.labels, ["self-hosted", "pantry"]);
        assert!(unit.is_self_hosted());
        let step = unit.current_step().unwrap();
        assert_eq!(
            (step.number, step.name.as_str()),
            (4, "Run actions/setup-node@v4")
        );
        let done = &jobs[0];
        assert_eq!(done.conclusion.as_deref(), Some("success"));
        assert_eq!(
            done.completed_at.unwrap() - done.started_at.unwrap(),
            13 * 60 + 20
        );
        // The same run a few minutes earlier, with jobs still waiting.
        let earlier = fixture.replace("\"status\": \"completed\"", "\"status\": \"queued\"");
        let jobs = parse_jobs("acme/pantry", &earlier).unwrap();
        assert_eq!(jobs.iter().filter(|j| j.status == "queued").count(), 3);
        assert!(
            jobs.iter()
                .filter(|j| j.status == "queued")
                .all(|j| j.started_at.is_none() && j.created_at.is_some())
        );
    }

    #[test]
    fn a_push_run_is_not_for_the_pr_it_lists() {
        let json = include_str!("../../tests/fixtures/gh-runs.json")
            .replace("\"pull_request\"", "\"push\"");
        let run = &parse_runs(&json).unwrap()[0];
        assert_eq!(run.pr, None);
        assert_eq!(run.label(), "fm/pantry-tour-followups push");
    }

    #[test]
    fn parses_runners() {
        let r = parse_runners(include_str!("../../tests/fixtures/gh-runners.json")).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "pantry-local-runner");
        assert_eq!(r[0].status, "online");
        assert_eq!(r[0].version, "2.338.2");
        assert_eq!(r[0].labels, ["self-hosted", "Linux", "X64", "pantry"]);
        assert!(parse_runners("{\"message\": \"Must have admin rights\"}").is_err());
    }
}
