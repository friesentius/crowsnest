//! Render snapshots: the whole screen, drawn from the recorded fixtures into
//! ratatui's `TestBackend`, at a narrow and a wide size. Review changes with
//! `cargo insta review` (or rerun with `INSTA_UPDATE=always`).

use crowsnest::app::{Action, App, all_rows};
use crowsnest::config::{Overrides, Panel, Setting, Settings};
use crowsnest::model::{
    Check, CheckState, Disk, Freshness, Host, Job, LastEvent, PrChecks, QuotaWindow, RepoHistory,
    RepoRunners, RunInfo, Runner, Service, Snapshot, StaleKind, Thresholds, Worker, WorkerState,
};
use crowsnest::options::Category;
use crowsnest::sources::{fleet, gh, history, nm, quota};
use crowsnest::timefmt::parse_iso;
use crowsnest::ui::{self, View, theme::Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use std::sync::Once;

const STALE: i64 = 30 * 60;

fn th() -> Thresholds {
    Thresholds::new(STALE)
}

/// A fresh app that has watched `snap` once, as the running binary does.
fn app_for(snap: &Snapshot) -> App {
    let mut app = App::default();
    app.observe(snap);
    app
}

/// Times on screen are local; pin the zone so snapshots match everywhere.
fn utc() {
    static ONCE: Once = Once::new();
    // SAFETY: every test calls this before anything reads the environment
    // or the time zone, and `Once` makes the other threads wait for it.
    ONCE.call_once(|| unsafe { std::env::set_var("TZ", "UTC") });
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// The recorded fleet, with one worker asking the captain for a decision.
fn snapshot() -> Snapshot {
    let json = fixture("fleet-snapshot.json").replacen(
        "\"open_decisions\": []",
        "\"open_decisions\": [{\"key\": \"layout-pick\", \"verb\": \"needs-decision\", \"summary\": \"pick a layout\"}]",
        1,
    );
    let fleet = fleet::parse(&json).unwrap();
    let now = fleet.generated.unwrap() + 40;
    let mut s = Snapshot {
        now,
        fleet: Some(fleet),
        ..Default::default()
    };
    for (id, file) in [
        ("pantry-tour-followups", "nm-status-ci.toon"),
        ("bs-p1a-parity-copy", "nm-status-test.toon"),
    ] {
        s.pipelines
            .insert(id.into(), nm::parse(&fixture(file)).unwrap());
    }
    let urls = ["https://github.com/acme/pantry/pull/122".to_string()];
    for pr in gh::parse_checks(&fixture("gh-checks.json"), &urls)
        .unwrap()
        .0
    {
        s.prs.insert(pr.url.clone(), pr);
    }
    s.runners = vec![
        pantry_runner(now),
        Runner {
            name: "bookshop-runner".into(),
            dir: "/srv/actions-runner-bookshop".into(),
            repo: Some("acme/bookshop".into()),
            scanned_at: Some(now - 3),
            listener_secs: Some(16 * 86_400),
            idle_since: Some(now - 3 * 3600),
            runner_log_at: Some(now - 20),
            ..Default::default()
        },
    ];
    let run = gh::parse_runs(&fixture("gh-runs.json")).unwrap().remove(0);
    let mut jobs = gh::parse_jobs("acme/pantry", &fixture("gh-jobs.json")).unwrap();
    for j in &mut jobs {
        j.run = Some(run.clone());
    }
    // The release PR's lint waits behind the running unit tests.
    jobs.push(queued_job(
        "Lint & typecheck",
        release_run(),
        parse_iso("2026-11-19T14:20:10Z"),
    ));
    let mut cache = history::RepoCache::default();
    for run in gh::parse_runs(&fixture("gh-runs-completed.json")).unwrap() {
        let mut js =
            gh::parse_jobs("acme/pantry", &fixture(&format!("gh-jobs-{}.json", run.id))).unwrap();
        for j in &mut js {
            j.run = Some(run.clone());
        }
        cache.insert(run, js);
    }
    let finished: Vec<Job> = jobs
        .iter()
        .filter(|j| j.status == "completed")
        .cloned()
        .collect();
    s.history
        .insert("acme/pantry".into(), cache.summary(&finished));
    s.history
        .insert("acme/bookshop".into(), RepoHistory::default());
    s.jobs = jobs
        .into_iter()
        .filter(|j| j.status != "completed")
        .collect();
    s.jobs_read_at = Some(now - 25);
    s.gh_runners.insert(
        "acme/pantry".into(),
        RepoRunners {
            read_at: Some(now - 25),
            runners: gh::parse_runners(&fixture("gh-runners.json")).unwrap(),
            ..Default::default()
        },
    );
    s.gh_runners.insert(
        "acme/bookshop".into(),
        RepoRunners {
            read_at: Some(now - 25),
            denied: true,
            ..Default::default()
        },
    );
    s.quota = quota::parse(&fixture("quota.json")).unwrap();
    s.host = Some(Host {
        name: "deckhand".into(),
        load: [3.42, 2.95, 2.71],
        cpus: 16,
        mem_total: 32 << 30,
        mem_available: 9 << 30,
        swap_total: 8 << 30,
        swap_free: 6 << 30,
        disks: vec![
            Disk {
                mount: "/".into(),
                used_pct: 61.0,
            },
            Disk {
                mount: "/home".into(),
                used_pct: 78.0,
            },
        ],
        psi_mem: Some(0.4),
        psi_cpu: Some(6.1),
    });
    for (tier, age) in [
        ("host", 1),
        ("runners", 3),
        ("fleet", 40),
        ("nm", 9),
        ("gh", 25),
        ("quota", 120),
    ] {
        s.fresh.insert(
            tier.into(),
            Freshness {
                ok_at: Some(now - age),
                error: None,
            },
        );
    }
    s
}

/// The pantry runner, 8m into the unit tests of run #309, writing output.
fn pantry_runner(now: i64) -> Runner {
    let started = parse_iso("2026-11-19T14:18:34Z").unwrap();
    Runner {
        name: "pantry-local-runner".into(),
        dir: "/srv/actions-runner-pantry".into(),
        repo: Some("acme/pantry".into()),
        busy: true,
        busy_secs: Some(now - 3 - started),
        scanned_at: Some(now - 3),
        listener_secs: Some(16 * 86_400 + 3 * 3600),
        runner_log_at: Some(now - 32),
        worker_log_at: Some(now - 4),
        step_output_at: Some(now - 1),
        unit: Some("actions-runner-pantry.service".into()),
        service: Some(Service {
            active: "active".into(),
            sub: "running".into(),
            restarts: Some(2),
            error: None,
        }),
        ..Default::default()
    }
}

/// Run #310, the pantry release PR.
fn release_run() -> RunInfo {
    RunInfo {
        id: 44_541_002_310,
        attempt: 1,
        workflow: "CI".into(),
        number: 310,
        event: "pull_request".into(),
        title: "Pantry release 0.4.0".into(),
        branch: "dev".into(),
        sha: "c41d07be93a8f2e6d5c4b3a291807f6e5d4c3b2a".into(),
        pr: Some(124),
        base: Some("main".into()),
        created_at: parse_iso("2026-11-19T14:20:08Z"),
    }
}

fn queued_job(name: &str, run: RunInfo, created: Option<i64>) -> Job {
    Job {
        repo: "acme/pantry".into(),
        name: name.into(),
        status: "queued".into(),
        branch: run.branch.clone(),
        id: 135_700_000_000 + name.len() as u64,
        run_id: run.id,
        workflow: run.workflow.clone(),
        created_at: created,
        labels: vec!["self-hosted".into(), "pantry".into()],
        run: Some(run),
        ..Default::default()
    }
}

fn draw(snap: &Snapshot, app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let view = View {
        snap,
        app,
        theme: Theme::new(true, false),
    };
    term.draw(|f| ui::draw(f, &view, "deckhand")).unwrap();
    term
}

/// Select `id` and open its detail drawer, through the same actions the keys use.
fn open(snap: &Snapshot, app: &mut App, id: &str) {
    while app.selected.as_deref() != Some(id) {
        app.apply(Action::Down, snap);
    }
    app.apply(Action::Toggle, snap);
}

#[test]
fn narrow_roster() {
    utc();
    let s = snapshot();
    insta::assert_snapshot!(draw(&s, &app_for(&s), 100, 34).backend());
}

#[test]
fn narrow_detail() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    open(&s, &mut app, "pantry-tour-followups");
    insta::assert_snapshot!(draw(&s, &app, 100, 34).backend());
}

#[test]
fn wide_detail() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    open(&s, &mut app, "pantry-tour-followups");
    insta::assert_snapshot!(draw(&s, &app, 170, 44).backend());
}

