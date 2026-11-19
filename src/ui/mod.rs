//! Rendering. The Roster layout: one two-line row per worker, most urgent
//! first, an inline drawer on Enter, and runner, host and usage panels. Below
//! 140 columns the panels sit under the roster; from 140 columns they move
//! to a right-hand column and the held captain calls get their own panel.

pub mod theme;

use crate::app::{App, Row, visible_rows};
use crate::model::{
    CheckState, PrChecks, Snapshot, StaleKind, StepStatus, Thresholds, WorkerState,
};
use crate::timefmt;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use theme::Theme;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width from which the wide layout is used.
pub const WIDE: u16 = 140;
const RIGHT_COL: u16 = 50;

pub struct View<'a> {
    pub snap: &'a Snapshot,
    pub app: &'a App,
    pub theme: Theme,
}

/// A line of content, or a separator drawn across the box borders.
enum Content<'a> {
    Line(Line<'a>),
    Sep,
}

fn span<'a>(text: impl Into<String>, style: Style) -> Span<'a> {
    Span::styled(text.into(), style)
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Truncate `text` to `w` cells, ending in an ellipsis when cut.
pub fn trunc(text: &str, w: usize) -> String {
    if text.width() <= w {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(ch);
        used += cw;
    }
    if w > 0 {
        out.push('\u{2026}');
    }
    out
}

/// Pad or truncate to exactly `w` cells.
pub fn rpad(text: &str, w: usize) -> String {
    let t = trunc(text, w);
    let pad = w.saturating_sub(t.width());
    format!("{t}{}", " ".repeat(pad))
}

pub fn lpad(text: &str, w: usize) -> String {
    let t = trunc(text, w);
    let pad = w.saturating_sub(t.width());
    format!("{}{t}", " ".repeat(pad))
}

/// Cut a line of spans to `w` cells.
fn fit(spans: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let sw = s.content.width();
        if used + sw <= w {
            used += sw;
            out.push(s);
        } else {
            let room = w.saturating_sub(used);
            if room > 0 {
                out.push(Span::styled(trunc(&s.content, room), s.style));
            }
            break;
        }
    }
    Line::from(out)
}

/// Join items with two spaces, wrapping onto new lines that start with `prefix`.
fn wrap_items(
    prefix: &[Span<'static>],
    cont: &[Span<'static>],
    items: Vec<Vec<Span<'static>>>,
    w: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = prefix.to_vec();
    let mut base = width_of(prefix);
    let mut used = base;
    for item in items {
        let iw = width_of(&item);
        if used > base && used + 2 + iw > w {
            lines.push(Line::from(std::mem::replace(&mut cur, cont.to_vec())));
            base = width_of(cont);
            used = base;
        }
        if used > base {
            cur.push(Span::raw("  "));
            used += 2;
        }
        used += iw;
        cur.extend(item);
    }
    if used > base {
        lines.push(fit(cur, w));
    }
    lines
}

fn bar(theme: &Theme, pct: f64, n: usize, style: Style) -> Vec<Span<'static>> {
    let full = ((pct.clamp(0.0, 100.0) / 100.0) * n as f64).round() as usize;
    vec![
        span(theme.bar_full().repeat(full), style),
        span(theme.bar_empty().repeat(n - full), theme.dim()),
    ]
}

fn gib(bytes: u64) -> String {
    format!("{:.1}G", bytes as f64 / (1u64 << 30) as f64)
}

/// Draw a box and its content; separators cross the side borders.
fn draw_box(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    title: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    border: Style,
    content: Vec<Content>,
) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    let mut title_spans = vec![span(" ", border)];
    title_spans.extend(title);
    title_spans.push(span(" ", border));
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .border_style(border)
        .title_top(Line::from(title_spans));
    if !right.is_empty() {
        let mut r = vec![span(" ", border)];
        r.extend(right);
        r.push(span(" ", border));
        block = block.title_top(Line::from(r).right_aligned());
    }
    let inner = block.inner(area);
    block.render(area, buf);
    let (tee_l, horiz, tee_r) = theme.sep_set();
    let x = inner.x + 1;
    let w = inner.width.saturating_sub(2) as usize;
    for (i, c) in content.into_iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.bottom() {
            break;
        }
        match c {
            Content::Sep => {
                buf.set_string(area.x, y, tee_l, border);
                buf.set_string(area.x + 1, y, horiz.repeat(area.width as usize - 2), border);
                buf.set_string(area.right() - 1, y, tee_r, border);
            }
            Content::Line(line) => {
                buf.set_line(x, y, &line, w as u16);
            }
        }
    }
}

struct Cols {
    task: usize,
    project: Option<usize>,
    step: Option<usize>,
    kind: Option<usize>,
}

/// Column widths. Columns that hold a value (a duration, a PR and its checks,
/// a step with its time) are sized for the longest value they can show, so
/// they never truncate; only free text (task, project) gives way when the
/// screen is narrow.
const PREFIX: usize = 13;
const PIPE: usize = 9;
/// `#1234 ✓ 12/12`, then a space before AGE.
const PRCOL: usize = 14;
/// The longest age, `23h59m`.
const AGE: usize = 6;
/// The longest step with its time, `document 23h59m`; also `parked 23h59m`.
const STEP: usize = 15;
const PROJECT: usize = 14;
const KIND: usize = 10;
const TASK_MIN: usize = 14;
const TASK_MAX: usize = 32;

impl Cols {
    /// Value columns first, then the project and step when they fit, then
    /// the task name and kind take what is left.
    fn for_width(w: usize) -> Cols {
        let base = PREFIX + 1 + PIPE + 1 + PRCOL + AGE;
        let mut c = Cols {
            task: TASK_MIN,
            project: Some(PROJECT),
            step: Some(STEP),
            kind: None,
        };
        let used = |c: &Cols| {
            base + c.task
                + c.project.map_or(0, |p| p + 1)
                + c.step.map_or(0, |s| s + 1)
                + c.kind.map_or(0, |k| k + 3)
        };
        if used(&c) > w {
            c.project = None;
        }
        if used(&c) > w {
            c.step = None;
        }
        let mut spare = w.saturating_sub(used(&c));
        if spare >= KIND + 3 {
            c.kind = Some(KIND);
            spare -= KIND + 3;
        }
        c.task = if w < used(&c) {
            (w.saturating_sub(used(&c) - c.task)).max(10)
        } else {
            c.task + spare.min(TASK_MAX - TASK_MIN)
        };
        c
    }

