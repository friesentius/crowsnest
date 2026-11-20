//! The runner panel (one lane per runner, its verdict in the title) and the
//! runner detail view.

use super::{Content, View, draw_box, fit, rpad, span, width_of};
use crate::config::Panel;
use crate::lanes::{
    self, Current, Fleet, JobCpu, Lane, Online, QueueState, Queued, Signal, Verdict, Why,
    short_name, step_name, tiny_name,
};
use crate::model::{Job, RunnerThresholds};
use crate::timefmt::{self, dur};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;

type Spans = Vec<Span<'static>>;

/// `HH:MM` local, or `Ddd HH:MM` on another day.
fn at(epoch: i64, now: i64) -> String {
    timefmt::short_when(epoch, now)
}

/// `left`, then `right` flush against the right edge; `left` gives way.
fn lr(left: Spans, right: Spans, w: usize) -> Line<'static> {
    let rw = width_of(&right);
    if rw == 0 {
        return fit(left, w);
    }
    let mut out = fit(left, w.saturating_sub(rw + 1)).spans;
    let lw = width_of(&out);
    out.push(Span::raw(" ".repeat(w.saturating_sub(lw + rw))));
    out.extend(right);
    fit(out, w)
}

/// A progress bar of `n` cells for `elapsed` of `typical`; past the
/// typical time it fills and ends in a warning mark.
fn progress(t: &Theme, elapsed: i64, typical: i64, n: usize, style: Style) -> Spans {
    if typical <= 0 || n == 0 {
        return Vec::new();
    }
    if elapsed > typical {
        return vec![
            span(t.bar_full().repeat(n - 1), style),
            span(t.warn_glyph(), t.bad()),
        ];
    }
    let full = ((elapsed as f64 / typical as f64) * n as f64).round() as usize;
    let full = full.min(n);
    vec![
        span(t.bar_full().repeat(full), style),
        span(t.bar_empty().repeat(n - full), t.dim()),
    ]
}

fn online_text(o: Online) -> Option<&'static str> {
    match o {
        Online::Yes => Some("online"),
        Online::No => Some("offline"),
        Online::Denied => Some("online ?"),
        Online::Unregistered => Some("not registered"),
        Online::Unknown => None,
    }
}

fn verdict_style(t: &Theme, v: Verdict) -> Style {
    match v {
        Verdict::Moving => t.good(),
        _ => t.verdict(v).1,
    }
}

/// The verdict as the panel words it: `✓ moving`, `SLOW`, `STALLED`, ...
fn verdict_word(t: &Theme, v: Verdict) -> String {
    match v {
        Verdict::Moving => format!("{} moving", t.ok_glyph()),
        Verdict::Idle => "idle".into(),
        v => v.word().into(),
    }
}

/// `7m/12m`, or `7m` without a typical time.
fn elapsed_of(c: &Current) -> String {
    match c.typical {
        Some(t) => format!("{}/{}", dur(c.elapsed), dur(t)),
        None => dur(c.elapsed),
    }
}

/// Wait text of a queued job, as a lane line ends it.
fn wait_text(t: &Theme, lane: &Lane, q: &Queued, first: bool, now: i64) -> (String, Style) {
    let w = format!("wait {}", dur(q.waited));
    match q.state {
        QueueState::Turn => match lane.next_start.filter(|_| first) {
            Some(s) => (format!("{w} {} {}", t.arrow(), at(s, now)), t.plain()),
            None => (w, t.plain()),
        },
        QueueState::Next => (format!("{w}, starting"), t.plain()),
        QueueState::NotPickingUp => (format!("{w} not picked up"), t.bad()),
        QueueState::Blocked => (format!("{w} blocked"), t.bad()),
        QueueState::Held => ("held by GitHub".into(), t.dim()),
    }
}

/// The lane's head line: glyph, name, verdict.
fn head(
    view: &View,
    lane: &Lane,
    name_w: usize,
    wide: bool,
    selected: bool,
    w: usize,
) -> Line<'static> {
    let t = &view.theme;
    let now = view.snap.now;
    let (glyph, gstyle) = t.verdict(lane.verdict);
    let name_style = if selected {
        t.key()
    } else if lane.verdict == Verdict::Idle {
        t.plain()
    } else {
        t.bold()
    };
    let mut l: Spans = vec![
        span(format!("{glyph} "), gstyle),
        span(rpad(lane.runner.label(), name_w), name_style),
        span(if wide { "  " } else { " " }, Style::new()),
    ];
    let vstyle = verdict_style(t, lane.verdict);
    let mut right: Spans = Vec::new();
    let ran = lane
        .last_ran
        .map(|r| (now - r).max(0))
        .map(|a| format!("{:>3}", dur(a)));
    match (lane.verdict, wide) {
        (Verdict::Idle, false) => {
            l.push(span("idle", t.dim()));
            if let Some(r) = ran {
                l.push(span(format!(" \u{b7} ran {r}"), t.dim()));
            }
        }
        (Verdict::Idle, true) => {
            l.push(span("idle", t.dim()));
            if let Some(o) = online_text(lane.online) {
                l.push(span(format!(" \u{b7} {o}"), t.dim()));
            }
            if let Some(r) = ran {
                l.push(span(format!(" \u{b7} ran {r} ago"), t.dim()));
            }
        }
        (v, false) => {
            l.push(span(verdict_word(t, v), vstyle));
            match (v, &lane.current) {
                (Verdict::Down, _) => {
                    let why = if lane.why == Why::Offline {
                        " offline"
                    } else {
                        " no listener"
                    };
                    l.push(span(why, t.dim()));
                }
                (Verdict::NotPickingUp, _) => {}
                (_, Some(c)) => l.push(span(format!(" {}", elapsed_of(c)), t.plain())),
                _ => {}
            }
        }
        (v, true) => {
            l.push(span(verdict_word(t, v), vstyle));
            let o = match (v, lane.why) {
                (Verdict::Down, Why::NoListener) => Some("no listener"),
                _ => online_text(lane.online),
            };
            if let Some(o) = o {
                l.push(span(
                    format!(" \u{b7} {o}"),
                    if lane.online == Online::No {
                        t.bad()
                    } else {
                        t.dim()
                    },
                ));
            }
            if let Some(wait) = lane.longest_wait() {
                right.push(span(
                    format!("q{} {}", lane.queue.len(), dur(wait)),
                    t.warn(),
                ));
            }
        }
    }
    lr(l, right, w)
}

