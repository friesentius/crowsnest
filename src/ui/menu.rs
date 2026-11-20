//! The options menu: a box over the middle of the screen, categories on the
//! left, the selected category's rows on the right, and what the selected row
//! does below them. Every change applies at once and is saved.

use super::theme::Theme;
use super::{View, fit, panel_mark, rpad, span, trunc, width_of};
use crate::options::{Category, Item, Target};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use unicode_width::UnicodeWidthStr;

/// The category column, between the left border and the divider.
const CAT_W: u16 = 14;
const MAX_W: u16 = 86;
/// Below this much room beside it, the box takes all of the width.
const SNAP: u16 = 20;
/// Description lines kept under the rows.
const MAX_ABOUT: usize = 4;
/// How long "saved" shows after a save, in seconds.
const SAVED_FOR: i64 = 3;

/// Draw the menu in `area`, a panel or column it may cover whole, or in
/// `fallback` (the whole body) when that is too small to hold it.
pub fn draw(view: &View, buf: &mut Buffer, area: Rect, fallback: Rect) {
    if view.app.menu.is_none() {
        return;
    }
    match geometry(view, area).or_else(|| geometry(view, fallback)) {
        Some(g) => draw_in(view, buf, &g),
        None => too_small(&view.theme, buf, fallback),
    }
}

/// Where the menu sits and how tall each of its parts is.
struct Geometry {
    r: Rect,
    rw: usize,
    list_h: u16,
    about_h: usize,
}

/// The menu's box in `area`, or None when `area` cannot hold it.
fn geometry(view: &View, area: Rect) -> Option<Geometry> {
    let app = view.app;
    // Where the box is narrower than `area` by a few cells, it takes the
    // whole width, so its edges are the borders around it and never cut
    // through the text beside it.
    let w = if area.width < MAX_W + SNAP {
        area.width
    } else {
        MAX_W
    };
    let rw = w.saturating_sub(CAT_W + 3) as usize;
    // The box keeps one size while you move between rows and categories.
    let all: Vec<Vec<Item>> = Category::ALL
        .iter()
        .map(|c| crate::options::items(*c, &app.settings, &app.overrides, &view.snap.quota))
        .collect();
    let about_h = all
        .iter()
        .flatten()
        .map(|i| wrap(&i.about, rw.saturating_sub(2)).len())
        .max()
        .unwrap_or(1)
        .clamp(1, MAX_ABOUT);
    let rows_wanted = all
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(Category::ALL.len());
    let chrome = about_h as u16 + 5;
    let list_h = (rows_wanted as u16).min(area.height.saturating_sub(chrome));
    // Room for the rows, as when the box kept two cells clear of each side.
    if rw < 30 || area.width < CAT_W + 37 || list_h < Category::ALL.len() as u16 {
        return None;
    }
    let h = list_h + chrome;
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    Some(Geometry {
        r,
        rw,
        list_h,
        about_h,
    })
}

