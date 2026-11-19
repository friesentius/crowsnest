//! What the options menu lists: its categories, the rows in each, and what
//! changing a row does. Pure, so key handling and drawing share one list.

use crate::config::{Overrides, Panel, Setting, Settings, usage_key};
use crate::model::QuotaWindow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Panels,
    Usage,
    Thresholds,
    Refresh,
    Display,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Panels,
        Category::Usage,
        Category::Thresholds,
        Category::Refresh,
        Category::Display,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Panels => "panels",
            Category::Usage => "usage",
            Category::Thresholds => "thresholds",
            Category::Refresh => "refresh",
            Category::Display => "display",
        }
    }

    /// The next category (`forward`) or the previous one, wrapping around.
    pub fn cycle(self, forward: bool) -> Category {
        let i = Category::ALL.iter().position(|c| *c == self).unwrap_or(0);
        let n = Category::ALL.len();
        Category::ALL[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// What one row of the menu changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Panel(Panel),
    /// Every window of one usage provider.
    Provider(String),
    /// One usage window, by quota-axi's window id.
    Window {
        provider: String,
        id: String,
    },
    /// A `[usage] hidden` entry quota-axi is not reporting right now.
    Absent(String),
    Setting(Setting),
}

/// One row of the menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub target: Target,
    pub label: String,
    /// A note beside the value, such as how much of a window is used.
    pub detail: String,
    pub value: String,
    /// Shown or in use; a row that is not is drawn dimmed.
    pub on: bool,
    /// Why the row cannot be changed right now.
    pub locked: Option<String>,
    /// What the row does, for the description box.
    pub about: String,
}

fn shown(on: bool) -> String {
    if on { "shown" } else { "hidden" }.into()
}

/// The rows of `cat`. `file` is what the menu edits; `overrides` are the
/// command-line settings that win over it for this run.
pub fn items(
    cat: Category,
    file: &Settings,
    overrides: &Overrides,
    quota: &[QuotaWindow],
) -> Vec<Item> {
    match cat {
        Category::Panels => Panel::ALL
            .into_iter()
            .map(|p| Item {
                target: Target::Panel(p),
                label: p.title().into(),
                detail: String::new(),
                value: shown(file.shown(p)),
                on: file.shown(p),
                locked: None,
                about: panel_about(p).into(),
            })
            .collect(),
        Category::Usage => usage_items(file, quota),
        Category::Thresholds => [
            Setting::StaleMinutes,
            Setting::ParkedMinutes,
            Setting::WaitGraceMinutes,
        ]
        .into_iter()
        .map(|s| setting_item(s, file, overrides))
        .collect(),
        Category::Refresh => [Setting::FleetSeconds, Setting::PipelineSeconds]
            .into_iter()
            .map(|s| setting_item(s, file, overrides))
            .collect(),
        Category::Display => vec![setting_item(Setting::Ascii, file, overrides)],
    }
}

fn panel_about(p: Panel) -> &'static str {
    match p {
        Panel::Roster => {
            "The workers, most urgent first. Hide it to give the screen to the other panels. Keys 1-5 show and hide panels from the dashboard too."
        }
        Panel::Runners => {
            "Self-hosted CI runners and the jobs queued for them. Keys 1-5 show and hide panels from the dashboard too."
        }
        Panel::Host => {
            "Load, memory, swap, disks and pressure on this machine. Keys 1-5 show and hide panels from the dashboard too."
        }
        Panel::Usage => {
            "How much of each agent usage window is spent. Hide single providers or windows under usage. Keys 1-5 show and hide panels from the dashboard too."
        }
        Panel::Held => {
            "Calls held for you. It has room only at 140 columns or wider, or when the roster is hidden. Keys 1-5 show and hide panels from the dashboard too."
        }
    }
}

fn usage_items(file: &Settings, quota: &[QuotaWindow]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut providers: Vec<&str> = Vec::new();
    for w in quota {
        if !providers.contains(&w.provider.as_str()) {
            providers.push(&w.provider);
        }
    }
    for p in providers {
        let p_on = !file.hidden_usage.contains(p);
        out.push(Item {
            target: Target::Provider(p.into()),
            label: p.into(),
            detail: String::new(),
            value: shown(p_on),
            on: p_on,
            locked: None,
            about: format!("Every {p} window. Hide one window on its own below."),
        });
        for w in quota.iter().filter(|w| w.provider == p) {
            let on = !file.usage_hidden(p, &w.id);
            out.push(Item {
                target: Target::Window {
                    provider: p.into(),
                    id: w.id.clone(),
                },
                label: format!("  {}", w.label),
                detail: format!("{:.0}% used", w.pct_used),
                value: shown(on),
                on,
                locked: (!p_on).then(|| format!("{p} is hidden")),
                about: format!(
                    "{p}/{} on its own. quota-axi reports what is left, so a window your plan does not include (0 of 0) reads as 100% used.",
                    w.id
                ),
            });
        }
    }
    for key in &file.hidden_usage {
        let reported = quota
            .iter()
            .any(|w| w.provider == *key || usage_key(&w.provider, &w.id) == *key);
        if !reported {
            out.push(Item {
                target: Target::Absent(key.clone()),
                label: key.clone(),
                detail: "not reported".into(),
                value: shown(false),
                on: false,
                locked: None,
                about: "Hidden in config.toml, but quota-axi is not reporting it now. Change it to stop hiding it.".into(),
            });
        }
    }
    out
}

fn value_text(s: Setting, v: u64) -> String {
    match s {
        Setting::StaleMinutes | Setting::ParkedMinutes | Setting::WaitGraceMinutes => {
            format!("{v} min")
        }
        Setting::FleetSeconds | Setting::PipelineSeconds => format!("{v} s"),
        Setting::Ascii => if v != 0 { "ascii" } else { "unicode" }.into(),
    }
}