/// The narrow panel's one line under a runner, if it has something to say.
fn narrow_detail(view: &View, lane: &Lane) -> Option<Spans> {
    let t = &view.theme;
    let now = view.snap.now;
    let lead = span(format!("  {} ", t.corner()), t.dim());
    let first = lane.queue.first();
    let waits = first
        .map(|q| format!(" \u{b7} {} waits", tiny_name(&q.job.name)))
        .unwrap_or_default();
    let text: (String, Style) = match (lane.verdict, lane.why) {
        (Verdict::Stalled, Why::Silent(s)) => (format!("silent {}{waits}", dur(s)), t.bad()),
        (Verdict::Stalled, _) => (format!("no worker{waits}"), t.bad()),
        // The head already says why it is down: say how long it has been.
        (Verdict::Down, _) => match (first, lane.runner.runner_log_at) {
            (Some(q), _) => (
                format!("{} waits, blocked", tiny_name(&q.job.name)),
                t.bad(),
            ),
            (None, Some(a)) => (format!("runner log {} ago", dur((now - a).max(0))), t.dim()),
            (None, None) => return None,
        },
        (Verdict::NotPickingUp, _) => {
            let q = first?;
            (
                format!("{} queued {}, idle", tiny_name(&q.job.name), dur(q.waited)),
                t.bad(),
            )
        }
        _ => match first {
            Some(q) => {
                let more = if lane.queue.len() > 1 {
                    format!(" +{}", lane.queue.len() - 1)
                } else {
                    String::new()
                };
                let (w, st) = wait_text(t, lane, q, true, now);
                (format!("{}{more} {w}", tiny_name(&q.job.name)), st)
            }
            None => {
                let c = lane.current.as_ref()?;
                let s = c.step?;
                (
                    format!(
                        "step {}/{} {}",
                        s.number,
                        c.job.map_or(0, Job::step_total),
                        step_name(&s.name)
                    ),
                    t.dim(),
                )
            }
        },
    };
    Some(vec![lead, span(text.0, text.1)])
}

/// Signal text: `out 1s ago`, `log 4s ago`, or `silent 14m`.
fn signal_text(t: &Theme, lane: &Lane) -> Option<(String, Style)> {
    let age = lane.signal_age?;
    if let Why::Silent(s) = lane.why {
        return Some((format!("silent {}", dur(s)), t.bad()));
    }
    // Past the silent limit but its processes are working: quiet, not stuck.
    match lane.job_cpu {
        Some(JobCpu::Busy(_)) => {
            return Some((format!("quiet {}, cpu busy", dur(age)), t.dim()));
        }
        Some(JobCpu::Measuring) => {
            return Some((format!("quiet {}, measuring cpu", dur(age)), t.dim()));
        }
        _ => {}
    }
    Some(match lane.signal {
        Signal::StepOutput => (format!("out {} ago", dur(age)), t.dim()),
        Signal::WorkerLog => (format!("log {} ago", dur(age)), t.dim()),
        Signal::None => (format!("no output for {}", dur(age)), t.dim()),
    })
}