    fn header(&self, theme: &Theme) -> Line<'static> {
        let mut s = format!("  {}{}", rpad("STATE", 11), rpad("TASK", self.task + 1));
        if let Some(p) = self.project {
            s.push_str(&rpad("PROJECT", p + 1));
        }
        s.push_str(&rpad("PIPELINE", PIPE + 1));
        if let Some(st) = self.step {
            s.push_str(&rpad("STEP", st + 1));
        }
        s.push_str(&rpad("PR \u{00b7} CI", PRCOL));
        s.push_str(&lpad("AGE", AGE));
        if self.kind.is_some() {
            s.push_str("   KIND");
        }
        Line::from(span(
            s,
            theme.dim().add_modifier(ratatui::style::Modifier::BOLD),
        ))
    }
}

fn pr_number(url: &str) -> Option<String> {
    crate::sources::gh::parse_pr_url(url).map(|(_, _, n)| format!("#{n}"))
}

fn pr_cell(theme: &Theme, row: &Row, w: usize) -> Vec<Span<'static>> {
    let Some(url) = row.pr_url().filter(|_| row.state != WorkerState::Idle) else {
        return vec![span(rpad("-", w), theme.dim())];
    };
    let num = pr_number(url).unwrap_or_else(|| "PR".into());
    let Some(pr) = row.pr else {
        return vec![span(rpad(&num, w), theme.bold())];
    };
    let (glyph, style, text) = pr_summary(theme, pr);
    let tail = format!(" {glyph} {text}");
    vec![
        span(num.clone(), theme.bold()),
        span(rpad(&tail, w.saturating_sub(num.width())), style),
    ]
}

fn pr_summary(theme: &Theme, pr: &PrChecks) -> (&'static str, Style, String) {
    let total = pr.checks.len();
    let pass = pr.count(CheckState::Pass) + pr.count(CheckState::Skipped);
    let state = if pr.state == "MERGED" {
        return (
            theme.check(CheckState::Pass).0,
            theme.good(),
            "merged".into(),
        );
    } else if pr.draft {
        return ("-", theme.dim(), "draft".into());
    } else if total == 0 {
        return ("-", theme.dim(), "none".into());
    } else if pr.count(CheckState::Fail) > 0 {
        CheckState::Fail
    } else if pass == total {
        CheckState::Pass
    } else {
        CheckState::Running
    };
    let (g, s) = theme.check(state);
    (g, s, format!("{pass}/{total}"))
}

fn step_label(row: &Row, now: i64) -> String {
    if let Some(p) = row.pipeline {
        if let Some(parked) = p.parked_for(now) {
            return format!("parked {}", timefmt::dur(parked));
        }
        if p.is_parked() {
            // Older no-mistakes does not say how long: name the gate instead
            // of the raw status, which is longer than the column.
            return p
                .gate_step()
                .map_or("at a gate".into(), |g| format!("{g} gate"));
        }
        if let Some(a) = &p.active {
            return match p.active_for(now) {
                Some(s) => format!("{} {}", a.step, timefmt::dur(s)),
                None => a.step.clone(),
            };
        }
        return p.outcome.clone().unwrap_or_else(|| p.status.clone());
    }
    match row.worker.kind.as_str() {
        "scout" => "scout".into(),
        _ if row.worker.mode == "no-mistakes" => "not run".into(),
        _ => String::new(),
    }
}

fn pipe_cells(theme: &Theme, row: &Row) -> Vec<Span<'static>> {
    match row.pipeline {
        Some(p) if !p.steps.is_empty() => {
            let mut v: Vec<Span> = p
                .steps
                .iter()
                .take(PIPE)
                .map(|s| {
                    let (g, st) = theme.step(s.status);
                    span(g, st)
                })
                .collect();
            if p.steps.len() < PIPE {
                v.push(span(" ".repeat(PIPE - p.steps.len()), Style::new()));
            }
            v
        }
        _ if row.worker.mode == "no-mistakes" && row.worker.parent.is_none() => {
            vec![span(theme.dot().repeat(PIPE), theme.dim())]
        }
        _ => vec![span(" ".repeat(PIPE), Style::new())],
    }
}

fn now_text(row: &Row, now: i64) -> String {
    let w = row.worker;
    let note = w
        .last_event
        .as_ref()
        .map(|e| e.note.trim().to_string())
        .filter(|n| !n.is_empty());
    match row.state {
        WorkerState::Captain => {
            let keys = w.open_decisions.join(", ");
            match note {
                Some(n) => format!("needs you ({keys}): {n}"),
                None => format!("needs you ({keys})"),
            }
        }
        WorkerState::Stale => {
            let reason = row
                .stale
                .as_ref()
                .map(|s| s.reason.clone())
                .unwrap_or_default();
            match note {
                Some(n) => format!("{reason}; last: {n}"),
                None => reason,
            }
        }
        _ => {
            if let Some(p) = row.pipeline {
                if let Some(a) = &p.active {
                    let ago = p
                        .quiet_for(now)
                        .map(|s| format!(" ({} ago)", timefmt::dur(s)))
                        .unwrap_or_default();
                    return format!("nm {} {}: {}{ago}", a.step, a.round, a.last_activity)
                        .replace("  ", " ");
                }
                if p.is_parked() {
                    let gate = p.gate_step().map(|g| format!("{g} ")).unwrap_or_default();
                    let parked = p
                        .parked_for(now)
                        .map(|s| format!(", parked {}", timefmt::dur(s)))
                        .unwrap_or_default();
                    return format!("nm {gate}gate: findings {}{parked}", p.findings);
                }
                if let Some(err) = &p.error {
                    return format!("nm {}: {err}", p.status);
                }
            }
            if let Some(until) = w.wait_until.filter(|u| *u > now) {
                let when = timefmt::short_when(until, now);
                return match note.as_deref().and_then(timefmt::until_iso) {
                    // The note already says when; show that time locally.
                    Some(iso) => note.as_deref().unwrap_or_default().replace(iso, &when),
                    None => format!("waiting until {when}: {}", note.unwrap_or_default()),
                };
            }
            match (row.state, note) {
                (WorkerState::Working, _) if w.source == "pane" && w.detail.contains("busy") => {
                    format!(
                        "{} (agent busy)",
                        w.last_event
                            .as_ref()
                            .map(|e| e.note.trim())
                            .filter(|n| !n.is_empty())
                            .unwrap_or("working")
                    )
                }
                (_, Some(n)) if !matches!(row.state, WorkerState::Idle | WorkerState::Unknown) => n,
                _ => w.detail.clone(),
            }
        }
    }
}

