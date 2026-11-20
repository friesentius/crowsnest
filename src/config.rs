//! Crowsnest's own settings, kept in `$XDG_CONFIG_HOME/crowsnest/config.toml`.
//!
//! The file is optional and only ever written by the options menu. Loading is
//! tolerant: an unknown or invalid entry is reported and ignored, and a file
//! that is not valid TOML is reported and left alone. Saving edits the file in
//! place, so comments and entries crowsnest does not know survive.

use crate::model::{RunnerThresholds, Thresholds};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, Item, TableLike, Value};

/// A panel the dashboard can show or hide, numbered by its hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Panel {
    Roster,
    Runners,
    Host,
    Usage,
    Held,
}

impl Panel {
    pub const ALL: [Panel; 5] = [
        Panel::Roster,
        Panel::Runners,
        Panel::Host,
        Panel::Usage,
        Panel::Held,
    ];

    /// The hotkey, 1 to 5.
    pub fn key(self) -> u8 {
        self as u8 + 1
    }

    pub fn from_key(c: char) -> Option<Panel> {
        let n = c.to_digit(10)? as usize;
        Panel::ALL.get(n.checked_sub(1)?).copied()
    }

    /// The name used in `config.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Panel::Roster => "roster",
            Panel::Runners => "runners",
            Panel::Host => "host",
            Panel::Usage => "usage",
            Panel::Held => "held",
        }
    }

    /// The name shown on screen.
    pub fn title(self) -> &'static str {
        match self {
            Panel::Held => "held calls",
            p => p.name(),
        }
    }

    pub fn from_name(name: &str) -> Option<Panel> {
        Panel::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// A single-valued setting: a number, or the ASCII switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Setting {
    StaleMinutes,
    ParkedMinutes,
    WaitGraceMinutes,
    RunnerSilentMinutes,
    RunnerOverdueFactor,
    RunnerPickupMinutes,
    RunnerIdleCpuSeconds,
    FleetSeconds,
    PipelineSeconds,
    Ascii,
}

impl Setting {
    pub const ALL: [Setting; 10] = [
        Setting::StaleMinutes,
        Setting::ParkedMinutes,
        Setting::WaitGraceMinutes,
        Setting::RunnerSilentMinutes,
        Setting::RunnerOverdueFactor,
        Setting::RunnerPickupMinutes,
        Setting::RunnerIdleCpuSeconds,
        Setting::FleetSeconds,
        Setting::PipelineSeconds,
        Setting::Ascii,
    ];