fn draw_in(view: &View, buf: &mut Buffer, g: &Geometry) {
    let t = &view.theme;
    let app = view.app;
    let Some(menu) = &app.menu else {
        return;
    };
    let items = app.menu_items(view.snap);
    let Geometry {
        r,
        rw,
        list_h,
        about_h,
    } = *g;
    let w = r.width;
    Clear.render(r, buf);
    let b = t.focus_border();
    let path = trunc(
        &app.config.path,
        (w as usize).saturating_sub(CAT_W as usize + 16),
    );
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_set(t.border_set())
        .border_style(b)
        .title_top(Line::from(vec![
            span(" ", b),
            span("OPTIONS", t.title()),
            span(" ", b),
        ]));
    if !path.is_empty() {
        block = block.title_top(
            Line::from(vec![span(" ", b), span(path, t.dim()), span(" ", b)]).right_aligned(),
        );
    }
    block.render(r, buf);

    let div = r.x + CAT_W + 1;
    let list_y = r.y + 1;
    let mid = list_y + list_h;
    let foot = mid + 1 + about_h as u16;
    let (tee_l, horiz, tee_r) = t.sep_set();
    let vert = t.g("\u{2502}", "|");
    buf.set_string(div, r.y, t.g("\u{252c}", "+"), b);
    for y in list_y..foot {
        buf.set_string(div, y, vert, b);
    }
    buf.set_string(div, mid, tee_l, b);
    buf.set_string(div + 1, mid, horiz.repeat(rw), b);
    buf.set_string(r.right() - 1, mid, tee_r, b);
    buf.set_string(r.x, foot, tee_l, b);
    buf.set_string(r.x + 1, foot, horiz.repeat(r.width as usize - 2), b);
    buf.set_string(div, foot, t.g("\u{2534}", "+"), b);
    buf.set_string(r.right() - 1, foot, tee_r, b);

    for (i, c) in Category::ALL.iter().enumerate() {
        let style = if *c == menu.category {
            t.key().add_modifier(Modifier::BOLD)
        } else {
            t.plain()
        };
        let line = Line::from(vec![
            span(" ", Style::new()),
            span(rpad(&format!(" {}", c.name()), CAT_W as usize - 2), style),
        ]);
        buf.set_line(r.x + 1, list_y + i as u16, &line, CAT_W);
    }

    let x = div + 1;
    let selected = menu.index(&items);
    if items.is_empty() {
        let msg = match view.snap.fresh.get("quota").and_then(|f| f.error.clone()) {
            Some(e) => e,
            None => "no usage windows reported yet".into(),
        };
        let line = Line::from(span(format!("   {}", trunc(&msg, rw - 4)), t.dim()));
        buf.set_line(x, list_y, &line, rw as u16);
    }
    let shown = list_h as usize;
    let top = selected.map_or(0, |i| (i + 1).saturating_sub(shown));
    let cols = Columns::new(&items, rw);
    for (row, (i, item)) in items.iter().enumerate().skip(top).take(shown).enumerate() {
        let line = option_line(t, item, Some(i) == selected, &cols);
        buf.set_line(x, list_y + row as u16, &line, rw as u16);
    }
    // More rows than room: arrows on the border say which way they are.
    if top > 0 {
        buf.set_string(r.right() - 1, list_y, t.g("\u{25b2}", "^"), b);
    }
    if top + shown < items.len() {
        buf.set_string(r.right() - 1, mid - 1, t.g("\u{25bc}", "v"), b);
    }

    let mut about = selected
        .and_then(|i| items.get(i))
        .map_or_else(Vec::new, |i| wrap(&i.about, rw - 2));
    // Text that does not fit ends in an ellipsis instead of stopping short.
    if about.len() > about_h {
        about.truncate(about_h);
        if let Some(last) = about.last_mut() {
            *last = trunc(&format!("{last} \u{2026}"), rw - 2);
        }
    }
    for (n, text) in about.iter().take(about_h).enumerate() {
        let line = Line::from(span(text.clone(), t.dim()));
        buf.set_line(x + 1, mid + 1 + n as u16, &line, rw as u16 - 2);
    }

    let iw = r.width as usize - 2;
    let mut hint = Vec::new();
    for (k, v) in [
        (t.g("\u{2191}\u{2193}", "j/k"), "option"),
        (t.g("\u{2190}\u{2192}", "h/l"), "change"),
        ("tab", "category"),
        ("1-5", "panels"),
        ("esc", "close"),
    ] {
        hint.push(span(format!(" {k} "), t.key()));
        hint.push(span(format!(" {v} "), t.dim()));
    }
    let status = match &app.config.saved {
        Some((_, Some(_))) => Some(span("not saved ", t.warn())),
        Some((at, None)) if view.snap.now - at < SAVED_FOR => Some(span("saved ", t.good())),
        _ => None,
    };
    if let Some(s) = status {
        let pad = iw.saturating_sub(width_of(&hint) + s.content.width());
        hint.push(span(" ".repeat(pad), Style::new()));
        hint.push(s);
    }
    buf.set_line(r.x + 1, foot + 1, &fit(hint, iw), iw as u16);
}

/// Widths shared by every row of one category, so the values line up.
struct Columns {
    label: usize,
    detail: usize,
    value: usize,
}

impl Columns {
    fn new(items: &[Item], w: usize) -> Self {
        let value = items
            .iter()
            .map(|i| i.value.width())
            .max()
            .unwrap_or(0)
            .max(6);
        let detail = items
            .iter()
            .map(|i| i.detail.width())
            .max()
            .unwrap_or(0)
            .min(w / 3);
        // The cursor, the label, the detail and the value with its arrows,
        // spaced, and a space before the border.
        let label = w.saturating_sub(3 + 1 + detail + 2 + value + 4 + 1);
        Columns {
            label,
            detail,
            value,
        }
    }
}

fn option_line(t: &Theme, item: &Item, selected: bool, c: &Columns) -> Line<'static> {
    let label = match item.target {
        Target::Panel(p) if t.ascii => format!("{}{}", panel_mark(t, p), item.label),
        Target::Panel(p) => format!("{} {}", panel_mark(t, p), item.label),
        _ => item.label.clone(),
    };
    let label_style = if selected {
        t.bold()
    } else if item.on {
        t.plain()
    } else {
        t.dim()
    };
    let value = rpad(&item.value, c.value);
    let (value, value_style) = if item.locked.is_some() {
        (format!("  {value}  "), t.dim())
    } else {
        let style = if selected {
            t.key().add_modifier(Modifier::BOLD)
        } else if item.value == "hidden" {
            t.warn()
        } else {
            t.plain()
        };
        (
            format!("{} {value} {}", t.g("\u{25c2}", "<"), t.g("\u{25b8}", ">")),
            style,
        )
    };
    let cursor = if selected { t.cursor() } else { " " };
    Line::from(vec![
        span(format!(" {cursor} "), t.title()),
        span(rpad(&label, c.label), label_style),
        span(" ", Style::new()),
        span(super::lpad(&item.detail, c.detail), t.dim()),
        span("  ", Style::new()),
        span(value, value_style),
        span(" ", Style::new()),
    ])
}

fn too_small(t: &Theme, buf: &mut Buffer, area: Rect) {
    let msg = "options need a larger terminal  (esc closes)";
    let w = (msg.width() as u16 + 4).min(area.width);
    if w < 6 || area.height < 3 {
        return;
    }
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - 3) / 2,
        w,
        3,
    );
    Clear.render(r, buf);
    Block::new()
        .borders(Borders::ALL)
        .border_set(t.border_set())
        .border_style(t.focus_border())
        .render(r, buf);
    let line: Line = Line::from(vec![Span::styled(trunc(msg, w as usize - 4), t.dim())]);
    buf.set_line(r.x + 2, r.y + 1, &line, w - 4);
}

/// Words of `text` in lines of at most `w` cells.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.width() + 1 + word.width() > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
