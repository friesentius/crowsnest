//! Firstmate's fleet: `bin/fm-fleet-snapshot.sh --json` (schema
//! `fm-fleet-snapshot.v1`). This is firstmate's documented machine contract;
//! crowsnest never reads firstmate's state files directly.

use crate::model::{Fleet, HeldCall, LastEvent, Worker};
use crate::timefmt;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const SCHEMA: &str = "fm-fleet-snapshot.v1";

#[derive(Deserialize, Default)]
#[serde(default)]
struct Snapshot {
    schema: String,
    generated: String,
    backlog: Backlog,
    tasks: Vec<Task>,
    secondmate_current: SecondmateCurrent,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Backlog {
    records: Vec<Record>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Record {
    id: Option<String>,
    title: Option<String>,
    state: Option<String>,
    repo: Option<String>,
    hold_kind: Option<String>,
    captain_actionable: bool,
    hold_age_days: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Task {
    id: String,
    kind: String,
    harness: String,
    mode: String,
    branch: Option<String>,
    project: Option<String>,
    paths: Paths,
    current_state: CurrentState,
    endpoint: Endpoint,
    pr: Pr,
    hints: Hints,
    backlog: Option<Record>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Paths {
    status_log: StatusLog,
    worktree: PathEntry,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusLog {
    last_event: Option<Event>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Event {
    state: String,
    note: String,
    raw: String,
    age_seconds: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PathEntry {
    path: Option<PathBuf>,
    present: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CurrentState {
    state: String,
    source: String,
    detail: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Endpoint {
    exists: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Pr {
    url: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Hints {
    pending_decision: serde_json::Value,
    blocked_event: serde_json::Value,
    open_decisions: Vec<serde_json::Value>,
}

fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        serde_json::Value::String(s) => s == "true" || s == "1",
        _ => false,
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SecondmateCurrent {
    records: Vec<SecondmateRecord>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SecondmateRecord {
    id: String,
    current: SecondmateState,
    active_children: Vec<Child>,
    freshness: SecondmateFreshness,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SecondmateState {
    state: String,
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SecondmateFreshness {
    age_seconds: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Child {
    id: String,
    kind: String,
    state: String,
    repo: Option<String>,
    name: Option<String>,
    source: String,
    doing: String,
}

/// Keys of the open decisions that wait on the captain. Firstmate's fold
/// also carries open `blocked` events in the same list, keyed by `verb`.
fn captain_decisions(list: &[serde_json::Value]) -> Vec<String> {
    list.iter()
        .filter(|v| {
            v.get("verb")
                .and_then(|x| x.as_str())
                .unwrap_or("needs-decision")
                == "needs-decision"
        })
        .map(decision_key)
        .collect()
}

fn decision_key(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(m) => ["key", "id", "summary"]
            .iter()
            .find_map(|k| m.get(*k).and_then(|x| x.as_str()).map(str::to_string))
            .unwrap_or_else(|| "decision".into()),
        other => other.to_string(),
    }
}

fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Parse `fm-fleet-snapshot.sh --json` output.
pub fn parse(json: &str) -> Result<Fleet, String> {
    let snap: Snapshot =
        serde_json::from_str(json).map_err(|e| format!("invalid snapshot JSON: {e}"))?;
    if snap.schema != SCHEMA {
        return Err(format!(
            "unsupported fleet schema {:?} (crowsnest reads {SCHEMA})",
            snap.schema
        ));
    }
    let secondmates: std::collections::HashMap<&str, &SecondmateRecord> = snap
        .secondmate_current
        .records
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();

    let generated = timefmt::parse_iso(&snap.generated);
    let mut workers = Vec::new();
    for t in &snap.tasks {
        let last = t
            .paths
            .status_log
            .last_event
            .as_ref()
            .filter(|e| !e.raw.is_empty());
        let mut w = Worker {
            id: t.id.clone(),
            kind: t.kind.clone(),
            project: t
                .backlog
                .as_ref()
                .and_then(|b| b.repo.clone())
                .or_else(|| t.project.as_deref().map(|p| base_name(Path::new(p))))
                .unwrap_or_default(),
            harness: t.harness.clone(),
            mode: t.mode.clone(),
            title: t.backlog.as_ref().and_then(|b| b.title.clone()),
            fm_state: t.current_state.state.clone(),
            source: t.current_state.source.clone(),
            detail: t.current_state.detail.clone(),
            branch: t.branch.clone(),
            worktree: t
                .paths
                .worktree
                .path
                .clone()
                .filter(|_| t.paths.worktree.present),
            pr_url: t.pr.url.clone(),
            open_decisions: captain_decisions(&t.hints.open_decisions),
            blocked_event: truthy(&t.hints.blocked_event),
            last_event: last.map(|e| LastEvent {
                state: e.state.clone(),
                note: e.note.clone(),
                age_secs: e.age_seconds,
                at: generated.zip(e.age_seconds).map(|(g, a)| g - a),
            }),
            wait_until: last.and_then(|e| timefmt::find_until(&e.raw)),
            parent: None,
            endpoint_exists: t.endpoint.exists,
        };
        if truthy(&t.hints.pending_decision) && w.open_decisions.is_empty() {
            w.open_decisions.push("decision".into());
        }
        if t.kind == "secondmate" {
            w.project = "2nd mate".into();
            if let Some(rec) = secondmates.get(t.id.as_str()) {
                w.fm_state = rec.current.state.clone();
                w.detail = match rec.current.state.as_str() {
                    "no_active_work" => "no active child work".into(),
                    other => rec
                        .current
                        .reason
                        .clone()
                        .unwrap_or_else(|| other.replace('_', " ")),
                };
                if w.endpoint_exists == Some(false) {
                    w.detail.push_str(" - pane gone");
                }
                if let Some(age) = rec.freshness.age_seconds.filter(|a| *a > 3600) {
                    w.detail
                        .push_str(&format!(", summary {} old", timefmt::dur(age)));
                }
                workers.push(w);
                for c in &rec.active_children {
                    workers.push(Worker {
                        id: c.id.clone(),
                        kind: c.kind.clone(),
                        project: c.repo.clone().unwrap_or_default(),
                        title: c.name.clone(),
                        fm_state: c.state.clone(),
                        source: c.source.clone(),
                        detail: c.doing.clone(),
                        parent: Some(t.id.clone()),
                        ..Default::default()
                    });
                }
                continue;
            }
        }
        workers.push(w);
    }

    let held = snap
        .backlog
        .records
        .iter()
        .filter(|r| r.captain_actionable)
        .map(|r| HeldCall {
            id: r.id.clone().unwrap_or_default(),
            title: r.title.clone().unwrap_or_default(),
            age_days: r.hold_age_days,
        })
        .collect();
    let next = snap
        .backlog
        .records
        .iter()
        .find(|r| r.state.as_deref() == Some("queued") && r.hold_kind.as_deref() != Some("captain"))
        .map(|r| HeldCall {
            id: r.id.clone().unwrap_or_default(),
            title: r.title.clone().unwrap_or_default(),
            age_days: None,
        });

    Ok(Fleet {
        generated,
        workers,
        held,
        next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/fleet-snapshot.json");

    #[test]
    fn parses_the_recorded_snapshot() {
        let fleet = parse(FIXTURE).expect("fixture parses");
        let ids: Vec<&str> = fleet.workers.iter().map(|w| w.id.as_str()).collect();
        assert!(ids.contains(&"pantry-tour-followups"), "{ids:?}");
        let w = fleet
            .workers
            .iter()
            .find(|w| w.id == "pantry-tour-followups")
            .unwrap();
        assert_eq!(w.kind, "ship");
        assert_eq!(w.mode, "no-mistakes");
        assert_eq!(w.project, "pantry");
        assert_eq!(w.fm_state, "working");
        assert_eq!(w.source, "run-step");
        assert!(w.pr_url.as_deref().unwrap().ends_with("/pull/122"));
        assert!(w.worktree.is_some());
        assert!(w.wait_until.is_some(), "until parsed from the status line");

        let sm = fleet
            .workers
            .iter()
            .find(|w| w.kind == "secondmate")
            .unwrap();
        assert_eq!(sm.fm_state, "no_active_work");
        assert!(sm.detail.contains("pane gone"));

        assert_eq!(fleet.held.len(), 8);
        assert_eq!(fleet.held[0].age_days, Some(8));
        assert_eq!(fleet.next.as_ref().unwrap().id, "pantry-release-0-4-0");
    }

    #[test]
    fn open_decisions_mark_the_captain() {
        let json = FIXTURE.replacen(
            "\"open_decisions\": []",
            "\"open_decisions\": [{\"key\": \"board-review\", \"verb\": \"needs-decision\", \"summary\": \"pick a layout\"}, {\"key\": \"\", \"verb\": \"blocked\", \"summary\": \"x\"}]",
            1,
        );
        let fleet = parse(&json).unwrap();
        assert_eq!(
            fleet.workers[0].open_decisions,
            vec!["board-review".to_string()]
        );
        assert!(
            !fleet.workers[0].blocked_event,
            "blocked_event comes from hints, not the list"
        );
    }

    #[test]
    fn rejects_other_schemas() {
        let err = parse("{\"schema\": \"fm-fleet-snapshot.v9\"}").unwrap_err();
        assert!(err.contains("unsupported fleet schema"));
        assert!(parse("not json").is_err());
    }
}