    pub fn section(self) -> &'static str {
        match self {
            Setting::FleetSeconds | Setting::PipelineSeconds => "refresh",
            Setting::Ascii => "display",
            _ => "thresholds",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Setting::StaleMinutes => "stale_minutes",
            Setting::ParkedMinutes => "parked_minutes",
            Setting::WaitGraceMinutes => "wait_grace_minutes",
            Setting::RunnerSilentMinutes => "runner_silent_minutes",
            Setting::RunnerOverdueFactor => "runner_overdue_factor",
            Setting::RunnerPickupMinutes => "runner_pickup_minutes",
            Setting::RunnerIdleCpuSeconds => "runner_idle_cpu_seconds",
            Setting::FleetSeconds => "fleet_seconds",
            Setting::PipelineSeconds => "pipeline_seconds",
            Setting::Ascii => "ascii",
        }
    }

    /// The command-line flag that overrides it.
    pub fn flag(self) -> &'static str {
        match self {
            Setting::StaleMinutes => "--stale-minutes",
            Setting::ParkedMinutes => "--parked-minutes",
            Setting::WaitGraceMinutes => "--wait-grace-minutes",
            Setting::RunnerSilentMinutes => "--runner-silent-minutes",
            Setting::RunnerOverdueFactor => "--runner-overdue-factor",
            Setting::RunnerPickupMinutes => "--runner-pickup-minutes",
            Setting::RunnerIdleCpuSeconds => "--runner-idle-cpu-seconds",
            Setting::FleetSeconds => "--fleet-interval",
            Setting::PipelineSeconds => "--pipeline-interval",
            Setting::Ascii => "--ascii",
        }
    }

    pub fn is_bool(self) -> bool {
        self == Setting::Ascii
    }

    /// Values are whole numbers in units of `1 / scale()`: decimal settings
    /// are kept in hundredths, so they compare and save exactly.
    pub fn scale(self) -> u64 {
        match self {
            Setting::RunnerOverdueFactor | Setting::RunnerIdleCpuSeconds => 100,
            _ => 1,
        }
    }

    /// The values a number may take in the file, inclusive, in units of
    /// `1 / scale()`. The lower bounds match what the command line allows.
    pub fn range(self) -> (u64, u64) {
        match self {
            Setting::StaleMinutes | Setting::ParkedMinutes => (1, 1440),
            Setting::WaitGraceMinutes => (0, 1440),
            Setting::RunnerSilentMinutes | Setting::RunnerPickupMinutes => (1, 1440),
            Setting::RunnerOverdueFactor => (100, 10_000),
            Setting::RunnerIdleCpuSeconds => (0, 360_000),
            Setting::FleetSeconds => (10, 3600),
            Setting::PipelineSeconds => (5, 3600),
            Setting::Ascii => (0, 1),
        }
    }

    /// The values the options menu steps through, in units of `1 / scale()`.
    pub fn steps(self) -> &'static [u64] {
        match self {
            Setting::StaleMinutes => &[5, 10, 15, 20, 30, 45, 60, 90, 120, 180, 240, 480],
            Setting::ParkedMinutes => &[1, 2, 3, 5, 10, 15, 20, 30, 45, 60, 120],
            Setting::WaitGraceMinutes => &[0, 1, 2, 5, 10, 15, 30, 60],
            Setting::RunnerSilentMinutes => &[2, 3, 5, 10, 15, 20, 30, 60],
            Setting::RunnerOverdueFactor => &[125, 150, 175, 200, 250, 300, 400, 500],
            Setting::RunnerPickupMinutes => &[1, 2, 3, 5, 10, 15, 30, 60],
            Setting::RunnerIdleCpuSeconds => &[0, 50, 100, 200, 500, 1000, 3000],
            Setting::FleetSeconds => &[10, 15, 20, 30, 45, 60, 90, 120, 300, 600],
            Setting::PipelineSeconds => &[5, 10, 15, 20, 30, 60, 120, 300],
            Setting::Ascii => &[0, 1],
        }
    }

    /// A value as written: `30`, or `2.5` for a decimal setting.
    pub fn show(self, v: u64) -> String {
        if self.scale() == 1 {
            return v.to_string();
        }
        let text = format!("{:.2}", v as f64 / self.scale() as f64);
        // Keep one decimal: 2.0, 2.5, 1.25.
        let trimmed = text.trim_end_matches('0');
        if trimmed.ends_with('.') {
            format!("{trimmed}0")
        } else {
            trimmed.into()
        }
    }

    /// The next step above (`up`) or below the current value, if any.
    pub fn step(self, current: u64, up: bool) -> Option<u64> {
        let steps = self.steps();
        if up {
            steps.iter().copied().find(|v| *v > current)
        } else {
            steps.iter().rev().copied().find(|v| *v < current)
        }
    }

    pub fn get(self, s: &Settings) -> u64 {
        match self {
            Setting::StaleMinutes => s.stale_minutes,
            Setting::ParkedMinutes => s.parked_minutes,
            Setting::WaitGraceMinutes => s.wait_grace_minutes,
            Setting::RunnerSilentMinutes => s.runner_silent_minutes,
            Setting::RunnerOverdueFactor => s.runner_overdue_hundredths,
            Setting::RunnerPickupMinutes => s.runner_pickup_minutes,
            Setting::RunnerIdleCpuSeconds => s.runner_idle_cpu_hundredths,
            Setting::FleetSeconds => s.fleet_seconds,
            Setting::PipelineSeconds => s.pipeline_seconds,
            Setting::Ascii => s.ascii as u64,
        }
    }

    pub fn set(self, s: &mut Settings, v: u64) {
        match self {
            Setting::StaleMinutes => s.stale_minutes = v,
            Setting::ParkedMinutes => s.parked_minutes = v,
            Setting::WaitGraceMinutes => s.wait_grace_minutes = v,
            Setting::RunnerSilentMinutes => s.runner_silent_minutes = v,
            Setting::RunnerOverdueFactor => s.runner_overdue_hundredths = v,
            Setting::RunnerPickupMinutes => s.runner_pickup_minutes = v,
            Setting::RunnerIdleCpuSeconds => s.runner_idle_cpu_hundredths = v,
            Setting::FleetSeconds => s.fleet_seconds = v,
            Setting::PipelineSeconds => s.pipeline_seconds = v,
            Setting::Ascii => s.ascii = v != 0,
        }
    }
}