/// Rows that changed this recently are drawn as a block of reverse video.
pub const FRESH_SECS: i64 = 10;

/// `changed 3m ago`, highlighted while the change is fresh.
fn changed_tag(t: &Theme, ago: i64) -> (String, Style) {
    let text = format!("changed {} ago", timefmt::dur(ago));
    let style = if ago < FRESH_SECS { t.fresh() } else { t.dim() };
    (text, style)
}

fn roster_row(view: &View, row: &Row, cols: &Cols, selected: bool, w: usize) -> Vec<Line<'static>> {
    let t = &view.theme;
    let wk = row.worker;
    let mut l1: Vec<Span<'static>> = Vec::new();
    l1.push(span(
        if selected {
            format!("{} ", t.cursor())
        } else {
            "  ".into()
        },
        t.bold(),
    ));
    let badge = format!("{} {}", t.state_glyph(row.state), row.state.word());
    l1.push(span(rpad(&badge, 10), t.state(row.state)));
    l1.push(span(" ", Style::new()));
    let name = match &wk.parent {
        Some(p) => format!("{p}/{}", wk.id),
        None => wk.id.clone(),
    };
    let name_style = if selected || row.state == WorkerState::Captain {
        t.bold()
    } else {
        t.plain()
    };
    l1.push(span(rpad(&name, cols.task), name_style));
    l1.push(span(" ", Style::new()));
    if let Some(p) = cols.project {
        let st = if matches!(row.state, WorkerState::Idle | WorkerState::Unknown) {
            t.dim()
        } else {
            t.plain()
        };
        l1.push(span(rpad(&wk.project, p), st));
        l1.push(span(" ", Style::new()));
    }
    l1.extend(pipe_cells(t, row));
    l1.push(span(" ", Style::new()));
    if let Some(s) = cols.step {
        let parked_late = row
            .stale
            .as_ref()
            .is_some_and(|s| s.kind == StaleKind::Parked);
        l1.push(span(
            rpad(&step_label(row, view.snap.now), s),
            if parked_late { t.warn() } else { t.dim() },
        ));
        l1.push(span(" ", Style::new()));
    }
    l1.extend(pr_cell(t, row, PRCOL));
    let age = wk
        .last_event
        .as_ref()
        .and_then(|e| e.age(view.snap.now))
        .map(timefmt::dur)
        .unwrap_or_else(|| "-".into());
    l1.push(span(lpad(&age, AGE), t.dim()));
    if let Some(k) = cols.kind {
        l1.push(span(format!("   {}", rpad(&wk.kind, k)), t.dim()));
    }
    let style = match row.state {
        WorkerState::Captain => t.captain(),
        WorkerState::Stale => t.fg(ratatui::style::Color::Yellow),
        WorkerState::Blocked | WorkerState::Failed => t.fg(ratatui::style::Color::Red),
        _ => t.dim(),
    };
    // The row's "changed Ns ago" sits at the right edge, so a quiet fleet
    // still shows when each worker last did something.
    let tag = view
        .app
        .changed_ago(&wk.id, view.snap.now)
        .map(|a| changed_tag(t, a));
    let tag_w = tag.as_ref().map_or(0, |(text, _)| text.width() + 1);
    let note_w = w.saturating_sub(PREFIX + tag_w);
    let mut l2 = vec![
        span(" ".repeat(PREFIX), Style::new()),
        span(rpad(&now_text(row, view.snap.now), note_w), style),
    ];
    if let Some((text, style)) = tag {
        l2.push(span(" ", Style::new()));
        l2.push(span(text, style));
    }
    let mut out = vec![fit(l1, w), fit(l2, w)];
    if view.app.expanded.contains(&wk.id) {
        out.extend(drawer(view, row, w));
    }
    out
}