fn setting_item(s: Setting, file: &Settings, overrides: &Overrides) -> Item {
    let (label, about) = match s {
        Setting::StaleMinutes => (
            "stale after",
            "A working task with no status line or pipeline activity this long is shown as stale.",
        ),
        Setting::ParkedMinutes => (
            "gate parked after",
            "A no-mistakes gate that waits on its worker this long turns the row stale.",
        ),
        Setting::WaitGraceMinutes => (
            "wait grace",
            "How long after a worker's declared wait ends before its row is flagged.",
        ),
        Setting::FleetSeconds => (
            "fleet every",
            "Time between fleet snapshots. Each one costs a few CPU-seconds.",
        ),
        Setting::PipelineSeconds => (
            "pipelines every",
            "Time between no-mistakes status reads for each live task.",
        ),
        Setting::Ascii => (
            "glyphs",
            "Unicode, or ASCII borders and glyphs for fonts without box drawing.",
        ),
    };
    let pinned = overrides.get(s);
    let mut about = about.to_string();
    if pinned.is_some() {
        about.push_str(&format!(
            " Set by {} for this run; config.toml keeps {}.",
            s.flag(),
            value_text(s, s.get(file))
        ));
    }
    Item {
        target: Target::Setting(s),
        label: label.into(),
        detail: if pinned.is_some() {
            format!("set by {}", s.flag())
        } else {
            String::new()
        },
        value: value_text(s, pinned.unwrap_or(s.get(file))),
        on: true,
        locked: pinned.map(|_| format!("set by {}", s.flag())),
        about,
    }
}

/// Change the row `target` in `file`: toggle it, or step a number up or down.
/// Returns false when the row cannot change.
pub fn change(target: &Target, file: &mut Settings, overrides: &Overrides, up: bool) -> bool {
    fn toggle<T: Ord>(set: &mut std::collections::BTreeSet<T>, v: T) {
        if set.contains(&v) {
            set.remove(&v);
        } else {
            set.insert(v);
        }
    }
    match target {
        Target::Panel(p) => toggle(&mut file.hidden_panels, *p),
        Target::Provider(p) => toggle(&mut file.hidden_usage, p.clone()),
        Target::Window { provider, id } => {
            if file.hidden_usage.contains(provider) {
                return false;
            }
            toggle(&mut file.hidden_usage, usage_key(provider, id));
        }
        Target::Absent(key) => return file.hidden_usage.remove(key),
        Target::Setting(s) => {
            if overrides.get(*s).is_some() {
                return false;
            }
            let next = if s.is_bool() {
                Some(1 - s.get(file).min(1))
            } else {
                s.step(s.get(file), up)
            };
            match next {
                Some(v) => s.set(file, v),
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(provider: &str, id: &str, used: f64) -> QuotaWindow {
        QuotaWindow {
            provider: provider.into(),
            id: id.into(),
            label: id.replace('_', " "),
            pct_used: used,
            ..Default::default()
        }
    }

    #[test]
    fn usage_rows_group_windows_under_their_provider() {
        let q = [
            win("claude", "five_hour", 41.0),
            win("copilot", "premium_interactions", 100.0),
            win("claude", "seven_day", 35.0),
        ];
        let mut file = Settings::default();
        file.hidden_usage.insert("copilot".into());
        file.hidden_usage.insert("codex".into());
        let rows = items(Category::Usage, &file, &Overrides::default(), &q);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "claude",
                "  five hour",
                "  seven day",
                "copilot",
                "  premium interactions",
                "codex"
            ]
        );
        assert_eq!(rows[4].detail, "100% used");
        assert_eq!(rows[4].value, "hidden", "hidden with its provider");
        assert!(rows[4].locked.is_some());
        assert_eq!(rows[5].target, Target::Absent("codex".into()));
    }

    #[test]
    fn changing_rows() {
        let mut file = Settings::default();
        let none = Overrides::default();
        let window = Target::Window {
            provider: "copilot".into(),
            id: "premium_interactions".into(),
        };
        assert!(change(&window, &mut file, &none, true));
        assert!(file.usage_hidden("copilot", "premium_interactions"));
        assert!(change(
            &Target::Provider("copilot".into()),
            &mut file,
            &none,
            false
        ));
        assert!(
            !change(&window, &mut file, &none, true),
            "a window of a hidden provider is locked"
        );
        assert!(change(
            &Target::Absent("copilot".into()),
            &mut file,
            &none,
            true
        ));
        assert!(!file.hidden_usage.contains("copilot"));

        let stale = Target::Setting(Setting::StaleMinutes);
        assert!(change(&stale, &mut file, &none, true));
        assert_eq!(file.stale_minutes, 45);
        assert!(change(&stale, &mut file, &none, false));
        assert!(change(&stale, &mut file, &none, false));
        assert_eq!(file.stale_minutes, 20);
        let mut pinned = Overrides::default();
        pinned.set(Setting::StaleMinutes, 5);
        assert!(!change(&stale, &mut file, &pinned, true), "a flag locks it");
        assert_eq!(file.stale_minutes, 20);
        let row = &items(Category::Thresholds, &file, &pinned, &[])[0];
        assert_eq!(row.value, "5 min");
        assert_eq!(row.locked.as_deref(), Some("set by --stale-minutes"));

        assert!(change(
            &Target::Setting(Setting::Ascii),
            &mut file,
            &none,
            false
        ));
        assert!(file.ascii);
        assert!(change(&Target::Panel(Panel::Host), &mut file, &none, true));
        assert!(!file.shown(Panel::Host));
    }
}
