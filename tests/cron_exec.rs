//! The cron execute pipelines (#120 PR3, §6.5): the `run` pipeline
//! (prechecks → lock → payload → spool → history → retry → run limit → state
//! commit → release/quarantine) and the `tick` scheduler handoff — a port of
//! ccc `agent_cron.py::run_execute` / `scheduler_execute` with the danso
//! mutation names (`spoolWrite`, `execute`).
//!
//! Command payloads run real child processes (`/bin/sh`); prompt payloads are
//! covered through the policy refusal and the subprocess failure path (the
//! test binary refuses the harness flags), never a provider call.

use chrono::{DateTime, Utc};
use danso::cron::locks::{lock_path, read_lock, write_lock_file};
use danso::cron::run;
use danso::cron::store::{self, store_path};
use danso::cron::tick;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

const AT: &str = "2026-09-25T09:30:00Z";
const LATER: &str = "2026-09-25T09:45:00Z";

/// The spool directory and the chat allowlist come from process-global env;
/// tests that touch them serialize here.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn utc(raw: &str) -> DateTime<Utc> {
    danso::cron::time::parse_utc(Some(raw), "test")
        .expect("parse")
        .expect("non-empty")
}

struct Fixture {
    /// Not read directly; dropping the fixture removes the store's directory.
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fixture {
    fn spool_dir(&self) -> PathBuf {
        self.path.parent().expect("parent").join("spool")
    }
}

fn make_fixture(tasks: Value) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, serde_json::to_string(&tasks).expect("serializable")).expect("write");
    Fixture { _dir: dir, path }
}

fn base_task(id: &str) -> Value {
    json!({
        "id": id,
        "schedule": "30 9 25 9 *",
        "prompt": "report",
        "enabled": true,
        "payload": { "kind": "command", "argv": ["/bin/sh", "-c", "echo hello"] },
    })
}

fn loaded(fixture: &Fixture) -> store::Store {
    store::load(&fixture.path).expect("valid store")
}

fn execute(fixture: &Fixture, store: &store::Store, id: &str, at: &str) -> (Value, i32) {
    run::execute(
        &fixture.path,
        store,
        &fixture.path.display().to_string(),
        id,
        Some(at),
        utc(LATER),
        None,
    )
}

fn read_store(fixture: &Fixture) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&fixture.path).expect("read store"))
        .expect("JSON")
}

/// Point the spool at the fixture for the duration of `body` and wipe it.
fn with_spool(fixture: &Fixture, body: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().expect("lock");
    // SAFETY: single-threaded relative to this variable by ENV_LOCK; restored
    // before the guard drops.
    unsafe { std::env::set_var("DANSO_AGENT_CRON_PUSH_SPOOL", fixture.spool_dir()) };
    let _ = std::fs::remove_dir_all(fixture.spool_dir());
    body();
    unsafe { std::env::remove_var("DANSO_AGENT_CRON_PUSH_SPOOL") };
}

fn spool_files(fixture: &Fixture) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixture.spool_dir())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

// --- run pipeline: the success shape ---

#[test]
fn a_successful_command_run_commits_every_mutation() {
    let mut task = base_task("a");
    task["notify"] = json!("telegram-owner");
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["status"], json!("success"));
        assert_eq!(result["headless"]["exitCode"], json!(0));
        assert_eq!(result["headless"]["payloadKind"], json!("command"));
        assert_eq!(result["headless"]["stdout"], json!("hello\n"));
        assert_eq!(result["notification"]["delivery"], json!("spooled"));
        assert_eq!(result["oneShotDisabled"], json!(false));
        assert_eq!(
            result["mutations"],
            json!({
                "lockAcquire": true,
                "taskStoreWrite": true,
                "historyAppend": true,
                "spoolWrite": true,
                "execute": true,
            })
        );
        let released = result["lock"]["release"]["state"].as_str().expect("state");
        assert_eq!(released, "released");
        assert!(!lock_path(&fixture.path, "a").exists(), "lock removed");
    });
    let persisted = read_store(&fixture);
    let task = &persisted["tasks"][0];
    assert_eq!(task["lastStatus"], json!("success"));
    assert_eq!(task["lastRunAt"], json!(AT));
    assert_eq!(task["runCount"], json!(0)); // no maxRuns declared: not counted
    assert_eq!(task["runHistory"].as_array().map(Vec::len), Some(1));
    assert_eq!(task["runHistory"][0]["status"], json!("success"));
    assert_eq!(task["runHistory"][0]["attempt"], json!(1));
    assert_eq!(task["runHistory"][0]["notifyState"], json!("spooled"));
    assert_eq!(task["runHistory"][0]["scheduledAt"], json!(AT));
    assert!(
        task.get("retryState").is_none(),
        "success clears retry state"
    );
    assert!(
        fixture.path.with_file_name("tasks.json.bak").exists(),
        "bak snapshot"
    );
}

