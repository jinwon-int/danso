//! The run-lock machinery (§6.5): `cron/locks/<id>.lock` written `O_EXCL`
//! 0600, per-task and store-level flock guards, stale reclaim, and the
//! `persist-failed` quarantine. A port of the ccc `agent_cron.py` lock
//! functions (`lock_path`, `write_lock`, `replace_lock`, `lock_command`,
//! `acquire_for_run`, `release_for_run`, `quarantine_persist_failure`).
//!
//! Lock staleness contract (§6.5): only a `bootId` change or the opt-in
//! `lockTimeoutSec` can make a lock stale — a wrong liveness guess must never
//! double-run a task. `lock_status` (in `due.rs`) owns that judgment.

use crate::cron::commit::short_text;
use crate::cron::due::{lock_status, read_only_mutations};
use crate::cron::store::{Task, atomic_write_bytes, locks_dir};
use crate::cron::time::{boot_id, fmt_dt};
use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde_json::{Value, json};
use std::fs::File;
use std::path::{Path, PathBuf};

/// `cron/locks/<id>.lock` (§6.5).
pub fn lock_path(store_path: &Path, task_id: &str) -> PathBuf {
    locks_dir(store_path).join(format!("{task_id}.lock"))
}

/// `cron/locks/<id>.guard` — serializes ownership checks and mutations for one
/// task's lock file (ccc `task_lock_guard`).
pub fn task_guard_path(store_path: &Path, task_id: &str) -> PathBuf {
    locks_dir(store_path).join(format!("{task_id}.guard"))
}

/// `cron/locks/store.lock` — serializes the whole read-modify-write of the
/// task store. An flock, not an O_EXCL run lock: the kernel releases it when
/// the process exits, so a crash cannot wedge every later mutation (ccc
/// `store_lock`).
pub fn store_lock_path(store_path: &Path) -> PathBuf {
    locks_dir(store_path).join("store.lock")
}

/// Read one lock file. Missing → `Null`; a non-object JSON document becomes
/// `{"raw": …}`; unparseable bytes become `{"error": …}` (ccc `read_lock`).
pub fn read_lock(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Value::Null,
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value @ Value::Object(_)) => value,
        Ok(value) => json!({ "raw": value }),
        Err(error) => json!({ "error": format!("invalid lock JSON: {error}") }),
    }
}

/// Blocking exclusive flock on a guard file, released on drop (ccc
/// `flock_guard(blocking=True)`).
pub struct FlockGuard {
    file: File,
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn flock_file(path: &Path) -> Result<FlockGuard, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create locks dir: {error}"))?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot open guard file: {error}"))?;
    file.lock_exclusive()
        .map_err(|error| format!("cannot lock guard file: {error}"))?;
    Ok(FlockGuard { file })
}

/// Hold the store-level flock across a read-modify-write (ccc `store_lock`).
pub fn store_flock(store_path: &Path) -> Result<FlockGuard, String> {
    flock_file(&store_lock_path(store_path))
}

/// Hold one task's guard flock (ccc `task_lock_guard`).
pub fn task_flock(store_path: &Path, task_id: &str) -> Result<FlockGuard, String> {
    flock_file(&task_guard_path(store_path, task_id))
}

/// `socket.gethostname()` equivalent; empty on failure (the lock record then
/// simply carries no host, which the reference tolerates).
fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: `buffer` is writable for at least the passed length and the
    // result is NUL-terminated by the kernel when it fits.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len() - 1);
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// ccc `write_lock`: create the lock file `O_EXCL` 0600 with one JSON line.
/// `AlreadyExists` means another acquirer won the race.
pub fn write_lock_file(path: &Path, payload: &Value) -> Result<(), std::io::Error> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string(payload)
        .map_err(|error| std::io::Error::other(format!("serialize lock: {error}")))?;
    text.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// ccc `replace_lock`: atomically replace an owned lock record without a torn
/// JSON window.
pub fn replace_lock_file(path: &Path, payload: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(payload)
        .map_err(|error| format!("serialize lock: {error}"))?;
    text.push('\n');
    atomic_write_bytes(path, text.as_bytes(), 0o600)
}

fn lock_payload(task_id: &str, run_id: &str, scheduled_at: &str, at: DateTime<Utc>) -> Value {
    json!({
        "taskId": task_id,
        "runId": run_id,
        "pid": std::process::id(),
        "host": hostname(),
        "bootId": boot_id(),
        "acquiredAt": fmt_dt(Some(at)),
        "scheduledAt": if scheduled_at.is_empty() {
            json!(fmt_dt(Some(at)))
        } else {
            json!(scheduled_at)
        },
    })
}

enum Reclaim {
    Reclaimed,
    /// Another reclaimer won the stale lock between the status read and the
    /// rename (ccc reports the after-status).
    Lost,
    Failed(String),
}

