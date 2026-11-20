//! `crowsnest`: run it in a pane beside firstmate. It only reads.

use clap::Parser;
use crowsnest::app::{App, ConfigStatus, DEFAULT_READ_OVERDUE_SECS};
use crowsnest::collector::{self, Config};
use crowsnest::config::{self, Loaded, Overrides, Setting, Settings, inside};
use crowsnest::keys::{self, Mode};
use crowsnest::timefmt::now_epoch;
use crowsnest::ui::{self, View, theme::Theme};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::{execute, terminal::SetTitle};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Read-only terminal dashboard for a firstmate fleet: workers, no-mistakes
/// pipelines, PR checks, self-hosted runners, host health and agent usage.
///
/// Crowsnest never writes firstmate state, steers workers, or runs anything
/// that changes the fleet. To act, talk to firstmate.
///
/// Settings changed in the options menu (press o) are saved to
/// $XDG_CONFIG_HOME/crowsnest/config.toml (or ~/.config/crowsnest/config.toml),
/// which must be outside the firstmate home. The options below that also have
/// a setting win over the file for the run they are given on.
///
/// Self-hosted runners are found from the Runner.Listener processes that are
/// running, from the folders and names set under [runners] in the config file
/// (default: actions-runner* in your home directory), and from the runner
/// directories listed in its `units`.
#[derive(Parser, Debug)]
#[command(version, about, long_about)]
struct Args {
    /// Firstmate home to watch (the directory holding bin/fm-fleet-snapshot.sh).
    #[arg(long, env = "FM_HOME", value_name = "DIR")]
    fm_home: PathBuf,

    /// Minutes without a status line or pipeline activity before a working
    /// task is shown as stale [default: 30, or the config file's].
    #[arg(long, value_name = "MIN")]
    stale_minutes: Option<u64>,

    /// Minutes a no-mistakes gate may wait on its worker before the row is
    /// flagged and counted as needing attention [default: 10, or the config
    /// file's].
    #[arg(long, value_name = "MIN")]
    parked_minutes: Option<u64>,

    /// Minutes after a worker's declared "until" time before the row is
    /// flagged because nothing followed it [default: 5, or the config
    /// file's].
    #[arg(long, value_name = "MIN")]
    wait_grace_minutes: Option<u64>,

    /// Seconds between fleet snapshots, each of which costs a few CPU-seconds
    /// [default: 60, or the config file's; at least 10].
    #[arg(long, value_name = "SECS")]
    fleet_interval: Option<u64>,

    /// Seconds between no-mistakes status reads for each live task
    /// [default: 15, or the config file's; at least 5].
    #[arg(long, value_name = "SECS")]
    pipeline_interval: Option<u64>,

    /// Directory to search for actions-runner* directories (repeatable;
    /// replaces the config file's [runners] roots; default: your home
    /// directory). Runners whose Runner.Listener is running are found
    /// wherever they are.
    #[arg(long = "runner-root", value_name = "DIR")]
    runner_roots: Vec<PathBuf>,

    /// The systemd unit that runs a runner directory, for the runner detail's
    /// service line (repeatable; wins over the config file's [runners]
    /// units; default: the unit of the running listener's cgroup, else the
    /// one `svc.sh install` recorded).
    #[arg(long = "runner-unit", value_name = "DIR=UNIT", value_parser = parse_runner_unit)]
    runner_units: Vec<(PathBuf, String)>,

    /// Minutes a busy runner's step output (or, without it, its worker log)
    /// may stay still before the runner is shown as STALLED [default: 10, or
    /// the config file's].
    #[arg(long, value_name = "MIN")]
    runner_silent_minutes: Option<u64>,

    /// A job running longer than this many times its typical duration is
    /// shown as SLOW [default: 2.0, or the config file's].
    #[arg(long, value_name = "FACTOR", value_parser = parse_factor)]
    runner_overdue_factor: Option<f64>,

    /// CPU-seconds a silent job's processes may use over the silent window
    /// and still count as idle (STALLED); more means the job is busy but
    /// quiet, and it is not flagged [default: 2.0, or the config file's].
    #[arg(long, value_name = "SECS")]
    runner_idle_cpu_seconds: Option<f64>,