#[test]
fn an_owner_spool_record_matches_the_reference_shape() {
    let mut task = base_task("a");
    task["notify"] = json!("telegram-owner");
    let fixture = make_fixture(json!({
        "version": 1,
        "tasks": [task]
    }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 0, "{result}");
        let files = spool_files(&fixture);
        assert_eq!(files.len(), 1, "{files:?}");
        let meta = std::fs::metadata(&files[0]).expect("metadata");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let record: Value =
            serde_json::from_str(&std::fs::read_to_string(&files[0]).expect("read spool"))
                .expect("JSON");
        assert_eq!(record["version"], json!(1));
        assert_eq!(record["event"], json!("AgentCronRun"));
        // dedup is agent-cron:<task>:<runId>:<status>; runId = <task>-<unix>-<pid>
        let run_id = result["runId"].as_str().expect("run id").to_string();
        assert_eq!(
            record["dedup"],
            json!(format!("agent-cron:a:{run_id}:success"))
        );
        assert_eq!(record["recipient"], json!("owner"));
        assert_eq!(record["status"], json!("success"));
        assert_eq!(record["taskId"], json!("a"));
        assert_eq!(record["scheduledAt"], json!(AT));
        assert_eq!(record["send"], json!(false));
        assert_eq!(record["redacted"], json!(true));
        assert_eq!(record["redactProfile"], json!("default"));
        assert!(
            record["text"]
                .as_str()
                .expect("text")
                .contains("status=success")
        );
        assert!(
            record["text"]
                .as_str()
                .expect("text")
                .contains("stdout: hello")
        );
        // ccc parity: the spool file itself carries no self-path; the run
        // result exposes the record location as notification.spoolPath.
        assert_eq!(
            result["notification"]["spoolPath"].as_str(),
            files[0].to_str(),
            "result points at the record"
        );
    });
}

// --- command failure, timeout, output cap, redaction ---

#[test]
fn a_failing_command_records_failed_and_schedules_a_retry_when_declared() {
    let mut task = base_task("a");
    task["payload"]["argv"] = json!(["/bin/sh", "-c", "echo boom >&2; exit 3"]);
    task["retryPolicy"] = json!({ "maxAttempts": 2, "backoffSec": 0 });
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["status"], json!("failed"));
        assert_eq!(result["headless"]["exitCode"], json!(3));
        assert_eq!(result["retry"]["cleared"], json!(false));
        assert_eq!(result["retry"]["attempt"], json!(1));
        assert_eq!(result["retry"]["exhausted"], json!(false));
        assert_eq!(
            result["retry"]["retryEligibleAt"],
            json!("2026-09-25T09:30:00Z"),
            "backoff 0: eligible immediately"
        );
        let persisted = read_store(&fixture);
        assert_eq!(persisted["tasks"][0]["lastStatus"], json!("failed"));
        assert_eq!(persisted["tasks"][0]["retryState"]["attempt"], json!(1));
        // The retry is due again immediately; the second failure exhausts.
        let store2 = loaded(&fixture);
        let (result2, code2) = execute(&fixture, &store2, "a", AT);
        assert_eq!(code2, 1, "{result2}");
        assert_eq!(result2["retry"]["exhausted"], json!(true));
        assert_eq!(result2["retry"]["attempt"], json!(2));
        assert!(result2["retry"]["retryEligibleAt"].is_null());
        let persisted2 = read_store(&fixture);
        assert_eq!(
            persisted2["tasks"][0]["retryState"]["lastStatus"],
            json!("exhausted")
        );
    });
}