/// The wide panel's lines under a runner, each with a priority: lower
/// numbers are kept when the panel is short.
fn wide_detail(view: &View, lane: &Lane, w: usize) -> Vec<(u8, Line<'static>)> {
    let t = &view.theme;
    let now = view.snap.now;
    let mut out = Vec::new();
    let stuck = lane.verdict.is_stuck();
    if let Some(c) = &lane.current {
        let (g, gs) = t.step(crate::model::StepStatus::Running);
        let mut left: Spans = vec![span(format!("  {g} "), gs)];
        let mut right: Spans = vec![span(dur(c.elapsed), t.plain())];
        if let Some(j) = c.job {
            left.push(span(short_name(&j.name).to_string(), t.plain()));
            left.push(span(format!(" {}", j.label()), t.dim()));
        } else {
            left.push(span("busy", t.plain()));
        }
        if let Some(typ) = c.typical {
            let style = if c.elapsed > typ { t.bad() } else { gs };
            right.push(span(" ", Style::new()));
            right.extend(progress(t, c.elapsed, typ, 10, style));
            right.push(span(format!(" ~{}", dur(typ)), t.dim()));
        }
        out.push((if stuck { 1 } else { 2 }, lr(left, right, w)));
        // The step name gives way before the signal does.
        let mut l: Spans = vec![span("    ", Style::new())];
        let mut right: Spans = Vec::new();
        if let (Some(s), Some(j)) = (c.step, c.job) {
            l.push(span(
                format!("step {}/{} ", s.number, j.step_total()),
                t.dim(),
            ));
            l.push(span(step_name(&s.name).to_string(), t.dim()));
            if let Some(e) = c.step_elapsed {
                right.push(span(format!("{} ", dur(e)), t.dim()));
            }
        }
        if let Some((text, st)) = signal_text(t, lane) {
            if l.len() > 1 {
                right.push(span("\u{b7} ", t.dim()));
            }
            right.push(span(text, st));
        }
        if l.len() > 1 || !right.is_empty() {
            let used = width_of(&l) + width_of(&right) + 1;
            let line = if used <= w {
                let gap = if l.len() > 1 { " " } else { "" };
                let mut all = l;
                all.push(span(gap, Style::new()));
                all.extend(right);
                fit(all, w)
            } else {
                lr(l, right, w)
            };
            out.push((if stuck { 1 } else { 3 }, line));
        }
    }
    for (i, q) in lane.queue.iter().enumerate() {
        let (g, gs) = t.check(crate::model::CheckState::Queued);
        let left: Spans = vec![
            span(format!("  {g} "), gs),
            span(short_name(&q.job.name).to_string(), t.plain()),
            span(format!(" {}", q.job.label()), t.dim()),
        ];
        let (text, st) = wait_text(t, lane, q, i == 0, now);
        out.push((
            if i == 0 { 2 } else { 4 },
            lr(left, vec![span(text, st)], w),
        ));
    }
    if lane.queue.len() > 1
        && let Some(d) = lane.drains_at
    {
        out.push((
            5,
            fit(
                vec![span(format!("    queue drains ~{}", at(d, now)), t.dim())],
                w,
            ),
        ));
    }
    out
}