    /// Minutes a job may stay queued while its runner is idle before the
    /// runner is shown as NOT PICKING UP [default: 5, or the config file's].
    #[arg(long, value_name = "MIN")]
    runner_pickup_minutes: Option<u64>,

    /// Path whose filesystem use is shown (repeatable; default: / and your
    /// home directory).
    #[arg(long = "disk", value_name = "PATH")]
    disks: Vec<PathBuf>,

    /// Do not call GitHub (no PR checks, no runner queue, history or
    /// registration).
    #[arg(long)]
    no_github: bool,

    /// Do not read agent usage windows through quota-axi.
    #[arg(long)]
    no_quota: bool,

    /// Only show these quota-axi providers (repeatable, e.g. claude).
    #[arg(long = "quota-provider", value_name = "NAME")]
    quota_providers: Vec<String>,

    /// Use ASCII glyphs and borders instead of Unicode [default: the config
    /// file's].
    #[arg(long)]
    ascii: bool,

    /// Collect everything once, print it as JSON, and exit.
    #[arg(long)]
    once: bool,

    /// Crowsnest's own cache directory (default: $XDG_CACHE_HOME/crowsnest).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

fn parse_runner_unit(s: &str) -> Result<(PathBuf, String), String> {
    let (dir, unit) = s
        .rsplit_once('=')
        .ok_or("expected DIR=UNIT, e.g. ~/actions-runner-app=actions-runner-app.service")?;
    if dir.is_empty() || !crowsnest::exec::valid_unit(unit) {
        return Err(format!("not a runner directory and systemd unit: {s}"));
    }
    Ok((PathBuf::from(dir), unit.to_string()))
}

fn parse_factor(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() && f >= 1.0 => Ok(f),
        _ => Err(format!("expected a factor of at least 1, got {s}")),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The settings given on the command line.
fn overrides(args: &Args) -> Overrides {
    let mut o = Overrides::default();
    o.runner_roots.clone_from(&args.runner_roots);
    o.runner_units.clone_from(&args.runner_units);
    for (setting, value) in [
        (Setting::StaleMinutes, args.stale_minutes),
        (Setting::ParkedMinutes, args.parked_minutes),
        (Setting::WaitGraceMinutes, args.wait_grace_minutes),
        (Setting::RunnerSilentMinutes, args.runner_silent_minutes),
        (
            Setting::RunnerOverdueFactor,
            args.runner_overdue_factor.map(hundredths),
        ),
        (Setting::RunnerPickupMinutes, args.runner_pickup_minutes),
        (
            Setting::RunnerIdleCpuSeconds,
            args.runner_idle_cpu_seconds.map(|s| hundredths(s.max(0.0))),
        ),
        (Setting::FleetSeconds, args.fleet_interval),
        (Setting::PipelineSeconds, args.pipeline_interval),
        (Setting::Ascii, args.ascii.then_some(1)),
    ] {
        if let Some(v) = value {
            o.set(setting, v);
        }
    }
    o
}

/// A decimal flag in the hundredths its setting keeps.
fn hundredths(v: f64) -> u64 {
    (v * 100.0).round() as u64
}

fn fleet_interval(s: &Settings) -> Duration {
    Duration::from_secs(s.fleet_seconds.max(10))
}

fn pipeline_interval(s: &Settings) -> Duration {
    Duration::from_secs(s.pipeline_seconds.max(5))
}

/// A fleet read is overdue after three missed reads, and never sooner than
/// the default.
fn read_overdue_secs(s: &Settings) -> i64 {
    (fleet_interval(s).as_secs() as i64 * 3).max(DEFAULT_READ_OVERDUE_SECS)
}

/// The collector's configuration, and what is wrong with the config file's
/// runner paths.
fn config(
    args: &Args,
    settings: &Settings,
    overrides: &Overrides,
    config_file: Option<&Path>,
) -> Result<(Config, Vec<String>), String> {
    let snapshot = args.fm_home.join("bin").join("fm-fleet-snapshot.sh");
    if !snapshot.is_file() {
        return Err(format!(
            "{} is not a firstmate home: {} is missing",
            args.fm_home.display(),
            snapshot.display()
        ));
    }
    let cache_dir = args
        .cache_dir
        .clone()
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("crowsnest")))
        .or_else(|| home().map(|h| h.join(".cache").join("crowsnest")))
        .ok_or("cannot pick a cache directory: set --cache-dir")?
        .join("fm-summary-cache");
    let fm_home = args.fm_home.canonicalize().unwrap_or(args.fm_home.clone());
    if cache_dir.starts_with(&fm_home) {
        return Err("the cache directory must be outside the firstmate home".into());
    }
    if let Some(file) = config_file
        && inside(file, &fm_home)
    {
        return Err("the config file must be outside the firstmate home".into());
    }
    let (runners, runner_problems) =
        config::discovery(&settings.runners, overrides, &fm_home, home().as_deref());
    let disk_paths = if args.disks.is_empty() {
        std::iter::once(PathBuf::from("/")).chain(home()).collect()
    } else {
        args.disks.clone()
    };
    let cfg = Config {
        fm_home,
        cache_dir,
        runners,
        disk_paths,
        fleet_interval: fleet_interval(settings),
        pipeline_interval: pipeline_interval(settings),
        github: !args.no_github,
        quota: !args.no_quota,
        quota_providers: args.quota_providers.clone(),
    };
    Ok((cfg, runner_problems))
}