#[test]
fn a_timed_out_command_is_never_a_success() {
    let mut task = base_task("a");
    task["payload"]["argv"] = json!(["/bin/sleep", "5"]);
    task["payload"]["timeoutSec"] = json!(1);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["status"], json!("timeout"));
        assert_eq!(result["headless"]["exitCode"], json!(124));
        assert_eq!(result["headless"]["timedOut"], json!(true));
        // 124 in the success list must not rescue a timed-out run.
        let mut task_value = base_task("a");
        task_value["payload"]["argv"] = json!(["/bin/sleep", "5"]);
        task_value["payload"]["timeoutSec"] = json!(1);
        task_value["successExitCodes"] = json!([0, 124]);
        let fixture2 = make_fixture(json!({ "version": 1, "tasks": [task_value] }));
        let store2 = loaded(&fixture2);
        let (result2, code2) = execute(&fixture2, &store2, "a", AT);
        assert_eq!(code2, 1, "{result2}");
        assert_eq!(result2["status"], json!("timeout"));
    });
}

#[test]
fn command_output_is_capped_at_output_max_bytes() {
    let mut task = base_task("a");
    task["payload"]["argv"] = json!([
        "/bin/sh",
        "-c",
        "echo; head -c 8000 /dev/zero | tr '\\0' 'x'"
    ]);
    task["payload"]["outputMaxBytes"] = json!(1024);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, _code) = execute(&fixture, &store, "a", AT);
        let stdout = result["headless"]["stdout"].as_str().expect("stdout");
        assert!(
            stdout.ends_with("\n… [truncated by outputMaxBytes]"),
            "{stdout:?}"
        );
        assert!(stdout.len() < 2048, "capped: {}", stdout.len());
    });
}

#[test]
fn spool_text_is_redacted_before_it_leaves_the_process() {
    let mut task = base_task("a");
    task["notify"] = json!("telegram-owner");
    task["payload"]["argv"] = json!([
        "/bin/sh",
        "-c",
        "echo token sk-abcdefghijklmnopqrstuv leaked"
    ]);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, _code) = execute(&fixture, &store, "a", AT);
        assert_eq!(result["status"], json!("success"));
        let files = spool_files(&fixture);
        assert_eq!(files.len(), 1);
        let text = {
            let record: Value =
                serde_json::from_str(&std::fs::read_to_string(&files[0]).expect("read spool"))
                    .expect("JSON");
            record["text"].as_str().expect("text").to_string()
        };
        assert!(text.contains("[REDACTED_CREDENTIAL]"), "{text}");
        assert!(!text.contains("sk-abcdefghijklmnopqrstuv"), "{text}");
    });
}

// --- notification policy paths ---

#[test]
fn notify_none_writes_no_spool_record() {
    let mut task = base_task("a");
    task["notify"] = json!("none");
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["notification"]["delivery"], json!("none"));
        assert_eq!(result["notification"]["policy"], json!("none"));
        assert_eq!(result["mutations"]["spoolWrite"], json!(false));
        assert_eq!(spool_files(&fixture).len(), 0);
        let persisted = read_store(&fixture);
        assert_eq!(
            persisted["tasks"][0]["runHistory"][0]["notifyState"],
            json!("none")
        );
    });
}

#[test]
fn on_failure_notify_skips_success_and_spools_failures() {
    let mut success_task = base_task("ok");
    success_task["notify"] = json!("telegram-owner-on-failure");
    let mut failed_task = base_task("bad");
    failed_task["notify"] = json!("telegram-owner-on-failure");
    failed_task["payload"]["argv"] = json!(["/bin/sh", "-c", "exit 7"]);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [success_task, failed_task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (ok_result, _) = execute(&fixture, &store, "ok", AT);
        assert_eq!(
            ok_result["notification"]["delivery"],
            json!("skipped-success")
        );
        let (bad_result, _) = execute(&fixture, &store, "bad", AT);
        assert_eq!(bad_result["notification"]["delivery"], json!("spooled"));
        assert_eq!(spool_files(&fixture).len(), 1, "only the failure spooled");
    });
}