/// Everything the options menu can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub hidden_panels: BTreeSet<Panel>,
    /// Usage entries to hide: a provider (`copilot`), or one window of it by
    /// quota-axi's window id (`copilot/premium_interactions`).
    pub hidden_usage: BTreeSet<String>,
    pub stale_minutes: u64,
    pub parked_minutes: u64,
    pub wait_grace_minutes: u64,
    pub runner_silent_minutes: u64,
    /// The runner overdue factor, in hundredths.
    pub runner_overdue_hundredths: u64,
    pub runner_pickup_minutes: u64,
    /// The runner idle CPU allowance in seconds, in hundredths.
    pub runner_idle_cpu_hundredths: u64,
    pub fleet_seconds: u64,
    pub pipeline_seconds: u64,
    pub ascii: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let runner = RunnerThresholds::default();
        Settings {
            hidden_panels: BTreeSet::new(),
            hidden_usage: BTreeSet::new(),
            stale_minutes: 30,
            parked_minutes: (Thresholds::DEFAULT_PARKED_SECS / 60) as u64,
            wait_grace_minutes: (Thresholds::DEFAULT_WAIT_GRACE_SECS / 60) as u64,
            runner_silent_minutes: (runner.silent_secs / 60) as u64,
            runner_overdue_hundredths: (runner.overdue_factor * 100.0).round() as u64,
            runner_pickup_minutes: (runner.pickup_secs / 60) as u64,
            runner_idle_cpu_hundredths: runner.idle_cpu_ms / 10,
            fleet_seconds: 60,
            pipeline_seconds: 15,
            ascii: false,
        }
    }
}

impl Settings {
    pub fn thresholds(&self) -> Thresholds {
        Thresholds {
            stale_secs: self.stale_minutes as i64 * 60,
            parked_secs: self.parked_minutes as i64 * 60,
            wait_grace_secs: self.wait_grace_minutes as i64 * 60,
        }
    }

    pub fn runner_thresholds(&self) -> RunnerThresholds {
        RunnerThresholds {
            silent_secs: self.runner_silent_minutes as i64 * 60,
            overdue_factor: self.runner_overdue_hundredths as f64 / 100.0,
            pickup_secs: self.runner_pickup_minutes as i64 * 60,
            idle_cpu_ms: self.runner_idle_cpu_hundredths * 10,
        }
    }

    pub fn shown(&self, p: Panel) -> bool {
        !self.hidden_panels.contains(&p)
    }

    /// Whether a usage window is hidden, by its provider or by itself.
    pub fn usage_hidden(&self, provider: &str, window: &str) -> bool {
        self.hidden_usage.contains(provider)
            || self.hidden_usage.contains(&usage_key(provider, window))
    }
}