/// Keep the lines that fit `avail` rows, dropping the least important
/// (highest priority number, latest first). Priority 0 is never dropped;
/// when those alone overflow, the last row says how many are hidden.
fn keep(lines: Vec<(u8, Line<'static>)>, avail: usize, t: &Theme) -> Vec<Line<'static>> {
    let mut lines = lines;
    while lines.len() > avail {
        let worst = lines.iter().map(|(p, _)| *p).max().unwrap_or(0);
        if worst == 0 {
            let hidden = lines.len() - avail + 1;
            lines.truncate(avail.saturating_sub(1));
            lines.push((
                0,
                Line::from(span(format!("+{hidden} more runners"), t.dim())),
            ));
            break;
        }
        let i = lines.iter().rposition(|(p, _)| *p == worst).unwrap_or(0);
        lines.remove(i);
    }
    lines.into_iter().map(|(_, l)| l).collect()
}

/// The panel's lines, before they are cut to fit.
fn panel_lines(view: &View, lanes: &[Lane], w: usize, wide: bool) -> Vec<(u8, Line<'static>)> {
    let t = &view.theme;
    let focused = view.app.runners_focused(view.snap);
    let keys = crate::app::runner_keys(view.snap);
    let sel = view.app.runner_index(&keys).filter(|_| focused);
    let max_label = lanes
        .iter()
        .map(|l| l.runner.label().width())
        .max()
        .unwrap_or(0);
    let name_w = max_label.min(w * 35 / 100).max(4);
    let mut lines = Vec::new();
    lines.extend(
        super::tier_error_lines(view, "runners", w)
            .into_iter()
            .map(|l| (0, l)),
    );
    for (i, lane) in lanes.iter().enumerate() {
        lines.push((0, head(view, lane, name_w, wide, sel == Some(i), w)));
        if wide {
            lines.extend(wide_detail(view, lane, w));
        } else if let Some(d) = narrow_detail(view, lane) {
            let p = match lane.verdict {
                v if v.is_stuck() => 1,
                Verdict::Slow => 2,
                _ if !lane.queue.is_empty() => 3,
                _ => 4,
            };
            lines.push((p, fit(d, w)));
        }
    }
    if wide && !lanes.is_empty() {
        let hint = if focused {
            let back = if view.app.settings.shown(Panel::Roster) {
                " \u{b7} Esc back"
            } else {
                ""
            };
            format!(
                "    {} runner \u{b7} Enter detail{back}",
                t.g("\u{2191}\u{2193}", "j/k")
            )
        } else {
            format!(
                "    Tab focus \u{b7} {} runner \u{b7} Enter detail",
                t.g("\u{2191}\u{2193}", "j/k")
            )
        };
        lines.push((6, fit(vec![span(hint, t.dim())], w)));
    }
    lines
}

/// Rows the wide panel wants, borders included.
pub fn wide_height(view: &View, w: u16) -> u16 {
    let lanes = lanes::lanes(view.snap, &view.app.runner_thresholds);
    let n = panel_lines(view, &lanes, w.saturating_sub(4) as usize, true)
        .len()
        .max(1);
    n as u16 + 2
}

pub fn panel(view: &View, buf: &mut Buffer, area: Rect, wide: bool) {
    let t = &view.theme;
    let w = area.width.saturating_sub(4) as usize;
    let avail = area.height.saturating_sub(2) as usize;
    let snap = view.snap;
    let lanes = lanes::lanes(snap, &view.app.runner_thresholds);
    let mut content: Vec<Content> = Vec::new();
    if snap.runners.is_empty() && !snap.fresh.get("runners").is_some_and(|f| f.error.is_some()) {
        content.push(Content::Line(Line::from(span(
            "no self-hosted runners found",
            t.dim(),
        ))));
    }
    content.extend(
        keep(panel_lines(view, &lanes, w, wide), avail, t)
            .into_iter()
            .map(Content::Line),
    );
    let mut title = vec![
        span(super::panel_mark(t, Panel::Runners), t.bold()),
        span("RUNNERS", t.bold()),
    ];
    match lanes::fleet(&lanes) {
        Fleet::Stuck(n) => title.push(span(
            format!(" {} {n} stalled", t.verdict(Verdict::Stalled).0),
            t.bad(),
        )),
        Fleet::Slow(n) => title.push(span(format!(" {} {n} slow", t.warn_glyph()), t.warn())),
        Fleet::Moving => title.push(span(
            if wide {
                format!(" {} all moving", t.ok_glyph())
            } else {
                format!(" {} moving", t.ok_glyph())
            },
            t.good(),
        )),
        Fleet::Quiet => {}
    }
    let queued: usize = lanes.iter().map(|l| l.queue.len()).sum();
    let right = if queued == 0 {
        vec![]
    } else if wide {
        vec![span(format!("queue {queued}"), t.warn())]
    } else {
        vec![span(format!("q{queued}"), t.warn())]
    };
    let border = if view.app.runners_focused(snap) {
        t.focus_border()
    } else {
        t.border()
    };
    draw_box(buf, area, t, title, right, border, content);
}

// ---------------------------------------------------------------------------
// Detail view
// ---------------------------------------------------------------------------

/// Label column of the detail view.
const LABEL: usize = 9;

/// Word-wrap `text` after `first` (the first line's lead) into lines of `w`
/// cells, continuing under `indent` spaces.
fn wrap_words(
    first: Spans,
    indent: usize,
    text: &str,
    style: Style,
    w: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur = first;
    let mut used = width_of(&cur);
    let mut fresh = true;
    for word in text.split_whitespace() {
        let ww = word.width();
        if !fresh && used + 1 + ww > w {
            lines.push(fit(std::mem::take(&mut cur), w));
            cur = vec![span(" ".repeat(indent), Style::new())];
            used = indent;
            fresh = true;
        }
        if !fresh {
            cur.push(span(" ", style));
            used += 1;
        }
        cur.push(span(word.to_string(), style));
        used += ww;
        fresh = false;
    }
    lines.push(fit(cur, w));
    lines
}

fn label(t: &Theme, text: &str) -> Span<'static> {
    span(rpad(text, LABEL), t.bold())
}

fn pad() -> Span<'static> {
    span(" ".repeat(LABEL), Style::new())
}