#[test]
fn a_chat_notification_requires_an_allowlisted_chat_id() {
    let mut task = base_task("a");
    task["notify"] = json!("telegram-chat");
    task["notifyChatId"] = json!("-1001234567890");
    let pristine = json!({ "version": 1, "tasks": [task.clone()] });
    let fixture = make_fixture(pristine.clone());
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        // Fail closed: an empty allowlist permits nothing. ENV_LOCK is already
        // held by with_spool for the whole body (the Mutex is not reentrant).
        unsafe { std::env::remove_var("DANSO_AGENT_CRON_NOTIFY_ALLOWED_CHATS") };
        let (result, _) = execute(&fixture, &store, "a", AT);
        assert_eq!(
            result["notification"]["delivery"],
            json!("blocked-not-allowlisted")
        );
        assert_eq!(result["mutations"]["spoolWrite"], json!(false));
        assert_eq!(spool_files(&fixture).len(), 0);
        // An allowlisted id spools with recipient=chat. Restore the pristine
        // store first: the first run consumed this schedule window (its
        // lastRunAt commit makes a same-instant rerun not-due).
        std::fs::write(
            &fixture.path,
            serde_json::to_string(&pristine).expect("serializable"),
        )
        .expect("restore store");
        unsafe { std::env::set_var("DANSO_AGENT_CRON_NOTIFY_ALLOWED_CHATS", "-1001234567890") };
        let store2 = loaded(&fixture);
        let (result2, _) = execute(&fixture, &store2, "a", AT);
        assert_eq!(result2["notification"]["delivery"], json!("spooled"));
        unsafe { std::env::remove_var("DANSO_AGENT_CRON_NOTIFY_ALLOWED_CHATS") };
        let files = spool_files(&fixture);
        assert_eq!(files.len(), 1);
        let record: Value =
            serde_json::from_str(&std::fs::read_to_string(&files[0]).expect("read spool"))
                .expect("JSON");
        assert_eq!(record["recipient"], json!("chat"));
        assert_eq!(record["chatId"], json!("-1001234567890"));
    });
}

// --- one-shot, persist failure, task removal ---

#[test]
fn a_successful_once_task_disables_itself() {
    let mut task = base_task("once");
    task["schedule"] = json!("at 2026-09-25T09:30:00Z");
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "once", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["oneShotDisabled"], json!(true));
        let persisted = read_store(&fixture);
        assert_eq!(persisted["tasks"][0]["enabled"], json!(false));
        assert!(!lock_path(&fixture.path, "once").exists());
    });
}

#[test]
fn keep_after_run_keeps_a_once_task_enabled() {
    let mut task = base_task("once");
    task["schedule"] = json!("at 2026-09-25T09:30:00Z");
    task["keepAfterRun"] = json!(true);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "once", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["oneShotDisabled"], json!(false));
        let persisted = read_store(&fixture);
        assert_eq!(persisted["tasks"][0]["enabled"], json!(true));
    });
}

#[test]
fn a_persist_failure_quarantines_instead_of_releasing() {
    let fixture = make_fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = loaded(&fixture);
    // Corrupt the on-disk store after the in-memory load: the state commit
    // re-reads it and must fail closed.
    std::fs::write(&fixture.path, "{ not json").expect("corrupt");
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["status"], json!("persist-failed"));
        assert!(result["persistError"].as_str().is_some());
        assert_eq!(result["mutations"]["taskStoreWrite"], json!(false));
        assert_eq!(result["mutations"]["historyAppend"], json!(false));
        assert_eq!(result["mutations"]["execute"], json!(true));
        let quarantine = read_lock(&lock_path(&fixture.path, "a"));
        assert_eq!(
            quarantine["state"],
            json!("persist-failed"),
            "lock retained"
        );
        assert_eq!(result["lock"]["release"]["state"], json!("persist-failed"));
    });
}