/// `path` with the home directory shown as `~`.
fn display_path(path: &Path) -> String {
    match home().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn run_tui(
    cfg: Config,
    loaded: Loaded,
    overrides: Overrides,
    config_file: Option<PathBuf>,
) -> std::io::Result<()> {
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let host = sysinfo::System::host_name().unwrap_or_default();
    let fm_home = cfg.fm_home.clone();
    let (tx, rx) = mpsc::channel();
    let (shared, handle) = collector::start(cfg, tx);
    let mut app = App::new(loaded.settings, overrides);
    app.fm_home = Some(fm_home.clone());
    app.home = home();
    app.config = ConfigStatus {
        path: config_file.as_deref().map(display_path).unwrap_or_default(),
        problems: loaded.problems,
        saved: None,
    };
    app.read_overdue_secs = read_overdue_secs(&app.effective());
    // The settings as last written to (or read from) the file. A save
    // writes every key that differs from these, so a change whose save
    // failed is written by the next save that works.
    let mut saved = app.settings.clone();
    let mut theme = Theme::new(color, app.effective().ascii);
    let mut terminal = ratatui::init();
    let mut last_title = usize::MAX;
    let mut last_draw = Instant::now() - Duration::from_secs(10);
    let mut dirty = true;
    let result = loop {
        while rx.try_recv().is_ok() {
            dirty = true;
        }
        if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
            let mut snap = shared.lock().unwrap().clone();
            snap.now = now_epoch();
            app.paused = handle.paused();
            app.observe(&snap);
            let asking = ui::asking(&snap, &app.thresholds);
            if asking != last_title {
                let title = if asking > 0 {
                    format!("crowsnest ({asking})")
                } else {
                    "crowsnest".into()
                };
                let _ = execute!(std::io::stdout(), SetTitle(title));
                last_title = asking;
            }
            let view = View {
                snap: &snap,
                app: &app,
                theme,
            };
            if let Err(e) = terminal.draw(|f| ui::draw(f, &view, &host)) {
                break Err(e);
            }
            last_draw = Instant::now();
            dirty = false;
        }
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let mode = if app.editing() {
                        Mode::Typing
                    } else if app.menu.is_some() {
                        Mode::Menu
                    } else {
                        Mode::Dashboard
                    };
                    if let Some(action) = keys::action(k.code, k.modifiers, mode) {
                        let mut snap = shared.lock().unwrap().clone();
                        snap.now = now_epoch();
                        let was_paused = app.paused;
                        let before = app.settings.clone();
                        if app.apply(action, &snap) {
                            handle.refresh();
                        }
                        if app.paused != was_paused {
                            handle.set_paused(app.paused);
                        }
                        if app.settings != before {
                            settings_changed(
                                &mut app,
                                &handle,
                                config_file.as_deref(),
                                &mut saved,
                                &fm_home,
                            );
                            theme = Theme::new(color, app.effective().ascii);
                        }
                        dirty = true;
                    }
                }
                Ok(Event::Resize(..)) => dirty = true,
                Ok(_) => {}
                Err(e) => break Err(e),
            },
            Ok(false) => {}
            Err(e) => break Err(e),
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = execute!(std::io::stdout(), SetTitle(""));
    ratatui::restore();
    result
}