/// One plain-language sentence or two on why the runner is in its state.
pub fn verdict_sentence(lane: &Lane, th: &RunnerThresholds, now: i64) -> String {
    let current = lane.current.as_ref();
    let cname = current
        .and_then(Current::name)
        .map(tiny_name)
        .unwrap_or("The job");
    let queue_note = |stuck: bool| -> String {
        let n = lane.queue.len();
        let first = lane
            .queue
            .first()
            .map(|q| tiny_name(&q.job.name))
            .unwrap_or("");
        match (n, stuck) {
            (0, _) => String::new(),
            (1, false) => format!(" The queued {first} is waiting its turn, not stuck."),
            (_, false) => format!(" The {n} queued jobs are waiting their turn, not stuck."),
            (1, true) => format!(" The queued {first} cannot start until this is fixed."),
            (_, true) => format!(" The {n} queued jobs cannot start until this is fixed."),
        }
    };
    let typ = |c: &Current| match c.typical {
        Some(t) => format!("a job that usually takes {}", dur(t)),
        None => "a job with no typical time yet (fewer than 3 past runs)".into(),
    };
    let signal = |age: i64| match lane.signal {
        Signal::StepOutput => format!("its step output advanced {} ago", dur(age)),
        Signal::WorkerLog => format!("its worker log advanced {} ago", dur(age)),
        Signal::None => format!("nothing has been written since it started {} ago", dur(age)),
    };
    match (lane.verdict, lane.why) {
        (Verdict::Down, Why::Offline) => {
            format!(
                "GitHub reports this runner offline, so it takes no jobs.{}",
                queue_note(true)
            )
        }
        (Verdict::Down, _) => format!(
            "No Runner.Listener process runs from {}, so this runner takes no jobs.{}",
            lane.runner.dir.display(),
            queue_note(true)
        ),
        (Verdict::Stalled, Why::NoWorker) => format!(
            "GitHub says {cname} is in progress here, but no Runner.Worker process is running.{}",
            queue_note(true)
        ),
        (Verdict::Stalled, Why::Silent(s)) => format!(
            "{cname} has written nothing for {} (the limit is {}){}. It is {} into {}.{}",
            dur(s),
            dur(th.silent_secs),
            match lane.job_cpu {
                Some(JobCpu::Idle(_)) => " and its processes are idle",
                _ if lane.runner.scanned_at.is_none() => {
                    ", and its processes cannot be seen (process table unreadable)"
                }
                _ => ", and it runs in a container whose CPU crowsnest cannot see",
            },
            current.map_or(String::new(), |c| dur(c.elapsed)),
            current.map_or(String::new(), typ),
            queue_note(true)
        ),
        (Verdict::NotPickingUp, Why::Unpicked(w)) => format!(
            "{} has been queued {} while this runner sits idle (the limit is {}).",
            lane.queue
                .first()
                .map_or("A job", |q| tiny_name(&q.job.name)),
            dur(w),
            dur(th.pickup_secs)
        ),
        (Verdict::Slow, _) => {
            let c = current;
            format!(
                "{cname} has run {}, past {}x its typical {}, but {}.{}",
                c.map_or(String::new(), |c| dur(c.elapsed)),
                th.overdue_factor,
                c.and_then(|c| c.typical).map_or(String::new(), dur),
                lane.signal_age.map_or("it is still running".into(), signal),
                queue_note(false)
            )
        }
        (Verdict::Moving, _) => {
            let c = current;
            let base = match c {
                Some(c) => format!("{cname} is {} into {}", dur(c.elapsed), typ(c)),
                None => "Busy".into(),
            };
            match (lane.signal_age, lane.job_cpu) {
                (Some(a), Some(JobCpu::Busy(ms))) => format!(
                    "{base}. It has written nothing for {}, but its processes are busy ({} of CPU in the last {}): quiet, not hung.{}",
                    dur(a),
                    timefmt::dur_ms(ms),
                    dur(th.silent_secs),
                    queue_note(false)
                ),
                (Some(a), Some(_)) => format!(
                    "{base}. It has written nothing for {}; its CPU is still being measured.{}",
                    dur(a),
                    queue_note(false)
                ),
                (Some(a), None) => format!("{base}, and {}.{}", signal(a), queue_note(false)),
                (None, _) => format!("{base}.{}", queue_note(false)),
            }
        }
        _ => {
            let ran = lane
                .last_ran
                .map(|r| format!(" It last finished a job {} ago.", dur(now - r)))
                .unwrap_or_default();
            if lane.queue.is_empty() {
                format!("Idle, nothing queued.{ran}")
            } else {
                format!("Idle, with a job queued: it should start now.{ran}")
            }
        }
    }
}

fn run_line(t: &Theme, j: &Job) -> Spans {
    let Some(r) = &j.run else {
        return vec![span(
            format!("{} \u{b7} {}", j.workflow, j.branch),
            t.plain(),
        )];
    };
    let mut parts = vec![format!("{} #{}", r.workflow, r.number), r.event.clone()];
    if let Some(n) = r.pr {
        parts.push(format!("#{n} {}", r.title));
    }
    match &r.base {
        Some(b) => parts.push(format!("{} {} {b}", r.branch, t.arrow())),
        None => parts.push(r.branch.clone()),
    }
    if r.sha.len() >= 7 {
        parts.push(r.sha[..7].to_string());
    }
    vec![span(parts.join(" \u{b7} "), t.plain())]
}

