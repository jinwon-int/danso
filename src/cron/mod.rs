//! The `danso cron` surface (§6.5) over the ccc-compatible `agent-cron`
//! store at `$DANSO_HOME/cron/tasks.json`.
//!
//! PR1 shipped the read-only queries (`list`, `describe <id>`, `due`); PR2
//! adds the run-lock surface (`lock`), the tick/run dry-run planners, and the
//! tested state-commit write path (`commit.rs`). Execute modes of `tick` and
//! `run` refuse with a typed, zero-mutation result until the payload
//! executors land in #120 PR3 — a half-execution is worse than a refusal.

pub mod commit;
pub mod due;
pub mod locks;
pub mod retry;
pub mod run;
pub mod schedule;
pub mod store;
pub mod tick;
pub mod time;

use crate::cron::due::{due_plan, normalize, read_only_mutations};
use crate::cron::schedule::parse_schedule;
use crate::cron::store::Store;
use crate::cron::time::{fmt_dt, parse_utc, truncate_minute};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Parser)]
pub struct CronArgs {
    /// Override the store path (default `$DANSO_HOME/cron/tasks.json`).
    #[arg(long, global = true, value_name = "PATH")]
    store: Option<PathBuf>,
    #[command(subcommand)]
    command: CronCommand,
}

