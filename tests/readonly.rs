//! The read-only guarantee, end to end: run the real binary against a scratch
//! firstmate home whose every external command is a logging shim, then check
//! that nothing under the home changed and that every command it ran was a
//! read.

use crowsnest::exec::ReadCommand;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Words that would change something if they reached a command line.
const MUTATING: &[&str] = &[
    "abort",
    "approve",
    "cancel",
    "close",
    "create",
    "delete",
    "edit",
    "fix",
    "kill",
    "merge",
    "post",
    "push",
    "rerun",
    "respond",
    "run",
    "send",
    "set",
    "start",
    "steer",
    "stop",
    "update",
    "write",
    "-X",
    "--input",
    "--field",
    "-F",
    "--raw-field",
    "--yes",
    "-y",
];

fn write_exec(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every file under `dir` with its size, mtime and content.
fn fingerprint(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                stack.push(p.clone());
            }
            let content = if m.is_file() {
                fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            out.insert(p, (m.len(), m.modified().unwrap(), content));
        }
    }
    out
}

/// One shim call: program name, arguments, working directory.
struct Call {
    program: String,
    args: Vec<String>,
    cwd: String,
    cache: String,
}

/// The calls logged under `log`, one file per call.
fn calls(log: &Path) -> Vec<Call> {
    fs::read_dir(log)
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(&l);
            let mut f = l.split('\u{1e}');
            let mut argv = f.next().unwrap().split('\u{1f}').map(str::to_string);
            Call {
                program: argv.next().unwrap(),
                args: argv.collect(),
                cwd: f.next().unwrap_or_default().to_string(),
                cache: f.next().unwrap_or_default().to_string(),
            }
        })
        .collect()
}

#[test]
fn once_reads_everything_and_writes_nothing() {
    let root =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("readonly-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let log = root.join("calls");
    let (home, shims, cache, runners) = (
        root.join("fm"),
        root.join("shims"),
        root.join("cache"),
        root.join("runners"),
    );
    for d in [
        home.join("bin"),
        home.join("state"),
        shims.clone(),
        log.clone(),
        runners.join("actions-runner-pantry"),
        runners.join("actions-runner-bookshop"),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    // The worktrees named in the fixture, relocated under the scratch root.
    let worktrees = root.join("worktrees");
    for wt in [
        "pantry-82978c/2/pantry",
        "bookshop-98774f/2/bookshop",
        "bookshop-98774f/3/bookshop",
    ] {
        fs::create_dir_all(worktrees.join(wt)).unwrap();
    }
    fs::write(home.join("state/fleet-ledger.jsonl"), "{}\n").unwrap();
    fs::write(home.join("state/some-task.status"), "working: example\n").unwrap();
    fs::copy(
        format!("{FIXTURES}/runner/.runner"),
        runners.join("actions-runner-pantry/.runner"),
    )
    .unwrap();
    // A second runner whose repository's runner list the token may not read.
    fs::write(
        runners.join("actions-runner-bookshop/.runner"),
        fs::read_to_string(format!("{FIXTURES}/runner/.runner"))
            .unwrap()
            .replace("pantry", "bookshop"),
    )
    .unwrap();

    // Each call logs to its own file: gh reads run in parallel, and writes
    // from concurrent shims to one shared file would interleave.
    let logger = r#"{ printf '%s' "$(basename "$0")"; for a in "$@"; do printf '\037%s' "$a"; done; printf '\036%s\036%s\n' "$PWD" "$FM_SNAPSHOT_CACHE_DIR"; } > "$(mktemp "$CN_LOG/call.XXXXXX")"
"#;
    write_exec(
        &home.join("bin/fm-fleet-snapshot.sh"),
        &format!(
            "{logger}sed \"s#/home/captain/.worktrees#$CN_WORKTREES#g\" \"$CN_FIXTURES/fleet-snapshot.json\"\n"
        ),
    );
    write_exec(
        &shims.join("no-mistakes"),
        &format!(
            r#"{logger}case "$PWD" in
  */pantry-82978c/2/pantry) cat "$CN_FIXTURES/nm-status-ci.toon" ;;
  */bookshop-98774f/2/bookshop) cat "$CN_FIXTURES/nm-status-test.toon" ;;
  *) echo "current_branch: main" ;;
esac
"#
        ),
    );
    // Like `gh api --include`: the status and rate-limit headers first.
    write_exec(
        &shims.join("gh"),
        &format!(
            r#"{logger}head() {{ printf 'HTTP/2.0 %s\r\nX-Ratelimit-Limit: 5000\r\nX-Ratelimit-Remaining: 4321\r\nX-Ratelimit-Reset: 4102444800\r\nX-Ratelimit-Resource: %s\r\n\r\n' "$1" "$2"; }}
case "$*" in
  "api graphql"*) head "200 OK" graphql; cat "$CN_FIXTURES/gh-checks.json" ;;
  *"repos/acme/bookshop/actions/runners"*) head "403 Forbidden" core; echo '{{"message": "Must have admin rights to Repository."}}'; echo "gh: Must have admin rights to Repository. (HTTP 403)" >&2; exit 1 ;;
  *) head "200 OK" core ;;
