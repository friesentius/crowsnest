//! Render snapshots: the whole screen, drawn from the recorded fixtures into
//! ratatui's `TestBackend`, at a narrow and a wide size. Review changes with
//! `cargo insta review` (or rerun with `INSTA_UPDATE=always`).

use crowsnest::app::{Action, App, all_rows};
use crowsnest::model::{
    Check, CheckState, Disk, Freshness, Host, LastEvent, PrChecks, Runner, Snapshot, StaleKind,
    Thresholds, Worker, WorkerState,
};
use crowsnest::sources::{fleet, gh, nm, quota};
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
    let mut app = App::new(th());
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
    let started = parse_iso("2026-11-19T14:12:51Z");
    s.runners = vec![
        Runner {
            name: "pantry-local-runner".into(),
            dir: "/srv/actions-runner-pantry".into(),
            repo: Some("acme/pantry".into()),
            busy: true,
            busy_secs: started.map(|t| now - t),
        },
        Runner {
            name: "bookshop-runner".into(),
            dir: "/srv/actions-runner-bookshop".into(),
            repo: Some("acme/bookshop".into()),
            ..Default::default()
        },
    ];
    s.jobs = gh::parse_jobs("acme/pantry", &fixture("gh-jobs.json")).unwrap();
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
    let rows = all_rows(snap, &th());
    while app.selected.as_deref() != Some(id) {
        app.apply(Action::Down, &rows);
    }
    app.apply(Action::Toggle, &rows);
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

/// The narrowest width that draws the job list (below it the panel shows counts only).
const WIDE_MIN: u16 = ui::WIDE;

fn job(repo: &str, name: &str, status: &str) -> crowsnest::model::Job {
    crowsnest::model::Job {
        repo: repo.into(),
        name: name.into(),
        status: status.into(),
        started_at: (status == "in_progress").then_some(0),
        ..Default::default()
    }
}

#[test]
fn every_runner_job_names_its_project_and_is_grouped_by_repo() {
    utc();
    let mut s = snapshot();
    s.now = 120;
    // Interleaved on purpose: grouping must not depend on arrival order.
    s.jobs = vec![
        job("acme/pantry", "Lint & typecheck", "queued"),
        job("acme/bookshop", "Release build", "queued"),
        job("acme/pantry", "Fresh-clone setup", "queued"),
        job("acme/pantry", "Unit tests", "in_progress"),
    ];
    for w in [WIDE_MIN, 180] {
        let term = draw(&s, &app_for(&s), w, 44);
        let text = term.backend().to_string();
        let lines: Vec<&str> = text.lines().collect();
        let pos = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing at {w}:\n{text}"))
        };
        let unit = pos("pantry \u{25d0} Unit tests");
        let lint = pos("pantry \u{25cb} queued Lint & typecheck");
        let fresh = pos("pantry \u{25cb} queued Fresh-clone setup");
        let release = pos("bookshop \u{25cb} queued Release build");
        assert!(unit < lint && lint < fresh && fresh < release, "{text}");
        // Truncation eats the job name, never the project label.
        let mut long_snap = s.clone();
        long_snap.jobs[0].name = "A very long job name that cannot possibly fit the panel".into();
        let long = draw(&long_snap, &app_for(&long_snap), w, 44)
            .backend()
            .to_string();
        let row = long
            .lines()
            .find(|l| l.contains("A very long"))
            .unwrap_or_else(|| panic!("long job missing at {w}:\n{long}"));
        assert!(row.contains("pantry \u{25cb} queued A very long"), "{row}");
        assert!(!row.contains("fit the panel"), "not truncated: {row}");
    }
}