/// The `[usage] hidden` entry for one window.
pub fn usage_key(provider: &str, window: &str) -> String {
    format!("{provider}/{window}")
}

/// Settings given on the command line for this run. They win over the file,
/// and the options menu shows them as locked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides(BTreeMap<Setting, u64>);

impl Overrides {
    pub fn set(&mut self, s: Setting, v: u64) {
        self.0.insert(s, v);
    }

    pub fn get(&self, s: Setting) -> Option<u64> {
        self.0.get(&s).copied()
    }

    /// The settings this run uses: the file's, with the overrides on top.
    pub fn apply(&self, file: &Settings) -> Settings {
        let mut s = file.clone();
        for (k, v) in &self.0 {
            k.set(&mut s, *v);
        }
        s
    }
}

/// The location: `$XDG_CONFIG_HOME/crowsnest/config.toml`, or
/// `~/.config/crowsnest/config.toml`. Always absolute: a relative
/// `XDG_CONFIG_HOME` is ignored, and a relative `HOME` gives none.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .filter(|p| p.is_absolute())?;
    Some(base.join("crowsnest").join("config.toml"))
}

/// What reading the file found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    pub settings: Settings,
    /// Entries that were ignored, and why. Empty for a missing file.
    pub problems: Vec<String>,
}

/// Read the settings at `path`. A missing file gives the defaults; nothing
/// here can fail, every problem is reported in [`Loaded::problems`].
pub fn load(path: &Path) -> Loaded {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::default(),
        Err(e) => Loaded {
            problems: vec![format!("cannot read {}: {e}", file_name(path))],
            ..Default::default()
        },
    }
}

/// Parse the text of a config file.
pub fn parse(text: &str) -> Loaded {
    let mut out = Loaded::default();
    let doc = match text.parse::<DocumentMut>() {
        Ok(d) => d,
        Err(e) => {
            out.problems.push(syntax_error(text, &e));
            return out;
        }
    };
    let s = &mut out.settings;
    let problems = &mut out.problems;
    for (name, item) in doc.iter() {
        let Some(table) = item.as_table_like() else {
            problems.push(format!("{name} must be a table, like [{name}]"));
            continue;
        };
        match name {
            "panels" => {
                for (key, item) in table.iter() {
                    if key != "hidden" {
                        problems.push(unknown(name, key));
                    } else {
                        names(item, "panels.hidden", problems, |n, problems| {
                            match Panel::from_name(&n) {
                                Some(p) => {
                                    s.hidden_panels.insert(p);
                                }
                                None => problems.push(format!(
                                    "panels.hidden: unknown panel \"{n}\" (roster, runners, host, usage, held)"
                                )),
                            }
                        });
                    }
                }
            }
            "usage" => {
                for (key, item) in table.iter() {
                    if key != "hidden" {
                        problems.push(unknown(name, key));
                    } else {
                        names(item, "usage.hidden", problems, |e, problems| {
                            if valid_usage_entry(&e) {
                                s.hidden_usage.insert(e);
                            } else {
                                problems.push(format!(
                                    "usage.hidden: \"{e}\" is not a provider or provider/window"
                                ));
                            }
                        });
                    }
                }
            }
            "thresholds" | "refresh" | "display" => scalars(name, table, s, problems),
            _ => problems.push(format!("unknown section [{name}]")),
        }
    }
    out
}

fn unknown(section: &str, key: &str) -> String {
    format!("unknown key {section}.{key}")
}

/// A one-line description of a TOML syntax error.
fn syntax_error(text: &str, e: &toml_edit::TomlError) -> String {
    let line = e
        .span()
        .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
    let msg = e.message().lines().next().unwrap_or("invalid TOML").trim();
    match line {
        Some(l) => format!("not valid TOML (line {l}: {msg}), using defaults"),
        None => format!("not valid TOML ({msg}), using defaults"),
    }
}