#[derive(Subcommand)]
enum CronCommand {
    /// List configured tasks (prompt-free projection).
    List {
        /// Emit the full JSON projection.
        #[arg(long)]
        json: bool,
    },
    /// Describe one task (prompt-free) with its live schedule/lock/health view.
    Describe {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Read-only due/retry resolver (`--at` plans for a fixed instant).
    Due {
        #[arg(long, value_name = "ISO8601")]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Probe, acquire, or release a task's run lock (§6.5).
    Lock {
        id: String,
        /// Lock operation (default `probe`).
        #[arg(long, value_enum, default_value_t = LockAction::Probe)]
        action: LockAction,
        /// Shorthand for `--action acquire`.
        #[arg(long, conflicts_with = "action")]
        acquire: bool,
        /// Shorthand for `--action release`.
        #[arg(long, conflicts_with = "action")]
        release: bool,
        /// Owner run id — required for acquire and release.
        #[arg(long, value_name = "RUN_ID")]
        run_id: Option<String>,
        /// Original occurrence time recorded in the lock payload.
        #[arg(long, value_name = "ISO8601")]
        scheduled_at: Option<String>,
        /// Plan for a fixed instant instead of now.
        #[arg(long, value_name = "ISO8601")]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Timer-facing tick: plan what would run. Execution (no `--dry-run`)
    /// refuses until the payload executors land in #120 PR3.
    Tick {
        /// Print the plan without touching the filesystem.
        #[arg(long)]
        dry_run: bool,
        /// Cap executions per tick (1-100; consumed by the PR3 pipeline).
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(i64).range(1..=100))]
        max_runs: Option<i64>,
        /// Plan for a fixed instant instead of now.
        #[arg(long, value_name = "ISO8601")]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// One manual run. Execution (no `--dry-run`) refuses until the payload
    /// executors land in #120 PR3.
    Run {
        id: String,
        /// Print the preview without touching the filesystem.
        #[arg(long)]
        dry_run: bool,
        /// Plan for a fixed instant instead of now.
        #[arg(long, value_name = "ISO8601")]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, PartialEq)]
enum LockAction {
    Probe,
    Acquire,
    Release,
}

impl LockAction {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Acquire => "acquire",
            Self::Release => "release",
        }
    }
}

fn resolve_store_path(override_path: Option<&Path>) -> Result<PathBuf, ()> {
    match override_path {
        Some(path) => Ok(path.to_path_buf()),
        None => crate::config::home()
            .map(|home| store::store_path(&home))
            .map_err(|_| ()),
    }
}

fn load_store(path: &Path) -> Result<Store, String> {
    store::load(path)
}

/// Run a cron command; returns the process exit code. Store/plan errors print
/// body-free messages on stderr (never HOME/DANSO_HOME or filesystem text).
pub fn run(args: CronArgs) -> i32 {
    let path = match resolve_store_path(args.store.as_deref()) {
        Ok(path) => path,
        Err(()) => {
            eprintln!("cron failed: could not resolve the state root");
            return 1;
        }
    };
    let store_display = path.display().to_string();
    let store = match load_store(&path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cron: {error}");
            return 1;
        }
    };
    match args.command {
        CronCommand::List { json } => {
            if json {
                let document = json!({
                    "version": 1,
                    "store": store_display,
                    "mutations": read_only_mutations(),
                    "tasks": store.tasks.iter().map(normalize).collect::<Vec<_>>(),
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&document).expect("serializable")
                );
            } else {
                emit_list(&store, &store_display);
            }
            0
        }
        CronCommand::Describe { id, json } => match store.tasks.iter().find(|task| task.id == id) {
            Some(task) => {
                let document = describe_document(task, &store_display);
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&document).expect("serializable")
                    );
                } else {
                    emit_describe(&document);
                }
                0
            }
            None => {
                eprintln!("cron describe failed: no such task id: {id}");
                1
            }
        },
        CronCommand::Due { at, json } => {
            let plan = due_plan(
                &store,
                &store_display,
                at.as_deref(),
                truncate_minute(Utc::now()),
            );
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&plan).expect("serializable")
                );
            } else {
                emit_due(&plan);
            }
            if plan["ok"].as_bool().unwrap_or(false) {
                0
            } else {
                1
            }
        }
        CronCommand::Lock {
            id,
            action,
            acquire,
            release,
            run_id,
            scheduled_at,
            at,
            json,
        } => {
            let action = if acquire {
                LockAction::Acquire
            } else if release {
                LockAction::Release
            } else {
                action
            };
            let run_id = run_id.unwrap_or_default();
            if matches!(action, LockAction::Acquire | LockAction::Release) && run_id.is_empty() {
                eprintln!("cron lock failed: --run-id is required for acquire/release");
                return 2;
            }
            let Some(task) = store.tasks.iter().find(|task| task.id == id) else {
                let document = json!({
                    "ok": false,
                    "taskId": id,
                    "lockState": "unknown-task",
                    "error": "task id not found",
                });
                if json {
                    print_document(&document);
                } else {
                    emit_lock(&document);
                }
                return 1;
            };
            let at = match parse_utc(at.as_deref(), "--at") {
                Ok(parsed) => parsed.unwrap_or_else(|| truncate_minute(Utc::now())),
                Err(error) => {
                    eprintln!("cron lock failed: {error}");
                    return 1;
                }
            };
            match locks::lock_command(
                &path,
                &id,
                task,
                action.as_str(),
                &run_id,
                scheduled_at.as_deref().unwrap_or_default(),
                at,
            ) {
                Ok((document, code)) => {
                    if json {
                        print_document(&document);
                    } else {
                        emit_lock(&document);
                    }
                    code
                }
                Err(error) => {
                    eprintln!("cron lock failed: {error}");
                    1
                }
            }
        }
        CronCommand::Tick {
            dry_run,
            max_runs,
            at,
            json,
        } => {
            let max_runs = max_runs.unwrap_or(10);
            if !dry_run {
                let at_display = match parse_utc(at.as_deref(), "--at") {
                    Ok(parsed) => fmt_dt(parsed.or_else(|| Some(truncate_minute(Utc::now()))))
                        .unwrap_or_default(),
                    Err(_) => String::new(),
                };
                let document = tick::execute_unavailable(&store_display, &at_display, max_runs);
                if json {
                    print_document(&document);
                } else {
                    emit_tick(&document);
                }
                return 2;
            }
            let plan = tick::dry_run(
                &store,
                &store_display,
                at.as_deref(),
                truncate_minute(Utc::now()),
            );
            if json {
                print_document(&plan);
            } else {
                emit_tick(&plan);
            }
            if plan["ok"].as_bool().unwrap_or(false) {
                0
            } else {
                1
            }
        }
        CronCommand::Run {
            id,
            dry_run,
            at,
            json,
        } => {
            if !dry_run {
                let document = run::execute_unavailable(&store_display, &id);
                if json {
                    print_document(&document);
                } else {
                    emit_run(&document);
                }
                return 2;
            }
            let (document, code) = run::dry_plan(
                &store,
                &store_display,
                &id,
                at.as_deref(),
                truncate_minute(Utc::now()),
            );
            if json {
                print_document(&document);
            } else {
                emit_run(&document);
            }
            code
        }
    }
}

fn print_document(document: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(document).expect("serializable")
    );
}

fn emit_lock(document: &serde_json::Value) {
    println!(
        "danso cron lock {}: {} ok={}",
        document["taskId"].as_str().unwrap_or_default(),
        document["lockState"].as_str().unwrap_or_default(),
        document["ok"].as_bool().unwrap_or(false),
    );
}

