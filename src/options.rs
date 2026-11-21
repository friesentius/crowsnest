//! What the options menu lists: its categories, the rows in each, and what
//! changing a row does. Pure, so key handling and drawing share one list.

use crate::config::{Overrides, Panel, Setting, Settings, usage_key};
use crate::model::QuotaWindow;
use crate::sources::runners::DEFAULT_NAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Panels,
    Usage,
    Thresholds,
    Refresh,
    Runners,
    Display,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Panels,
        Category::Usage,
        Category::Thresholds,
        Category::Refresh,
        Category::Runners,
        Category::Display,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Panels => "panels",
            Category::Usage => "usage",
            Category::Thresholds => "thresholds",
            Category::Refresh => "refresh",
            Category::Runners => "runners",
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

/// The three lists of the `[runners]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerList {
    /// `roots`: folders searched for runner directories.
    Roots,
    /// `names`: patterns for the folder names looked for.
    Names,
    /// `units`: runner directory = systemd unit.
    Units,
}

impl RunnerList {
    /// What an entry is called on screen.
    pub fn noun(self) -> &'static str {
        match self {
            RunnerList::Roots => "folder",
            RunnerList::Names => "name",
            RunnerList::Units => "unit",
        }
    }

    /// What the prompt asks for.
    pub fn prompt(self) -> &'static str {
        match self {
            RunnerList::Roots => "folder",
            RunnerList::Names => "name",
            RunnerList::Units => "DIR=UNIT",
        }
    }

    /// What the prompt accepts, for the line under it.
    pub fn help(self) -> &'static str {
        match self {
            RunnerList::Roots => {
                "An absolute path, or one under ~/. Its subfolders named like the names below are searched."
            }
            RunnerList::Names => "A folder name; * and ? match. Like actions-runner*.",
            RunnerList::Units => {
                "A runner directory and its systemd unit, like ~/actions-runner-app=actions-runner-app.service."
            }
        }
    }
}

/// What one row of the menu changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `[runners] processes`.
    RunnerProcesses,
    /// A saved entry of a runner list, by its key (the path, glob or
    /// directory). Changing the row removes it.
    RunnerEntry(RunnerList, String),
    /// The row that types a new entry.
    RunnerAdd(RunnerList),
    /// What a list searches while it has no entries; it cannot change.
    RunnerDefault(RunnerList),
    /// An entry a command-line flag gives for this run; it cannot change.
    RunnerFlag(RunnerList, String),
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
            Setting::RunnerSilentMinutes,
            Setting::RunnerOverdueFactor,
            Setting::RunnerPickupMinutes,
            Setting::RunnerIdleCpuSeconds,
        ]
        .into_iter()
        .map(|s| setting_item(s, file, overrides))
        .collect(),
        Category::Refresh => [
            Setting::FleetSeconds,
            Setting::PipelineSeconds,
            Setting::GithubSeconds,
        ]
        .into_iter()
        .map(|s| setting_item(s, file, overrides))
        .collect(),
        Category::Runners => runner_items(file, overrides),
        Category::Display => vec![setting_item(Setting::Ascii, file, overrides)],
    }
}