fn now_section(view: &View, lane: &Lane, w: usize) -> Vec<Line<'static>> {
    let t = &view.theme;
    let now = view.snap.now;
    let Some(c) = &lane.current else {
        let text = match lane.last_ran {
            Some(r) => format!(
                "nothing running \u{b7} last finished a job {} ago",
                dur(now - r)
            ),
            None => "nothing running".into(),
        };
        return vec![fit(vec![label(t, "NOW"), span(text, t.dim())], w)];
    };
    let mut out = Vec::new();
    let Some(j) = c.job else {
        out.push(fit(
            vec![
                label(t, "NOW"),
                span(
                    format!("busy {} (GitHub not read yet)", dur(c.elapsed)),
                    t.plain(),
                ),
            ],
            w,
        ));
        return out;
    };
    let mut l = vec![label(t, "NOW")];
    l.extend(run_line(t, j));
    out.push(fit(l, w));
    let (g, gs) = t.step(crate::model::StepStatus::Running);
    let mut l = vec![
        pad(),
        span(format!("{g} "), gs),
        span(j.name.clone(), t.bold()),
    ];
    match c.typical {
        Some(typ) => {
            l.push(span(
                format!("   {} of ~{}  ", dur(c.elapsed), dur(typ)),
                t.plain(),
            ));
            let style = if c.elapsed > typ { t.bad() } else { gs };
            l.extend(progress(t, c.elapsed, typ, 24, style));
        }
        None => l.push(span(
            format!("   {}, no typical time yet", dur(c.elapsed)),
            t.plain(),
        )),
    }
    out.push(fit(l, w));
    // Steps: the finished ones before the last two fold into a count.
    let done: Vec<_> = j.steps.iter().filter(|s| s.status == "completed").collect();
    let mut items: Vec<Spans> = Vec::new();
    // Show the last two finished steps; fold two or more before them.
    let shown = if done.len() >= 4 { 2 } else { done.len() };
    if shown < done.len() {
        let last = done[done.len() - shown - 1].number;
        items.push(vec![span(
            format!("{} 1-{last} done", t.ok_glyph()),
            t.good(),
        )]);
    }
    for s in done.iter().rev().take(shown).rev() {
        let ok = s.conclusion.as_deref() != Some("failure");
        let d = s
            .started_at
            .zip(s.completed_at)
            .map(|(a, b)| format!(" {}", dur(b - a)))
            .unwrap_or_default();
        items.push(vec![span(
            format!(
                "{} {} {}{d}",
                if ok {
                    t.ok_glyph()
                } else {
                    t.verdict(Verdict::Stalled).0
                },
                s.number,
                step_name(&s.name)
            ),
            if ok { t.good() } else { t.bad() },
        )]);
    }
    if let Some(s) = j.steps.iter().find(|s| s.status == "in_progress") {
        let mut text = format!("{g} {} {}", s.number, step_name(&s.name));
        if let Some(e) = c.step_elapsed {
            text.push_str(&format!(" {}", dur(e)));
        }
        let mut item = vec![span(text, gs)];
        if let Some(st) = c.step_typical {
            item.push(span(
                format!(" (typ {})", timefmt::dur_ms(st as u64 * 1000)),
                t.dim(),
            ));
        }
        items.push(item);
    }
    let pending: Vec<u32> = j
        .steps
        .iter()
        .filter(|s| s.status != "completed" && s.status != "in_progress")
        .map(|s| s.number)
        .collect();
    if let (Some(a), Some(b)) = (pending.first(), pending.last()) {
        let range = if a == b {
            a.to_string()
        } else {
            format!("{a}-{b}")
        };
        items.push(vec![span(format!("{} {range} pending", t.dot()), t.dim())]);
    }
    if !items.is_empty() {
        out.extend(super::wrap_items(&[pad()], &[pad()], items, w));
    }
    let mut l = vec![pad()];
    let mut bits: Vec<(String, Style)> = Vec::new();
    let r = lane.runner;
    let job_start = j.started_at.or(r.busy_since());
    let age = |at: Option<i64>| at.map(|a| dur((now - a).max(0)));
    let limit = dur(view.app.runner_thresholds.silent_secs);
    match (lane.why, lane.job_cpu, lane.signal_age) {
        (Why::Silent(s), cpu, _) => {
            let how = match cpu {
                Some(JobCpu::Idle(ms)) => {
                    format!(
                        "job processes idle ({} CPU in {limit})",
                        timefmt::dur_ms(ms)
                    )
                }
                _ if r.scanned_at.is_none() => "job processes not visible".into(),
                _ => "runs in a container, CPU not visible".into(),
            };
            bits.push((format!("silent {} (limit {limit}), {how}", dur(s)), t.bad()));
        }
        (_, Some(JobCpu::Busy(ms)), Some(a)) => bits.push((
            format!(
                "quiet {}, job processes busy ({} CPU in {limit})",
                dur(a),
                timefmt::dur_ms(ms)
            ),
            t.plain(),
        )),
        (_, Some(JobCpu::Measuring), Some(a)) => {
            bits.push((format!("quiet {}, measuring job CPU", dur(a)), t.plain()))
        }
        _ => {}
    }
    if let Some(a) = age(r
        .step_output_at
        .filter(|o| job_start.is_none_or(|b| *o >= b)))
    {
        bits.push((format!("step output {a} ago"), t.dim()));
    }
    if let Some(a) = age(r.worker_log_at) {
        bits.push((format!("worker log {a} ago"), t.dim()));
    }
    if let Some(s) = j.started_at {
        bits.push((format!("started {}", at(s, now)), t.dim()));
    }
    for (i, (text, st)) in bits.into_iter().enumerate() {
        if i > 0 {
            l.push(span(" \u{b7} ", t.dim()));
        }
        l.push(span(text, st));
    }
    out.push(fit(l, w));
    out
}