esac
case "$*" in
  "api graphql"*) ;;
  *"repos/acme/bookshop/"*) echo '{{"total_count": 0, "workflow_runs": []}}' ;;
  *"/actions/runners"*) cat "$CN_FIXTURES/gh-runners.json" ;;
  *"status=completed"*) cat "$CN_FIXTURES/gh-runs-completed.json" ;;
  *"/jobs"*)
    id=$(printf '%s' "$*" | sed -n 's#.*/runs/\([0-9]*\)/jobs.*#\1#p')
    if [ -f "$CN_FIXTURES/gh-jobs-$id.json" ]; then cat "$CN_FIXTURES/gh-jobs-$id.json"; else cat "$CN_FIXTURES/gh-jobs.json"; fi ;;
  *"/actions/runs?"*) cat "$CN_FIXTURES/gh-runs.json" ;;
  *) echo "unexpected" >&2; exit 1 ;;
esac
"#
        ),
    );
    write_exec(
        &shims.join("systemctl"),
        &format!(
            "{logger}printf 'LoadState=loaded\\nActiveState=active\\nSubState=running\\nNRestarts=2\\n'\n"
        ),
    );
    write_exec(
        &shims.join("quota-axi"),
        &format!("{logger}cat \"$CN_FIXTURES/quota.json\"\n"),
    );

    // A config file with one entry crowsnest does not know: reported on
    // stderr, never rewritten by a read. It also turns off finding runners
    // from running listeners, so the real runners of the machine the test
    // runs on stay out of the snapshot.
    let config_dir = root.join("config");
    let config = config_dir.join("crowsnest/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        "[thresholds]\nstale_minutes = 45\n[display]\ntheme = \"amber\"\n[runners]\nprocesses = false\n",
    )
    .unwrap();
    let config_before = fingerprint(&config_dir);

    let before = fingerprint(&home);
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_crowsnest"))
        .args(["--once", "--fm-home"])
        .arg(&home)
        .arg("--cache-dir")
        .arg(&cache)
        .arg("--runner-root")
        .arg(&runners)
        .arg("--runner-unit")
        .arg(format!(
            "{}=actions-runner-pantry.service",
            runners.join("actions-runner-pantry").display()
        ))
        .args(["--disk", "/"])
        .env("PATH", path)
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CN_LOG", &log)
        .env("CN_FIXTURES", FIXTURES)
        .env("CN_WORKTREES", &worktrees)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("crowsnest: config: unknown key display.theme"),
        "{stderr}"
    );
    assert!(
        config_before == fingerprint(&config_dir),
        "--once wrote config"
    );

    // Everything was read.
    for tier in ["host", "runners", "fleet", "nm", "gh", "quota"] {
        let f = &snap["fresh"][tier];
        assert!(f["error"].is_null(), "{tier}: {}", f["error"]);
        assert!(f["ok_at"].is_i64(), "{tier} was read");
    }
    assert_eq!(snap["fleet"]["workers"].as_array().unwrap().len(), 6);
    assert_eq!(snap["pipelines"].as_object().unwrap().len(), 2);
    assert_eq!(
        snap["prs"]["https://github.com/acme/pantry/pull/122"]["checks"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(snap["runners"][0]["repo"], "acme/bookshop");
    assert_eq!(snap["runners"][1]["repo"], "acme/pantry");
    assert_eq!(snap["jobs"][0]["name"], "Unit + e2e tests");
    assert_eq!(snap["jobs"][0]["run"]["pr"], 122);
    assert_eq!(snap["jobs"][0]["steps"].as_array().unwrap().len(), 9);
    // The runner list is read where the token may, and denied (not an
    // error) where it may not.
    assert_eq!(
        snap["gh_runners"]["acme/pantry"]["runners"][0]["version"],
        "2.338.2"
    );
    assert_eq!(snap["gh_runners"]["acme/bookshop"]["denied"], true);
    assert!(snap["gh_runners"]["acme/bookshop"]["error"].is_null());
    // History: the finished runs, and the jobs already done in the live one.
    let h = &snap["history"]["acme/pantry"];
    assert!(h["error"].is_null(), "{}", h["error"]);
    assert_eq!(h["jobs"].as_array().unwrap().len(), 19);
    assert!(
        h["typical"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["job"] == "Unit + e2e tests" && t["step"].is_null() && t["secs"] == 2400)
    );
    assert_eq!(snap["runners"][1]["unit"], "actions-runner-pantry.service");
    assert_eq!(snap["runners"][1]["service"]["active"], "active");
    assert_eq!(snap["runners"][1]["service"]["restarts"], 2);
    assert_eq!(snap["quota"].as_array().unwrap().len(), 6);
    // The rate-limit counters came with the answers.
    assert_eq!(snap["gh_budget"]["core"]["remaining"], 4321);
    assert_eq!(snap["gh_budget"]["graphql"]["limit"], 5000);
    assert!(snap["gh_budget"]["core"]["paused_until"].is_null());

    // Nothing under the firstmate home changed, and nothing was added.
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        fingerprint(&home).keys().collect::<Vec<_>>()
    );
    assert!(before == fingerprint(&home), "a file under FM_HOME changed");

    // Every command was a read.
    let calls = calls(&log);
    let programs: Vec<&str> = calls.iter().map(|c| c.program.as_str()).collect();
    for p in [
        "fm-fleet-snapshot.sh",
        "no-mistakes",
        "gh",
        "quota-axi",
        "systemctl",
    ] {
        assert!(programs.contains(&p), "{p} was called: {programs:?}");
    }
    for c in &calls {
        let args: Vec<&str> = c.args.iter().map(String::as_str).collect();
        for a in &args {
            assert!(
                !MUTATING.contains(a),
                "{} {args:?} has mutating word {a}",
                c.program
            );
        }
        match c.program.as_str() {
            "fm-fleet-snapshot.sh" => {
                assert_eq!(args, ["--json"]);
                assert!(
                    Path::new(&c.cache).starts_with(&cache),
                    "summary cache is crowsnest's own"
                );
            }
            "no-mistakes" => {
                assert_eq!(args, ["axi", "status"]);
                assert!(c.cwd.starts_with(worktrees.to_str().unwrap()));
            }
            "gh" => match args.as_slice() {
                ["api", "graphql", "--include", "-f", q] => {
                    assert!(
                        q.starts_with("query=query {") && !q.contains("mutation"),
                        "{q}"
                    )
                }
                ["api", "--method", "GET", "--include", p]
                | ["api", "--method", "GET", "--include", "-H", _, p] => {
                    assert!(
                        ["repos/acme/pantry/actions/", "repos/acme/bookshop/actions/"]
                            .iter()
                            .any(|r| p.starts_with(r)),
                        "{p}"
                    )
                }
                other => panic!("unexpected gh call {other:?}"),
            },
            "systemctl" => assert_eq!(
                args,
                [
                    "--user",
                    "show",
                    "--property=LoadState,ActiveState,SubState,NRestarts",
                    "--",
                    "actions-runner-pantry.service"
                ]
            ),
            "quota-axi" => assert_eq!(args, ["--no-credential-refresh", "--json"]),
            other => panic!("unexpected program {other}"),
        }
    }
    let _ = fs::remove_dir_all(&root);
}

