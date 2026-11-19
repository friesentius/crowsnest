# crowsnest

A read-only terminal dashboard for a [firstmate](https://github.com/kunchenguid/firstmate) fleet.
Run it in a pane beside firstmate to see, at a glance, every crewmate, its no-mistakes pipeline, its pull request's checks, your self-hosted CI runners, the host's health, and how much agent usage is left.
It is written in Rust with [ratatui](https://ratatui.rs), and it is a single binary.

```text
╭ crowsnest firstmate @ deckhand ──────────────────────────────────────────── 14:27:06 ↻ fleet 40s ╮
│  ? 1 asking you    ◐ 3 working   ✓ 0 done   ○ 2 idle   ⚑ 8 held calls                            │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│   STATE      TASK                        PROJECT           PIPELINE  STEP        PR · CI     AGE │
│ ▸ ? CAPTAIN  fleet-tui-dashboard         crowsnest         ········· not run     -            7s │
│              needs you (layout-pick): promoted to ship; building crowsnest v1 (Roster, Rust/rat… │
│   ◐ WORKING  pantry-tour-followups       pantry            ✓✓✓✓✓✓✓✓◐ ci 35m      #122 ◐ 3/4  40m │
│              nm ci round 2: CI checks running, waiting for results... (34m ago)                  │
│   ◐ WORKING  bs-p1a-parity-copy          bookshop          ✓✓✓◐····· test 4m     -            5m │
│              nm test starting: claude producing output (6s ago)                                  │
│   ◐ WORKING  bs-p1b-headers-hero         bookshop          ········· not run     -            1m │
│              no-mistakes fix round, waiting until 14:36                                          │
│   ○ IDLE     millwork                    2nd mate                                -             - │
│              no active child work - pane gone                                                    │
│   ○ IDLE     timesheet                   2nd mate                                -            9d │
│              no active child work - pane gone, summary 8d old                                    │
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
╭ RUNNERS ──────────────────────╮╭ HOST ──────────────────────────╮╭ USAGE ────────────────────────╮
│ ● pantry   busy 14m           ││ load 3.42 2.95 2.71  16 cpu    ││ copilot premium … ──────   0% │
│ ○ bookshop idle               ││ mem  ━━━━━━──  72% 9.0G avail  ││ claude session    ━━━━──  59% │
│                               ││ swap ━━──────  25% 6.0G free   ││ claude week       ━━━━──  65% │
│                               ││ disk / 61%  /home 78%          ││ claude Fable week ━━━━━─  89% │
│                               ││ psi  mem 0.4%  cpu 6.1%        ││ +2 more windows               │
╰───────────────────────────────╯╰────────────────────────────────╯╰───────────────────────────────╯
 ↑↓  select  ⏎  detail  f  filter  r  refresh  p  pause  ?  help  q  quit     nm 9s gh 25s quota 2m 
```

Workers that need you sort to the top and stand out.
Press Enter on a row to open its detail: every pipeline step with its duration, each PR check, the last status line, and the worktree.
At 140 columns or more the panels move to a right-hand column, and the held captain calls get their own box.

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
| `--fleet-interval SECS` | 60 | Seconds between fleet snapshots. |
| `--runner-root DIR` | your home | Where to look for `actions-runner*` directories (repeatable). |
| `--disk PATH` | `/` and your home | Filesystems shown in the host panel (repeatable). |
| `--no-github`, `--no-quota` | | Skip the GitHub or quota-axi reads. |
| `--ascii` | | ASCII borders and glyphs, for fonts without box drawing. |
| `--once` | | Collect everything once, print it as JSON, and exit. |

`NO_COLOR` is honoured.
Colours are the terminal's 16 ANSI colours, so they follow your dark or light theme, and every colour is paired with a glyph and a word.

## Keys

| Key | Action |
| --- | --- |
| `↑` `↓` or `j` `k` | Select a worker (`g` and `G`: first and last) |
| `Enter`, `l` or `→` | Open or close its detail |
| `Esc`, `h` or `←` | Close the detail |
| `f` | Filter: all, active, needs you |
| `r` | Refresh everything now |
| `p` | Pause or resume automatic refresh |
| `?` | Help |
| `q` or `Ctrl-C` | Quit |

There are deliberately no action keys.
To act on the fleet, talk to firstmate.

## States

| State | Meaning |
| --- | --- |
| `? CAPTAIN` | The worker has an open decision for you. |
| `✗ BLOCKED` | The worker reported it is blocked. |
| `✗ FAILED` | The worker or its pipeline failed. |
| `! STALE` | Working or paused, but with no busy pane, no moving pipeline, and no status line within the stale threshold (or its declared wait ended more than 5 minutes ago). |
| `◆ GATE` | Its no-mistakes run is parked at a gate. |
| `◐ WORKING` | Working. |
| `‖ PAUSED` | Paused, for example on a declared wait (`until <time>` in its status line). |
| `✓ DONE` | Reported done. |
| `~ UNKNOWN` | Firstmate cannot tell, for example because the worker's pane is gone. |
| `○ IDLE` | Nothing in flight, for example a secondmate with no active child work. |

Rows sort in that order, so whatever needs you is always first.
The terminal title shows how many workers are asking for you, for example `crowsnest (1)`.

## Where the data comes from

Crowsnest reads documented, stable interfaces only, and never firstmate's internal files.

| Panel | Source | Default cadence |
| --- | --- | --- |
| Roster, held calls | `bin/fm-fleet-snapshot.sh --json`, schema `fm-fleet-snapshot.v1` | 60 s, or 20 s after the opt-in fleet ledger (`state/fleet-ledger.jsonl`) changes |
| Pipeline strip and steps | `no-mistakes axi status`, inside each no-mistakes task's worktree | 15 s |
| PR checks | one `gh api graphql` read query for every PR on screen | 60 s while anything is active, otherwise 5 min |
| Runners | `actions-runner*/.runner` files and a local process scan for `Runner.Worker` | 5 s |
| Runner queue | `gh api --method GET repos/<owner>/<repo>/actions/runs` and `.../jobs` | with PR checks |
| Host | load, memory, swap, disks, and Linux pressure stall information | 2 s |
| Usage | `quota-axi --no-credential-refresh --json` | 5 min |

Each source runs on its own thread, with a timeout, and never overlaps itself.
A source that fails keeps its last good data and is marked with `!` in the footer (a fleet snapshot error is spelled out under the summary line).
When its read fails outright, it is retried less often, doubling up to 5 minutes, until it recovers; a read where only some items failed, such as one unresolvable PR or one worktree, is still marked but keeps its normal cadence.
The fleet snapshot is the expensive one (a few CPU-seconds per run), which is why it runs once a minute; everything else is close to free.
Crowsnest itself, not counting the commands it runs, measured about 1% of one core and 7 MB of memory over two minutes against a six-worker fleet.

`gh`, `no-mistakes` and `quota-axi` are optional.
When one is missing, its source is marked with `!` and the rest keeps working.

## Read-only by construction

- `src/exec.rs` is the only place crowsnest starts a program, and it can only start the read commands listed above, with fixed arguments.
- GraphQL mutations and any REST method other than GET are refused before anything runs.
- quota-axi always gets `--no-credential-refresh`, so a read can never renew a vendor login.
- The fleet snapshot's optional summary cache is pointed at crowsnest's own cache directory (`$XDG_CACHE_HOME/crowsnest`), never at the firstmate home.
- Crowsnest takes no firstmate locks, moves no inbox files, and never runs the `steer` or `watch` commands the snapshot lists.
- `tests/readonly.rs` runs the real binary against a scratch firstmate home with logging shims for every command, and fails if any file under the home changes or any command was not a read.

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
- `app.rs` holds UI state and key handling, with no I/O.
- `ui/` draws a `Snapshot`.

The fixtures in `tests/fixtures` are real outputs with names, paths and accounts replaced.
`docs/upstream.md` explains how crowsnest relates to firstmate upstream.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
Unless you explicitly state otherwise, any contribution you submit for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
