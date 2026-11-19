//! Colours and glyphs.
//!
//! Only the 16 ANSI colours are used, so the terminal's own palette decides
//! the shades in dark and light themes. Secondary text uses the DIM modifier,
//! never a grey that may vanish on a light background. Every colour is paired
//! with a glyph and a word, so `NO_COLOR` loses no information.

use crate::model::{CheckState, StepStatus, WorkerState};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub color: bool,
    pub ascii: bool,
}

impl Theme {
    pub fn new(color: bool, ascii: bool) -> Self {
        Theme { color, ascii }
    }

    pub fn fg(&self, c: Color) -> Style {
        if self.color {
            Style::new().fg(c)
        } else {
            Style::new()
        }
    }
    pub fn plain(&self) -> Style {
        Style::new()
    }
    pub fn dim(&self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }
    pub fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
    pub fn border(&self) -> Style {
        self.dim()
    }
    pub fn focus_border(&self) -> Style {
        self.fg(Color::Cyan)
    }
    pub fn key(&self) -> Style {
        Style::new().add_modifier(Modifier::REVERSED)
    }
    pub fn warn(&self) -> Style {
        self.fg(Color::Yellow).add_modifier(Modifier::BOLD)
    }
    pub fn bad(&self) -> Style {
        self.fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    pub fn good(&self) -> Style {
        self.fg(Color::Green)
    }
    pub fn captain(&self) -> Style {
        self.fg(Color::Magenta)
    }
    /// A row that changed a moment ago: a block of reverse video.
    pub fn fresh(&self) -> Style {
        self.fg(Color::Green)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    }
    pub fn title(&self) -> Style {
        self.fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }

    pub fn state(&self, s: WorkerState) -> Style {
        match s {
            WorkerState::Captain => self
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            WorkerState::Blocked | WorkerState::Failed => self.bad(),
            WorkerState::Stale => self.warn(),
            WorkerState::Gate => self.fg(Color::Blue).add_modifier(Modifier::BOLD),
            WorkerState::Working | WorkerState::Paused => self.fg(Color::Cyan),
            WorkerState::Done => self.good(),
            WorkerState::Unknown | WorkerState::Idle => self.dim(),
        }
    }

    pub fn state_glyph(&self, s: WorkerState) -> &'static str {
        match (s, self.ascii) {
            (WorkerState::Captain, _) => "?",
            (WorkerState::Blocked | WorkerState::Failed, false) => "\u{2717}",
            (WorkerState::Blocked | WorkerState::Failed, true) => "x",
            (WorkerState::Stale, _) => "!",
            (WorkerState::Gate, false) => "\u{25c6}",
            (WorkerState::Gate, true) => "#",
            (WorkerState::Working, false) => "\u{25d0}",
            (WorkerState::Working, true) => "*",
            (WorkerState::Paused, false) => "\u{2016}",
            (WorkerState::Paused, true) => "=",
            (WorkerState::Done, false) => "\u{2713}",
            (WorkerState::Done, true) => "v",
            (WorkerState::Unknown, _) => "~",
            (WorkerState::Idle, false) => "\u{25cb}",
            (WorkerState::Idle, true) => "o",
        }
    }

    pub fn step(&self, s: StepStatus) -> (&'static str, Style) {
        let a = self.ascii;
        match s {
            StepStatus::Completed => (if a { "v" } else { "\u{2713}" }, self.good()),
            StepStatus::Running => (if a { "*" } else { "\u{25d0}" }, self.fg(Color::Cyan)),
            StepStatus::Fixing => (if a { "*" } else { "\u{25d0}" }, self.fg(Color::Yellow)),
            StepStatus::Awaiting => (
                if a { "#" } else { "\u{25c6}" },
                self.fg(Color::Blue).add_modifier(Modifier::BOLD),
            ),
            StepStatus::Failed => (if a { "x" } else { "\u{2717}" }, self.bad()),
            StepStatus::Skipped => ("-", self.dim()),
            StepStatus::Pending | StepStatus::Other => {
                (if a { "." } else { "\u{00b7}" }, self.dim())
            }
        }
    }

    pub fn check(&self, s: CheckState) -> (&'static str, Style) {
        let a = self.ascii;
        match s {
            CheckState::Pass => (if a { "v" } else { "\u{2713}" }, self.good()),
            CheckState::Fail => (if a { "x" } else { "\u{2717}" }, self.bad()),
            CheckState::Running => (if a { "*" } else { "\u{25d0}" }, self.fg(Color::Cyan)),
            CheckState::Queued => (if a { "o" } else { "\u{25cb}" }, self.fg(Color::Yellow)),
            CheckState::Skipped => ("-", self.dim()),
        }
    }

    pub fn g(&self, unicode: &'static str, ascii: &'static str) -> &'static str {
        if self.ascii { ascii } else { unicode }
    }

    pub fn flag(&self) -> &'static str {
        self.g("\u{2691}", "F")
    }
    pub fn warn_glyph(&self) -> &'static str {
        self.g("\u{25b2}", "^")
    }
    pub fn cursor(&self) -> &'static str {
        self.g("\u{25b8}", ">")
    }
    pub fn rail(&self) -> &'static str {
        self.g("\u{250a}", "|")
    }
    pub fn dot(&self) -> &'static str {
        self.g("\u{00b7}", ".")
    }
    pub fn refresh(&self) -> &'static str {
        self.g("\u{21bb}", "@")
    }
    pub fn busy(&self) -> &'static str {
        self.g("\u{25cf}", "*")
    }
    pub fn idle(&self) -> &'static str {
        self.g("\u{25cb}", "o")
    }
    pub fn bar_full(&self) -> &'static str {
        self.g("\u{2501}", "#")
    }
    pub fn bar_empty(&self) -> &'static str {
        self.g("\u{2500}", "-")
    }

    pub fn border_set(&self) -> border::Set<'static> {
        if self.ascii {
            border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            }
        } else {
            border::ROUNDED
        }
    }

    /// (left tee, horizontal, right tee) for separators inside a box.
    pub fn sep_set(&self) -> (&'static str, &'static str, &'static str) {
        if self.ascii {
            ("+", "-", "+")
        } else {
            ("\u{251c}", "\u{2500}", "\u{2524}")
        }
    }

    /// Bar colour for "used" percentages: higher is worse.
    pub fn used(&self, pct: f64) -> Style {
        if pct >= 85.0 {
            self.bad()
        } else if pct >= 60.0 {
            self.fg(Color::Yellow)
        } else {
            self.good()
        }
    }
}