fn emit_tick(document: &serde_json::Value) {
    println!("# danso cron tick\n");
    println!(
        "- store: `{}`",
        document["store"].as_str().unwrap_or_default()
    );
    println!("- at: `{}`", document["at"].as_str().unwrap_or_default());
    if let Some(error) = document["error"].as_str() {
        println!("- error: `{error}`");
        println!(
            "- mutations: `{}`",
            serde_json::to_string(&document["mutations"]).expect("serializable")
        );
        return;
    }
    println!(
        "- mode: dry-run/read-only; no lock acquire, execution, spool write, scheduler install, or state writes\n"
    );
    let actions = document["actions"].as_array().cloned().unwrap_or_default();
    if actions.is_empty() {
        println!("No cron tasks are defined.");
        return;
    }
    println!("| task | action | reason | scheduled at | lock |");
    println!("|---|---|---|---|---|");
    for action in &actions {
        println!(
            "| `{}` | `{}` | `{}` | `{}` | `{}` |",
            action["taskId"].as_str().unwrap_or_default(),
            action["action"].as_str().unwrap_or_default(),
            action["reason"].as_str().unwrap_or_default(),
            action["scheduledAt"].as_str().unwrap_or_default(),
            action["lockState"].as_str().unwrap_or_default(),
        );
    }
}

fn emit_run(document: &serde_json::Value) {
    let title = if document["mode"].as_str() == Some("run-dry-run-read-only") {
        "# danso cron run dry-run plan"
    } else {
        "# danso cron run"
    };
    println!("{title}\n");
    println!(
        "- task: `{}`",
        document["taskId"].as_str().unwrap_or_default()
    );
    if let Some(error) = document["error"].as_str() {
        println!("- error: `{error}`");
        return;
    }
    println!("- at: `{}`", document["at"].as_str().unwrap_or_default());
    println!(
        "- due: `{}` status=`{}` scheduledAt=`{}`",
        document["due"].as_bool().unwrap_or(false),
        document["status"].as_str().unwrap_or_default(),
        document["scheduledAt"].as_str().unwrap_or_default(),
    );
    if let Some(lock) = document.get("lock").filter(|lock| lock.is_object()) {
        println!(
            "- lock: `{}` `{}`",
            lock["state"].as_str().unwrap_or_default(),
            lock["path"].as_str().unwrap_or_default(),
        );
    }
    if let Some(headless) = document
        .get("headless")
        .filter(|headless| headless.is_object())
    {
        println!(
            "- headless: `{}` execute=`{}` exit=`{}`",
            headless["command"].as_str().unwrap_or_default(),
            headless["execute"].as_bool().unwrap_or(false),
            headless["exitCode"]
                .as_i64()
                .map(|code| code.to_string())
                .unwrap_or_default(),
        );
    }
    println!(
        "- mutations: `{}`",
        serde_json::to_string(&document["mutations"]).expect("serializable")
    );
}

fn emit_list(store: &Store, store_display: &str) {
    println!("# danso cron tasks\n");
    println!("- store: `{store_display}`");
    println!(
        "- mode: store/list/due only; no execution, push, scheduler, systemd, crontab, or state writes\n"
    );
    if store.tasks.is_empty() {
        println!("No cron tasks are defined.");
        return;
    }
    println!("| id | schedule | enabled | notify | catch-up | tools | last status |");
    println!("|---|---|---:|---|---|---|---|");
    for task in &store.tasks {
        let projection = normalize(task);
        let tools = if task.allowed_tools.is_empty() {
            "(default)".to_string()
        } else {
            task.allowed_tools.join(",")
        };
        println!(
            "| `{}` | `{}` | {} | `{}` | `{}` | `{tools}` | `{}` |",
            projection["id"].as_str().unwrap_or_default(),
            projection["schedule"].as_str().unwrap_or_default(),
            if projection["enabled"].as_bool().unwrap_or(false) {
                "true"
            } else {
                "false"
            },
            projection["notify"].as_str().unwrap_or_default(),
            projection["catchUpPolicy"].as_str().unwrap_or_default(),
            projection["lastStatus"].as_str().unwrap_or("unknown"),
        );
    }
}