/// SIMULATED from the live check of finding 3: two runners, one registered
/// to a repository the `gh` login cannot read (GitHub answers 404 to its run
/// lists). The queue of the other must still be read and shown.
#[test]
fn one_unreadable_repository_does_not_blank_the_queue_of_the_others() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("unreadable-repo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (home, shims, runners) = (root.join("fm"), root.join("shims"), root.join("runners"));
    for d in [
        home.join("bin"),
        home.join("state"),
        shims.clone(),
        runners.join("actions-runner-pantry"),
        runners.join("actions-runner-bookshop"),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    let runner_file = fs::read_to_string(format!("{FIXTURES}/runner/.runner")).unwrap();
    fs::write(runners.join("actions-runner-pantry/.runner"), &runner_file).unwrap();
    // `acme/bookshop` sorts first: it is asked before pantry.
    fs::write(
        runners.join("actions-runner-bookshop/.runner"),
        runner_file.replace("pantry", "bookshop"),
    )
    .unwrap();
    write_exec(
        &home.join("bin/fm-fleet-snapshot.sh"),
        "echo '{\"workers\": [], \"secondmates\": []}'\n",
    );
    write_exec(
        &shims.join("gh"),
        r#"head() { printf 'HTTP/2.0 %s\r\nX-Ratelimit-Limit: 5000\r\nX-Ratelimit-Remaining: 4321\r\nX-Ratelimit-Reset: 4102444800\r\nX-Ratelimit-Resource: core\r\n\r\n' "$1"; }
case "$*" in
  *"repos/acme/bookshop/"*) head "404 Not Found"; echo '{"message": "Not Found"}'; echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
  *"/actions/runners"*) head "200 OK"; cat "$CN_FIXTURES/gh-runners.json" ;;
  *"status=completed"*) head "200 OK"; cat "$CN_FIXTURES/gh-runs-completed.json" ;;
  *"/jobs"*) head "200 OK"; cat "$CN_FIXTURES/gh-jobs.json" ;;
  *"/actions/runs?"*) head "200 OK"; cat "$CN_FIXTURES/gh-runs.json" ;;
  *) head "200 OK"; echo '{}' ;;