#[test]
fn a_task_removed_while_running_still_reports_the_run() {
    let fixture = make_fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = loaded(&fixture);
    std::fs::remove_file(&fixture.path).expect("remove store");
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["ok"], json!(true), "the run itself succeeded");
        assert_eq!(result["mutations"]["taskStoreWrite"], json!(false));
        assert_eq!(result["mutations"]["historyAppend"], json!(false));
        assert_eq!(result["mutations"]["execute"], json!(true));
        assert_eq!(result["lock"]["release"]["state"], json!("released"));
    });
}

// --- prechecks and the lock ---

#[test]
fn prechecks_refuse_zero_mutation_with_ok_true() {
    // run-limit reached
    let mut task = base_task("limited");
    task["maxRuns"] = json!(3);
    task["runCount"] = json!(3);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    let (result, code) = execute(&fixture, &store, "limited", AT);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["status"], json!("run-limit-reached"));
    assert_eq!(
        result["mutations"],
        json!({
            "lockAcquire": false, "taskStoreWrite": false, "historyAppend": false,
            "spoolWrite": false, "execute": false,
        })
    );
    // disabled
    let mut task = base_task("off");
    task["enabled"] = json!(false);
    let fixture2 = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store2 = loaded(&fixture2);
    let (result, code) = execute(&fixture2, &store2, "off", AT);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["status"], json!("disabled"));
    // not due: the 09:30 window was consumed by lastRunAt (occurrences are
    // scanned strictly after it).
    let mut task = base_task("a");
    task["lastRunAt"] = json!(AT);
    let fixture3 = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store3 = loaded(&fixture3);
    let (result, code) = execute(&fixture3, &store3, "a", LATER);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["status"], json!("not-due"));
    // unknown task
    let fixture4 = make_fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store4 = loaded(&fixture4);
    let (result, code) = execute(&fixture4, &store4, "ghost", AT);
    assert_eq!(code, 1);
    assert_eq!(result["error"], json!("task id not found"));
}

#[test]
fn a_held_lock_refuses_the_run_without_mutations() {
    let fixture = make_fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = loaded(&fixture);
    let payload = json!({
        "taskId": "a", "runId": "other-1-2", "pid": std::process::id(),
        "bootId": danso::cron::time::boot_id(),
        "acquiredAt": AT, "scheduledAt": AT,
    });
    write_lock_file(&lock_path(&fixture.path, "a"), &payload).expect("lock");
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "a", AT);
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["status"], json!("locked"));
        assert_eq!(
            result["mutations"],
            json!({
                "lockAcquire": false, "taskStoreWrite": false, "historyAppend": false,
                "spoolWrite": false, "execute": false,
            })
        );
        // The pre-existing holder was not touched.
        let holder = read_lock(&lock_path(&fixture.path, "a"));
        assert_eq!(holder["runId"], json!("other-1-2"));
    });
}

// --- prompt payload policy ---

#[test]
fn a_prompt_task_with_a_tool_policy_is_refused_fail_closed() {
    let mut task = base_task("policy");
    task["payload"] = json!({ "kind": "prompt" });
    task["allowedTools"] = json!(["Bash"]);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "policy", AT);
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["ok"], json!(false));
        assert_eq!(result["status"], json!("unsupported-tool-policy"));
        assert_eq!(
            result["mutations"],
            json!({
                "lockAcquire": false, "taskStoreWrite": false, "historyAppend": false,
                "spoolWrite": false, "execute": false,
            })
        );
        assert!(!lock_path(&fixture.path, "policy").exists());
        assert_eq!(spool_files(&fixture).len(), 0);
    });
    // permissionMode refuses the same way.
    let mut task = base_task("perm");
    task["payload"] = json!({ "kind": "prompt" });
    task["permissionMode"] = json!("acceptEdits");
    let fixture2 = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store2 = loaded(&fixture2);
    let (result, code) = execute(&fixture2, &store2, "perm", AT);
    assert_eq!(code, 1);
    assert_eq!(result["status"], json!("unsupported-tool-policy"));
    // Command payloads ignore these fields (ccc parity): this runs.
    let mut task = base_task("cmd");
    task["allowedTools"] = json!(["Bash"]);
    let fixture3 = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store3 = loaded(&fixture3);
    with_spool(&fixture3, || {
        let (result, code) = execute(&fixture3, &store3, "cmd", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["status"], json!("success"));
    });
}