/// One-task detail: the prompt-free projection plus the live schedule,
/// lock, and due view (the due row at plan time = now).
fn describe_document(task: &store::Task, store_display: &str) -> serde_json::Value {
    let mut document = normalize(task);
    document["store"] = serde_json::json!(store_display);
    document["mutations"] = read_only_mutations();
    let schedule = parse_schedule(&task.schedule, &task.timezone);
    match schedule {
        Ok(schedule) => {
            document["scheduleKind"] = serde_json::json!(schedule.kind());
            document["scheduleValid"] = serde_json::json!(true);
        }
        Err(error) => {
            document["scheduleKind"] = serde_json::Value::Null;
            document["scheduleValid"] = serde_json::json!(false);
            document["scheduleError"] = serde_json::json!(error);
        }
    }
    let plan = due_plan(
        &Store {
            version: 1,
            tasks: vec![task.clone()],
        },
        store_display,
        None,
        truncate_minute(Utc::now()),
    );
    if let Some(row) = plan["tasks"].as_array().and_then(|rows| rows.first()) {
        for key in [
            "status",
            "due",
            "dueCount",
            "missedRuns",
            "scheduledAt",
            "nextDueAt",
            "lockPath",
            "lockState",
            "holderAlive",
            "lockAgeSec",
            "lockTimeoutSec",
            "runLimit",
            "retryEligibleAt",
            "retryAttempt",
            // Why a row is `invalid-schedule` for a non-schedule reason
            // (`lastRunAt`/`notBefore`/`anchorAt`) or carries a retry
            // error: `due` reports these, `describe` must not hide them.
            "error",
            "retryError",
        ] {
            if let Some(value) = row.get(key) {
                document[key] = value.clone();
            }
        }
    }
    document
}

fn emit_describe(document: &serde_json::Value) {
    println!(
        "# danso cron task: {}\n",
        document["id"].as_str().unwrap_or_default()
    );
    for key in [
        "name",
        "schedule",
        "scheduleKind",
        "scheduleValid",
        "scheduleError",
        "enabled",
        "timezone",
        "notify",
        "catchUpPolicy",
        "maxCatchup",
        "payload",
        "allowedTools",
        "permissionMode",
        "notBefore",
        "maxRuns",
        "runCount",
        "lockTimeoutSec",
        "maxRunHistory",
        "retryPolicy",
        "retryState",
        "lastRunAt",
        "lastStatus",
        "lastRunId",
        "runHistoryCount",
        "status",
        "error",
        "retryError",
        "due",
        "dueCount",
        "missedRuns",
        "scheduledAt",
        "nextDueAt",
        "lockPath",
        "lockState",
        "holderAlive",
    ] {
        if let Some(value) = document.get(key) {
            if value.is_null() {
                continue;
            }
            println!(
                "- {key}: {}",
                serde_json::to_string(value).expect("serializable")
            );
        }
    }
    println!(
        "- mode: read-only describe; no execution, push, scheduler, systemd, crontab, or state writes"
    );
}

fn emit_due(plan: &serde_json::Value) {
    println!("# danso cron due plan\n");
    println!("- store: `{}`", plan["store"].as_str().unwrap_or_default());
    println!("- at: `{}`", plan["at"].as_str().unwrap_or_default());
    println!(
        "- mode: dry-run/read-only; no execution, push, scheduler, systemd, crontab, or state writes\n"
    );
    let tasks = plan["tasks"].as_array().cloned().unwrap_or_default();
    if tasks.is_empty() {
        println!("No cron tasks are defined.");
    } else {
        println!(
            "| id | status | schedule | due | due count | missed | scheduled at | next due | lock |"
        );
        println!("|---|---|---|---:|---:|---:|---|---|---|");
        for task in &tasks {
            println!(
                "| `{}` | `{}` | `{}` | {} | {} | {} | `{}` | `{}` | `{}` |",
                task["id"].as_str().unwrap_or_default(),
                task["status"].as_str().unwrap_or_default(),
                task["schedule"].as_str().unwrap_or_default(),
                if task["due"].as_bool().unwrap_or(false) {
                    "true"
                } else {
                    "false"
                },
                task["dueCount"].as_i64().unwrap_or(0),
                task["missedRuns"].as_i64().unwrap_or(0),
                task["scheduledAt"].as_str().unwrap_or_default(),
                task["nextDueAt"].as_str().unwrap_or_default(),
                task["lockState"].as_str().unwrap_or_default(),
            );
        }
    }
    if let Some(errors) = plan["errors"].as_array()
        && !errors.is_empty()
    {
        println!("\n## Errors");
        for error in errors {
            println!("- {}", error.as_str().unwrap_or_default());
        }
    }
}
