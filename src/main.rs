//! `crowsnest`: run it in a pane beside firstmate. It only reads.

use clap::Parser;
use crowsnest::app::{Action, App, visible_rows};
use crowsnest::collector::{self, Config};
use crowsnest::timefmt::now_epoch;
use crowsnest::ui::{self, View, theme::Theme};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal::SetTitle};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Read-only terminal dashboard for a firstmate fleet: workers, no-mistakes
/// pipelines, PR checks, self-hosted runners, host health and agent usage.
///
/// Crowsnest never writes firstmate state, steers workers, or runs anything
/// that changes the fleet. To act, talk to firstmate.
#[derive(Parser, Debug)]
#[command(version, about, long_about)]
struct Args {
    /// Firstmate home to watch (the directory holding bin/fm-fleet-snapshot.sh).
    #[arg(long, env = "FM_HOME", value_name = "DIR")]
    fm_home: PathBuf,

    /// Minutes without a status line or pipeline activity before a working
    /// task is shown as stale.
    #[arg(long, default_value_t = 30, value_name = "MIN")]
    stale_minutes: u64,

    /// Seconds between fleet snapshots (each one costs a few CPU-seconds).
    #[arg(long, default_value_t = 60, value_name = "SECS")]
    fleet_interval: u64,

    /// Seconds between no-mistakes status reads for each live task.
    #[arg(long, default_value_t = 15, value_name = "SECS")]
    pipeline_interval: u64,

    /// Directory to search for actions-runner* directories (repeatable;
    /// default: your home directory).
    #[arg(long = "runner-root", value_name = "DIR")]
    runner_roots: Vec<PathBuf>,

    /// Path whose filesystem use is shown (repeatable; default: / and your
    /// home directory).
    #[arg(long = "disk", value_name = "PATH")]
    disks: Vec<PathBuf>,

    /// Do not call GitHub (no PR checks, no runner queue).
    #[arg(long)]
    no_github: bool,

    /// Do not read agent usage windows through quota-axi.
    #[arg(long)]
    no_quota: bool,

    /// Only show these quota-axi providers (repeatable, e.g. claude).
    #[arg(long = "quota-provider", value_name = "NAME")]
    quota_providers: Vec<String>,

    /// Use ASCII glyphs and borders instead of Unicode.
    #[arg(long)]
    ascii: bool,

    /// Collect everything once, print it as JSON, and exit.
    #[arg(long)]
    once: bool,

    /// Crowsnest's own cache directory (default: $XDG_CACHE_HOME/crowsnest).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn config(args: &Args) -> Result<Config, String> {
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
    let runner_roots = if args.runner_roots.is_empty() {
        home().into_iter().collect()
    } else {
        args.runner_roots.clone()
    };
    let disk_paths = if args.disks.is_empty() {
        std::iter::once(PathBuf::from("/")).chain(home()).collect()
    } else {
        args.disks.clone()
    };
    Ok(Config {
        fm_home,
        cache_dir,
        runner_roots,
        disk_paths,
        fleet_interval: Duration::from_secs(args.fleet_interval.max(10)),
        fleet_min_gap: Duration::from_secs(20),
        pipeline_interval: Duration::from_secs(args.pipeline_interval.max(5)),
        github: !args.no_github,
        quota: !args.no_quota,
        quota_providers: args.quota_providers.clone(),
    })
}

fn key_action(code: KeyCode, mods: KeyModifiers) -> Option<Action> {
    Some(match code {
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Home | KeyCode::Char('g') => Action::Top,
        KeyCode::End | KeyCode::Char('G') => Action::Bottom,
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => Action::Toggle,
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => Action::Collapse,
        KeyCode::Char('f') => Action::Filter,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('p') => Action::Pause,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('q') => Action::Quit,
        _ => return None,
    })
}

fn run_tui(cfg: Config, args: &Args) -> std::io::Result<()> {
    let theme = Theme::new(
        std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        args.ascii,
    );
    let stale_secs = (args.stale_minutes * 60) as i64;
    let host = sysinfo::System::host_name().unwrap_or_default();
    let (tx, rx) = mpsc::channel();
    let (shared, handle) = collector::start(cfg, tx);
    let mut app = App::new(stale_secs);
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
            let asking = ui::asking(&snap, stale_secs);
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
                    if let Some(action) = key_action(k.code, k.modifiers) {
                        let mut snap = shared.lock().unwrap().clone();
                        snap.now = now_epoch();
                        let rows = visible_rows(&snap, &app);
                        let was_paused = app.paused;
                        if app.apply(action, &rows) {
                            handle.refresh();
                        }
                        if app.paused != was_paused {
                            handle.set_paused(app.paused);
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

fn main() -> ExitCode {
    let args = Args::parse();
    let cfg = match config(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("crowsnest: {e}");
            return ExitCode::from(2);
        }
    };
    if args.once {
        let snap = collector::collect_once(&cfg);
        println!(
            "{}",
            serde_json::to_string_pretty(&snap).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }
    match run_tui(cfg, &args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crowsnest: {e}");
            ExitCode::FAILURE
        }
    }
}
