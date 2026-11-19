//! Render snapshots: the whole screen, drawn from the recorded fixtures into
//! ratatui's `TestBackend`, at a narrow and a wide size. Review changes with
//! `cargo insta review` (or rerun with `INSTA_UPDATE=always`).

use crowsnest::app::{Action, App, all_rows};
use crowsnest::model::{Disk, Freshness, Host, Runner, Snapshot, WorkerState};
use crowsnest::sources::{fleet, gh, nm, quota};
use crowsnest::timefmt::parse_iso;
use crowsnest::ui::{self, View, theme::Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use std::sync::Once;

const STALE: i64 = 30 * 60;

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
    let rows = all_rows(snap, STALE);
    while app.selected.as_deref() != Some(id) {
        app.apply(Action::Down, &rows);
    }
    app.apply(Action::Toggle, &rows);
}

#[test]
fn narrow_roster() {
    utc();
    let s = snapshot();
    insta::assert_snapshot!(draw(&s, &App::new(STALE), 100, 34).backend());
}

#[test]
fn narrow_detail() {
    utc();
    let s = snapshot();
    let mut app = App::new(STALE);
    open(&s, &mut app, "pantry-tour-followups");
    insta::assert_snapshot!(draw(&s, &app, 100, 34).backend());
}

#[test]
fn wide_detail() {
    utc();
    let s = snapshot();
    let mut app = App::new(STALE);
    open(&s, &mut app, "pantry-tour-followups");
    insta::assert_snapshot!(draw(&s, &app, 170, 44).backend());
}

#[test]
fn ascii_and_help() {
    utc();
    let s = snapshot();
    let mut app = App::new(STALE);
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
    let rows = all_rows(&s, STALE);
    assert_eq!(rows[0].state, WorkerState::Captain);
    assert_eq!(ui::asking(&s, STALE), 1);
    let term = draw(&s, &App::new(STALE), 100, 34);
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
    let app = App::new(STALE);
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
    let mut app = App::new(STALE);
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