fn queue_section(view: &View, lane: &Lane, w: usize) -> Vec<Line<'static>> {
    let t = &view.theme;
    let now = view.snap.now;
    if lane.queue.is_empty() {
        return vec![fit(vec![label(t, "QUEUE"), span("empty", t.dim())], w)];
    }
    let mut out = Vec::new();
    let name_w = lane
        .queue
        .iter()
        .map(|q| q.job.name.width())
        .max()
        .unwrap_or(0)
        .min(32);
    for (i, q) in lane.queue.iter().enumerate() {
        let (g, gs) = t.check(crate::model::CheckState::Queued);
        let run = q
            .job
            .run
            .as_ref()
            .map(|r| format!("{} #{} \u{b7} {}", r.workflow, r.number, r.label()))
            .unwrap_or_else(|| q.job.label());
        let mut l = vec![
            if i == 0 { label(t, "QUEUE") } else { pad() },
            span(format!("{g} "), gs),
            span(rpad(&q.job.name, name_w), t.plain()),
            span(format!("  {run}"), t.dim()),
        ];
        if let Some(c) = q.job.created_at {
            l.push(span(
                format!("   queued {}, waited {}", at(c, now), dur(q.waited)),
                t.plain(),
            ));
        }
        let state = match q.state {
            QueueState::Blocked => Some(("  blocked", t.bad())),
            QueueState::NotPickingUp => Some(("  not picked up", t.bad())),
            QueueState::Held => Some(("  held by GitHub (approval or concurrency)", t.dim())),
            _ => None,
        };
        if let Some((text, st)) = state {
            l.push(span(text, st));
        }
        out.push(fit(l, w));
    }
    let current = lane.current.as_ref().and_then(Current::name).map(tiny_name);
    let summary = match (lane.verdict, lane.next_start) {
        (Verdict::Down, _) => "blocked: the runner is down".to_string(),
        (Verdict::Stalled, _) => format!(
            "blocked: {} must finish or be cancelled first",
            current.unwrap_or("the running job")
        ),
        (Verdict::NotPickingUp, _) => format!(
            "not picked up: the runner has been idle {} (the limit is {})",
            lane.runner
                .idle_since
                .map_or("a while".into(), |s| dur(now - s)),
            dur(view.app.runner_thresholds.pickup_secs)
        ),
        (_, Some(start)) => {
            let mut s = match current {
                Some(c) => format!("next to start ~{} when {c} ends", at(start, now)),
                None => format!("next to start ~{}", at(start, now)),
            };
            if lane.queue.len() == 1
                && let Some(typ) = lane.queue[0].typical
            {
                s.push_str(&format!("; ~{}", dur(typ)));
            }
            if let Some(d) = lane.drains_at {
                s.push_str(&format!("; queue drains ~{}", at(d, now)));
            }
            if lane.queue.len() > 1 {
                s.push_str("; GitHub picks which job goes next");
            }
            s
        }
        (Verdict::Slow, None) => "no estimate: the running job is past its typical time".into(),
        _ => "no estimate yet: fewer than 3 past runs of the running job".into(),
    };
    out.push(fit(vec![pad(), span(summary, t.dim())], w));
    out
}

fn recent_section(view: &View, lane: &Lane, w: usize, rows: usize) -> Vec<Line<'static>> {
    let t = &view.theme;
    let now = view.snap.now;
    let Some(h) = lane.history else {
        let text = if view.snap.fresh.contains_key("gh") {
            "no finished jobs read yet"
        } else {
            "GitHub is not read (--no-github)"
        };
        return vec![fit(vec![label(t, "RECENT"), span(text, t.dim())], w)];
    };
    let mut out = Vec::new();
    let mine: Vec<_> = h
        .jobs
        .iter()
        .filter(|j| j.runner == lane.runner.name)
        .collect();
    if mine.is_empty() {
        let text = h
            .error
            .clone()
            .unwrap_or_else(|| "no finished jobs on this runner in the runs read".into());
        out.push(fit(vec![label(t, "RECENT"), span(text, t.dim())], w));
    }
    let name_w = mine
        .iter()
        .map(|j| short_name(&j.name).width())
        .max()
        .unwrap_or(0)
        .min(28);
    let label_w = mine
        .iter()
        .map(|j| j.label.width())
        .max()
        .unwrap_or(0)
        .min(18);
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for (i, j) in mine.iter().take(rows.saturating_sub(1).max(1)).enumerate() {
        let ok = j.conclusion == "success";
        let (g, gs) = match j.conclusion.as_str() {
            "success" => (t.ok_glyph(), t.good()),
            "skipped" | "cancelled" | "neutral" => ("-", t.dim()),
            _ => (t.verdict(Verdict::Stalled).0, t.bad()),
        };
        let mut l = vec![
            if i == 0 { label(t, "RECENT") } else { pad() },
            span(j.started_at.map_or("     ".into(), |s| at(s, now)), t.dim()),
            span(format!(" {g} "), gs),
            span(
                rpad(short_name(&j.name), name_w),
                if ok { t.plain() } else { t.bad() },
            ),
            span(format!(" {}", rpad(&j.label, label_w)), t.dim()),
            span(
                format!(" {:>6}", j.ran_secs().map(dur).unwrap_or_default()),
                t.plain(),
            ),
        ];
        // The typical time once per job, on its latest run.
        let key = (j.workflow.as_str(), j.name.as_str());
        if !seen.contains(&key) {
            seen.push(key);
            if let Some(typ) = h
                .typical(&j.workflow, &j.name, None)
                .filter(|x| x.samples >= lanes::MIN_SAMPLES)
            {
                l.push(span(format!("  typ {}", dur(typ.secs)), t.dim()));
            }
        }
        out.push(fit(l, w));
    }
    if let Some(st) = lanes::stats(lane, now) {
        let mut bits = Vec::new();
        if let Some(p) = st.busy_pct {
            if st.busy_from <= now - lanes::BUSY_WINDOW_SECS {
                bits.push(format!("busy {p:.0}% of the last 6h"));
            } else {
                bits.push(format!("busy {p:.0}% since {}", at(st.busy_from, now)));
            }
        }
        bits.push(if st.today {
            format!("{} jobs today", st.jobs)
        } else {
            format!("{} jobs since {}", st.jobs, at(st.since, now))
        });
        if let (Some(m), Some(x)) = (st.wait_median, st.wait_max) {
            bits.push(format!("queue wait median {}, max {}", dur(m), dur(x)));
        }
        let lead = if out.is_empty() {
            label(t, "RECENT")
        } else {
            pad()
        };
        out.push(fit(vec![lead, span(bits.join(" \u{b7} "), t.dim())], w));
    }
    out
}

