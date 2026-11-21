# crowsnest

A read-only terminal dashboard for a [firstmate](https://github.com/kunchenguid/firstmate) fleet.
Run it in a pane beside firstmate to see, at a glance, every crewmate, its no-mistakes pipeline, its pull request's checks, your self-hosted CI runners, the host's health, and how much of each agent usage window is spent.
It is written in Rust with [ratatui](https://ratatui.rs), and it is a single binary.

```text
╭ ¹crowsnest firstmate @ deckhand ────────────────────── 14:27:06 ↻ read 40s ago · changed 47s ago ╮
│  ? 1 asking you    ! 2 stale   ◆ 1 at gate   ◐ 4 working   ✓ 0 done   ○ 2 idle   ⚑ 8 held calls  │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│   STATE      TASK                  PROJECT        PIPELINE  STEP            PR · CI          AGE │
│ ▸ ? CAPTAIN  fleet-tui-dashboard   crowsnest      ········· not run         -                47s │
│              needs you (layout-pick): promoted to ship; building crowsnest v1 (… changed 47s ago │
│   ! STALE    bs-p1c-type-pairing   bookshop       ✓✓◆······ parked 1h17m    -              1h20m │
│              parked at review gate for 1h17m, waiting on the worker; last: pa… changed 1h17m ago │
│   ! STALE    pantry-nav-refresh    pantry                                   -                25m │
│              declared wait ended 25m ago; last: waiting for runner capacity bef… changed 25m ago │
│   ◆ GATE     bs-p1d-footer-links   bookshop       ✓✓◆······ parked 4m       #1234 ✓ 12/12  2h20m │
│              nm review gate: findings 2 awaiting, parked 4m                       changed 4m ago │
│   ◐ WORKING  pantry-tour-followups pantry         ✓✓✓✓✓✓✓✓◐ ci 35m          #122 ◐ 3/4       41m │
│              nm ci round 2: CI checks running, waiting for results... (34m ago)  changed 35m ago │
│   ◐ WORKING  bs-p1a-parity-copy    bookshop       ✓✓✓◐····· test 4m         -                 6m │
│              nm test starting: claude producing output (6s ago)                   changed 4m ago │
│   ◐ WORKING  bs-p1b-headers-hero   bookshop       ········· not run         -                 2m │
│              no-mistakes fix round, waiting until 14:36                           changed 2m ago │
│   ◐ WORKING  pantry-rss-feed       pantry         ✓✓✓✓◐···· document 1h05m  -                 1m │
│              nm document starting: claude producing output (6s ago)               changed 1m ago │
│   ○ IDLE     millwork              2nd mate                                 -                  - │
│              no active child work - pane gone                                                    │
│   ○ IDLE     timesheet             2nd mate                                 -                 9d │
│              no active child work - pane gone, summary 8d old                     changed 9d ago │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│ next pantry-release-0-4-0 - Next: pantry release 0 4 0                                           │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
╭ ²RUNNERS ✓ moving ──────── q1 ╮╭ ³HOST ─────────────────────────╮╭ ⁴USAGE ───────────────────────╮
│ ◐ pantry   ✓ moving 8m/40m    ││ load 3.42 2.95 2.71  16 cpu    ││ copilot premium … ━━━━━━ 100% │
│   └ Lint wait 6m → 14:58      ││ mem  ━━━━━━──  72% 9.0G avail  ││ claude session    ━━────  41% │
│ ○ bookshop idle               ││ swap ━━──────  25% 6.0G free   ││ claude week       ━━────  35% │
│                               ││ disk / 61%  /home 78%          ││ claude Fable week ━─────  11% │
│                               ││ psi  mem 0.4%  cpu 6.1%        ││ +2 more windows               │
╰───────────────────────────────╯╰────────────────────────────────╯╰───────────────────────────────╯
 ↑↓  select  ⏎  detail  tab  runners  o  options  ?  help  q  quit            nm 9s gh 25s quota 2m 
```

Workers that need you sort to the top and stand out.
The USAGE panel shows how much of each window is used (quota-axi reports what is left, crowsnest flips it), most-used first, with the reset time beside it.
A window being spent faster than its clock runs (quota-axi's pace `ahead`) will run out before it resets: its percentage turns yellow and gets a `▲`, and where there is room the reset time is replaced by when it runs out at its average rate so far (`86%▲  out Fri 12:18`). Windows on or under pace look as usual.
Press Enter on a row to open its detail: every pipeline step with its duration, each PR check, the last status line, and the worktree.
Press Tab to move to the runner panel, pick a runner with the arrows, and press Enter for its detail (see [Runners](#runners)).
At 140 columns or more the panels move to a right-hand column, and the held captain calls get their own box.
The small number before each panel's title is its key: press `1` to `5` to hide or show that panel, and the others take its room.
Press `o` for the options menu, where you can also hide single usage providers or windows and change the thresholds and refresh intervals (see [Settings](#settings)).

## Install

Crowsnest needs Rust 1.95 or newer.

```sh
cargo install --locked --git https://github.com/friesentius/crowsnest
```

## Run

```sh
crowsnest --fm-home ~/firstmate     # or: FM_HOME=~/firstmate crowsnest
```

The firstmate home is the directory that holds `bin/fm-fleet-snapshot.sh`.
There is no default path: crowsnest uses `--fm-home` or `FM_HOME`, and refuses to start if neither points at a firstmate home.

`crowsnest --help` owns the full list of options.
The ones you are most likely to want:

| Option | Default | What it does |
| --- | --- | --- |
| `--stale-minutes MIN` | 30 | How long a working task may go without a status line or pipeline activity before it is shown as stale. |
| `--parked-minutes MIN` | 10 | How long a no-mistakes gate may wait on its worker before the row is flagged stale and counted in the header. |
| `--wait-grace-minutes MIN` | 5 | How long after a worker's declared `until` time its row is flagged because nothing followed. |
| `--fleet-interval SECS` | 60 | Seconds between fleet snapshots. |
| `--pipeline-interval SECS` | 15 | Seconds between no-mistakes status reads for each live task. |
| `--github-interval SECS` | 60 | Seconds between GitHub reads while a PR, busy runner or queued job is on screen (at least 30; 5 minutes otherwise). |
| `--runner-root DIR` | your home | Folders to search for runner directories (repeatable); replaces the config file's `[runners] roots`. Runners whose listener is running are found wherever they are (see [Finding runners](#finding-runners)). |
| `--runner-silent-minutes MIN` | 10 | How long a busy runner's step output may stay still (with its job's processes idle) before it is shown as STALLED. |
| `--runner-idle-cpu-seconds SECS` | 2 | CPU-seconds a silent job's processes may use over that window and still count as idle. |
| `--runner-overdue-factor FACTOR` | 2.0 | A job running longer than this many times its typical duration is shown as SLOW. |
| `--runner-pickup-minutes MIN` | 5 | How long a job may stay queued while its runner is idle before the runner is shown as NOT PICKING UP. |
| `--runner-unit DIR=UNIT` | the running listener's unit, else the one `svc.sh install` recorded | The systemd unit that runs a runner directory, for the lane's restart count and the detail's service line (repeatable); wins over the config file's `[runners] units`. |
| `--disk PATH` | `/` and your home | Filesystems shown in the host panel (repeatable). |
| `--no-github`, `--no-quota` | | Skip the GitHub or quota-axi reads. |
| `--ascii` | | ASCII borders and glyphs, for fonts without box drawing. |
| `--once` | | Collect everything once, print it as JSON, and exit. |

The stale, parked, wait and runner thresholds, the two intervals, `--ascii`, and the runner folders and units are also [settings](#settings).
A flag wins over the config file for the run it is given on, and the options menu shows that setting as locked.

`NO_COLOR` is honoured.
Colours are the terminal's 16 ANSI colours, so they follow your dark or light theme, and every colour is paired with a glyph and a word.

## Keys

| Key | Action |
| --- | --- |
| `↑` `↓` or `j` `k` | Select a worker (`g` and `G`: first and last) |
| `Enter`, `l` or `→` | Open or close its detail |
| `Esc`, `h` or `←` | Close the detail |
| `Tab` | Move the keys to the runner panel and back; there `↑` `↓` pick a runner, `Enter` opens its detail, `Esc` goes back |
| `f` | Filter: all, active, needs you |
| `r` | Refresh everything now |
| `p` | Pause or resume automatic refresh |
| `o` or `F2` | Options menu |
| `1` to `5` | Hide or show the roster, runners, host, usage or held calls panel |
| `?` | Help |
| `q` or `Ctrl-C` | Quit |

There are deliberately no action keys.
To act on the fleet, talk to firstmate.

## Settings

Press `o` (or `F2`) for the options menu.
Like btop's, it lists categories on the left (panels, usage, thresholds, refresh, runners, display), the selected category's settings on the right, and what the selected setting does underneath.

| Key in the menu | Action |
| --- | --- |
| `↑` `↓` or `j` `k` | Select a setting (`g` and `G`: first and last) |
| `←` `→` or `h` `l` | Change it (`Enter` and `Space` work like `→`); on a saved runner entry this removes it, on an `add` row it opens a line to type in |
| while typing | `Enter` adds what you typed, `Esc` gives up, `Backspace` deletes; every other key, including `1` to `5` and `o`, is text |
| `Tab`, `Shift-Tab` | Next or previous category |
| `1` to `5` | Hide or show a panel, as on the dashboard |
| `Esc`, `o`, `F2` or `q` | Close the menu (`Ctrl-C` still quits) |

Every change applies at once and is saved to `$XDG_CONFIG_HOME/crowsnest/config.toml`, or `~/.config/crowsnest/config.toml` when `XDG_CONFIG_HOME` is not set (a relative `XDG_CONFIG_HOME` is ignored).
That is the only place settings are read from; crowsnest refuses to start if it is inside the firstmate home.
If `config.toml` is a symlink, as dotfile managers make it, the change is written to the file it points to, which keeps its permissions.
The file is created the first time you change something, including when you hide a panel with `1` to `5`, so panel visibility is remembered.
You can also write it by hand; every key is optional:

```toml
[panels]
hidden = ["runners"]                     # roster, runners, host, usage, held

[usage]
hidden = ["copilot/premium_interactions"] # a provider, or provider/window-id

[thresholds]
stale_minutes = 30
parked_minutes = 10
wait_grace_minutes = 5
runner_silent_minutes = 10
runner_overdue_factor = 2.0
runner_pickup_minutes = 5
runner_idle_cpu_seconds = 2.0

[refresh]
fleet_seconds = 60
pipeline_seconds = 15
github_seconds = 60

[display]
ascii = false

[runners]
processes = true                          # find runners from running Runner.Listener processes
roots = ["~/ci", "/srv/runners"]          # folders searched for runner folders (default: your home)
names = ["actions-runner*", "gh-*"]       # folder name patterns, * and ? (default: actions-runner*)
units = { "~/ci/actions-runner-app" = "actions-runner-app.service" }  # runner folder = systemd unit
```

| Key | Default | What it does |
| --- | --- | --- |
| `panels.hidden` | `[]` | Panels to hide: `roster`, `runners`, `host`, `usage`, `held`. The others take their room; with the roster hidden, the panels fill the screen. |
| `usage.hidden` | `[]` | Usage entries to hide: a whole provider (`copilot`), or one window as `provider/window-id` (`copilot/premium_interactions`), using quota-axi's provider names and window ids. The USAGE title counts what is hidden. |
| `thresholds.stale_minutes` | 30 | As `--stale-minutes`, 1 to 1440. |
| `thresholds.parked_minutes` | 10 | As `--parked-minutes`, 1 to 1440. |
| `thresholds.wait_grace_minutes` | 5 | As `--wait-grace-minutes`, 0 to 1440. |
| `thresholds.runner_silent_minutes` | 10 | As `--runner-silent-minutes`, 1 to 1440. |
| `thresholds.runner_overdue_factor` | 2.0 | As `--runner-overdue-factor`, 1.0 to 100.0. |
| `thresholds.runner_pickup_minutes` | 5 | As `--runner-pickup-minutes`, 1 to 1440. |
| `thresholds.runner_idle_cpu_seconds` | 2.0 | As `--runner-idle-cpu-seconds`, 0 to 3600. |
| `refresh.fleet_seconds` | 60 | As `--fleet-interval`, 10 to 3600. |
| `refresh.pipeline_seconds` | 15 | As `--pipeline-interval`, 5 to 3600. |
| `refresh.github_seconds` | 60 | As `--github-interval`, 30 to 3600. |
| `display.ascii` | `false` | As `--ascii`. |
| `runners.processes` | `true` | Find runners from the `Runner.Listener` processes that are running, and read each one's systemd unit from its cgroup. See [Finding runners](#finding-runners). |
| `runners.roots` | `[]` | Folders whose subfolders are searched for runner folders. Empty: your home directory. `--runner-root` replaces it for a run. A path starts with `/` or `~/`; one inside the firstmate home is refused and reported. |
| `runners.names` | `[]` | Patterns (`*` and `?`) a subfolder's name must match to be taken for a runner folder. Empty: `actions-runner*`. |
| `runners.units` | `{}` | Runner folder to systemd unit. A folder named here is a runner even when it is under no `roots` folder and has stopped. `--runner-unit` wins for its folder. Written inline (`units = { "dir" = "unit" }`) or as a `[runners.units]` table. |

Hiding a usage window is mostly for windows your plan does not have.
quota-axi reports how much of each window is left, and a window with an allowance of 0 (for example premium interactions on a free plan) is 0 of 0 left, which crowsnest can only show as 100% used.

An unknown key or a value of the wrong type or range is ignored, and the rest of the file still applies.
The footer says what was ignored (`! config: unknown key display.theme`), help (`?`) lists every ignored entry, and `--once` prints them to stderr.
A file that is not valid TOML is ignored as a whole and never overwritten; fix it by hand.
Saving rewrites only the entries whose setting you changed, so your comments, any other keys and any ignored entries stay exactly as written (and are still reported) until you change that setting in the menu; a list crowsnest changes is rewritten, along with anything ignored or commented inside it.

## States

| State | Meaning |
| --- | --- |
| `? CAPTAIN` | The worker has an open decision for you. |
| `✗ BLOCKED` | The worker reported it is blocked. |
| `✗ FAILED` | The worker or its pipeline failed. |
| `! STALE` | Needs a nudge. Either it is working or paused with no busy pane, no moving pipeline, and no status line within the stale threshold; or its no-mistakes run has waited at a gate on the worker longer than `--parked-minutes`; or its declared wait ended more than `--wait-grace-minutes` ago. The second line says which. |
| `◆ GATE` | Its no-mistakes run is parked at a gate, for less than `--parked-minutes`. The STEP column shows how long (`parked 4m`). |
| `◐ WORKING` | Working. |
| `‖ PAUSED` | Paused, for example on a declared wait (`until <time>` in its status line). |
| `✓ DONE` | Reported done, and its pipeline (if any) is finished. A task reported done whose no-mistakes run is still moving shows `WORKING` and its running step until the run ends. |
| `~ UNKNOWN` | Firstmate cannot tell, for example because the worker's pane is gone. |
| `○ IDLE` | Nothing in flight, for example a secondmate with no active child work. |

Rows sort in that order, so whatever needs you is always first.
The terminal title shows how many workers are asking for you, for example `crowsnest (1)`.

A parked gate is the quiet kind of stall: no-mistakes reports the run as `running` while it waits for its agent, and nothing else moves.
Crowsnest reads the gate's `awaiting_agent: parked <duration>` and keeps that time counting between reads, so a gate that waits past the threshold turns into a `! STALE` row (`parked at review gate for 1h17m, waiting on the worker`) and is counted in the header as `! 2 stale (1 parked, 1 wait over)`.
A busy pane does not hide a stalled gate: the gate is waiting for an answer either way.
A no-mistakes that does not report how long a gate has waited (no `awaiting_agent` line) still shows `GATE` with the gate's name, but cannot be escalated by time.
A worker whose declared wait has passed is flagged the same way, except while its pipeline is still moving, which counts as the work.

## Runners

The RUNNERS panel has one lane per self-hosted runner, and its title answers "is a runner stuck?" at a glance: `✗ 1 stalled`, `▲ 1 slow`, `✓ all moving`, or nothing while every runner is idle.

```text
╭ RUNNERS ✗ 1 stalled ────────────────── queue 1 ╮
│ ◐ pantry    ✓ moving · online            q1 6m │
│   ◐ Unit + e2e tests #122   8m ━━──────── ~40m │
│     step 4/9 setup-node 8m · out 1s ago        │
│   ○ Lint & typecheck #124      wait 6m → 14:58 │
│ ○ bookshop  idle · online ?                    │
│ ✗ millwork  DOWN · offline                     │
│ ▲ larder    SLOW                               │
│   ◐ Build docs site #41    52m ━━━━━━━━━▲ ~21m │
│     out 1s ago                                 │
│     Tab focus · ↑↓ runner · Enter detail       │
╰────────────────────────────────────────────────╯
```

A busy runner's lane shows the running job with its PR (or branch), how long it has run against its typical duration, the current step, and when its output last moved.
Below it are the jobs queued for that runner, oldest first, with how long each has waited and, for the next one, an estimated start (`→ 14:58`) from the typical durations of what is ahead.
Below 140 columns the lane folds into one line under the runner.
GitHub picks which queued job goes next, so only the next start and the time the queue drains are estimates worth trusting; the detail says so.
Each repository's queue is read on its own. One that cannot be read (a deleted or private repository, an organization that needs SSO) is shown as a partial error (`gh !`), keeps the jobs of its last good read, and never blanks the queue of the other runners; its runners' detail says `Idle, queue not read.` and `QUEUE  not read: gh: HTTP 404: Not Found` instead of `QUEUE  empty`, and no job of that repository is called NOT PICKING UP until it is read again.

A typical duration is the median of the job's (or step's) latest successful runs, at most 10; with fewer than 3 samples crowsnest shows the elapsed time only and never calls the job slow.

| Verdict | Rule |
| --- | --- |
| `◐ moving` | Busy, and none of the below. |
| `○ idle` | Nothing running. |
| `▲ SLOW` | Running longer than `--runner-overdue-factor` times its typical duration, and not stalled. |
| `✗ STALLED` | Busy, its step output (`_diag/pages`, or the worker log when there is none) has not moved for `--runner-silent-minutes`, and the job's processes used less than `--runner-idle-cpu-seconds` of CPU over that time. Or GitHub says a job is in progress on it and no `Runner.Worker` runs. |
| `✗ NOT PICKING UP` | A job has been queued for it past `--runner-pickup-minutes` while it sat idle, counted from when it went idle. |
| `✗ DOWN` | No `Runner.Listener` runs from its directory, or GitHub reports it offline. |

Silence alone is not enough to call a job stalled: a test suite or a long build can print nothing for many minutes while it works, so a silent job whose processes are busy is shown as `quiet 14m, cpu busy` and is not counted.
Two limits follow from reading CPU:

- A job stuck in a busy loop keeps using CPU, so it is never STALLED; it shows as SLOW once it passes the overdue factor.
- A job whose steps run in a container (`docker` or `podman` under the worker) does its work outside the worker's process tree, where crowsnest cannot see its CPU; for those, silence alone decides, as it does when the process table cannot be read.

Stalled runners are not counted in the roster's `! stale` yet.

Enter on a runner opens its detail: the verdict in a sentence, the run and job with every step (the running one against its typical time), the queue with the estimated start and drain, recent jobs on that runner with their duration and result plus its busy share and queue waits, and its health: the listener and its uptime, whether GitHub lists it online, its version and labels, its systemd service with restarts, and what the listener last wrote to its console (`Listening for Jobs`, `Running job: ...`).
Crowsnest follows the runner's newest worker and runner logs in `_diag`: a log is first read from its end, searching back up to 2 MiB for the line that tells the state, and after that only the bytes it grew by are read (never the whole file, and nothing when it did not change), so a line that scrolled past 64 KiB is not forgotten. From the worker log the detail names the step the worker started and whether the process it started is still running (`worker log: step "Run actions/setup-node@v4" since 14:18, process running`); the lane shows that step when GitHub has not been read. From the runner log, a console error such as `Runner connect error: Error: Conflict. Retrying until reconnected.` is shown in yellow in the health section and on the runner's lane as `console error since 13:26: Error: Conflict.` (the listener's own framing is dropped, and a long reason wraps). The listener prints its console error once per outage and says nothing when it reconnects, so the error is judged by the clock: it is shown while the newest failing line after it (an `ERR` line, `Attempt N of ... failed`, `Back off ...`, `Sleeping for N seconds before retrying`) is under 5 minutes old, and ends at the next ordinary console line or once no failing line has been written for 5 minutes. Failing lines with no console error in view read `listener retrying since 13:26`. A single failed attempt, the routine kind the listener logs now and then, is not shown.
GitHub's runner list needs repository admin read; without it the detail says `online ?` and the rest still works.
The service line and the lane's restart count (`2 restarts`, or `↺2` in a narrow panel) need the runner's systemd unit; [Finding runners](#finding-runners) says where it comes from.

### Finding runners

Crowsnest finds runners and their units without being told, and the `[runners]` section of `config.toml` (also in the options menu, under `runners`) adds to that or narrows it. It only reads.

A runner folder is a directory that holds the `.runner` file `config.sh` writes. They are collected from these places, in this order, each folder once:

1. Folders under `runners.roots` (default: your home directory, or the `--runner-root` folders) whose name matches `runners.names` (default `actions-runner*`).
2. Folders named in `runners.units`.
3. The folders of running `Runner.Listener` processes, found from the process table when `runners.processes` is on (the default): the nearest folder above the listener's executable that holds `.runner`, so a runner installed anywhere is found without configuration.
4. Folders whose listener ran earlier in this crowsnest run. A runner that stops stays on screen as `DOWN` and keeps its unit instead of vanishing.

A runner's systemd unit is the first of these that has one:

1. `--runner-unit DIR=UNIT`, then `runners.units`.
2. The unit in the cgroup of its running listener (`/proc/<pid>/cgroup`, Linux only). Hand-written units, `svc.sh` units and user units all show up this way, with no setup. A listener started from a terminal is under no runner unit and has none.
3. The `.service` file `svc.sh install` writes in the runner folder.
4. The unit its listener was last seen in during this run.

A unit is read with `systemctl [--user] show`, in your user's systemd first and then the system's.
The limits: a runner whose listener is not running is found only by steps 1 and 2 (a folder under `roots`, or a `units` entry); a runner registered to an organization shows no GitHub queue, because the queue is read per repository; and on a system without `/proc` (macOS), folders still come from the process table but no unit is read from a cgroup, so give those a `units` entry.

When nothing is found, the RUNNERS panel says so and names what it looked at, for example `listeners: none running` and `~ for actions-runner*`, and points at the options menu.

## Is it updating?

A quiet fleet should look up to date, not frozen:

- The top right of the roster reads `↻ read 4s ago · changed 41m ago` (`↻ 4s · Δ41m` when the title has less room): when the fleet snapshot was last read, and when anything in it last changed.
  A fleet read older than three fleet intervals (at least 3 minutes) turns yellow with a `▲`, unless refresh is paused; the footer shows the other sources' ages.
- Every row ends with `changed 3m ago`: when that row's state, pipeline, PR checks or status line last changed.
  For a row crowsnest has only just met, that is its last status line, the start of its pipeline's active step, or the moment its gate parked, whichever is latest.
  A row that changes while you watch is drawn as a block of reverse video for 10 seconds, then settles.
- The AGE column and every step time keep counting between reads instead of freezing at the last one.

## Where the data comes from

Crowsnest reads documented, stable interfaces only, and never firstmate's internal files.

| Panel | Source | Default cadence |
| --- | --- | --- |
| Roster, held calls | `bin/fm-fleet-snapshot.sh --json`, schema `fm-fleet-snapshot.v1` | 60 s (`--fleet-interval`), or at once on `r`; crowsnest never reads firstmate's state files, so nothing else speeds it up |
| Pipeline strip and steps | `no-mistakes axi status`, inside each no-mistakes task's worktree | 15 s |
| PR checks | one `gh api graphql` read query for every PR on screen | 60 s (`--github-interval`) while anything is active, otherwise 5 min |
| Runners | `.runner` files in runner folders; a local process scan for `Runner.Listener`, `Runner.Worker` and the CPU time of the processes under the worker (not the worker itself); the modification times of the newest `_diag` logs and step output pages, and the newest worker and runner logs, followed from where they were last read (first read from the end, searching back up to 2 MiB) when they changed | 5 s (folders re-listed every 60 s, and when a listener starts or stops) |
| Runner units | `/proc/<pid>/cgroup` of each running `Runner.Listener` (Linux), and the `.service` file `svc.sh install` writes | when folders are listed |
| Runner services | `systemctl [--user] show --property=LoadState,ActiveState,SubState,NRestarts` for a runner with a known unit | 30 s |
| Runner queue | `gh api --method GET repos/<owner>/<repo>/actions/runs?status=queued\|in_progress&per_page=100` and each run's `.../jobs` (8 at a time); runs beyond 100 of either are counted as `+N runs unread` | with PR checks, which a queued job keeps on their active cadence |
| Runner history | `gh api --method GET repos/<owner>/<repo>/actions/runs?status=completed&per_page=10`, and the jobs of each run not read before (a finished run never changes, so it is read once) | 10 min, or when a watched run finishes |
| Runner registration | `gh api --method GET repos/<owner>/<repo>/actions/runners` (needs admin read) | 5 min |
| Host | load, memory, swap, disks, and Linux pressure stall information | 2 s |
| Usage | `quota-axi --no-credential-refresh --json` | 5 min |

Each source runs on its own thread, with a timeout, and never overlaps itself.

GitHub is read through your `gh` login, whose 5000 requests an hour (REST and GraphQL separately) are shared with everything else using it.
Every `gh api` read asks for the response headers, so crowsnest knows how much of each budget is left. A list asked for again carries the ETag of its last answer; when nothing changed GitHub answers 304, which does not count against the budget, and the last answer is reused.
When fewer requests are left than would last until the budget resets at the current pace (keeping half for other programs), GitHub reads slow down; under a tenth of the REST budget the footer says `gh 7% left 4m`, with the age of the GitHub data beside it, and while the wait between reads is longer than the cadence the settings give (the budget stretched it, up to the time to the reset) the RUNNERS panel says `GitHub reads slowed: next 14:48, data 12m old`.
When GitHub refuses for rate limit, crowsnest stops asking until the reset time GitHub gives: the last data stays, the footer says `gh limit → 14:48`, and the RUNNERS panel says when reads resume and how old its queue is. A rate-limit refusal is not counted as a failure.
A source that fails keeps its last good data and is marked with `!` in the footer (`host !`, `runners !`, `nm !`, `gh !`, `quota !`); the fleet snapshot's error is spelled out under the summary line, and the host and runner errors at the top of their panels.
The host fails outright only when load, memory and disks all failed; the runners fail outright when a root cannot be listed and no runner was found. An unreadable root, an unreadable or malformed `.runner` file, or a process table that cannot be scanned is otherwise a partial failure: the runners that were read stay.
When its read fails outright, it is retried less often, doubling up to 5 minutes, until it recovers; a read where only some items failed, such as one unresolvable PR or one worktree, is still marked but keeps its normal cadence.
The fleet snapshot is the expensive one (a few CPU-seconds per run), which is why it runs once a minute; everything else is close to free.
Crowsnest itself, not counting the commands it runs, measured about 1% of one core and 7 MB of memory over two minutes against a six-worker fleet.

`gh`, `no-mistakes` and `quota-axi` are optional.
When one is missing, its source is marked with `!` and the rest keeps working.

## Read-only by construction

- `src/exec.rs` is the only place crowsnest starts a program, and it can only start the read commands listed above, with fixed arguments.
- GraphQL mutations and any REST method other than GET are refused before anything runs; the only header crowsnest sends is `If-None-Match` with an ETag GitHub gave it, checked against the ETag format first.
- quota-axi always gets `--no-credential-refresh`, so a read can never renew a vendor login.
- The fleet snapshot's optional summary cache is pointed at crowsnest's own cache directory (`$XDG_CACHE_HOME/crowsnest`), never at the firstmate home.
- Crowsnest takes no firstmate locks, moves no inbox files, and never runs the `steer` or `watch` commands the snapshot lists.
- `tests/readonly.rs` runs the real binary against a scratch firstmate home with logging shims for every command, and fails if any file under the home changes or any command was not a read.
- The only file crowsnest writes is its own `config.toml`, and only when you change a setting; a config path inside the firstmate home is refused, and `--once` never writes it.
- Runner folders and units in `config.toml` that lie inside the firstmate home are refused and reported, and a search by folder name never enters it, so crowsnest never reads there on a config file's say-so.

There is no telemetry, and crowsnest makes no network calls of its own.

## Development

```sh
cargo test                  # unit, render snapshot and read-only tests
cargo clippy --all-targets -- -D warnings
cargo insta review          # after an intentional UI change
```

The code is split so the data layer can be reused without the UI:

- `sources/` turns each command's output into the types in `model.rs`, and knows nothing about the screen.
- `collector.rs` schedules the sources.
- `app.rs` holds UI state and key handling, with no I/O, including noticing which rows changed.
- `config.rs` reads and writes the settings file, `options.rs` lists what the options menu shows, and `keys.rs` maps keys to actions.
- `ui/` draws a `Snapshot`.

The fixtures in `tests/fixtures` are real outputs with names, paths and accounts replaced.
`docs/upstream.md` explains how crowsnest relates to firstmate upstream.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
Unless you explicitly state otherwise, any contribution you submit for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