#[test]
fn ascii_and_help() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    app.help = true;
    let mut term = Terminal::new(TestBackend::new(100, 34)).unwrap();
    let view = View {
        snap: &s,
        app: &app,
        theme: Theme::new(false, true),
    };
    term.draw(|f| ui::draw(f, &view, "deckhand")).unwrap();
    let text = term.backend().to_string();
    let odd: String = text.chars().filter(|c| !c.is_ascii()).collect();
    assert!(odd.is_empty(), "--ascii drew {odd:?}");
    insta::assert_snapshot!(term.backend());
}

#[test]
fn the_captain_row_sorts_first_and_stands_out() {
    utc();
    let s = snapshot();
    let rows = all_rows(&s, &th());
    assert_eq!(rows[0].state, WorkerState::Captain);
    assert_eq!(ui::asking(&s, &th()), 1);
    let term = draw(&s, &app_for(&s), 100, 34);
    let buf = term.backend().buffer();
    // The first roster row starts below the box title and summary lines.
    let (y, x) = (0..buf.area.height)
        .find_map(|y| {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            line.find("CAPTAIN")
                .map(|i| (y, line[..i].chars().count() as u16))
        })
        .expect("a CAPTAIN row is drawn");
    let cell = &buf[(x, y)];
    assert_eq!(cell.fg, Color::Magenta);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn no_colour_keeps_every_word() {
    utc();
    let s = snapshot();
    let app = app_for(&s);
    let render = |color| {
        let mut term = Terminal::new(TestBackend::new(100, 34)).unwrap();
        let view = View {
            snap: &s,
            app: &app,
            theme: Theme::new(color, false),
        };
        term.draw(|f| ui::draw(f, &view, "deckhand")).unwrap();
        let buf = term.backend().buffer().clone();
        assert!(
            color
                || buf
                    .content
                    .iter()
                    .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
        );
        term.backend().to_string()
    };
    assert_eq!(
        render(true),
        render(false),
        "colour only ever repeats what the text says"
    );
}

#[test]
fn an_open_drawer_taller_than_the_roster_is_cut_not_dropped() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    open(&s, &mut app, "pantry-tour-followups");
    let text = draw(&s, &app, 100, 24).backend().to_string();
    let row = text
        .lines()
        .position(|l| l.contains("pantry-tour-followups"))
        .expect("the selected row is drawn");
    assert!(
        text.lines()
            .skip(row)
            .any(|l| l.contains("branch fm/pantry-tour-followups")),
        "the drawer starts under its row:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Stalls and freshness: a gate parked on its worker, a declared wait that has
// passed, and a quiet fleet that must still read as up to date.
// ---------------------------------------------------------------------------

fn worker(id: &str, project: &str, state: &str, note: &str, age: i64, now: i64) -> Worker {
    Worker {
        id: id.into(),
        kind: "ship".into(),
        project: project.into(),
        harness: "claude".into(),
        mode: "no-mistakes".into(),
        fm_state: state.into(),
        source: "status-log".into(),
        branch: Some(format!("fm/{id}")),
        worktree: Some(std::env::temp_dir()),
        last_event: Some(LastEvent {
            state: state.into(),
            note: note.into(),
            age_secs: Some(age),
            at: Some(now - age),
        }),
        ..Default::default()
    }
}

fn parked(secs: i64, now: i64) -> crowsnest::model::Pipeline {
    let mut p = nm::parse(&fixture("nm-status-parked.toon")).unwrap();
    p.parked_secs = Some(secs);
    p.read_at = Some(now);
    p
}

fn checks(n: usize) -> Vec<Check> {
    (0..n)
        .map(|i| Check {
            name: format!("check {i}"),
            state: CheckState::Pass,
            started_at: None,
        })
        .collect()
}

/// The quiet fleet plus the stalls it was missing.
fn stalled() -> Snapshot {
    let mut s = snapshot();
    let now = s.now;
    // The review gate has waited 1h17m on its worker, whose own declared wait
    // ended more than an hour ago.
    let mut stuck = worker(
        "bs-p1c-type-pairing",
        "bookshop",
        "working",
        "paused: review findings, back to answer the gate",
        4800,
        now,
    );
    stuck.wait_until = parse_iso("2026-11-19T13:20Z");
    // Parked at its gate for only four minutes: still a quiet row.
    let mut fresh_gate = worker(
        "bs-p1d-footer-links",
        "bookshop",
        "working",
        "nm review round 1 found 2 issues",
        8400,
        now,
    );
    fresh_gate.pr_url = Some("https://github.com/acme/bookshop/pull/1234".into());
    // No pipeline at all: a worker that said it would be back at 14:02.
    let mut waiting = worker(
        "pantry-nav-refresh",
        "pantry",
        "paused",
        "waiting for runner capacity before re-running the e2e suite",
        1500,
        now,
    );
    waiting.mode = "direct".into();
    waiting.wait_until = parse_iso("2026-11-19T14:02Z");
    // A live step whose time needs every column it has.
    let docs = worker(
        "pantry-rss-feed",
        "pantry",
        "working",
        "nm document round 1",
        60,
        now,
    );
    s.pipelines
        .insert("bs-p1c-type-pairing".into(), parked(77 * 60, now));
    s.pipelines
        .insert("bs-p1d-footer-links".into(), parked(240, now));
    let mut running = nm::parse(&fixture("nm-status-test.toon")).unwrap();
    running.read_at = Some(now);
    running.steps[3].status = crowsnest::model::StepStatus::Completed;
    running.steps[4].status = crowsnest::model::StepStatus::Running;
    if let Some(a) = running.active.as_mut() {
        a.step = "document".into();
        a.active_secs = Some(65 * 60);
    }
    s.pipelines.insert("pantry-rss-feed".into(), running);
    s.prs.insert(
        "https://github.com/acme/bookshop/pull/1234".into(),
        PrChecks {
            url: "https://github.com/acme/bookshop/pull/1234".into(),
            number: 1234,
            state: "OPEN".into(),
            checks: checks(12),
            ..Default::default()
        },
    );
    s.fleet
        .as_mut()
        .unwrap()
        .workers
        .extend([stuck, fresh_gate, waiting, docs]);
    s
}

fn row_text(term: &Terminal<TestBackend>, needle: &str) -> String {
    let text = term.backend().to_string();
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line has {needle:?}:\n{text}"))
        .to_string()
}

#[test]
fn a_gate_parked_past_the_threshold_stands_out() {
    utc();
    let s = stalled();
    let rows = all_rows(&s, &th());
    let by = |id: &str| rows.iter().find(|r| r.worker.id == id).unwrap();
    let late = by("bs-p1c-type-pairing");
    assert_eq!(late.state, WorkerState::Stale);
    assert_eq!(late.stale.as_ref().unwrap().kind, StaleKind::Parked);
    let early = by("bs-p1d-footer-links");
    assert_eq!(early.state, WorkerState::Gate);
    // The late gate sorts above the quiet one.
    let pos = |id: &str| rows.iter().position(|r| r.worker.id == id).unwrap();
    assert!(pos("bs-p1c-type-pairing") < pos("bs-p1d-footer-links"));

    let term = draw(&s, &app_for(&s), 120, 46);
    let l1 = row_text(&term, "bs-p1c-type-pairing");
    assert!(l1.contains("! STALE"), "{l1}");
    assert!(l1.contains("parked 1h17m"), "{l1}");
    let text = term.backend().to_string();
    assert!(
        text.contains("parked at review gate for 1h17m, waiting on the worker"),
        "{text}"
    );
    let quiet = row_text(&term, "bs-p1d-footer-links");
    assert!(
        quiet.contains("GATE") && quiet.contains("parked 4m"),
        "{quiet}"
    );
    assert!(text.contains("nm review gate: findings 2 awaiting, parked 4m"));

    // The parked time is drawn in the attention colour; the quiet gate's is not.
    let buf = term.backend().buffer();
    let style_of = |needle: &str| {
        let (y, x) = (0..buf.area.height)
            .find_map(|y| {
                let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                line.find(needle)
                    .map(|i| (y, line[..i].chars().count() as u16))
            })
            .unwrap();
        buf[(x, y)].clone()
    };
    assert_eq!(style_of("parked 1h17m").fg, Color::Yellow);
    assert_ne!(style_of("parked 4m").fg, Color::Yellow);
}

#[test]
fn parked_time_keeps_counting_between_reads_and_crosses_the_threshold() {
    utc();
    let mut s = stalled();
    let now = s.now;
    s.pipelines
        .insert("bs-p1d-footer-links".into(), parked(240, now));
    let state = |s: &Snapshot| {
        all_rows(s, &th())
            .iter()
            .find(|r| r.worker.id == "bs-p1d-footer-links")
            .unwrap()
            .state
    };
    assert_eq!(state(&s), WorkerState::Gate);
    // No new read: six more minutes pass and the 4m gate is now 10m.
    s.now += 6 * 60;
    assert_eq!(state(&s), WorkerState::Stale);
    // The threshold is configurable.
    let patient = Thresholds {
        parked_secs: 3600,
        ..th()
    };
    let rows = all_rows(&s, &patient);
    let r = rows.iter().find(|r| r.worker.id == "bs-p1d-footer-links");
    assert_eq!(r.unwrap().state, WorkerState::Gate);
}

#[test]
fn a_passed_declared_wait_is_flagged() {
    utc();
    let s = stalled();
    let rows = all_rows(&s, &th());
    let r = rows
        .iter()
        .find(|r| r.worker.id == "pantry-nav-refresh")
        .unwrap();
    assert_eq!(r.state, WorkerState::Stale);
    assert_eq!(r.stale.as_ref().unwrap().kind, StaleKind::WaitPassed);
    let term = draw(&s, &app_for(&s), 120, 46);
    let text = term.backend().to_string();
    assert!(text.contains("declared wait ended 25m ago"), "{text}");
    assert!(row_text(&term, "pantry-nav-refresh").contains("! STALE"));
}

#[test]
fn the_header_counts_what_needs_attention() {
    utc();
    let s = stalled();
    let term = draw(&s, &app_for(&s), 180, 46);
    let header = row_text(&term, "asking you");
    // Two stale rows: the parked gate and the passed wait.
    assert!(
        header.contains("! 2 stale (1 parked, 1 wait over)"),
        "{header}"
    );
    assert!(header.contains("1 at gate"), "{header}");
    // Narrow screens keep the count and drop the breakdown.
    let narrow = draw(&s, &app_for(&s), 100, 46);
    let header = row_text(&narrow, "asking you");
    assert!(header.contains("! 2 stale"), "{header}");
    assert!(!header.contains("wait over"), "{header}");
    assert!(header.contains("held calls"), "{header}");
    assert!(!header.contains('\u{2026}'), "{header}");
}

/// A later reading of the quiet fleet, `secs` on: the pantry worker's CI run
/// moves from `ci` to a finished state, and nothing else happens.
fn later(before: &Snapshot, secs: i64) -> Snapshot {
    let mut s = before.clone();
    s.now += secs;
    for f in s.fresh.values_mut() {
        f.ok_at = Some(s.now);
    }
    s
}

#[test]
fn a_changed_row_is_highlighted_then_settles_and_the_header_says_so() {
    utc();
    let before = snapshot();
    let mut app = app_for(&before);
    // Nothing has changed since crowsnest started watching: every row says when
    // its last status line was, and the header says the fleet is quiet but read.
    let term = draw(&before, &app, 180, 40);
    let line = row_text(&term, "pantry-tour-followups");
    assert!(line.contains("pantry-tour-followups"));
    let text = term.backend().to_string();
    assert!(text.contains("changed 35m ago"), "{text}");
    assert!(
        text.contains("read 40s ago \u{b7} changed 47s ago"),
        "{text}"
    );

    // Ten seconds later the CI step of bs-p1a moves on.
    let mut after = later(&before, 10);
    {
        let p = after.pipelines.get_mut("bs-p1a-parity-copy").unwrap();
        p.steps[3].status = crowsnest::model::StepStatus::Completed;
        p.steps[4].status = crowsnest::model::StepStatus::Running;
        p.active.as_mut().unwrap().step = "document".into();
    }
    app.observe(&after);
    assert_eq!(app.changed_ago("bs-p1a-parity-copy", after.now), Some(0));
    assert_eq!(app.last_change_ago(after.now), Some(0));
    let term = draw(&after, &app, 180, 40);
    let text = term.backend().to_string();
    assert!(text.contains("read 0s ago \u{b7} changed 0s ago"), "{text}");
    // Only that row carries the highlight: its tag is reverse video.
    let buf = term.backend().buffer();
    let fresh: Vec<u16> = (0..buf.area.height)
        .filter(|&y| {
            (0..buf.area.width).any(|x| {
                let c = &buf[(x, y)];
                c.symbol() == "c" && c.modifier.contains(Modifier::REVERSED) && {
                    let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                    line.contains("changed 0s ago") && c.fg == Color::Green
                }
            })
        })
        .collect();
    assert_eq!(fresh.len(), 1, "one highlighted row: {text}");
    let line: String = (0..buf.area.width)
        .map(|x| buf[(x, fresh[0])].symbol())
        .collect();
    assert!(line.contains("nm document"), "{line}");

    // Half a minute on, the highlight is gone but the time is still there.
    let calm = later(&after, 30);
    app.observe(&calm);
    let term = draw(&calm, &app, 180, 40);
    let text = term.backend().to_string();
    assert!(
        text.contains("read 0s ago \u{b7} changed 30s ago"),
        "{text}"
    );
    let buf = term.backend().buffer();
    assert!(
        buf.content
            .iter()
            .all(|c| !(c.fg == Color::Green && c.modifier.contains(Modifier::REVERSED))),
        "the highlight settles"
    );
}

#[test]
fn an_overdue_fleet_read_is_warned_unless_refresh_is_paused() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    // Only the fleet is overdue: the other tiers keep reading fine.
    let mut old = s.clone();
    old.now += 600;
    for (tier, f) in old.fresh.iter_mut() {
        if tier != "fleet" {
            f.ok_at = Some(old.now - 2);
        }
    }
    app.observe(&old);
    let text = draw(&old, &app, 180, 40).backend().to_string();
    assert!(text.contains("\u{25b2} \u{21bb} read 10m ago"), "{text}");
    // A slow fleet interval raises the bar.
    app.read_overdue_secs = 900;
    let text = draw(&old, &app, 180, 40).backend().to_string();
    assert!(text.contains("\u{21bb} read 10m ago"), "{text}");
    assert!(!text.contains("\u{25b2} \u{21bb}"), "{text}");
    app.read_overdue_secs = 180;
    app.paused = true;
    let text = draw(&old, &app, 180, 40).backend().to_string();
    assert!(text.contains("\u{21bb} read 10m ago"), "{text}");
    assert!(!text.contains("\u{25b2} \u{21bb}"), "{text}");
}

#[test]
fn the_freshness_cue_is_chosen_by_fit() {
    utc();
    let s = stalled();
    let cue = |w| row_text(&draw(&s, &app_for(&s), w, 46), "crowsnest firstmate");
    assert!(
        cue(100).contains("14:27:06 \u{21bb} read 40s ago \u{b7} changed 47s ago"),
        "{}",
        cue(100)
    );
    assert!(
        cue(60).contains("\u{21bb} 40s \u{b7} \u{394}47s"),
        "{}",
        cue(60)
    );
}

#[test]
fn a_new_worker_is_a_change_and_a_failed_read_is_not() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    let mut t = later(&s, 5);
    t.fleet.as_mut().unwrap().workers.push(worker(
        "pantry-new-task",
        "pantry",
        "working",
        "starting",
        3,
        t.now,
    ));
    app.observe(&t);
    assert_eq!(app.changed_ago("pantry-new-task", t.now), Some(0));
    // A read that failed leaves no fleet; the rows keep their last change.
    let mut gone = later(&t, 20);
    gone.fleet = None;
    app.observe(&gone);
    assert_eq!(app.changed_ago("pantry-new-task", gone.now), Some(20));
}