/// Put changed settings into effect and save them. A failed save is shown in
/// the footer; the change still holds for this run.
fn settings_changed(
    app: &mut App,
    handle: &collector::Handle,
    file: Option<&Path>,
    saved: &mut Settings,
    fm_home: &Path,
) {
    let effective = app.effective();
    handle.set_intervals(fleet_interval(&effective), pipeline_interval(&effective));
    let (runners, runner_problems) = config::discovery(
        &app.settings.runners,
        &app.overrides,
        fm_home,
        home().as_deref(),
    );
    handle.set_discovery(runners);
    app.read_overdue_secs = read_overdue_secs(&effective);
    let result = match file {
        Some(path) => config::save(path, saved, &app.settings).map(|()| {
            *saved = app.settings.clone();
            // What is left of the file's problems after the save.
            app.config.problems = config::load(path).problems;
            app.config.problems.extend(runner_problems);
        }),
        None => Err("no config directory: set XDG_CONFIG_HOME or HOME".into()),
    };
    app.config.saved = Some((now_epoch(), result.err()));
}

fn main() -> ExitCode {
    let args = Args::parse();
    let config_file = config::default_path();
    let loaded = config_file.as_deref().map(config::load).unwrap_or_default();
    let overrides = overrides(&args);
    let settings = overrides.apply(&loaded.settings);
    let mut loaded = loaded;
    let cfg = match config(&args, &settings, &overrides, config_file.as_deref()) {
        Ok((c, problems)) => {
            loaded.problems.extend(problems);
            c
        }
        Err(e) => {
            eprintln!("crowsnest: {e}");
            return ExitCode::from(2);
        }
    };
    if args.once {
        // stdout is the JSON; the config file's problems go to stderr.
        for p in &loaded.problems {
            eprintln!("crowsnest: config: {p}");
        }
        let snap = collector::collect_once(&cfg);
        println!(
            "{}",
            serde_json::to_string_pretty(&snap).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }
    match run_tui(cfg, loaded, overrides, config_file) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crowsnest: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_flags_given_override_the_config() {
        let args = Args::try_parse_from(["crowsnest", "--fm-home", "/fm"]).unwrap();
        assert_eq!(overrides(&args), Overrides::default());
        let args = Args::try_parse_from([
            "crowsnest",
            "--fm-home",
            "/fm",
            "--stale-minutes",
            "45",
            "--fleet-interval",
            "5",
            "--ascii",
            "--runner-overdue-factor",
            "1.25",
            "--runner-idle-cpu-seconds=-1",
        ])
        .unwrap();
        let o = overrides(&args);
        assert_eq!(o.get(Setting::StaleMinutes), Some(45));
        assert_eq!(o.get(Setting::ParkedMinutes), None);
        assert_eq!(o.get(Setting::Ascii), Some(1));
        assert_eq!(o.get(Setting::RunnerOverdueFactor), Some(125));
        assert_eq!(o.get(Setting::RunnerIdleCpuSeconds), Some(0));
        assert_eq!(o.get(Setting::RunnerSilentMinutes), None);
        let s = o.apply(&Settings::default());
        assert_eq!(fleet_interval(&s), Duration::from_secs(10), "still clamped");
        assert_eq!(read_overdue_secs(&s), DEFAULT_READ_OVERDUE_SECS);
    }

    #[test]
    fn a_config_file_in_the_firstmate_home_is_refused() {
        let fm = Path::new("/srv/fm");
        assert!(inside(Path::new("/srv/fm/state/config.toml"), fm));
        assert!(!inside(
            Path::new("/home/x/.config/crowsnest/config.toml"),
            fm
        ));
    }
}