#[test]
fn a_prompt_task_runs_as_a_subprocess_and_records_its_outcome() {
    let mut task = base_task("prompted");
    task["payload"] = json!({ "kind": "prompt" });
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "prompted", AT);
        // The child here is this test binary, which rejects the harness
        // flags — the pipeline records that as a failed run with a non-zero
        // exit and a captured stderr, exactly like any failing payload.
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["status"], json!("failed"));
        assert_ne!(result["headless"]["exitCode"], json!(0));
        assert!(
            !result["headless"]["stderr"]
                .as_str()
                .expect("stderr")
                .is_empty()
        );
        assert_eq!(result["mutations"]["execute"], json!(true));
        assert_eq!(result["mutations"]["historyAppend"], json!(true));
        assert_eq!(result["lock"]["release"]["state"], json!("released"));
        // The run's session directory was created under cron/runs/.
        let runs = fixture.path.parent().expect("parent").join("runs/prompted");
        assert!(runs.is_dir(), "{runs:?}");
    });
}

// --- tick scheduler handoff ---

#[test]
fn the_tick_executes_up_to_max_runs_and_aggregates_mutations() {
    let fixture = make_fixture(json!({
        "version": 1,
        "tasks": [
            base_task("a"),
            base_task("b"),
            { "id": "c", "schedule": "30 9 25 9 *", "prompt": "x", "enabled": false,
              "payload": { "kind": "command", "argv": ["/bin/true"] } },
            { "id": "d", "schedule": "0 0 1 1 *", "prompt": "x", "enabled": true,
              "lastRunAt": AT,
              "payload": { "kind": "command", "argv": ["/bin/true"] } },
        ]
    }));
    let store = loaded(&fixture);
    let (result, code) = tick::execute(
        &fixture.path,
        &store,
        &fixture.path.display().to_string(),
        Some(AT),
        utc(LATER),
        2,
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["mode"], json!("tick-execute"));
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["plannedActions"], json!(4));
    assert_eq!(result["runnableActions"], json!(2));
    assert_eq!(result["executedActions"], json!(2));
    assert_eq!(result["truncated"], json!(false));
    let results = result["results"].as_array().cloned().expect("results");
    assert_eq!(results.len(), 2);
    for (index, run_result) in results.iter().enumerate() {
        assert_eq!(run_result["status"], json!("success"), "#{index}");
        assert_eq!(run_result["lock"]["release"]["state"], json!("released"));
    }
    assert_eq!(
        result["mutations"],
        json!({
            "lockAcquire": true,
            "taskStoreWrite": true,
            "historyAppend": true,
            "spoolWrite": false,
            "execute": true,
        })
    );
    let persisted = read_store(&fixture);
    assert_eq!(persisted["tasks"][0]["lastStatus"], json!("success"));
    assert_eq!(persisted["tasks"][1]["lastStatus"], json!("success"));
    assert!(persisted["tasks"][1].get("lastRunAt").is_some());
    // The disabled and not-due tasks were never executed.
    assert!(
        persisted["tasks"][2].get("lastStatus").is_none()
            || persisted["tasks"][2]["lastStatus"].is_null()
    );
    assert!(
        persisted["tasks"][3].get("lastStatus").is_none()
            || persisted["tasks"][3]["lastStatus"].is_null()
    );
}

#[test]
fn the_tick_caps_executions_at_max_runs() {
    let fixture = make_fixture(json!({
        "version": 1,
        "tasks": [base_task("a"), base_task("b"), base_task("c")]
    }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = tick::execute(
            &fixture.path,
            &store,
            &fixture.path.display().to_string(),
            Some(AT),
            utc(LATER),
            2,
        );
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["runnableActions"], json!(3));
        assert_eq!(result["executedActions"], json!(2));
        assert_eq!(result["truncated"], json!(true));
    });
}