/// Values that carry a duration or a count must never be cut, whatever the
/// screen width.
#[test]
fn value_columns_never_truncate_at_common_widths() {
    utc();
    let s = stalled();
    for width in [100u16, 160, 200] {
        let term = draw(&s, &app_for(&s), width, 46);
        let text = term.backend().to_string();
        for (needle, why) in [
            ("2h20m", "a long AGE"),
            ("1h20m", "a long AGE"),
            ("#1234 \u{2713} 12/12", "a four-digit PR with its checks"),
            ("document 1h05m", "a step with its time"),
            ("parked 1h17m", "a parked gate"),
            ("parked 4m", "a parked gate"),
        ] {
            assert!(
                text.contains(needle),
                "{why} ({needle:?}) is cut at {width} columns:\n{text}"
            );
        }
        // No value is ever replaced by an ellipsis: only task, project and
        // free text may be cut, and none of the shown rows needs it here.
        for line in text.lines().filter(|l| {
            ["STATE", "STALE", "GATE", "WORKING", "IDLE", "CAPTAIN"]
                .iter()
                .any(|w| l.contains(w))
        }) {
            for cut in [
                "2h2\u{2026}",
                "1h1\u{2026}",
                "1h0\u{2026}",
                "#1\u{2026}",
                "par\u{2026}",
            ] {
                assert!(!line.contains(cut), "{cut:?} at {width}: {line}");
            }
        }
        let header = row_text(&term, "asking you");
        assert!(
            !header.contains('\u{2026}'),
            "header cut at {width}: {header}"
        );
    }
}