/// Move a stale lock aside (`<id>.lock.stale.<unix_ts>.<pid>`, ccc names it
/// the same way) so a fresh `O_EXCL` acquire can proceed.
fn reclaim_stale(path: &Path, at: DateTime<Utc>) -> Reclaim {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside = path.with_file_name(format!(
        "{file_name}.stale.{}.{}",
        at.timestamp(),
        std::process::id()
    ));
    match std::fs::rename(path, &aside) {
        Ok(()) => Reclaim::Reclaimed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Reclaim::Lost,
        Err(error) => Reclaim::Failed(error.to_string()),
    }
}

/// ccc `acquire_for_run_guarded`: acquire one task's run lock while the task
/// guard flock is held. Returns whether it was acquired plus the detail object
/// (`state` of `acquired`/`held`/`persist-failed`/`stale`/`error`, `path`,
/// `holder`).
pub fn acquire_for_run_guarded(
    store_path: &Path,
    task_id: &str,
    task: &Task,
    run_id: &str,
    scheduled_at: &str,
    at: DateTime<Utc>,
) -> (bool, Value) {
    let path = lock_path(store_path, task_id);
    let locks = locks_dir(store_path);
    let status = lock_status(task_id, task, at, &locks);
    let state = status["lockState"].as_str().unwrap_or_default().to_string();
    if state == "held" || state == "persist-failed" {
        return (
            false,
            json!({ "state": state, "path": path.display().to_string(), "holder": status.get("holder") }),
        );
    }
    if state == "stale" {
        match reclaim_stale(&path, at) {
            Reclaim::Reclaimed => {}
            Reclaim::Lost => {
                let after = lock_status(task_id, task, at, &locks);
                return (
                    false,
                    json!({
                        "state": after["lockState"].as_str().unwrap_or("held"),
                        "path": path.display().to_string(),
                        "holder": after.get("holder"),
                    }),
                );
            }
            Reclaim::Failed(error) => {
                return (
                    false,
                    json!({ "state": "error", "path": path.display().to_string(), "error": error }),
                );
            }
        }
    }
    let payload = lock_payload(task_id, run_id, scheduled_at, at);
    if let Err(error) = write_lock_file(&path, &payload) {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            let after = lock_status(task_id, task, at, &locks);
            return (
                false,
                json!({
                    "state": after["lockState"].as_str().unwrap_or("held"),
                    "path": path.display().to_string(),
                    "holder": after.get("holder"),
                }),
            );
        }
        return (
            false,
            json!({ "state": "error", "path": path.display().to_string(), "error": error.to_string() }),
        );
    }
    (
        true,
        json!({
            "state": "acquired",
            "path": path.display().to_string(),
            "holder": payload,
        }),
    )
}

/// ccc `acquire_for_run`: the guarded acquire under the task guard flock.
pub fn acquire_for_run(
    store_path: &Path,
    task_id: &str,
    task: &Task,
    run_id: &str,
    scheduled_at: &str,
    at: DateTime<Utc>,
) -> Result<(bool, Value), String> {
    let _guard = task_flock(store_path, task_id)?;
    Ok(acquire_for_run_guarded(
        store_path,
        task_id,
        task,
        run_id,
        scheduled_at,
        at,
    ))
}

/// ccc `release_for_run`: release a run lock owned by exactly `run_id`.
pub fn release_for_run(store_path: &Path, task_id: &str, run_id: &str) -> Result<Value, String> {
    let _guard = task_flock(store_path, task_id)?;
    let path = lock_path(store_path, task_id);
    let lock = read_lock(&path);
    if lock.is_null() {
        return Ok(json!({ "ok": true, "state": "free" }));
    }
    if lock["runId"].as_str() != Some(run_id) {
        return Ok(json!({ "ok": false, "state": "release-mismatch", "holder": lock }));
    }
    std::fs::remove_file(&path).map_err(|error| format!("cannot remove lock: {error}"))?;
    Ok(json!({ "ok": true, "state": "released" }))
}

/// ccc `quarantine_persist_failure`: retain a non-expiring lock after a
/// run-state write failure. Releasing the ordinary run lock would let the next
/// tick execute the same occurrence again, while its timeout would eventually
/// reclaim a merely retained lock. Only an explicit matching
/// `lock --action release --run-id` unblocks it.
pub fn quarantine_persist_failure(
    store_path: &Path,
    task_id: &str,
    run_id: &str,
    at: DateTime<Utc>,
) -> Result<Value, String> {
    let _guard = task_flock(store_path, task_id)?;
    let path = lock_path(store_path, task_id);
    let lock = read_lock(&path);
    if lock.is_null() {
        return Ok(json!({
            "ok": false,
            "state": "persist-quarantine-missing",
            "path": path.display().to_string(),
        }));
    }
    if lock["runId"].as_str() != Some(run_id) {
        return Ok(json!({
            "ok": false,
            "state": "persist-quarantine-mismatch",
            "path": path.display().to_string(),
            "holder": lock,
        }));
    }
    let mut quarantined = lock;
    if let Value::Object(fields) = &mut quarantined {
        fields.insert("state".to_string(), json!("persist-failed"));
        fields.insert("persistFailedAt".to_string(), json!(fmt_dt(Some(at))));
    }
    if let Err(error) = replace_lock_file(&path, &quarantined) {
        return Ok(json!({
            "ok": false,
            "state": "persist-quarantine-error",
            "path": path.display().to_string(),
            "error": short_text(&error, 4000),
        }));
    }
    Ok(json!({
        "ok": true,
        "state": "persist-failed",
        "path": path.display().to_string(),
        "holder": quarantined,
    }))
}