fn drawer(view: &View, row: &Row, w: usize) -> Vec<Line<'static>> {
    let t = &view.theme;
    let now = view.snap.now;
    let prefix = vec![
        span(" ".repeat(PREFIX), Style::new()),
        span(format!("{} ", t.rail()), t.dim()),
    ];
    let lab = |s: &str| span(format!("{s} "), t.dim());
    let mut out = Vec::new();
    let wk = row.worker;
    let mut head = prefix.clone();
    if let Some(b) = &wk.branch {
        head.extend([
            lab("branch"),
            span(b.clone(), t.plain()),
            span("  ", Style::new()),
        ]);
    }
    if let Some(p) = row.pipeline {
        head.extend([
            lab("run"),
            span(p.run_id.chars().take(8).collect::<String>(), t.plain()),
            span("  ", Style::new()),
            lab("head"),
            span(p.head.clone(), t.plain()),
            span("  ", Style::new()),
            lab("run status"),
            span(p.status.clone(), t.plain()),
        ]);
        if let Some(a) = p.active.as_ref().filter(|a| !a.round.is_empty()) {
            // no-mistakes already words it as `round 2`.
            head.extend([span("  ", Style::new()), span(a.round.clone(), t.plain())]);
        }
    } else {
        head.extend([
            lab(&wk.kind),
            lab(&wk.harness),
            span(wk.detail.clone(), t.dim()),
        ]);
    }
    out.push(fit(head, w));
    if let Some(p) = row.pipeline {
        let items = p
            .steps
            .iter()
            .map(|s| {
                let (g, st) = t.step(s.status);
                let d = if s.status == StepStatus::Running || s.status == StepStatus::Fixing {
                    p.active
                        .as_ref()
                        .filter(|a| a.step == s.name)
                        .and_then(|_| p.active_for(now))
                        .map(timefmt::dur)
                        .unwrap_or_default()
                } else if s.duration_ms > 0 {
                    timefmt::dur_ms(s.duration_ms)
                } else {
                    String::new()
                };
                let mut v = vec![span(format!("{} ", s.name), t.plain()), span(g, st)];
                if s.findings > 0 {
                    v.push(span(format!(" {}f", s.findings), t.warn()));
                }
                if !d.is_empty() {
                    v.push(span(format!(" {d}"), t.dim()));
                }
                v
            })
            .collect();
        out.extend(wrap_items(&prefix, &prefix, items, w));
        let mut f = prefix.clone();
        f.push(lab("findings"));
        let fs = if p.findings == "none" {
            t.good()
        } else {
            t.warn()
        };
        f.push(span(p.findings.clone(), fs));
        if let Some(parked) = p.parked_for(now) {
            let late = parked >= view.app.thresholds.parked_secs;
            f.extend([
                span("  ", Style::new()),
                lab("parked"),
                span(
                    format!(
                        "{}{}",
                        p.gate_step()
                            .map(|g| format!("at {g} "))
                            .unwrap_or_default(),
                        timefmt::dur(parked)
                    ),
                    if late { t.warn() } else { t.plain() },
                ),
            ]);
        }
        if let Some(e) = &p.error {
            f.extend([
                span("  ", Style::new()),
                lab("error"),
                span(e.clone(), t.bad()),
            ]);
        }
        out.push(fit(f, w));
    }
    if let Some(pr) = row.pr {
        let mut pfx = prefix.clone();
        pfx.push(lab(&format!("PR #{} checks", pr.number)));
        let items = pr
            .checks
            .iter()
            .map(|c| {
                let (g, st) = t.check(c.state);
                let mut text = format!("{g} {}", c.name);
                if c.state == CheckState::Running
                    && let Some(s) = c.started_at
                {
                    text.push_str(&format!(" {}", timefmt::dur(now - s)));
                }
                vec![span(text, st)]
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            let mut l = pfx;
            l.push(span("none reported", t.dim()));
            out.push(fit(l, w));
        } else {
            let mut cont = prefix.clone();
            cont.push(span(
                " ".repeat(width_of(&pfx) - width_of(&prefix)),
                Style::new(),
            ));
            out.extend(wrap_items(&pfx, &cont, items, w));
        }
    }
    if let Some(e) = &wk.last_event {
        let mut l = prefix.clone();
        let ago = e
            .age(now)
            .map(|a| format!("{} ago", timefmt::dur(a)))
            .unwrap_or_default();
        l.push(lab(&format!("last status {ago}")));
        l.push(span(format!("{}: {}", e.state, e.note.trim()), t.plain()));
        out.push(fit(l, w));
    }
    if let Some(u) = wk.wait_until {
        let mut l = prefix.clone();
        l.push(lab("declared wait until"));
        l.push(span(
            timefmt::short_when(u, now),
            if u < now { t.warn() } else { t.plain() },
        ));
        out.push(fit(l, w));
    }
    if let Some(dir) = &wk.worktree {
        let mut l = prefix;
        l.push(lab("worktree"));
        l.push(span(dir.display().to_string(), t.dim()));
        out.push(fit(l, w));
    }
    out
}

/// The header's counts. `level` 0 is the full form; 1 drops the age of the
/// oldest held call; 2 drops the breakdown of what is stale; 3 also drops an
/// empty `done` count and tightens the gaps. [`summary`] picks the fullest
/// level that fits.
fn summary_at(view: &View, rows: &[Row], level: u8) -> Vec<Span<'static>> {
    let t = &view.theme;
    let count = |s: &[WorkerState]| rows.iter().filter(|r| s.contains(&r.state)).count();
    let mut v: Vec<Span<'static>> = Vec::new();
    let add = |v: &mut Vec<Span<'static>>, text: String, style: Style| {
        if !v.is_empty() {
            v.push(span(if level >= 3 { "  " } else { "   " }, Style::new()));
        }
        v.push(span(text, style));
    };
    let asking = count(&[WorkerState::Captain]);
    if asking > 0 {
        add(
            &mut v,
            format!(" ? {asking} asking you "),
            t.state(WorkerState::Captain),
        );
    }
    let blocked = count(&[WorkerState::Blocked, WorkerState::Failed]);
    if blocked > 0 {
        add(
            &mut v,
            format!("{} {blocked} blocked", t.state_glyph(WorkerState::Blocked)),
            t.bad(),
        );
    }
    let stale = count(&[WorkerState::Stale]);
    if stale > 0 {
        let of = |k: StaleKind| {
            rows.iter()
                .filter(|r| r.stale.as_ref().is_some_and(|s| s.kind == k))
                .count()
        };
        let mut text = format!("! {stale} stale");
        let parts: Vec<String> = [
            (of(StaleKind::Parked), "parked"),
            (of(StaleKind::WaitPassed), "wait over"),
            (of(StaleKind::Quiet), "quiet"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        if level < 2 && !parts.is_empty() {
            text.push_str(&format!(" ({})", parts.join(", ")));
        }
        add(&mut v, text, t.warn());
    }
    let gate = count(&[WorkerState::Gate]);
    if gate > 0 {
        add(
            &mut v,
            format!("{} {gate} at gate", t.state_glyph(WorkerState::Gate)),
            t.state(WorkerState::Gate),
        );
    }
    add(
        &mut v,
        format!(
            "{} {} working",
            t.state_glyph(WorkerState::Working),
            count(&[WorkerState::Working, WorkerState::Paused])
        ),
        t.state(WorkerState::Working),
    );
    if level < 3 || count(&[WorkerState::Done]) > 0 {
        add(
            &mut v,
            format!(
                "{} {} done",
                t.state_glyph(WorkerState::Done),
                count(&[WorkerState::Done])
            ),
            t.good(),
        );
    }
    add(
        &mut v,
        format!(
            "{} {} idle",
            t.state_glyph(WorkerState::Idle),
            count(&[WorkerState::Idle, WorkerState::Unknown])
        ),
        t.dim(),
    );
    if let Some(f) = &view.snap.fleet
        && !f.held.is_empty()
    {
        let oldest = f.held.iter().filter_map(|h| h.age_days).max();
        let mut text = format!("{} {} held calls", t.flag(), f.held.len());
        if let (Some(d), true) = (oldest, level < 1) {
            text.push_str(&format!(" (oldest {d}d)"));
        }
        add(&mut v, text, t.captain());
    }
    if let Some(h) = &view.snap.host {
        let swap = h.swap_used_pct();
        let mem = h.mem_used_pct();
        if mem >= 85.0 {
            add(
                &mut v,
                format!("{} mem {:.0}%", t.warn_glyph(), mem),
                t.warn(),
            );
        } else if swap >= 50.0 {
            add(
                &mut v,
                format!("{} swap {:.0}%", t.warn_glyph(), swap),
                t.warn(),
            );
        }
    }
    v
}

fn summary(view: &View, rows: &[Row], w: usize) -> Vec<Span<'static>> {
    let mut v = summary_at(view, rows, 0);
    for level in 1..=3 {
        if width_of(&v) <= w {
            break;
        }
        v = summary_at(view, rows, level);
    }
    v
}