#[test]
fn stalled_roster_100() {
    utc();
    let s = stalled();
    insta::assert_snapshot!(draw(&s, &app_for(&s), 100, 46).backend());
}

#[test]
fn stalled_roster_180() {
    utc();
    let s = stalled();
    insta::assert_snapshot!(draw(&s, &app_for(&s), 180, 44).backend());
}

#[test]
fn stalled_detail_shows_the_parked_gate() {
    utc();
    let s = stalled();
    let mut app = app_for(&s);
    open(&s, &mut app, "bs-p1c-type-pairing");
    let term = draw(&s, &app, 140, 46);
    let text = term.backend().to_string();
    assert!(text.contains("parked at review 1h17m"), "{text}");
    assert!(text.contains("declared wait until 13:20"), "{text}");
}

// ---------------------------------------------------------------------------
// Runner lanes: one lane per runner, its verdict in the panel title, and the
// detail view a selected runner opens.
// ---------------------------------------------------------------------------

/// Four runners: pantry busy and moving with a queue, bookshop idle,
/// millwork reported offline by GitHub, and larder past twice its typical
/// time while still writing output.
fn fleet_of_runners() -> Snapshot {
    let mut s = snapshot();
    let now = s.now;
    let mut millwork = Runner {
        name: "millwork-runner".into(),
        dir: "/srv/actions-runner-millwork".into(),
        repo: Some("acme/millwork".into()),
        scanned_at: Some(now - 3),
        listener_secs: Some(2 * 86_400),
        idle_since: Some(now - 40 * 60),
        ..Default::default()
    };
    let mut larder = pantry_runner(now);
    larder.name = "larder-runner".into();
    larder.dir = "/srv/actions-runner-larder".into();
    larder.repo = Some("acme/larder".into());
    larder.busy_secs = Some(52 * 60);
    larder.unit = None;
    larder.service = None;
    let mut slow = queued_job("Build docs site", release_run(), Some(now - 53 * 60));
    slow.repo = "acme/larder".into();
    slow.status = "in_progress".into();
    slow.runner = Some("larder-runner".into());
    slow.started_at = Some(now - 3 - 52 * 60);
    slow.run.as_mut().unwrap().title = "Larder docs refresh".into();
    slow.run.as_mut().unwrap().pr = Some(41);
    s.jobs.push(slow);
    s.history.insert(
        "acme/larder".into(),
        RepoHistory {
            typical: vec![crowsnest::model::Typical {
                workflow: "CI".into(),
                job: "Build docs site".into(),
                step: None,
                secs: 21 * 60,
                samples: 6,
            }],
            ..Default::default()
        },
    );
    s.gh_runners.insert(
        "acme/millwork".into(),
        RepoRunners {
            read_at: Some(now - 25),
            runners: vec![crowsnest::model::GhRunner {
                name: "millwork-runner".into(),
                status: "offline".into(),
                version: "2.338.2".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    millwork.runner_log_at = Some(now - 41 * 60);
    s.runners.push(millwork);
    s.runners.push(larder);
    s
}

/// The quiet fleet, but pantry is 24m into its unit tests and the
/// setup-node step has written nothing for 14 minutes.
fn stalled_runner() -> Snapshot {
    let mut s = snapshot();
    let now = s.now;
    let r = &mut s.runners[0];
    r.busy_secs = Some(24 * 60 - 3);
    r.step_output_at = Some(now - 14 * 60);
    r.worker_log_at = Some(now - 6);
    // Its processes have used under a CPU-second since the output stopped:
    // a hung download, not a quiet test.
    r.job_cpu = vec![
        (now - 20 * 60, 41_000),
        (now - 10 * 60, 41_300),
        (now, 41_700),
    ];
    let unit = &mut s.jobs[0];
    unit.started_at = Some(now - 24 * 60);
    unit.steps[3].started_at = Some(now - 23 * 60);
    s
}

/// The same silence, but the job's processes are busy: a quiet test suite.
fn quiet_runner() -> Snapshot {
    let mut s = stalled_runner();
    let now = s.now;
    s.runners[0].job_cpu = vec![
        (now - 20 * 60, 41_000),
        (now - 10 * 60, 201_000),
        (now, 380_000),
    ];
    s
}

#[test]
fn a_quiet_job_whose_processes_work_is_not_stalled() {
    utc();
    let s = quiet_runner();
    let text = draw_light(&s, &app_for(&s), 180, 44);
    assert!(text.contains("RUNNERS \u{2713} all moving"), "{text}");
    assert!(text.contains("quiet 14m, cpu busy"), "{text}");
    let mut app = app_for(&s);
    open_runner(&s, &mut app, "pantry-local-runner");
    let text = draw_light(&s, &app, 180, 44);
    assert!(
        text.contains("quiet 14m, job processes busy (2m59s CPU in 10m)"),
        "{text}"
    );
    assert!(text.contains("quiet, not hung"), "{text}");
}

fn draw_light(snap: &Snapshot, app: &App, w: u16, h: u16) -> String {
    draw(snap, app, w, h).backend().to_string()
}

fn lines_with<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.contains(needle)).collect()
}

#[test]
fn a_busy_runner_shows_its_job_progress_and_queue() {
    utc();
    let s = snapshot();
    let text = draw_light(&s, &app_for(&s), 180, 44);
    assert!(text.contains("RUNNERS \u{2713} all moving"), "{text}");
    let head = lines_with(&text, "\u{25d0} pantry")[0];
    assert!(head.contains("\u{2713} moving \u{b7} online"), "{head}");
    assert!(head.contains("q1 6m"), "{head}");
    assert!(
        text.contains("\u{25d0} Unit + e2e tests #122"),
        "the running job, with its PR:\n{text}"
    );
    assert!(
        text.contains("8m \u{2501}\u{2501}"),
        "elapsed then a bar:\n{text}"
    );
    assert!(text.contains("~40m"), "the typical time:\n{text}");
    assert!(
        text.contains("step 4/9 setup-node 8m \u{b7} out 1s ago"),
        "{text}"
    );
    // The release PR's lint waits its turn, with an estimated start: the
    // unit tests typically end 40m after 14:18:34.
    assert!(
        text.contains("\u{25cb} Lint & typecheck #124") && text.contains("wait 6m \u{2192} 14:58"),
        "{text}"
    );
    // The idle runner says when it last ran, and that GitHub would not say
    // whether it is online.
    let idle = lines_with(&text, "\u{25cb} bookshop")[0];
    assert!(idle.contains("idle \u{b7} online ?"), "{idle}");
}

#[test]
fn the_narrow_panel_keeps_the_verdict_and_the_next_start() {
    utc();
    let s = snapshot();
    let text = draw_light(&s, &app_for(&s), 100, 34);
    assert!(text.contains("RUNNERS \u{2713} moving"), "{text}");
    assert!(text.contains("q1 \u{256e}"), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    let pantry = lines
        .iter()
        .position(|l| l.contains("\u{25d0} pantry"))
        .unwrap();
    assert!(
        lines[pantry].contains("\u{2713} moving 8m/40m"),
        "{}",
        lines[pantry]
    );
    // Its queue sits right under it, never under another runner.
    assert!(
        lines[pantry + 1].contains("\u{2514} Lint wait 6m \u{2192} 14:58"),
        "{}",
        lines[pantry + 1]
    );
    assert!(lines[pantry + 2].contains("\u{25cb} bookshop idle"));
}

#[test]
fn a_silent_runner_is_stalled_in_the_panel_title() {
    utc();
    let s = stalled_runner();
    for (w, h) in [(100u16, 34u16), (180, 44)] {
        let text = draw_light(&s, &app_for(&s), w, h);
        assert!(text.contains("RUNNERS \u{2717} 1 stalled"), "{w}:\n{text}");
        let head = lines_with(&text, "\u{2717} pantry")[0];
        assert!(head.contains("STALLED"), "{head}");
        assert!(text.contains("silent 14m"), "{w}:\n{text}");
    }
    let mut app = app_for(&s);
    open_runner(&s, &mut app, "pantry-local-runner");
    let detail = draw_light(&s, &app, 180, 44);
    assert!(
        detail.contains("silent 14m (limit 10m), job processes idle (400ms CPU in 10m)"),
        "{detail}"
    );
    let wide = draw_light(&s, &app_for(&s), 180, 44);
    assert!(wide.contains("wait 6m blocked"), "{wide}");
    // Stalled runners do not count in the roster's stale count yet.
    assert!(row_text(&draw(&s, &app_for(&s), 180, 44), "asking you").contains("1 asking you"));
    assert!(!row_text(&draw(&s, &app_for(&s), 180, 44), "asking you").contains("stale"));
}

#[test]
fn offline_and_slow_runners_are_called_out() {
    utc();
    let s = fleet_of_runners();
    let text = draw_light(&s, &app_for(&s), 180, 50);
    assert!(text.contains("RUNNERS \u{2717} 1 stalled"), "{text}");
    let down = lines_with(&text, "millwork")[0];
    assert!(
        down.contains("\u{2717} millwork") && down.contains("DOWN \u{b7} offline"),
        "{down}"
    );
    let slow = lines_with(&text, "\u{25b2} larder")[0];
    assert!(slow.contains("SLOW"), "{slow}");
    assert!(text.contains("52m \u{2501}"), "{text}");
    // Without the offline runner the worst is the slow one.
    let mut s2 = s.clone();
    s2.gh_runners.remove("acme/millwork");
    let text = draw_light(&s2, &app_for(&s2), 180, 50);
    assert!(text.contains("RUNNERS \u{25b2} 1 slow"), "{text}");
}

/// Tab to the runner panel, down to `name`, and Enter its detail.
fn open_runner(s: &Snapshot, app: &mut App, name: &str) {
    let keys = crowsnest::app::runner_keys(s);
    app.apply(Action::Focus, s);
    while s.runners[app.runner_index(&keys).unwrap()].name != name {
        app.apply(Action::Down, s);
    }
    app.apply(Action::Toggle, s);
}

#[test]
fn enter_on_a_runner_opens_its_detail() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    open_runner(&s, &mut app, "pantry-local-runner");
    let text = draw_light(&s, &app, 100, 34);
    for needle in [
        "RUNNER pantry-local-runner \u{b7} acme/pantry",
        "Esc back",
        "VERDICT  \u{2713} moving  Unit is 8m into a job that usually takes 40m",
        "NOW      CI #309 \u{b7} pull_request \u{b7} #122 fix: per-account",
        "Unit + e2e tests   8m of ~40m",
        "\u{25d0} 4 setup-node 8m (typ 10m20s)",
        "5-9 pending",
        "QUEUE    \u{25cb} Lint & typecheck",
        "next to start ~14:58 when Unit ends",
        "RECENT",
        "HEALTH   listener \u{25cf} up 16d",
        "GitHub online   v2.338.2   labels self-hosted Linux X64 pantry",
        "service actions-runner-pantry.service active, 2 restarts",
    ] {
        assert!(text.contains(needle), "{needle:?} missing:\n{text}");
    }
    // Esc goes back to the panel.
    app.apply(Action::Collapse, &s);
    let text = draw_light(&s, &app, 100, 34);
    assert!(!text.contains("Esc back"), "{text}");
    assert!(text.contains("tab  roster"), "{text}");
}

#[test]
fn the_detail_degrades_without_admin_read_or_a_unit() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    open_runner(&s, &mut app, "bookshop-runner");
    let text = draw_light(&s, &app, 100, 34);
    assert!(
        text.contains("VERDICT  \u{25cb} idle  Idle, nothing queued."),
        "{text}"
    );
    assert!(
        text.contains("GitHub online ? (reading runners needs repo admin)"),
        "{text}"
    );
    assert!(text.contains("service not known"), "{text}");
}

#[test]
fn runner_lanes_100() {
    utc();
    let s = fleet_of_runners();
    insta::assert_snapshot!(draw(&s, &app_for(&s), 100, 34).backend());
}

#[test]
fn runner_lanes_180() {
    utc();
    let s = fleet_of_runners();
    insta::assert_snapshot!(draw(&s, &app_for(&s), 180, 50).backend());
}

#[test]
fn runner_stalled_180() {
    utc();
    let s = stalled_runner();
    insta::assert_snapshot!(draw(&s, &app_for(&s), 180, 44).backend());
}

#[test]
fn runner_detail_100() {
    utc();
    let s = snapshot();
    let mut app = app_for(&s);
    open_runner(&s, &mut app, "pantry-local-runner");
    insta::assert_snapshot!(draw(&s, &app, 100, 34).backend());
}

#[test]
fn runner_detail_stalled_180() {
    utc();
    let s = stalled_runner();
    let mut app = app_for(&s);
    open_runner(&s, &mut app, "pantry-local-runner");
    insta::assert_snapshot!(draw(&s, &app, 180, 44).backend());
}

/// An app with `settings`, opened on the options menu at `category` when
/// one is given, and moved to the row labelled `row`.
fn with_settings(
    snap: &Snapshot,
    settings: Settings,
    overrides: Overrides,
    menu: Option<(Category, &str)>,
) -> App {
    let mut app = App::new(settings, overrides);
    app.observe(snap);
    app.config.path = "~/.config/crowsnest/config.toml".into();
    if let Some((category, row)) = menu {
        app.apply(Action::Options, snap);
        while app.menu.as_ref().unwrap().category != category {
            app.apply(Action::Category { next: true }, snap);
        }
        for _ in 0..20 {
            let items = app.menu_items(snap);
            let i = app.menu.as_ref().unwrap().index(&items);
            if i.is_some_and(|i| items[i].label.trim() == row) {
                break;
            }
            app.apply(Action::Down, snap);
        }
    }
    app
}

fn hide(panels: &[Panel], usage: &[&str]) -> Settings {
    let mut s = Settings::default();
    s.hidden_panels.extend(panels.iter().copied());
    s.hidden_usage.extend(usage.iter().map(|u| u.to_string()));
    s
}

#[test]
fn options_menu_on_panels_100() {
    utc();
    let s = snapshot();
    let app = with_settings(
        &s,
        hide(&[Panel::Runners], &[]),
        Overrides::default(),
        Some((Category::Panels, "runners")),
    );
    insta::assert_snapshot!(draw(&s, &app, 100, 40).backend());
}

#[test]
fn options_menu_on_usage_180() {
    utc();
    let s = snapshot();
    let app = with_settings(
        &s,
        hide(&[], &["copilot/premium_interactions"]),
        Overrides::default(),
        Some((Category::Usage, "premium interactions")),
    );
    insta::assert_snapshot!(draw(&s, &app, 180, 50).backend());
}

#[test]
fn options_menu_shows_flags_as_locked() {
    utc();
    let s = snapshot();
    let mut o = Overrides::default();
    o.set(Setting::StaleMinutes, 45);
    let app = with_settings(
        &s,
        Settings::default(),
        o,
        Some((Category::Thresholds, "stale after")),
    );
    let term = draw(&s, &app, 100, 40);
    let row = row_text(&term, "stale after");
    assert!(row.contains("set by --stale-minutes"), "{row}");
    assert!(
        row.contains("45 min") && !row.contains("\u{25c2} 45"),
        "{row}"
    );
    let about = term.backend().to_string();
    assert!(about.contains("keeps 30 min."), "{about}");
}

#[test]
fn a_hidden_provider_leaves_the_usage_panel() {
    utc();
    let s = snapshot();
    let app = with_settings(&s, hide(&[], &["copilot"]), Overrides::default(), None);
    let term = draw(&s, &app, 100, 34);
    let text = term.backend().to_string();
    assert!(!text.contains("copilot"), "{text}");
    assert!(row_text(&term, "USAGE").contains("3 hidden"));
    assert!(
        row_text(&term, "session").contains("41%"),
        "one provider left: no name"
    );
    insta::assert_snapshot!(term.backend());
}

#[test]
fn panels_hidden_100() {
    utc();
    let s = snapshot();
    let app = with_settings(
        &s,
        hide(&[Panel::Runners], &["copilot/premium_interactions"]),
        Overrides::default(),
        None,
    );
    insta::assert_snapshot!(draw(&s, &app, 100, 34).backend());
}

#[test]
fn panels_hidden_180() {
    utc();
    let s = snapshot();
    let app = with_settings(
        &s,
        hide(&[Panel::Runners, Panel::Host], &[]),
        Overrides::default(),
        None,
    );
    insta::assert_snapshot!(draw(&s, &app, 180, 44).backend());
}

#[test]
fn roster_hidden_180_puts_lists_left_and_gauges_right() {
    utc();
    let s = snapshot();
    let app = with_settings(&s, hide(&[Panel::Roster], &[]), Overrides::default(), None);
    insta::assert_snapshot!(draw(&s, &app, 180, 44).backend());
}

#[test]
fn roster_hidden_100_stacks_the_panels() {
    utc();
    let s = snapshot();
    let app = with_settings(&s, hide(&[Panel::Roster], &[]), Overrides::default(), None);
    let term = draw(&s, &app, 100, 34);
    let text = term.backend().to_string();
    assert!(
        text.contains("HELD CALLS 8"),
        "the held calls get room: {text}"
    );
    insta::assert_snapshot!(term.backend());
}

#[test]
fn a_hidden_roster_leaves_its_keys_out_of_the_footer() {
    utc();
    let s = fleet_of_runners();
    let footer = |app: &App| {
        let text = draw(&s, app, 180, 44).backend().to_string();
        text.lines().last().unwrap().to_string()
    };
    let shown = with_settings(&s, Settings::default(), Overrides::default(), None);
    let f = footer(&shown);
    assert!(
        f.contains(" select ") && f.contains(" tab ") && f.contains(" filter "),
        "{f}"
    );

    // The runners have the keys; there is no roster to filter or Tab to.
    let mut app = with_settings(&s, hide(&[Panel::Roster], &[]), Overrides::default(), None);
    let f = footer(&app);
    assert!(f.contains(" runner ") && f.contains(" detail "), "{f}");
    for gone in [" select ", " filter ", " tab ", " roster ", " esc "] {
        assert!(!f.contains(gone), "{gone:?} in {f}");
    }
    assert!(f.contains(" o ") && f.contains(" options "), "{f}");
    app.apply(Action::Toggle, &s);
    let f = footer(&app);
    assert!(
        f.contains(" esc ") && f.contains(" back "),
        "the detail closes: {f}"
    );

    // Both hidden: only the keys that work anywhere.
    let app = with_settings(
        &s,
        hide(&[Panel::Roster, Panel::Runners], &[]),
        Overrides::default(),
        None,
    );
    let f = footer(&app);
    for gone in [" select ", " runner ", " detail ", " filter ", " tab "] {
        assert!(!f.contains(gone), "{gone:?} in {f}");
    }
    assert!(
        f.contains(" refresh ") && f.contains(" options ") && f.contains(" quit "),
        "{f}"
    );
}

#[test]
fn only_the_roster() {
    utc();
    let s = snapshot();
    let all_but_roster = hide(
        &[Panel::Runners, Panel::Host, Panel::Usage, Panel::Held],
        &[],
    );
    for w in [100, 180] {
        let app = with_settings(&s, all_but_roster.clone(), Overrides::default(), None);
        let term = draw(&s, &app, w, 34);
        let text = term.backend().to_string();
        assert!(!text.contains("HOST"), "{text}");
        let top = row_text(&term, "crowsnest firstmate");
        assert!(
            top.ends_with("\u{256e}\""),
            "the roster spans the screen: {top}"
        );
        assert!(row_text(&term, "hidden").contains("hidden ²runners ³host ⁴usage ⁵held"));
    }
}

#[test]
fn every_panel_hidden_says_how_to_get_one_back() {
    utc();
    let s = snapshot();
    let app = with_settings(&s, hide(&Panel::ALL, &[]), Overrides::default(), None);
    let term = draw(&s, &app, 100, 34);
    row_text(
        &term,
        "every panel is hidden: press 1-5 to show one, or o for options",
    );
}

#[test]
fn config_problems_show_in_the_footer() {
    utc();
    let s = snapshot();
    let mut app = with_settings(&s, Settings::default(), Overrides::default(), None);
    app.config.problems = vec![
        "unknown key display.theme".into(),
        "refresh.fleet_seconds must be a whole number from 10 to 3600".into(),
    ];
    let footer = |app: &App| {
        let term = draw(&s, app, 180, 44);
        let text = term.backend().to_string();
        text.lines().last().unwrap().to_string()
    };
    let f = footer(&app);
    assert!(
        f.contains("! config: unknown key display.theme (+1 more)"),
        "{f}"
    );
    let narrow = draw(&s, &app, 100, 34).backend().to_string();
    let f = narrow.lines().last().unwrap();
    assert!(
        f.contains("! config: unknown key display.theme (+1 more)"),
        "100 columns: {f}"
    );
    // A problem too long for the footer is cut; the count after it stays.
    app.config.problems = vec![
        "unknown key thresholds.fleet_seconds".into(),
        "unknown key display.theme".into(),
    ];
    let narrow = draw(&s, &app, 100, 34).backend().to_string();
    let f = narrow.lines().last().unwrap();
    assert!(
        f.contains("! config: unknown key thresholds.fleet_\u{2026} (+1 more)"),
        "100 columns: {f}"
    );
    assert!(f.contains(" ?  help  q  quit "), "100 columns: {f}");
    app.config
        .problems
        .push("refresh.fleet_seconds must be a whole number from 10 to 3600".into());
    // Help lists every problem in full.
    app.help = true;
    let text = draw(&s, &app, 100, 34).backend().to_string();
    assert!(text.contains("config.toml entries ignored:"), "{text}");
    assert!(
        text.contains("refresh.fleet_seconds must be a whole number from 10 to 3600"),
        "{text}"
    );
    app.help = false;
    app.config.saved = Some((s.now, Some("cannot save /x/config.toml: read-only".into())));
    let f = footer(&app);
    assert!(f.contains("! not saved: cannot save"), "{f}");
}

#[test]
fn a_config_problem_leaves_room_for_quit_and_the_freshness_markers() {
    utc();
    let s = snapshot();
    let problems = || -> Vec<String> {
        vec![
            "unknown key thresholds.fleet_seconds".into(),
            "unknown key display.theme".into(),
        ]
    };
    let footer = |app: &App| {
        let text = draw(&s, app, 100, 34).backend().to_string();
        text.lines().last().unwrap().to_string()
    };
    let check = |f: &str| {
        assert!(f.contains(" ?  help  q  quit "), "{f}");
        assert!(f.ends_with("nm 9s gh 25s quota 2m \""), "{f}");
    };

    // The list of hidden panels gives way to the note.
    let mut app = with_settings(
        &s,
        hide(
            &[Panel::Roster, Panel::Runners, Panel::Host, Panel::Usage],
            &[],
        ),
        Overrides::default(),
        None,
    );
    app.config.problems = problems();
    let f = footer(&app);
    check(&f);
    assert!(f.contains("! config: unknown key thresholds"), "{f}");
    assert!(f.contains("(+1 more)"), "{f}");

    let mut app = with_settings(
        &s,
        hide(&[Panel::Runners, Panel::Host], &[]),
        Overrides::default(),
        None,
    );
    app.config.problems = problems();
    app.filter = crowsnest::app::Filter::NeedsYou;
    app.paused = true;
    let f = footer(&app);
    check(&f);
    assert!(
        f.contains("filter: needs you") && f.contains("PAUSED"),
        "{f}"
    );
    assert!(
        f.contains("! config: unknown") && f.contains("(+1 more)"),
        "{f}"
    );

    // A failed save from a panel hotkey is never crowded out: "? help"
    // gives way to it.
    app.config.saved = Some((s.now, Some("cannot save /x/config.toml: read-only".into())));
    for width in [100, 80] {
        let f = draw(&s, &app, width, 34).backend().to_string();
        let f = f.lines().last().unwrap();
        assert!(f.contains("! not saved") && f.contains(" q  quit "), "{f}");
        assert!(f.ends_with("nm 9s gh 25s quota 2m \""), "{f}");
    }
    app.paused = false;
    app.filter = crowsnest::app::Filter::All;
    let f = footer(&app);
    check(&f);
    assert!(f.contains("! not saved: cannot save"), "{f}");
}

#[test]
fn the_menu_never_panics_and_says_when_it_has_no_room() {
    utc();
    let s = snapshot();
    let mut items_long = s.clone();
    // More usage windows than the box has rows for: the selection scrolls.
    for i in 0..30 {
        items_long.quota.push(QuotaWindow {
            provider: "extra".into(),
            id: format!("w{i}"),
            label: format!("window {i}"),
            ..Default::default()
        });
    }
    let mut app = with_settings(
        &items_long,
        hide(&Panel::ALL, &[]),
        Overrides::default(),
        Some((Category::Usage, "")),
    );
    app.apply(Action::Bottom, &items_long);
    for (w, h) in [
        (180, 50),
        (100, 40),
        (60, 20),
        (49, 16),
        (40, 12),
        (20, 6),
        (8, 3),
        (1, 1),
    ] {
        let term = draw(&items_long, &app, w, h);
        let text = term.backend().to_string();
        if w >= 60 {
            assert!(
                text.contains("window 29"),
                "{w}x{h} scrolls to the selection:\n{text}"
            );
        } else if w >= 40 {
            assert!(text.contains("larger"), "{w}x{h}:\n{text}");
        }
    }
    let ascii = {
        let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let view = View {
            snap: &s,
            app: &with_settings(
                &s,
                Settings::default(),
                Overrides::default(),
                Some((Category::Panels, "host")),
            ),
            theme: Theme::new(false, true),
        };
        term.draw(|f| ui::draw(f, &view, "deckhand")).unwrap();
        term.backend().to_string()
    };
    let odd: String = ascii.chars().filter(|c| !c.is_ascii()).collect();
    assert!(odd.is_empty(), "--ascii drew {odd:?}");
    assert!(ascii.contains("| > 3 host"), "{ascii}");
}
