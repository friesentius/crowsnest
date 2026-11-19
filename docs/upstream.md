# Crowsnest and firstmate upstream

Crowsnest lives in its own repository for now.
It is built so that it could be offered to [kunchenguid/firstmate](https://github.com/kunchenguid/firstmate) later, in whatever shape the maintainers prefer.
Nothing has been proposed upstream yet.

## The related upstream work

- Issue [#5499, "Dashboard of the entire crewmate fleet"](https://github.com/kunchenguid/firstmate/issues/5499) asks for the same thing crowsnest does: one view of every crewmate as it moves through no-mistakes and GitHub CI.
  It was triaged as ready for a PR, opt-in, as two focused changes (collect the data, then render it) that stay read-only and compose with the existing fleet and bearings surfaces.
- PR [#5562, `fm-flow-snapshot.sh`](https://github.com/kunchenguid/firstmate/pull/5562) is the collect half of that issue.
  It adds a read-only `fm-flow-snapshot.v1` JSON document with each agent's no-mistakes steps and GitHub check rollup, layered over `fm-fleet-snapshot.sh`.
  It is open and unmerged, and it has no renderer in the repository yet.

Crowsnest is a renderer, so it is the missing render half of #5499, and the natural consumer of #5562 if that lands.

## How crowsnest stays upstream-ready

- **Documented contracts only.**
  The fleet comes from `bin/fm-fleet-snapshot.sh --json`, and crowsnest refuses any schema other than `fm-fleet-snapshot.v1` with a clear error.
  It never reads `state/*.meta`, status logs or library internals.
  The only firstmate file it looks at directly is the opt-in fleet ledger (`state/fleet-ledger.jsonl`, documented as the stable hook for outside tools), and only its modification time, as a refresh trigger.
- **No hard-coded home.**
  The firstmate home comes from `--fm-home` or `FM_HOME`, and nothing in the code names a machine, user, project or account.
  Runner directories and disks are discovered or configured.
- **A separable data layer.**
  `src/sources/` turns each command's output into the types in `src/model.rs` and knows nothing about the UI.
  A `flow` source reading `fm-flow-snapshot.v1` could replace the fleet and no-mistakes sources without touching `src/ui/`.
- **Opt-in and read-only.**
  Crowsnest is never launched by firstmate, has no action keys, runs only the read commands in `src/exec.rs`, and `tests/readonly.rs` proves it writes nothing under the firstmate home.
- **Vendor-neutral.**
  Usage shows whatever providers quota-axi reports; nothing assumes a particular agent.
- **Licensing.**
  MIT OR Apache-2.0, so the code can move under firstmate's MIT licence.

## Possible upstream shapes, smallest first

1. A small opt-in launcher, for example `bin/fm-crowsnest.sh`, that runs an installed `crowsnest` with `FM_HOME` set and prints install instructions when it is missing, plus a docs page.
   Firstmate already relies on external binaries it does not vendor (no-mistakes, quota-axi, lavish-axi), so this follows an existing pattern.
2. Contract additions to `fm-fleet-snapshot.sh` that also help bearings: a backend-neutral per-task activity field (so "stale" no longer has to be inferred) and a structured `wait_until` (crowsnest currently parses `until <ISO>` from the status note).
3. If #5562 lands, crowsnest reads `fm-flow-snapshot.v1` instead of calling `no-mistakes axi status` per worktree, and becomes that PR's renderer.

Firstmate's shell helpers are plain bash, so vendoring Rust source into `bin/` is not a realistic shape.

## When the time comes

Open an issue on kunchenguid/firstmate first, link #5499 and #5562, and offer shape 1, rather than sending an unsolicited PR.
Any PR to firstmate must go through no-mistakes, as its contributing guide requires.