fn age_of(snap: &Snapshot, tier: &str) -> Option<i64> {
    snap.fresh
        .get(tier)
        .and_then(|f| f.ok_at)
        .map(|t| snap.now - t)
}

/// When the fleet was last read and when something last changed in it, so a
/// quiet fleet reads as up to date rather than frozen:
/// `↻ read 4s ago · changed 41m ago`.
fn freshness_cue(view: &View, short: bool) -> (String, Style) {
    let t = &view.theme;
    let now = view.snap.now;
    // The fleet snapshot is what the rows are made of; the footer has the
    // other tiers. A fleet read that is stuck must not hide behind them.
    let Some(read) = age_of(view.snap, "fleet") else {
        return (String::new(), t.dim());
    };
    let changed = view.app.last_change_ago(now).map(timefmt::dur);
    let overdue = read > view.app.read_overdue_secs && !view.app.paused;
    let mark = if overdue {
        format!("{} ", t.warn_glyph())
    } else {
        String::new()
    };
    let text = match (short, changed) {
        (false, Some(c)) => format!(
            " {mark}{} read {} ago \u{b7} changed {c} ago",
            t.refresh(),
            timefmt::dur(read)
        ),
        (false, None) => format!(" {mark}{} read {} ago", t.refresh(), timefmt::dur(read)),
        (true, Some(c)) => format!(
            " {mark}{} {} \u{b7} {}{c}",
            t.refresh(),
            timefmt::dur(read),
            t.g("\u{394}", "chg ")
        ),
        (true, None) => format!(" {mark}{} {}", t.refresh(), timefmt::dur(read)),
    };
    (text, if overdue { t.warn() } else { t.dim() })
}

fn roster(view: &View, buf: &mut Buffer, area: Rect, host: &str) {
    let t = &view.theme;
    let w = area.width.saturating_sub(4) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let rows = visible_rows(view.snap, view.app);
    let all = crate::app::all_rows(view.snap, &view.app.thresholds);
    let mut top: Vec<Content> = vec![Content::Line(fit(summary(view, &all, w), w))];
    if let Some(err) = view.snap.fresh.get("fleet").and_then(|f| f.error.clone()) {
        let shown = if view.snap.fleet.is_some() {
            format!("! fleet: {err} (showing the last good snapshot)")
        } else {
            format!("! fleet: {err}")
        };
        top.push(Content::Line(fit(vec![span(shown, t.warn())], w)));
    }
    top.push(Content::Sep);
    let cols = Cols::for_width(w);
    top.push(Content::Line(cols.header(t)));

    let mut bottom: Vec<Content> = Vec::new();
    if let Some(next) = view.snap.fleet.as_ref().and_then(|f| f.next.clone()) {
        bottom.push(Content::Sep);
        bottom.push(Content::Line(fit(
            vec![
                span("next ", t.dim()),
                span(next.id.clone(), t.bold()),
                span(format!(" - {}", next.title), t.dim()),
            ],
            w,
        )));
    }

    let avail = inner_h.saturating_sub(top.len() + bottom.len());
    let sel = view.app.selected_index(&rows);
    let blocks: Vec<Vec<Line<'static>>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| roster_row(view, r, &cols, Some(i) == sel, w))
        .collect();
    let mut body: Vec<Line<'static>> = Vec::new();
    if view.snap.fleet.is_none() {
        body.push(Line::from(span(
            "  reading the fleet (fm-fleet-snapshot.sh --json)...",
            t.dim(),
        )));
    } else if rows.is_empty() {
        let msg = match view.app.filter {
            crate::app::Filter::All => "  no workers in flight",
            crate::app::Filter::Active => "  nothing active (press f to change the filter)",
            crate::app::Filter::NeedsYou => "  nothing needs you (press f to change the filter)",
        };
        body.push(Line::from(span(msg, t.dim())));
    } else {
        // Scroll so the selected row, with its drawer, is visible.
        let mut start = 0;
        if let Some(s) = sel {
            let upto: usize = blocks[..=s].iter().map(Vec::len).sum();
            if upto > avail {
                let mut acc = upto;
                while start < s && acc > avail {
                    acc -= blocks[start].len();
                    start += 1;
                }
            }
        }
        let mut hidden_below = 0;
        for (i, b) in blocks.iter().enumerate().skip(start) {
            if body.len() + b.len() <= avail {
                body.extend(b.iter().cloned());
            } else if body.is_empty() {
                body.extend(b.iter().take(avail).cloned());
                hidden_below = blocks.len() - i - 1;
                break;
            } else {
                hidden_below = blocks.len() - i;
                break;
            }
        }
        if hidden_below > 0 || start > 0 {
            let note = match (start, hidden_below) {
                (0, n) => format!("  {n} more below"),
                (s, 0) => format!("  {s} more above"),
                (s, n) => format!("  {s} more above, {n} below"),
            };
            if body.len() >= avail && !body.is_empty() {
                body.pop();
            }
            body.push(Line::from(span(note, t.dim())));
        }
    }
    let pad = avail.saturating_sub(body.len());
    let mut content = top;
    content.extend(body.into_iter().map(Content::Line));
    content.extend((0..pad).map(|_| Content::Line(Line::default())));
    content.extend(bottom);

    let mut title = vec![span("crowsnest", t.title())];
    if !host.is_empty() {
        title.push(span(format!(" firstmate @ {host}"), t.dim()));
    }
    // The long cue when it fits beside the title with room to spare.
    let room =
        (area.width as usize).saturating_sub(width_of(&title) + timefmt::clock(0).width() + 12);
    let (mut cue, mut cue_style) = freshness_cue(view, false);
    if cue.width() > room {
        (cue, cue_style) = freshness_cue(view, true);
    }
    draw_box(
        buf,
        area,
        t,
        title,
        vec![
            span(timefmt::clock(view.snap.now), t.bold()),
            span(cue, cue_style),
        ],
        t.border(),
        content,
    );
}