fn valid_usage_entry(e: &str) -> bool {
    match e.split_once('/') {
        Some((p, w)) => !p.trim().is_empty() && !w.trim().is_empty(),
        None => !e.trim().is_empty(),
    }
}

/// Hand each string of an array to `each`, in order, reporting anything
/// else.
fn names(
    item: &Item,
    what: &str,
    problems: &mut Vec<String>,
    mut each: impl FnMut(String, &mut Vec<String>),
) {
    let Some(arr) = item.as_array() else {
        problems.push(format!(
            "{what} must be a list of names, like [\"a\", \"b\"]"
        ));
        return;
    };
    for v in arr.iter() {
        match v.as_str() {
            Some(s) => each(s.to_string(), problems),
            None => problems.push(format!(
                "{what}: {} is not a name in quotes",
                v.to_string().trim()
            )),
        }
    }
}

fn scalars(section: &str, table: &dyn TableLike, s: &mut Settings, problems: &mut Vec<String>) {
    for (key, item) in table.iter() {
        let Some(setting) = Setting::ALL
            .into_iter()
            .find(|x| x.section() == section && x.key() == key)
        else {
            problems.push(unknown(section, key));
            continue;
        };
        let name = format!("{section}.{key}");
        if setting.is_bool() {
            match item.as_bool() {
                Some(b) => setting.set(s, b as u64),
                None => problems.push(format!("{name} must be true or false")),
            }
            continue;
        }
        let (lo, hi) = setting.range();
        let scale = setting.scale();
        // A decimal setting takes a whole number too; a whole one does not
        // take a decimal.
        let value = match (item.as_integer(), item.as_float()) {
            (Some(n), _) => Some(n as f64 * scale as f64),
            (None, Some(f)) if scale > 1 => Some((f * scale as f64).round()),
            _ => None,
        };
        match value {
            Some(v) if v.is_finite() && v >= lo as f64 && v <= hi as f64 => {
                setting.set(s, v as u64)
            }
            _ => problems.push(format!(
                "{name} must be a {} from {} to {}",
                if scale > 1 { "number" } else { "whole number" },
                setting.show(lo),
                setting.show(hi)
            )),
        }
    }
}

const HEADER: &str = "\
# crowsnest settings, written by its options menu (press o).
# Every key is optional. The README lists what each one does.
";

/// The text of the config file after a change from `before` to `after`,
/// starting from `existing` so its comments, the entries crowsnest does not
/// know, and any entry whose setting did not change are kept exactly as
/// written. A new file gets every key. Fails only when `existing` is not
/// valid TOML, which is never overwritten.
pub fn render(
    existing: Option<&str>,
    before: &Settings,
    after: &Settings,
) -> Result<String, String> {
    let mut doc = match existing {
        Some(text) => text
            .parse::<DocumentMut>()
            .map_err(|e| syntax_error(text, &e).replace(", using defaults", ""))?,
        None => DocumentMut::new(),
    };
    let all = existing.is_none();
    let names =
        |it: &mut dyn Iterator<Item = &str>| -> Value { Value::Array(it.collect::<Array>()) };
    if all || before.hidden_panels != after.hidden_panels {
        set(
            &mut doc,
            "panels",
            "hidden",
            names(&mut after.hidden_panels.iter().map(|p| p.name())),
        );
    }
    if all || before.hidden_usage != after.hidden_usage {
        set(
            &mut doc,
            "usage",
            "hidden",
            names(&mut after.hidden_usage.iter().map(String::as_str)),
        );
    }
    for setting in Setting::ALL {
        if !all && setting.get(before) == setting.get(after) {
            continue;
        }
        let v = setting.get(after);
        let v = if setting.is_bool() {
            Value::from(v != 0)
        } else if setting.scale() > 1 {
            Value::from(v as f64 / setting.scale() as f64)
        } else {
            Value::from(v as i64)
        };
        set(&mut doc, setting.section(), setting.key(), v);
    }
    Ok(match existing {
        Some(_) => doc.to_string(),
        None => format!("{HEADER}\n{doc}"),
    })
}

