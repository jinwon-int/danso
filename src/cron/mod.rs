//! The `danso cron` surface (§6.5) over the ccc-compatible `agent-cron`
//! store at `$DANSO_HOME/cron/tasks.json`.
//!
//! PR1 shipped the read-only queries (`list`, `describe <id>`, `due`); PR2
//! added the run-lock surface (`lock`), the tick/run dry-run planners, and the
//! tested state-commit write path (`commit.rs`); PR3 replaces the execute-mode
//! refusals with the payload executors (`exec.rs`: command argv and prompt
//! harness runs) and the notify spool write path (`notify.rs`), wiring the
//! full run pipeline: prechecks → lock → payload → spool → history → retry →
//! run limit → state commit → release/quarantine. PR4 adds the store-mutation
//! surface (`crud.rs`: add/edit/remove/enable/disable under the store flock
//! with the ccc exit-code contract, plus the ccc `tasks.json` import path).

pub mod commit;
pub mod crud;
pub mod due;
pub mod exec;
pub mod locks;
pub mod notify;
pub mod retry;
pub mod run;
pub mod schedule;
pub mod store;
pub mod tick;
pub mod time;

use crate::cron::due::{due_plan, normalize, read_only_mutations};
use crate::cron::schedule::parse_schedule;
use crate::cron::store::Store;
use crate::cron::time::{parse_utc, truncate_minute};
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
    /// Timer-facing tick: plan what would run, or execute the due tasks.
    Tick {
        /// Print the plan without touching the filesystem.
        #[arg(long)]
        dry_run: bool,
        /// Cap executions per tick (1-100).
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(i64).range(1..=100))]
        max_runs: Option<i64>,
        /// Plan for a fixed instant instead of now.
        #[arg(long, value_name = "ISO8601")]
        at: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// One manual run of a task (the payload, its spool notification, and the
    /// durable run-state commit).
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
    /// Create a task in the store (validated, atomic; never executes).
    Add {
        id: String,
        #[command(flatten)]
        flags: CrudFlags,
    },
    /// Set-only partial update (payload flags merge; `--argv` replaces the
    /// whole argv; no clear semantics — unset by remove+add).
    Edit {
        id: String,
        #[command(flatten)]
        flags: CrudFlags,
    },
    /// Remove a task from the store (store-only; never executes).
    Remove {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Enable a disabled task.
    Enable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Disable a task (an in-flight run is not interrupted).
    Disable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Import tasks from a ccc-compatible `tasks.json` (schema v1,
    /// fail-closed). Ids already present in the store are skipped.
    Import {
        /// Source store path.
        #[arg(long, value_name = "PATH")]
        from: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// The `add`/`edit` flag table, mirroring ccc `CRUD_VALUE_FLAGS` /
/// `CRUD_BOOL_FLAGS` (`--json` lives here too, like ccc's parse layer).
#[derive(clap::Args)]
struct CrudFlags {
    #[arg(long)]
    schedule: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    timezone: Option<String>,
    #[arg(long)]
    notify: Option<String>,
    #[arg(long, value_name = "CHAT_ID")]
    notify_chat_id: Option<String>,
    #[arg(long)]
    permission_mode: Option<String>,
    #[arg(long)]
    catch_up_policy: Option<String>,
    #[arg(long, value_name = "ISO8601")]
    anchor_at: Option<String>,
    #[arg(long, value_name = "ISO8601")]
    not_before: Option<String>,
    #[arg(long)]
    redact_profile: Option<String>,
    #[arg(long, value_delimiter = ',')]
    allowed_tools: Option<Vec<String>>,
    #[arg(long, value_delimiter = ',', value_parser = clap::value_parser!(i64))]
    success_exit_codes: Option<Vec<i64>>,
    #[arg(long)]
    max_catchup: Option<i64>,
    #[arg(long)]
    lock_timeout_sec: Option<i64>,
    #[arg(long)]
    max_run_history: Option<i64>,
    #[arg(long)]
    max_runs: Option<i64>,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    timeout_sec: Option<i64>,
    #[arg(long)]
    output_max_bytes: Option<i64>,
    /// Each `--argv` occurrence appends one command word (ccc argv bucket).
    /// ccc consumes the next token blindly, so hyphen-leading words like
    /// `-c` are values here too.
    #[arg(long = "argv", allow_hyphen_values = true)]
    argv: Vec<String>,
    #[arg(long)]
    keep_after_run: bool,
    #[arg(long)]
    disabled: bool,
    #[arg(long)]
    json: bool,
}

impl From<&CrudFlags> for crud::FieldFlags {
    fn from(flags: &CrudFlags) -> Self {
        Self {
            schedule: flags.schedule.clone(),
            prompt: flags.prompt.clone(),
            name: flags.name.clone(),
            timezone: flags.timezone.clone(),
            notify: flags.notify.clone(),
            notify_chat_id: flags.notify_chat_id.clone(),
            permission_mode: flags.permission_mode.clone(),
            catch_up_policy: flags.catch_up_policy.clone(),
            anchor_at: flags.anchor_at.clone(),
            not_before: flags.not_before.clone(),
            redact_profile: flags.redact_profile.clone(),
            allowed_tools: flags.allowed_tools.clone(),
            success_exit_codes: flags.success_exit_codes.clone(),
            max_catchup: flags.max_catchup,
            lock_timeout_sec: flags.lock_timeout_sec,
            max_run_history: flags.max_run_history,
            max_runs: flags.max_runs,
            cwd: flags.cwd.clone(),
            model: flags.model.clone(),
            timeout_sec: flags.timeout_sec,
            output_max_bytes: flags.output_max_bytes,
            argv: flags.argv.clone(),
            keep_after_run: flags.keep_after_run,
            disabled: flags.disabled,
        }
    }
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
                let (document, code) = tick::execute(
                    &path,
                    &store,
                    &store_display,
                    at.as_deref(),
                    truncate_minute(Utc::now()),
                    max_runs.max(1) as usize,
                );
                if json {
                    print_document(&document);
                } else {
                    emit_tick(&document);
                }
                return code;
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
                let (document, code) = run::execute(
                    &path,
                    &store,
                    &store_display,
                    &id,
                    at.as_deref(),
                    truncate_minute(Utc::now()),
                    None,
                );
                if json {
                    print_document(&document);
                } else {
                    emit_run(&document);
                }
                return code;
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
        CronCommand::Add { id, flags } => {
            let json = flags.json;
            crud_dispatch(
                crud::add(&path, &store_display, &id, &crud::FieldFlags::from(&flags)),
                "add",
                json,
            )
        }
        CronCommand::Edit { id, flags } => {
            let json = flags.json;
            crud_dispatch(
                crud::edit(&path, &store_display, &id, &crud::FieldFlags::from(&flags)),
                "edit",
                json,
            )
        }
        CronCommand::Remove { id, json } => crud_dispatch(
            crud::simple(&path, &store_display, "remove", &id),
            "remove",
            json,
        ),
        CronCommand::Enable { id, json } => crud_dispatch(
            crud::simple(&path, &store_display, "enable", &id),
            "enable",
            json,
        ),
        CronCommand::Disable { id, json } => crud_dispatch(
            crud::simple(&path, &store_display, "disable", &id),
            "disable",
            json,
        ),
        CronCommand::Import { from, json } => {
            crud_dispatch(crud::import(&path, &store_display, &from), "import", json)
        }
    }
}

/// Print one CRUD result (JSON passthrough or the ccc one-line report) and
/// return its exit code; store/flock/IO errors print body-free on stderr.
fn crud_dispatch(outcome: Result<(serde_json::Value, i32), String>, mode: &str, json: bool) -> i32 {
    match outcome {
        Ok((document, code)) => {
            emit_crud(mode, &document, json);
            code
        }
        Err(error) => {
            eprintln!("cron: {error}");
            1
        }
    }
}

fn emit_crud(mode: &str, document: &serde_json::Value, as_json: bool) {
    if as_json {
        print_document(document);
        return;
    }
    if document["ok"].as_bool().unwrap_or(false) {
        if mode == "import" {
            println!(
                "danso cron import OK: added {} skipped {}",
                document["added"].as_array().map(Vec::len).unwrap_or(0),
                document["skipped"].as_array().map(Vec::len).unwrap_or(0),
            );
        } else {
            println!(
                "danso cron {mode} OK: {}",
                document["taskId"].as_str().unwrap_or_default()
            );
        }
    } else {
        eprintln!(
            "danso cron {mode} failed: {}",
            document["error"].as_str().unwrap_or_default()
        );
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
    if document["mode"].as_str() == Some("tick-execute") {
        emit_tick_execute(document);
        return;
    }
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

fn emit_tick_execute(document: &serde_json::Value) {
    println!("# danso cron tick execute\n");
    println!(
        "- store: `{}`",
        document["store"].as_str().unwrap_or_default()
    );
    println!("- at: `{}`", document["at"].as_str().unwrap_or_default());
    println!(
        "- planned: {} runnable: {} executed: {} (maxRuns={}{})",
        document["plannedActions"].as_i64().unwrap_or(0),
        document["runnableActions"].as_i64().unwrap_or(0),
        document["executedActions"].as_i64().unwrap_or(0),
        document["maxRuns"].as_i64().unwrap_or(0),
        if document["truncated"].as_bool().unwrap_or(false) {
            ", truncated"
        } else {
            ""
        },
    );
    let results = document["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        println!("No runnable cron tasks.");
    } else {
        println!("\n| task | status | exit | notification | lock release |");
        println!("|---|---|---|---|---|");
        for result in &results {
            println!(
                "| `{}` | `{}` | `{}` | `{}` | `{}` |",
                result["taskId"].as_str().unwrap_or_default(),
                result["status"].as_str().unwrap_or_default(),
                result["headless"]["exitCode"],
                result["notification"]["delivery"]
                    .as_str()
                    .unwrap_or_default(),
                result["lock"]["release"]["state"]
                    .as_str()
                    .unwrap_or_default(),
            );
        }
    }
    if let Some(errors) = document["errors"].as_array()
        && !errors.is_empty()
    {
        println!("\n## Errors");
        for error in errors {
            println!("- {}", error.as_str().unwrap_or_default());
        }
    }
    println!(
        "\n- mutations: `{}`",
        serde_json::to_string(&document["mutations"]).expect("serializable")
    );
}

fn emit_run(document: &serde_json::Value) {
    let title = if document["mode"].as_str() == Some("run-dry-run-read-only") {
        "# danso cron run dry-run plan"
    } else {
        "# danso cron run result"
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
    if document["mode"].as_str() == Some("run-execute") {
        println!(
            "- status: `{}`",
            document["status"].as_str().unwrap_or_default()
        );
        println!(
            "- runId: `{}`",
            document["runId"].as_str().unwrap_or_default()
        );
        if let Some(lock) = document.get("lock").filter(|lock| lock.is_object()) {
            println!(
                "- lock: `{}` release=`{}`",
                lock["state"].as_str().unwrap_or_default(),
                lock["release"]["state"].as_str().unwrap_or_default(),
            );
        }
        if let Some(headless) = document
            .get("headless")
            .filter(|headless| headless.is_object())
        {
            println!(
                "- payload: `{}` exit=`{}`",
                headless["payloadKind"].as_str().unwrap_or_default(),
                headless["exitCode"]
                    .as_i64()
                    .map(|code| code.to_string())
                    .unwrap_or_default(),
            );
        }
        println!(
            "- notification: `{}`",
            document["notification"]["delivery"]
                .as_str()
                .unwrap_or_default(),
        );
        if let Some(retry) = document.get("retry").filter(|retry| retry.is_object()) {
            println!(
                "- retry: cleared=`{}` attempt=`{}` eligibleAt=`{}`",
                retry["cleared"].as_bool().unwrap_or(false),
                retry["attempt"],
                retry["retryEligibleAt"].as_str().unwrap_or("(none)"),
            );
        }
        if let Some(error) = document["persistError"].as_str() {
            println!("- persistError: `{error}`");
        }
        if let Some(release) = document
            .get("releaseError")
            .filter(|release| release.is_object())
        {
            println!(
                "- releaseError: `{}`",
                release["state"].as_str().unwrap_or_default()
            );
        }
        println!(
            "- mutations: `{}`",
            serde_json::to_string(&document["mutations"]).expect("serializable")
        );
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