fn runner_panel(view: &View, buf: &mut Buffer, area: Rect, with_jobs: bool) {
    let t = &view.theme;
    let w = area.width.saturating_sub(4) as usize;
    let snap = view.snap;
    let queued = |repo: &Option<String>| {
        snap.jobs
            .iter()
            .filter(|j| j.status == "queued" && repo.as_deref() == Some(j.repo.as_str()))
            .count()
    };
    let mut lines: Vec<Content> = Vec::new();
    if snap.runners.is_empty() {
        lines.push(Content::Line(Line::from(span(
            "no self-hosted runners found",
            t.dim(),
        ))));
    }
    let label = |r: &crate::model::Runner| -> String {
        r.repo
            .as_deref()
            .and_then(|repo| repo.rsplit('/').next())
            .map(str::to_string)
            .unwrap_or_else(|| r.name.clone())
    };
    let name_w = snap
        .runners
        .iter()
        .map(|r| label(r).width())
        .max()
        .unwrap_or(0)
        .min(w.saturating_sub(14))
        .max(6);
    for r in &snap.runners {
        let q = queued(&r.repo);
        let mut l = if r.busy {
            vec![
                span(
                    format!("{} ", t.busy()),
                    t.fg(ratatui::style::Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                span(rpad(&label(r), name_w), t.bold()),
                span(" busy", t.fg(ratatui::style::Color::Cyan)),
                span(
                    r.busy_secs
                        .map(|s| format!(" {}", timefmt::dur(s)))
                        .unwrap_or_default(),
                    t.plain(),
                ),
            ]
        } else {
            vec![
                span(format!("{} ", t.idle()), t.dim()),
                span(rpad(&label(r), name_w), t.plain()),
                span(" idle", t.dim()),
            ]
        };
        if q > 0 {
            l.push(span(format!(" q{q}"), t.warn()));
        }
        lines.push(Content::Line(fit(l, w)));
    }
    if with_jobs {
        // Group by repo (first-seen order), each repo's running jobs before its
        // queued ones, and name the project on every line so a job can never
        // read as belonging to the runner row above it.
        let mut repos: Vec<&str> = Vec::new();
        for j in &snap.jobs {
            if !repos.contains(&j.repo.as_str()) {
                repos.push(&j.repo);
            }
        }
        for repo_full in repos {
            let repo = repo_full.rsplit('/').next().unwrap_or(repo_full);
            let running = |running: bool| {
                snap.jobs
                    .iter()
                    .filter(move |j| j.repo == repo_full && (j.status == "in_progress") == running)
            };
            for j in running(true).chain(running(false)) {
                let line = if j.status == "in_progress" {
                    let d = j
                        .started_at
                        .map(|s| format!(" {}", timefmt::dur(snap.now - s)))
                        .unwrap_or_default();
                    vec![
                        span(format!("{repo} "), t.bold()),
                        span(
                            format!("{} {}", t.step(StepStatus::Running).0, j.name),
                            t.fg(ratatui::style::Color::Cyan),
                        ),
                        span(d, t.dim()),
                    ]
                } else {
                    vec![
                        span(format!("{repo} "), t.bold()),
                        span(
                            format!("{} queued ", t.idle()),
                            t.fg(ratatui::style::Color::Yellow),
                        ),
                        span(j.name.clone(), t.dim()),
                    ]
                };
                lines.push(Content::Line(fit(line, w)));
            }
        }
    }
    let total: usize = snap.jobs.iter().filter(|j| j.status == "queued").count();
    let right = if total > 0 {
        vec![span(format!("queue {total}"), t.warn())]
    } else {
        vec![]
    };
    draw_box(
        buf,
        area,
        t,
        vec![span("RUNNERS", t.bold())],
        right,
        t.border(),
        lines,
    );
}

fn host_panel(view: &View, buf: &mut Buffer, area: Rect) {
    let t = &view.theme;
    let w = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Content> = Vec::new();
    if let Some(h) = &view.snap.host {
        let n = w.saturating_sub(22).clamp(4, 16);
        lines.push(Content::Line(fit(
            vec![
                span("load ", t.plain()),
                span(format!("{:.2}", h.load[0]), t.bold()),
                span(format!(" {:.2} {:.2}", h.load[1], h.load[2]), t.dim()),
                span(format!("  {} cpu", h.cpus), t.dim()),
            ],
            w,
        )));
        let mem = h.mem_used_pct();
        let mut l = vec![span("mem  ", t.plain())];
        l.extend(bar(t, mem, n, t.used(mem)));
        l.push(span(format!(" {:>3.0}%", mem), t.plain()));
        l.push(span(format!(" {} avail", gib(h.mem_available)), t.dim()));
        lines.push(Content::Line(fit(l, w)));
        if h.swap_total > 0 {
            let swap = h.swap_used_pct();
            let mut l = vec![span("swap ", t.plain())];
            l.extend(bar(t, swap, n, t.used(swap)));
            let warn = if swap >= 50.0 { t.warn_glyph() } else { "" };
            l.push(span(
                format!(" {:>3.0}%{warn}", swap),
                if swap >= 50.0 { t.warn() } else { t.plain() },
            ));
            l.push(span(format!(" {} free", gib(h.swap_free)), t.dim()));
            lines.push(Content::Line(fit(l, w)));
        }
        let mut d = vec![span("disk", t.plain())];
        for (i, disk) in h.disks.iter().enumerate() {
            let gap = if i == 0 { " " } else { "  " };
            d.push(span(
                format!("{gap}{} {:.0}%", disk.mount, disk.used_pct),
                t.used(disk.used_pct),
            ));
        }
        lines.push(Content::Line(fit(d, w)));
        if h.psi_mem.is_some() || h.psi_cpu.is_some() {
            lines.push(Content::Line(fit(
                vec![
                    span("psi ", t.plain()),
                    span(
                        format!(
                            " mem {:.1}%  cpu {:.1}%",
                            h.psi_mem.unwrap_or(0.0),
                            h.psi_cpu.unwrap_or(0.0)
                        ),
                        t.dim(),
                    ),
                ],
                w,
            )));
        }
    }
    draw_box(
        buf,
        area,
        t,
        vec![span("HOST", t.bold())],
        vec![],
        t.border(),
        lines,
    );
}

fn usage_panel(view: &View, buf: &mut Buffer, area: Rect) {
    let t = &view.theme;
    let w = area.width.saturating_sub(4) as usize;
    let rows = area.height.saturating_sub(2) as usize;
    let q = &view.snap.quota;
    let one_provider = q
        .iter()
        .all(|x| x.provider == q.first().map(|f| f.provider.clone()).unwrap_or_default());
    let mut lines: Vec<Content> = Vec::new();
    if q.is_empty() {
        let msg = match view.snap.fresh.get("quota").and_then(|f| f.error.clone()) {
            Some(e) => trunc(&e, w),
            None => "no usage windows reported".into(),
        };
        lines.push(Content::Line(Line::from(span(msg, t.dim()))));
    }
    // Scarcest windows first, so a nearly spent one is never hidden.
    let mut q: Vec<&crate::model::QuotaWindow> = q.iter().collect();
    q.sort_by(|a, b| a.pct_remaining.total_cmp(&b.pct_remaining));
    let n = 6;
    let with_reset = w >= 44;
    let label_w = w
        .saturating_sub(n + 6 + if with_reset { 13 } else { 0 })
        .clamp(6, 24);
    let shown = if q.len() > rows {
        rows.saturating_sub(1)
    } else {
        q.len()
    };
    for win in q.iter().take(shown) {
        let label = if one_provider {
            win.label.clone()
        } else {
            format!("{} {}", win.provider, win.label)
        };
        let mut l = vec![
            span(rpad(&label, label_w), t.plain()),
            span(" ", Style::new()),
        ];
        l.extend(bar(t, win.pct_remaining, n, t.remaining(win.pct_remaining)));
        l.push(span(format!(" {:>3.0}%", win.pct_remaining), t.plain()));
        if let (Some(r), true) = (win.resets_at, with_reset) {
            l.push(span(
                format!(
                    "  {} {}",
                    t.refresh(),
                    timefmt::short_when(r, view.snap.now)
                ),
                t.dim(),
            ));
        }
        lines.push(Content::Line(fit(l, w)));
    }
    if q.len() > shown {
        lines.push(Content::Line(Line::from(span(
            format!("+{} more windows", q.len() - shown),
            t.dim(),
        ))));
    }
    draw_box(
        buf,
        area,
        t,
        vec![span("USAGE", t.bold())],
        vec![],
        t.border(),
        lines,
    );
}

/// Backlog items held for the captain, oldest first (wide layout only).
fn held_panel(view: &View, buf: &mut Buffer, area: Rect) {
    let t = &view.theme;
    let w = area.width.saturating_sub(4) as usize;
    let rows = area.height.saturating_sub(2) as usize;
    let held = view
        .snap
        .fleet
        .as_ref()
        .map(|f| f.held.clone())
        .unwrap_or_default();
    let mut lines = Vec::new();
    if held.is_empty() {
        lines.push(Content::Line(Line::from(span(
            "nothing held for you",
            t.dim(),
        ))));
    }
    // Leave a row for the hint, and one for "+N more" when the list is cut.
    let room = rows.saturating_sub(1);
    let shown = if held.len() > room {
        room.saturating_sub(1)
    } else {
        held.len()
    };
    for h in held.iter().take(shown) {
        let age = h.age_days.map(|d| format!("{d}d")).unwrap_or_default();
        let l = vec![
            span(format!("{} ", t.flag()), t.captain()),
            span(rpad(&h.id, w.saturating_sub(6)), t.plain()),
            span(lpad(&age, 4), t.dim()),
        ];
        lines.push(Content::Line(fit(l, w)));
    }
    if shown < held.len() {
        let more = format!("+{} more", held.len() - shown);
        lines.push(Content::Line(Line::from(span(more, t.dim()))));
    }
    if !held.is_empty() && rows > lines.len() {
        let hint = "answer these in chat with firstmate";
        lines.push(Content::Line(Line::from(span(trunc(hint, w), t.dim()))));
    }
    let count = if held.is_empty() {
        String::new()
    } else {
        format!(" {}", held.len())
    };
    draw_box(
        buf,
        area,
        t,
        vec![span(
            format!("HELD CALLS{count}"),
            t.captain().add_modifier(ratatui::style::Modifier::BOLD),
        )],
        vec![],
        t.border(),
        lines,
    );
}

fn footer(view: &View, buf: &mut Buffer, area: Rect) {
    let t = &view.theme;
    let keys = [
        (t.g("\u{2191}\u{2193}", "j/k"), "select"),
        (t.g("\u{23ce}", "ret"), "detail"),
        ("f", "filter"),
        ("r", "refresh"),
        ("p", "pause"),
        ("?", "help"),
        ("q", "quit"),
    ];
    let mut left = Vec::new();
    for (k, v) in keys {
        left.push(span(format!(" {k} "), t.key()));
        left.push(span(format!(" {v} "), t.dim()));
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if view.app.filter != crate::app::Filter::All {
        right.push(span(
            format!("filter: {}  ", view.app.filter.label()),
            t.bold(),
        ));
    }
    if view.app.paused {
        right.push(span("PAUSED  ", t.warn()));
    }
    for tier in ["nm", "gh", "quota"] {
        let Some(f) = view.snap.fresh.get(tier) else {
            continue;
        };
        if f.error.is_some() {
            right.push(span(format!("{tier} ! "), t.warn()));
        } else if let Some(a) = age_of(view.snap, tier) {
            right.push(span(format!("{tier} {} ", timefmt::dur(a)), t.dim()));
        }
    }
    let rw = width_of(&right);
    let w = area.width as usize;
    let mut spans = fit(left, w.saturating_sub(rw + 1)).spans;
    let lw = width_of(&spans);
    spans.push(span(" ".repeat(w.saturating_sub(lw + rw)), Style::new()));
    spans.extend(right);
    buf.set_line(area.x, area.y, &Line::from(spans), area.width);
}

fn help(view: &View, buf: &mut Buffer, area: Rect) {
    let t = &view.theme;
    let mut lines: Vec<Content> = Vec::new();
    let key = |k: &str, v: &str| {
        Content::Line(Line::from(vec![
            span(rpad(k, 14), t.bold()),
            span(v.to_string(), t.plain()),
        ]))
    };
    lines.push(key("up/down j/k", "select a worker (g/G: first/last)"));
    lines.push(key("enter / l", "open or close its detail"));
    lines.push(key("esc / h", "close detail"));
    lines.push(key("f", "filter: all, active, needs you"));
    lines.push(key("r", "refresh everything now"));
    lines.push(key("p", "pause or resume automatic refresh"));
    lines.push(key("q / ctrl-c", "quit"));
    lines.push(Content::Sep);
    for s in [
        (WorkerState::Captain, "it has an open decision for you"),
        (WorkerState::Blocked, "it reported it is blocked"),
        (WorkerState::Failed, "it or its pipeline failed"),
        (
            WorkerState::Stale,
            "quiet, parked at a gate, or its wait passed",
        ),
        (WorkerState::Gate, "its no-mistakes run is parked at a gate"),
        (WorkerState::Working, "working"),
        (
            WorkerState::Paused,
            "paused, for example on a declared wait",
        ),
        (WorkerState::Done, "reported done"),
        (WorkerState::Unknown, "firstmate cannot tell"),
        (WorkerState::Idle, "nothing in flight"),
    ] {
        lines.push(Content::Line(Line::from(vec![
            span(
                rpad(&format!("{} {}", t.state_glyph(s.0), s.0.word()), 14),
                t.state(s.0),
            ),
            span(s.1, t.plain()),
        ])));
    }
    lines.push(Content::Sep);
    for text in [
        "Each row ends with when it changed; a block means just now.",
        "The header: when the fleet was last read, and last changed.",
    ] {
        lines.push(Content::Line(Line::from(span(text, t.dim()))));
    }
    lines.push(Content::Sep);
    lines.push(Content::Line(Line::from(span(
        "crowsnest only reads. Act on the fleet by talking to firstmate.",
        t.dim(),
    ))));
    lines.push(Content::Line(Line::from(span(
        "Press any key to close.",
        t.dim(),
    ))));
    let w = 72.min(area.width.saturating_sub(4));
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    Clear.render(r, buf);
    draw_box(
        buf,
        r,
        t,
        vec![span("HELP", t.title())],
        vec![],
        t.focus_border(),
        lines,
    );
}

/// Draw one frame.
pub fn draw(frame: &mut Frame, view: &View, host: &str) {
    let area = frame.area();
    render(frame.buffer_mut(), area, view, host);
}

/// Render into any buffer (used by the TestBackend snapshot tests).
pub fn render(buf: &mut Buffer, area: Rect, view: &View, host: &str) {
    let [main, foot] = Layout::vertical([Constraint::Min(6), Constraint::Length(1)]).areas(area);
    if area.width >= WIDE {
        let [left, right] =
            Layout::horizontal([Constraint::Min(60), Constraint::Length(RIGHT_COL)]).areas(main);
        roster(view, buf, left, host);
        let runner_h = (view.snap.runners.len().max(1) + view.snap.jobs.len() + 2) as u16;
        let usage_h = (view.snap.quota.len().max(1) + 2) as u16;
        let [r1, r2, r3, r4] = Layout::vertical([
            Constraint::Length(runner_h.min(right.height / 3)),
            Constraint::Length(7),
            Constraint::Length(usage_h),
            Constraint::Min(0),
        ])
        .areas(right);
        runner_panel(view, buf, r1, true);
        host_panel(view, buf, r2);
        usage_panel(view, buf, r3);
        if r4.height >= 3 {
            held_panel(view, buf, r4);
        }
    } else {
        let panels_h = if main.height >= 22 { 7 } else { 0 };
        let [roster_area, panels] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(panels_h)]).areas(main);
        roster(view, buf, roster_area, host);
        if panels_h > 0 {
            let [a, b, c] = Layout::horizontal([
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
            ])
            .areas(panels);
            runner_panel(view, buf, a, false);
            host_panel(view, buf, b);
            usage_panel(view, buf, c);
        }
    }
    footer(view, buf, foot);
    if view.app.help {
        help(view, buf, area);
    }
    if view.theme.ascii {
        ascii_fold(buf, area);
    }
}

/// `--ascii` promises a screen any terminal font can draw, so fold what is
/// left (ellipses, and whatever the fleet's own text contains) to ASCII.
fn ascii_fold(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if !cell.symbol().is_ascii() {
                let folded = match cell.symbol() {
                    "\u{2026}" => "~",
                    "\u{b7}" => ".",
                    "\u{2014}" | "\u{2013}" => "-",
                    _ => "?",
                };
                cell.set_symbol(folded);
            }
        }
    }
}

/// Number of workers asking for the captain, for the terminal title.
pub fn asking(snap: &Snapshot, th: &Thresholds) -> usize {
    crate::app::all_rows(snap, th)
        .iter()
        .filter(|r| r.state == WorkerState::Captain)
        .count()
}
