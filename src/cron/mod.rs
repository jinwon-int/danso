//! The read-only `danso cron` surface (§6.5): `list`, `describe <id>`, and
//! `due` over the ccc-compatible `agent-cron` store at
//! `$DANSO_HOME/cron/tasks.json`.
//!
//! Every command is store/plan-only: no locks are acquired, nothing
//! executes, nothing is written. Each JSON result carries the `mutations`
//! object with every field false — typed proof of read-onlyness. Writes
//! (`add`/`edit`/`tick`/`lock`) land in later PRs of #120.

pub mod due;
pub mod retry;
pub mod schedule;
pub mod store;
pub mod time;

use crate::cron::due::{due_plan, normalize, read_only_mutations};
use crate::cron::schedule::parse_schedule;
use crate::cron::store::Store;
use crate::cron::time::truncate_minute;
use chrono::Utc;
use clap::{Parser, Subcommand};
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
    }
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
            tasks: vec![clone_task(task)],
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
        ] {
            if let Some(value) = row.get(key) {
                document[key] = value.clone();
            }
        }
    }
    document
}

fn clone_task(task: &store::Task) -> store::Task {
    serde_json::from_value(serde_json::to_value(task).expect("serializable task"))
        .expect("round trip")
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