esac
"#,
    );
    let config_dir = root.join("config");
    fs::create_dir_all(config_dir.join("crowsnest")).unwrap();
    fs::write(
        config_dir.join("crowsnest/config.toml"),
        "[runners]\nprocesses = false\n",
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_crowsnest"))
        .args(["--once", "--no-quota", "--fm-home"])
        .arg(&home)
        .arg("--cache-dir")
        .arg(root.join("cache"))
        .arg("--runner-root")
        .arg(&runners)
        .args(["--disk", "/"])
        .env("PATH", path)
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("CN_FIXTURES", FIXTURES)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // Pantry's queue is read although bookshop's repository answered 404.
    assert_eq!(snap["jobs"][0]["name"], "Unit + e2e tests");
    let reads = &snap["queue_reads"];
    assert!(reads["acme/pantry"]["error"].is_null(), "{reads}");
    assert!(reads["acme/pantry"]["read_at"].is_i64(), "{reads}");
    assert_eq!(
        reads["acme/bookshop"]["error"], "gh: HTTP 404: Not Found",
        "{reads}"
    );
    assert!(reads["acme/bookshop"]["read_at"].is_null(), "{reads}");
    // A partial error: shown, and the read still counts.
    let gh = &snap["fresh"]["gh"];
    assert_eq!(
        gh["error"], "acme/bookshop queue: gh: HTTP 404: Not Found",
        "{gh}"
    );
    assert!(gh["ok_at"].is_i64(), "{gh}");
    assert!(snap["jobs_read_at"].is_i64());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_allowlist_refuses_writes() {
    let refused = |c: ReadCommand| c.invocation().is_err();
    assert!(refused(ReadCommand::GhGraphql {
        query: "mutation { closePullRequest }".into()
    }));
    assert!(refused(ReadCommand::GhGraphql {
        query: "query { x } mutation { y }".into()
    }));
    assert!(refused(ReadCommand::GhApiGet {
        path: "user".into(),
        etag: None,
    }));
    assert!(refused(ReadCommand::GhApiGet {
        path: "repos/a/b/pulls/1 -X DELETE".into(),
        etag: None,
    }));
    assert!(refused(ReadCommand::GhApiGet {
        path: "repos/a/b/actions/runs".into(),
        etag: Some("\"x\" -X DELETE".into()),
    }));
    assert!(refused(ReadCommand::QuotaAxi {
        providers: vec!["claude --login".into()]
    }));
    assert!(refused(ReadCommand::SystemctlShow {
        unit: "--now".into(),
        user: true
    }));
    // Every allowed invocation is free of mutating words.
    let all = [
        ReadCommand::FleetSnapshot {
            fm_home: "/fm".into(),
            cache_dir: "/c".into(),
        },
        ReadCommand::NoMistakesStatus {
            worktree: "/w".into(),
        },
        ReadCommand::GhGraphql {
            query: "query { viewer { login } }".into(),
        },
        ReadCommand::GhApiGet {
            path: "repos/a/b/actions/runs?status=queued".into(),
            etag: None,
        },
        ReadCommand::GhApiGet {
            path: "repos/a/b/actions/runs?status=queued".into(),
            etag: Some("W/\"36458da1f62c\"".into()),
        },
        ReadCommand::QuotaAxi {
            providers: vec!["claude".into()],
        },
        ReadCommand::SystemctlShow {
            unit: "actions-runner-pantry.service".into(),
            user: true,
        },
    ];
    for c in all {
        let inv = c.invocation().unwrap();
        assert!(
            inv.args.iter().all(|a| !MUTATING.contains(&a.as_str())),
            "{inv:?}"
        );
    }
}