#[test]
fn an_empty_or_idle_tick_executes_nothing() {
    let fixture = make_fixture(json!({ "version": 1, "tasks": [] }));
    let store = loaded(&fixture);
    let (result, code) = tick::execute(
        &fixture.path,
        &store,
        &fixture.path.display().to_string(),
        Some(AT),
        utc(LATER),
        10,
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["executedActions"], json!(0));
    assert_eq!(
        result["mutations"],
        json!({
            "lockAcquire": false, "taskStoreWrite": false, "historyAppend": false,
            "spoolWrite": false, "execute": false,
        })
    );
}

#[test]
fn run_limit_disables_a_task_during_the_tick_and_stops_later_runs() {
    let mut task = base_task("once-only");
    task["maxRuns"] = json!(1);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task, base_task("b")] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        // First tick: the task runs and hits its limit (disabled).
        let (result, code) = tick::execute(
            &fixture.path,
            &store,
            &fixture.path.display().to_string(),
            Some(AT),
            utc(LATER),
            10,
        );
        assert_eq!(code, 0, "{result}");
        let persisted = read_store(&fixture);
        assert_eq!(persisted["tasks"][0]["enabled"], json!(false));
        assert_eq!(persisted["tasks"][0]["runCount"], json!(1));
        // Second tick at the same instant: the limit precheck refuses.
        let store2 = loaded(&fixture);
        let (result2, _) = tick::execute(
            &fixture.path,
            &store2,
            &fixture.path.display().to_string(),
            Some(AT),
            utc(LATER),
            10,
        );
        let results = result2["results"].as_array().cloned().unwrap_or_default();
        assert!(
            results
                .iter()
                .all(|run_result| run_result["status"] != json!("success")
                    || run_result["taskId"] == json!("b")),
            "the limited task must not run again: {results:?}"
        );
    });
}

#[test]
fn a_run_limit_cancelled_retry_drops_retry_eligibility() {
    let mut task = base_task("limited-retry");
    task["maxRuns"] = json!(1);
    task["retryPolicy"] = json!({ "maxAttempts": 3, "backoffSec": 60 });
    task["payload"]["argv"] = json!(["/bin/sh", "-c", "exit 1"]);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, _code) = execute(&fixture, &store, "limited-retry", AT);
        assert_eq!(result["status"], json!("failed"));
        assert_eq!(result["runLimit"]["reached"], json!(true));
        assert_eq!(result["retry"]["cancelledByRunLimit"], json!(true));
        assert!(result["retry"]["retryEligibleAt"].is_null());
        let persisted = read_store(&fixture);
        // apply_run_limit dropped the pending retry state when disabling.
        assert!(
            persisted["tasks"][0].get("retryState").is_none()
                || persisted["tasks"][0]["retryState"].is_null()
        );
        assert_eq!(persisted["tasks"][0]["enabled"], json!(false));
    });
}

// --- spool hygiene ---

#[test]
fn a_spawn_failure_is_a_failed_run_not_a_crash() {
    let mut task = base_task("missing-binary");
    task["payload"]["argv"] = json!(["/nonexistent-binary-0192837465", "--flag"]);
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "missing-binary", AT);
        assert_eq!(code, 1, "{result}");
        assert_eq!(result["status"], json!("failed"));
        assert_eq!(result["headless"]["exitCode"], json!(127));
        assert!(
            result["headless"]["stderr"]
                .as_str()
                .expect("stderr")
                .contains("No such file")
        );
        assert_eq!(result["lock"]["release"]["state"], json!("released"));
    });
}

#[test]
fn command_payload_cwd_is_honored() {
    let mut task = base_task("cwd");
    task["payload"]["argv"] = json!(["/bin/sh", "-c", "pwd"]);
    task["payload"]["cwd"] = json!("/tmp");
    let fixture = make_fixture(json!({ "version": 1, "tasks": [task] }));
    let store = loaded(&fixture);
    with_spool(&fixture, || {
        let (result, code) = execute(&fixture, &store, "cwd", AT);
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["headless"]["stdout"], json!("/tmp\n"));
        assert_eq!(result["headless"]["cwd"], json!("/tmp"));
    });
}
