//! A task's no-mistakes pipeline: `no-mistakes axi status`, run inside the
//! task's worktree. The output is TOON; see [`crate::toon`].

use crate::model::{ActiveStep, Pipeline, Step, StepStatus};
use crate::timefmt;
use crate::toon::{self, Toon, col};

/// Parse `no-mistakes axi status`. Returns `None` when the worktree has no run.
pub fn parse(text: &str) -> Option<Pipeline> {
    let doc = toon::parse(text);
    // A run on another branch is reported as `other_branch_run`; it is not
    // this task's pipeline.
    let run = doc.get("run")?;
    let steps = run
        .table("steps")
        .iter()
        .map(|r| Step {
            name: col(r, "step").to_string(),
            status: StepStatus::parse(col(r, "status")),
            findings: col(r, "findings").parse().unwrap_or(0),
            duration_ms: col(r, "duration_ms").parse().unwrap_or(0),
        })
        .collect();
    let active = run.table("active_steps").first().map(|r| {
        let last = col(r, "last_activity").to_string();
        ActiveStep {
            step: col(r, "step").to_string(),
            status: col(r, "status").to_string(),
            active_secs: timefmt::parse_go_dur(col(r, "active_for")),
            last_activity_secs: activity_age(&last),
            last_activity: activity_text(&last),
            round: col(r, "round").to_string(),
        }
    });
    let nonempty = |s: Option<&str>| s.filter(|v| !v.is_empty()).map(str::to_string);
    // `gate:` is an object with the step's name, or just the name.
    let gate = match doc.get("gate") {
        Some(Toon::Scalar(name)) => nonempty(Some(name)),
        Some(g) => nonempty(g.str("step")),
        None => None,
    };
    Some(Pipeline {
        run_id: run.str("id").unwrap_or_default().to_string(),
        branch: run.str("branch").unwrap_or_default().to_string(),
        status: run.str("status").unwrap_or_default().to_string(),
        head: run.str("head").unwrap_or_default().to_string(),
        pr_url: nonempty(run.str("pr")),
        findings: run.str("findings").unwrap_or("none").to_string(),
        steps,
        active,
        outcome: nonempty(doc.str("outcome")),
        error: nonempty(doc.str("error")),
        gate,
        // `awaiting_agent: parked 1h17m`: how long the run has waited on the agent.
        parked_secs: run
            .str("awaiting_agent")
            .and_then(|a| a.strip_prefix("parked "))
            .and_then(timefmt::parse_go_dur),
        read_at: None,
    })
}

/// `quiet 34m57s ago: log: ...` or `6s ago: claude producing output` -> age.
fn activity_age(s: &str) -> Option<i64> {
    let s = s.strip_prefix("quiet ").unwrap_or(s);
    let (head, _) = s.split_once(" ago")?;
    timefmt::parse_go_dur(head)
}

/// The human part after `<age> ago: `, without a leading `log: `.
fn activity_text(s: &str) -> String {
    let rest = s.split_once(" ago: ").map(|(_, r)| r).unwrap_or(s);
    rest.strip_prefix("log: ")
        .unwrap_or(rest)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_round_waiting_for_checks() {
        let p = parse(include_str!("../../tests/fixtures/nm-status-ci.toon")).unwrap();
        assert_eq!(p.status, "running");
        assert_eq!(p.steps.len(), 9);
        assert_eq!(p.steps[3].name, "test");
        assert_eq!(p.steps[3].duration_ms, 291_885);
        assert_eq!(p.steps[8].status, StepStatus::Running);
        assert_eq!(p.findings, "none");
        assert!(p.pr_url.as_deref().unwrap().ends_with("/pull/122"));
        let a = p.active.clone().unwrap();
        assert_eq!(a.step, "ci");
        assert_eq!(a.active_secs, Some(35 * 60 + 1));
        assert_eq!(a.last_activity_secs, Some(34 * 60 + 57));
        assert_eq!(a.last_activity, "CI checks running, waiting for results...");
        assert_eq!(a.round, "round 2");
        assert!(p.is_live());
    }

    #[test]
    fn test_step_running_with_findings() {
        let p = parse(include_str!("../../tests/fixtures/nm-status-test.toon")).unwrap();
        assert_eq!(p.findings, "1 auto-fix");
        assert_eq!(p.steps[2].findings, 1);
        assert_eq!(p.steps[4].status, StepStatus::Pending);
        assert_eq!(p.pr_url, None);
        assert_eq!(p.active.unwrap().last_activity_secs, Some(6));
    }

    #[test]
    fn a_run_parked_at_its_gate() {
        let p = parse(include_str!("../../tests/fixtures/nm-status-parked.toon")).unwrap();
        // no-mistakes leaves the run's status at `running` while it waits.
        assert_eq!(p.status, "running");
        assert_eq!(p.gate.as_deref(), Some("review"));
        assert_eq!(p.parked_secs, Some(77 * 60));
        assert_eq!(p.gate_step(), Some("review"));
        assert_eq!(p.findings, "2 awaiting");
        assert!(p.is_parked());
        assert!(!p.is_live());
        assert!(p.active.is_none());
        // A gate named by a bare scalar, and a run parked for days.
        let bare =
            parse("run:\n  id: x\n  status: running\n  awaiting_agent: parked 2d14h\ngate: test\n")
                .unwrap();
        assert_eq!(bare.gate.as_deref(), Some("test"));
        assert_eq!(bare.parked_secs, Some(2 * 86_400 + 14 * 3600));
        // A gate that is not waiting on the agent has no parked time.
        let p = parse(include_str!("../../tests/fixtures/nm-status-ci.toon")).unwrap();
        assert_eq!((p.gate, p.parked_secs), (None, None));
    }

    #[test]
    fn finished_run_and_other_branch() {
        let p = parse(include_str!(
            "../../tests/fixtures/nm-status-cancelled.toon"
        ))
        .unwrap();
        assert_eq!(p.status, "cancelled");
        assert_eq!(p.outcome.as_deref(), Some("cancelled"));
        assert_eq!(p.steps[8].status, StepStatus::Failed);
        assert!(!p.is_live());
        assert!(parse("current_branch: main\nother_branch_run:\n  id: x\n").is_none());
        assert!(parse("").is_none());
    }
}