fn health_section(view: &View, lane: &Lane, w: usize) -> Vec<Line<'static>> {
    let t = &view.theme;
    let now = view.snap.now;
    let r = lane.runner;
    let mut l = vec![label(t, "HEALTH"), span("listener ", t.dim())];
    match (r.scanned_at, r.listener_secs) {
        (None, _) => l.push(span("? process table unreadable", t.dim())),
        (Some(_), Some(up)) => l.push(span(format!("{} up {}", t.busy(), dur(up)), t.good())),
        (Some(_), None) => l.push(span(
            format!("{} not running", t.verdict(Verdict::Down).0),
            t.bad(),
        )),
    }
    l.push(span("   GitHub ", t.dim()));
    let (text, st) = match lane.online {
        Online::Yes => ("online".to_string(), t.good()),
        Online::No => ("offline".into(), t.bad()),
        Online::Denied => (
            "online ? (reading runners needs repo admin)".into(),
            t.dim(),
        ),
        Online::Unregistered => ("not in the repository's runner list".into(), t.warn()),
        Online::Unknown => ("not read".into(), t.dim()),
    };
    l.push(span(text, st));
    if let Some(g) = lane.gh {
        if !g.version.is_empty() {
            l.push(span(format!("   v{}", g.version), t.plain()));
        }
        if !g.labels.is_empty() {
            l.push(span("   labels ", t.dim()));
            l.push(span(g.labels.join(" "), t.plain()));
        }
    }
    let mut out = vec![fit(l, w)];
    let mut l = vec![pad()];
    match (&r.unit, &r.service) {
        (Some(u), Some(s)) if s.error.is_none() => {
            l.push(span(format!("service {u} "), t.dim()));
            let st = if s.active == "active" {
                t.good()
            } else {
                t.bad()
            };
            l.push(span(s.active.clone(), st));
            if let Some(n) = s.restarts.filter(|n| *n > 0) {
                l.push(span(
                    format!(", {n} restart{}", if n == 1 { "" } else { "s" }),
                    t.plain(),
                ));
            }
        }
        (Some(u), Some(s)) => l.push(span(
            format!("service {u}: {}", s.error.as_deref().unwrap_or("unknown")),
            t.dim(),
        )),
        (Some(u), None) => l.push(span(format!("service {u}: not read yet"), t.dim())),
        (None, _) => l.push(span(
            "service not known (svc.sh install, or --runner-unit DIR=UNIT)",
            t.dim(),
        )),
    }
    if let Some(a) = r.runner_log_at {
        l.push(span(
            format!(" \u{b7} runner log {} ago", dur((now - a).max(0))),
            t.dim(),
        ));
    }
    out.push(fit(l, w));
    out
}

/// The selected runner's detail, drawn over `area`.
pub fn detail(view: &View, buf: &mut Buffer, area: Rect) {
    let t = &view.theme;
    let keys = crate::app::runner_keys(view.snap);
    let Some(i) = view.app.runner_index(&keys) else {
        return;
    };
    let lanes = lanes::lanes(view.snap, &view.app.runner_thresholds);
    let Some(lane) = lanes.get(i) else { return };
    let w = area.width.saturating_sub(4) as usize;
    let inner = area.height.saturating_sub(2) as usize;
    let now = view.snap.now;

    let word = match lane.verdict {
        Verdict::Moving => verdict_word(t, Verdict::Moving),
        v => format!("{} {}", t.verdict(v).0, v.word()),
    };
    let first = vec![
        label(t, "VERDICT"),
        span(
            word,
            verdict_style(t, lane.verdict).add_modifier(Modifier::BOLD),
        ),
        span("  ", Style::new()),
    ];
    let sentence = verdict_sentence(lane, &view.app.runner_thresholds, now);
    let verdict = wrap_words(first, LABEL, &sentence, t.plain(), w);
    let now_l = now_section(view, lane, w);
    let queue = queue_section(view, lane, w);
    let health = health_section(view, lane, w);
    // RECENT takes what is left, after four separators.
    let fixed = verdict.len() + now_l.len() + queue.len() + health.len() + 4;
    let recent = recent_section(view, lane, w, inner.saturating_sub(fixed).max(2));

    let mut content: Vec<Content> = Vec::new();
    for (i, section) in [verdict, now_l, queue, recent, health]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            content.push(Content::Sep);
        }
        content.extend(section.into_iter().map(Content::Line));
    }
    let repo = lane
        .runner
        .repo
        .as_deref()
        .map(|r| format!(" \u{b7} {r}"))
        .unwrap_or_default();
    Clear.render(area, buf);
    draw_box(
        buf,
        area,
        t,
        vec![
            span("RUNNER ", t.title()),
            span(lane.runner.name.clone(), t.bold()),
            span(repo, t.dim()),
        ],
        vec![span("Esc back", t.dim())],
        t.focus_border(),
        content,
    );
}