/// ccc `lock_command_guarded` under the task guard flock: apply one CLI lock
/// operation. Returns the result document and the process exit code.
///
/// `mutations` (§6.5): `probe` carries the all-false read-only proof; a
/// successful `acquire` carries `lockAcquire: true`; `release` is none of the
/// five tracked mutations and carries no object (ccc omits it there too).
pub fn lock_command(
    store_path: &Path,
    task_id: &str,
    task: &Task,
    action: &str,
    run_id: &str,
    scheduled_at: &str,
    at: DateTime<Utc>,
) -> Result<(Value, i32), String> {
    let _guard = task_flock(store_path, task_id)?;
    let path = lock_path(store_path, task_id);
    let locks = locks_dir(store_path);
    let status = lock_status(task_id, task, at, &locks);
    let mut result = json!({ "ok": true, "taskId": task_id, "action": action });
    if let Value::Object(fields) = &status {
        for (key, value) in fields {
            result[key.as_str()] = value.clone();
        }
    }
    match action {
        "probe" => {
            result["mutations"] = read_only_mutations();
            return Ok((result, 0));
        }
        "release" => {
            let holder = status.get("holder").cloned().unwrap_or(json!({}));
            if status["lockState"].as_str() == Some("free") {
                result["lockState"] = json!("free");
                result["ok"] = json!(true);
                return Ok((result, 0));
            }
            if holder["runId"].as_str() != Some(run_id) {
                result["ok"] = json!(false);
                result["lockState"] = json!("release-mismatch");
                result["runId"] = json!(run_id);
                return Ok((result, 1));
            }
            std::fs::remove_file(&path).map_err(|error| format!("cannot remove lock: {error}"))?;
            result["ok"] = json!(true);
            result["lockState"] = json!("released");
            result["runId"] = json!(run_id);
            return Ok((result, 0));
        }
        _ => {}
    }
    // acquire
    let mut reclaimed = false;
    let state = status["lockState"].as_str().unwrap_or_default().to_string();
    if state == "held" || state == "persist-failed" {
        result["ok"] = json!(false);
        result["lockState"] = json!(state);
        result["runId"] = json!(run_id);
        result["mutations"] = read_only_mutations();
        return Ok((result, 1));
    }
    if state == "stale" {
        match reclaim_stale(&path, at) {
            Reclaim::Reclaimed => reclaimed = true,
            Reclaim::Lost => {
                let after = lock_status(task_id, task, at, &locks);
                let mut result =
                    json!({ "ok": false, "taskId": task_id, "action": action, "runId": run_id });
                if let Value::Object(fields) = &after {
                    for (key, value) in fields {
                        result[key.as_str()] = value.clone();
                    }
                }
                return Ok((result, 1));
            }
            Reclaim::Failed(error) => {
                let result = json!({
                    "ok": false,
                    "taskId": task_id,
                    "action": action,
                    "lockState": "error",
                    "error": error,
                    "runId": run_id,
                });
                return Ok((result, 1));
            }
        }
    }
    let payload = lock_payload(task_id, run_id, scheduled_at, at);
    if let Err(error) = write_lock_file(&path, &payload) {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            let after = lock_status(task_id, task, at, &locks);
            let mut result =
                json!({ "ok": false, "taskId": task_id, "action": action, "runId": run_id });
            if let Value::Object(fields) = &after {
                for (key, value) in fields {
                    result[key.as_str()] = value.clone();
                }
            }
            result["mutations"] = read_only_mutations();
            return Ok((result, 1));
        }
        return Err(format!("cannot write lock: {error}"));
    }
    result = json!({
        "ok": true,
        "taskId": task_id,
        "action": action,
        "lockPath": path.display().to_string(),
        "lockState": "acquired",
        "runId": run_id,
        "reclaimedStale": reclaimed,
        "holder": payload,
        "mutations": { "lockAcquire": true, "taskStoreWrite": false, "historyAppend": false, "spoolWrite": false, "execute": false },
    });
    Ok((result, 0))
}