/// Set `section.key`, keeping the key and the comments above and after it.
fn set(doc: &mut DocumentMut, section: &str, key: &str, mut value: Value) {
    let entry = doc.entry(section).or_insert_with(toml_edit::table);
    if entry.as_table_like().is_none() {
        *entry = toml_edit::table();
    }
    let table = entry.as_table_like_mut().expect("just made a table");
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        Some(item) => *item = Item::Value(value),
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

/// A path as errors name it: the options menu already shows where it is.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

/// Save a change from `before` to `after` to `path`: create the directory
/// and file on first save, rewrite only the entries whose setting changed,
/// and replace the file in one rename so a reader never sees half of it. A
/// symlinked file is written through to its target, keeping its permissions.
pub fn save(path: &Path, before: &Settings, after: &Settings) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {}: {e}", file_name(path))),
    };
    let text = render(existing.as_deref(), before, after)?;
    if existing.as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    let target = match existing {
        Some(_) => std::fs::canonicalize(path)
            .map_err(|e| format!("cannot resolve {}: {e}", file_name(path)))?,
        None => path.to_path_buf(),
    };
    let dir = target
        .parent()
        .ok_or_else(|| format!("{} has no directory", target.display()))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create the directory for {}: {e}", file_name(path)))?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|()| match std::fs::metadata(&target) {
            Ok(m) => std::fs::set_permissions(&tmp, m.permissions()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .and_then(|()| std::fs::rename(&tmp, &target))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot save {}: {e}", file_name(path))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panels_map_to_their_hotkeys() {
        for p in Panel::ALL {
            let c = char::from_digit(p.key() as u32, 10).unwrap();
            assert_eq!(Panel::from_key(c), Some(p));
            assert_eq!(Panel::from_name(p.name()), Some(p));
        }
        assert_eq!(Panel::from_key('0'), None);
        assert_eq!(Panel::from_key('6'), None);
    }

    #[test]
    fn steps_move_to_the_next_preset_from_any_value() {
        let s = Setting::StaleMinutes;
        assert_eq!(s.step(30, true), Some(45));
        assert_eq!(s.step(30, false), Some(20));
        assert_eq!(
            s.step(33, true),
            Some(45),
            "a hand-written value joins the steps"
        );
        assert_eq!(s.step(33, false), Some(30));
        assert_eq!(s.step(480, true), None);
        assert_eq!(s.step(5, false), None);
    }

    #[test]
    fn overrides_win_over_the_file() {
        let mut o = Overrides::default();
        o.set(Setting::StaleMinutes, 45);
        o.set(Setting::Ascii, 1);
        let s = o.apply(&Settings::default());
        assert_eq!(s.stale_minutes, 45);
        assert!(s.ascii);
        assert_eq!(s.parked_minutes, 10);
    }

    #[test]
    fn a_new_file_has_every_key() {
        let d = Settings::default();
        let text = render(None, &d, &d).unwrap();
        assert!(text.starts_with("# crowsnest settings"), "{text}");
        for s in Setting::ALL {
            assert!(text.contains(&format!("\n{} = ", s.key())), "{text}");
        }
        assert!(text.contains("\n\n[panels]\nhidden = []\n"), "{text}");
        assert_eq!(parse(&text), Loaded::default());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crowsnest-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let l = load(&scratch("missing").join("config.toml"));
        assert_eq!(l, Loaded::default());
    }

    #[test]
    fn save_creates_the_file_and_loads_back() {
        let dir = scratch("roundtrip");
        let path = dir.join("crowsnest").join("config.toml");
        let mut s = Settings::default();
        s.hidden_panels.insert(Panel::Runners);
        s.hidden_panels.insert(Panel::Held);
        s.hidden_usage.insert("copilot/premium_interactions".into());
        s.hidden_usage.insert("codex".into());
        s.stale_minutes = 45;
        s.wait_grace_minutes = 0;
        s.fleet_seconds = 120;
        s.ascii = true;
        save(&path, &Settings::default(), &s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hidden = [\"runners\", \"held\"]"), "{text}");
        assert_eq!(
            load(&path),
            Loaded {
                settings: s.clone(),
                problems: vec![]
            }
        );
        // Saving what is already there leaves the file alone.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        save(&path, &s, &s).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "no temp file left"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_keeps_comments_and_unknown_entries() {
        let dir = scratch("keep");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = "\
# my settings
[thresholds]
stale_minutes = 20 # quiet fleet
future_knob = 3
# a calm gate
parked_minutes = 15

[usage]
hidden = [
  \"copilot/premium_interactions\", # free plan
]

[display]
theme = \"amber\"
";
        std::fs::write(&path, mine).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings.stale_minutes, 20);
        assert_eq!(
            loaded.problems,
            [
                "unknown key thresholds.future_knob",
                "unknown key display.theme"
            ]
        );
        let before = loaded.settings;
        let mut s = before.clone();
        s.stale_minutes = 30;
        s.hidden_panels.insert(Panel::Host);
        save(&path, &before, &s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(
                "# my settings\n[thresholds]\nstale_minutes = 30 # quiet fleet\nfuture_knob = 3\n# a calm gate\nparked_minutes = 15\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("hidden = [\n  \"copilot/premium_interactions\", # free plan\n]"),
            "an unchanged list keeps its comments: {text}"
        );
        assert!(text.contains("theme = \"amber\""), "{text}");
        let again = load(&path);
        assert_eq!(again.settings, s);
        assert_eq!(
            again.problems.len(),
            2,
            "unknown entries are still reported"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_entries_are_reported_and_ignored() {
        let l = parse(
            "\
display = true
[panels]
hidden = [\"runners\", \"graphs\", 4]
[usage]
hidden = \"copilot\"
[thresholds]
stale_minutes = \"soon\"
parked_minutes = 0
wait_grace_minutes = 2
[refresh]
fleet_seconds = 5
pipeline_seconds = 30.5
[colours]
accent = \"cyan\"
",
        );
        let mut want = Settings::default();
        want.hidden_panels.insert(Panel::Runners);
        want.wait_grace_minutes = 2;
        assert_eq!(l.settings, want, "the valid entries still apply");
        assert_eq!(
            l.problems,
            [
                "display must be a table, like [display]",
                "panels.hidden: unknown panel \"graphs\" (roster, runners, host, usage, held)",
                "panels.hidden: 4 is not a name in quotes",
                "usage.hidden must be a list of names, like [\"a\", \"b\"]",
                "thresholds.stale_minutes must be a whole number from 1 to 1440",
                "thresholds.parked_minutes must be a whole number from 1 to 1440",
                "refresh.fleet_seconds must be a whole number from 10 to 3600",
                "refresh.pipeline_seconds must be a whole number from 5 to 3600",
                "unknown section [colours]",
            ]
        );
        let bad = parse("[usage]\nhidden = [\"copilot/\", \"\", \"claude/five_hour\"]\n");
        assert_eq!(bad.problems.len(), 2);
        assert!(bad.settings.usage_hidden("claude", "five_hour"));
    }

    #[test]
    fn runner_thresholds_take_decimals() {
        let l = parse(
            "[thresholds]\nrunner_silent_minutes = 15\nrunner_overdue_factor = 2.5\nrunner_idle_cpu_seconds = 3\nrunner_pickup_minutes = 1.5\n",
        );
        assert_eq!(
            l.problems,
            ["thresholds.runner_pickup_minutes must be a whole number from 1 to 1440"]
        );
        let r = l.settings.runner_thresholds();
        assert_eq!(r.silent_secs, 15 * 60);
        assert_eq!(r.overdue_factor, 2.5);
        assert_eq!(r.idle_cpu_ms, 3000);
        assert_eq!(r.pickup_secs, 5 * 60, "the bad value is ignored");
        assert_eq!(
            Settings::default().runner_thresholds(),
            RunnerThresholds::default()
        );
        let text = render(None, &l.settings, &l.settings).unwrap();
        assert!(text.contains("runner_overdue_factor = 2.5\n"), "{text}");
        assert!(text.contains("runner_idle_cpu_seconds = 3.0\n"), "{text}");
        assert_eq!(parse(&text).settings, l.settings);
        let low = parse("[thresholds]\nrunner_overdue_factor = 0.5\n");
        assert_eq!(
            low.problems,
            ["thresholds.runner_overdue_factor must be a number from 1.0 to 100.0"]
        );
        assert_eq!(Setting::RunnerOverdueFactor.show(125), "1.25");
        assert_eq!(Setting::RunnerOverdueFactor.step(200, true), Some(250));
    }

    #[test]
    fn a_broken_file_is_reported_and_never_overwritten() {
        let dir = scratch("broken");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        let broken = "[panels]\nhidden = [\"runners\"\n[thresholds]\n";
        std::fs::write(&path, broken).unwrap();
        let l = load(&path);
        assert_eq!(l.settings, Settings::default());
        assert_eq!(l.problems.len(), 1);
        assert!(
            l.problems[0].starts_with("not valid TOML (line "),
            "{:?}",
            l.problems
        );
        let d = Settings::default();
        let err = save(&path, &d, &d).unwrap_err();
        assert!(err.starts_with("not valid TOML"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn invalid_entries_are_kept_until_their_own_setting_changes() {
        let dir = scratch("invalid-kept");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = "\
display = true
[panels]
hidden = [\"runners\", \"graphs\"] # graphs is coming
[thresholds]
runner_pickup_minutes = 1.5 # want 90s
";
        std::fs::write(&path, mine).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.problems.len(), 3, "{:?}", loaded.problems);
        let before = loaded.settings;

        // A change to another setting leaves every invalid entry as written.
        let mut s = before.clone();
        s.stale_minutes = 45;
        save(&path, &before, &s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("{mine}stale_minutes = 45\n"),
            "only the changed key is written"
        );
        assert_eq!(load(&path).problems, loaded.problems, "still reported");

        // Changing a setting rewrites its own entry, and only that one.
        let before = s.clone();
        s.runner_pickup_minutes = 10;
        save(&path, &before, &s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("runner_pickup_minutes = 10 # want 90s\n"),
            "{text}"
        );
        assert!(
            text.contains("hidden = [\"runners\", \"graphs\"] # graphs is coming\n"),
            "{text}"
        );
        assert!(text.starts_with("display = true\n"), "{text}");

        let before = s.clone();
        s.hidden_panels.remove(&Panel::Runners);
        save(&path, &before, &s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hidden = [] # graphs is coming\n"), "{text}");
        assert!(text.starts_with("display = true\n"), "{text}");

        let before = s.clone();
        s.ascii = true;
        save(&path, &before, &s).unwrap();
        let again = load(&path);
        assert_eq!(again.settings, s);
        assert!(again.problems.is_empty(), "{:?}", again.problems);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("symlink");
        let dotfiles = dir.join("dotfiles");
        let config = dir.join("config");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let real = dotfiles.join("config.toml");
        std::fs::write(&real, "[thresholds]\nstale_minutes = 30\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = config.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let before = load(&link).settings;
        let mut s = before.clone();
        s.stale_minutes = 45;
        save(&link, &before, &s).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is kept"
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "[thresholds]\nstale_minutes = 45\n"
        );
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(&dotfiles).unwrap().count(),
            1,
            "no temp file left"
        );
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