fn runner_items(file: &Settings, overrides: &Overrides) -> Vec<Item> {
    let r = &file.runners;
    let mut out = vec![Item {
        target: Target::RunnerProcesses,
        label: "from processes".into(),
        detail: String::new(),
        value: if r.processes { "on" } else { "off" }.into(),
        on: r.processes,
        locked: None,
        about: "Find runners from the Runner.Listener processes that are running, wherever they are, and read each one's systemd unit from its cgroup. A runner that has stopped is found only through a folder or a unit below."
            .into(),
    }];
    let entry = |list: RunnerList, key: &str, label: String, detail: String, about: String| Item {
        target: Target::RunnerEntry(list, key.to_string()),
        label,
        detail,
        value: "remove".into(),
        on: true,
        locked: None,
        about,
    };
    let add = |list: RunnerList, about: &str| Item {
        target: Target::RunnerAdd(list),
        label: format!("add {}", list.noun()),
        detail: String::new(),
        value: "add".into(),
        on: true,
        locked: None,
        about: about.into(),
    };
    let fixed = |list: RunnerList, key: &str, label: String, detail: String, flag: &str| Item {
        target: Target::RunnerFlag(list, key.to_string()),
        label,
        detail,
        value: "searched".into(),
        on: true,
        locked: Some(format!("set by {flag}")),
        about: format!(
            "Set by {flag} for this run. Flags win over config.toml, which keeps its own entries."
        ),
    };

    // Folders.
    if !overrides.runner_roots.is_empty() {
        for p in &overrides.runner_roots {
            let p = p.display().to_string();
            out.push(fixed(
                RunnerList::Roots,
                &p,
                format!("folder {p}"),
                "set by --runner-root".into(),
                "--runner-root",
            ));
        }
        for p in &r.roots {
            out.push(Item {
                on: false,
                ..entry(
                    RunnerList::Roots,
                    p,
                    format!("folder {p}"),
                    "not used this run".into(),
                    format!(
                        "{p} stays in config.toml but --runner-root replaces it for this run. Press \u{2192} to remove it."
                    ),
                )
            });
        }
    } else {
        if r.roots.is_empty() {
            out.push(Item {
                target: Target::RunnerDefault(RunnerList::Roots),
                label: "folder ~".into(),
                detail: "default".into(),
                value: "searched".into(),
                on: true,
                locked: Some("default".into()),
                about: "Searched while no folder is set: the subfolders of your home directory that are named like the names below. Add a folder to replace it."
                    .into(),
            });
        }
        for p in &r.roots {
            out.push(entry(
                RunnerList::Roots,
                p,
                format!("folder {}", p),
                String::new(),
                format!("{p} is searched for runner folders. Press \u{2192} to stop."),
            ));
        }
        out.push(add(
            RunnerList::Roots,
            "Press Enter, then type a folder to search for runner folders. The folders you add replace your home directory.",
        ));
    }

    // Names.
    if r.names.is_empty() {
        out.push(Item {
            target: Target::RunnerDefault(RunnerList::Names),
            label: format!("name {DEFAULT_NAME}"),
            detail: "default".into(),
            value: "searched".into(),
            on: true,
            locked: Some("default".into()),
            about: "A folder is taken for a runner's when its name matches (* and ? match anything) and it holds the .runner file. Add a name to replace this one."
                .into(),
        });
    }
    for g in &r.names {
        out.push(entry(
            RunnerList::Names,
            g,
            format!("name {g}"),
            String::new(),
            format!("Folders named {g} are looked for. Press \u{2192} to stop."),
        ));
    }
    out.push(add(
        RunnerList::Names,
        "Press Enter, then type a folder name to look for in the folders above. * and ? match. The names you add replace actions-runner*.",
    ));

    // Units.
    for (dir, unit) in &overrides.runner_units {
        let dir = dir.display().to_string();
        out.push(fixed(
            RunnerList::Units,
            &dir,
            format!("unit {dir} \u{2192} {unit}"),
            "set by --runner-unit".into(),
            "--runner-unit",
        ));
    }
    for (dir, unit) in &r.units {
        out.push(entry(
            RunnerList::Units,
            dir,
            format!("unit {} \u{2192} {unit}", dir),
            String::new(),
            format!(
                "The runner in {dir} runs as the systemd unit {unit}, and is found even when it has stopped. Press \u{2192} to remove."
            ),
        ));
    }
    out.push(add(
        RunnerList::Units,
        "Press Enter, then type DIR=UNIT, like ~/actions-runner-app=actions-runner-app.service. Without one, the unit comes from the running listener's cgroup, else from the .service file svc.sh install wrote.",
    ));
    out
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
                    "Only the {} window of {p}. quota-axi reports what is left, so a window your plan does not include (0 of 0) reads as 100% used.",
                    w.label
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
        Setting::StaleMinutes
        | Setting::ParkedMinutes
        | Setting::WaitGraceMinutes
        | Setting::RunnerSilentMinutes
        | Setting::RunnerPickupMinutes => format!("{v} min"),
        Setting::RunnerOverdueFactor => format!("{}x", s.show(v)),
        Setting::RunnerIdleCpuSeconds => format!("{} s", s.show(v)),
        Setting::FleetSeconds | Setting::PipelineSeconds | Setting::GithubSeconds => {
            format!("{v} s")
        }
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
        Setting::RunnerSilentMinutes => (
            "runner stalled after",
            "A busy runner whose step output (or worker log) has not moved this long is shown as STALLED.",
        ),
        Setting::RunnerOverdueFactor => (
            "runner slow at",
            "A job running longer than this many times its typical duration is shown as SLOW.",
        ),
        Setting::RunnerPickupMinutes => (
            "runner pickup within",
            "A job queued this long while its runner is idle shows the runner as NOT PICKING UP.",
        ),
        Setting::RunnerIdleCpuSeconds => (
            "runner idle CPU",
            "CPU-seconds a silent job may use over the silent window and still count as idle, so STALLED. More means busy but quiet.",
        ),
        Setting::FleetSeconds => (
            "fleet every",
            "Time between fleet snapshots. Each one costs a few CPU-seconds.",
        ),
        Setting::PipelineSeconds => (
            "pipelines every",
            "Time between no-mistakes status reads for each live task.",
        ),
        Setting::GithubSeconds => (
            "GitHub every",
            "Time between GitHub reads while a PR, busy runner or queued job is on screen (5 min otherwise). Unchanged answers cost nothing, and reads slow down by themselves when the shared 5000/h budget runs low.",
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

fn remove(list: &mut Vec<String>, key: &str) -> bool {
    let before = list.len();
    list.retain(|x| x != key);
    list.len() != before
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
        Target::RunnerProcesses => file.runners.processes = !file.runners.processes,
        Target::RunnerEntry(list, key) => {
            let r = &mut file.runners;
            return match list {
                RunnerList::Roots => remove(&mut r.roots, key),
                RunnerList::Names => remove(&mut r.names, key),
                RunnerList::Units => r.units.remove(key).is_some(),
            };
        }
        Target::RunnerAdd(_) | Target::RunnerDefault(_) | Target::RunnerFlag(..) => return false,
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

    fn runner_rows(file: &Settings, o: &Overrides) -> Vec<(String, String, bool)> {
        items(Category::Runners, file, o, &[])
            .into_iter()
            .map(|r| (r.label, r.value, r.locked.is_some()))
            .collect()
    }

    fn row(label: &str, value: &str, locked: bool) -> (String, String, bool) {
        (label.into(), value.into(), locked)
    }

    #[test]
    fn runner_rows_say_what_is_searched_by_default() {
        assert_eq!(
            runner_rows(&Settings::default(), &Overrides::default()),
            [
                row("from processes", "on", false),
                row("folder ~", "searched", true),
                row("add folder", "add", false),
                row("name actions-runner*", "searched", true),
                row("add name", "add", false),
                row("add unit", "add", false),
            ]
        );
    }

    #[test]
    fn runner_rows_list_entries_and_changing_one_removes_it() {
        let mut file = Settings::default();
        file.runners.roots = vec!["/srv/ci".into()];
        file.runners.names = vec!["gh-*".into()];
        file.runners
            .units
            .insert("/srv/ci/gh-one".into(), "gh-one.service".into());
        let o = Overrides::default();
        assert_eq!(
            runner_rows(&file, &o),
            [
                row("from processes", "on", false),
                row("folder /srv/ci", "remove", false),
                row("add folder", "add", false),
                row("name gh-*", "remove", false),
                row("add name", "add", false),
                row(
                    "unit /srv/ci/gh-one \u{2192} gh-one.service",
                    "remove",
                    false
                ),
                row("add unit", "add", false),
            ],
            "a set list has no default row"
        );
        let remove = |t: Target, file: &mut Settings| change(&t, file, &o, true);
        assert!(remove(
            Target::RunnerEntry(RunnerList::Units, "/srv/ci/gh-one".into()),
            &mut file
        ));
        assert!(file.runners.units.is_empty());
        assert!(remove(
            Target::RunnerEntry(RunnerList::Roots, "/srv/ci".into()),
            &mut file
        ));
        assert!(remove(
            Target::RunnerEntry(RunnerList::Names, "gh-*".into()),
            &mut file
        ));
        assert_eq!(file.runners, Settings::default().runners);
        assert!(
            !remove(
                Target::RunnerEntry(RunnerList::Names, "gone".into()),
                &mut file
            ),
            "nothing to remove"
        );
        for fixed in [
            Target::RunnerAdd(RunnerList::Roots),
            Target::RunnerDefault(RunnerList::Names),
            Target::RunnerFlag(RunnerList::Units, "/x".into()),
        ] {
            assert!(!remove(fixed, &mut file), "typed or fixed, not toggled");
        }
        assert!(change(&Target::RunnerProcesses, &mut file, &o, false));
        assert!(!file.runners.processes);
    }

    #[test]
    fn runner_flags_show_as_locked_rows() {
        let mut file = Settings::default();
        file.runners.roots = vec!["/srv/ci".into()];
        let mut o = Overrides::default();
        o.runner_roots.push("/flag/root".into());
        o.runner_units
            .push(("/flag/root/a".into(), "a.service".into()));
        assert_eq!(
            runner_rows(&file, &o),
            [
                row("from processes", "on", false),
                row("folder /flag/root", "searched", true),
                row("folder /srv/ci", "remove", false),
                row("name actions-runner*", "searched", true),
                row("add name", "add", false),
                row("unit /flag/root/a \u{2192} a.service", "searched", true),
                row("add unit", "add", false),
            ],
            "a flag replaces the folders: no add row, the file's stay"
        );
        let rows = items(Category::Runners, &file, &o, &[]);
        assert_eq!(rows[1].detail, "set by --runner-root");
        assert_eq!(rows[2].detail, "not used this run");
        assert!(!rows[2].on, "drawn dimmed");
        // Locked rows do not change.
        assert!(!change(&rows[1].target, &mut file, &o, true));
    }
}
