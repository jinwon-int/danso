//! The payload executors (§6.5): one headless run of a task's payload — a
//! deterministic `command` argv or a `prompt` harness run — a port of ccc
//! `agent_cron.py::run_headless` plus its `_cap_bytes` output cap.
//!
//! Invocation design (documented PR divergence): ccc drove its prompt runs
//! through `CCC_HEADLESS_CMD` with `CCC_ALLOWED_TOOLS` / `CCC_PERMISSION_MODE`
//! / `CCC_MODEL` environment overrides. danso *is* the harness, so the prompt
//! runner is a subprocess of the current executable
//! (`danso --print --session <run session> [--model M] -- <prompt>`) and only
//! the model pin is propagated — danso has no per-tool allowlist or permission
//! mode to hand down yet, so a prompt task that declares either is refused
//! fail-closed by the run pipeline (`unsupported-tool-policy`) instead of
//! running with different permissions than its author asked for.

use crate::cron::store::{PayloadKind, Task};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// ccc `DEFAULT_OUTPUT_MAX_BYTES`.
pub const DEFAULT_OUTPUT_MAX_BYTES: i64 = 65536;

/// One finished payload run. `exit_code` is the child's exit status, negative
/// when it died on a signal (the Python `subprocess` convention the reference
/// store records use).
#[derive(Debug, Clone)]
pub struct HeadlessOutcome {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Effective `(timeout_sec, output_max_bytes)` for a task's payload, with the
/// ccc defaults and clamps (`timeoutSec >= 1`, `outputMaxBytes >= 1024`).
/// `None` payload (or a prompt payload) defaults to the prompt timeout.
pub fn payload_limits(task: &Task) -> (i64, i64) {
    let kind = task
        .payload
        .as_ref()
        .map(|payload| payload.kind)
        .unwrap_or(PayloadKind::Prompt);
    let default_timeout = match kind {
        PayloadKind::Command => crate::cron::run::DEFAULT_COMMAND_TIMEOUT_SEC,
        PayloadKind::Prompt => crate::cron::run::DEFAULT_PROMPT_TIMEOUT_SEC,
    };
    let timeout = task
        .payload
        .as_ref()
        .and_then(|payload| payload.timeout_sec)
        .filter(|timeout| *timeout >= 1)
        .unwrap_or(default_timeout);
    let cap = task
        .payload
        .as_ref()
        .and_then(|payload| payload.output_max_bytes)
        .filter(|cap| *cap >= 1024)
        .unwrap_or(DEFAULT_OUTPUT_MAX_BYTES);
    (timeout, cap)
}

/// ccc `_cap_bytes`: clip captured output to `cap` UTF-8 bytes and mark the
/// clip. The cap is bytes, the marking is ASCII-safe.
fn cap_bytes(text: String, cap: i64) -> String {
    let cap = cap.max(0) as usize;
    let raw = text.into_bytes();
    if raw.len() <= cap {
        return String::from_utf8_lossy(&raw).into_owned();
    }
    let mut clipped = String::from_utf8_lossy(&raw[..cap]).into_owned();
    clipped.push_str("\n… [truncated by outputMaxBytes]");
    clipped
}

/// Wait for a child up to `timeout`. `Ok(None)` means the timeout elapsed
/// first (the caller kills). A 50 ms poll matches the reference's resolution
/// needs; headless runs are minutes-scale, tests use 1-2 s timeouts.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<Option<std::process::ExitStatus>, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
        if start.elapsed() >= timeout {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Python `subprocess` exit-code convention: negative for signal death.
fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt as _;
    match status.code() {
        Some(code) => code as i64,
        None => status.signal().map(|signal| -(signal as i64)).unwrap_or(-1),
    }
}

/// ccc `run_headless` command branch: spawn the argv, capture both streams,
/// map a timeout to exit 124 with `timedOut` set.
fn run_command_payload(
    task: &Task,
    timeout: Duration,
    cap: i64,
) -> Result<HeadlessOutcome, String> {
    let payload = task
        .payload
        .as_ref()
        .ok_or_else(|| "payload kind 'command' requires a payload".to_string())?;
    let argv = payload
        .argv
        .as_ref()
        .filter(|argv| !argv.is_empty())
        .ok_or_else(|| "payload kind 'command' requires argv".to_string())?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = &payload.cwd {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let timed_out = wait_with_timeout(&mut child, timeout)?.is_none();
    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(HeadlessOutcome {
            exit_code: 124,
            stdout: String::new(),
            stderr: format!("command timed out after {}s", timeout.as_secs().max(1)),
            timed_out: true,
        });
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    Ok(HeadlessOutcome {
        exit_code: exit_code(output.status),
        stdout: cap_bytes(String::from_utf8_lossy(&output.stdout).into_owned(), cap),
        stderr: cap_bytes(String::from_utf8_lossy(&output.stderr).into_owned(), cap),
        timed_out: false,
    })
}

/// The per-run session file for a prompt payload (§7 `cron/runs/`, a danso
/// addition documented in the PR: the harness requires a durable session path
/// and the run is its writer).
pub fn session_path(store_path: &Path, task_id: &str, run_id: &str) -> PathBuf {
    store_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("runs")
        .join(task_id)
        .join(format!("{run_id}.jsonl"))
}

/// ccc `run_headless` prompt branch, danso invocation: a subprocess of the
/// current executable in print mode. The timeout applies to the whole child.
fn run_prompt_payload(
    task: &Task,
    session: &Path,
    timeout: Duration,
) -> Result<HeadlessOutcome, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot resolve the danso executable: {error}"))?;
    let mut command = Command::new(exe);
    command.arg("--print");
    command.arg("--session").arg(session);
    if let Some(model) = task
        .payload
        .as_ref()
        .and_then(|payload| payload.model.clone())
    {
        command.arg("--model").arg(model);
    }
    command.arg("--").arg(&task.prompt);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let timed_out = wait_with_timeout(&mut child, timeout)?.is_none();
    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(HeadlessOutcome {
            exit_code: 124,
            stdout: String::new(),
            stderr: format!("prompt run timed out after {}s", timeout.as_secs().max(1)),
            timed_out: true,
        });
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    Ok(HeadlessOutcome {
        exit_code: exit_code(output.status),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        timed_out: false,
    })
}

/// One payload run. `Err` is the ccc exception path: the run could not even
/// start (unresolvable executable, spawn failure), which the pipeline records
/// as exit 127 / status `failed`.
pub fn run_payload(
    task: &Task,
    store_path: &Path,
    run_id: &str,
) -> Result<HeadlessOutcome, String> {
    let (timeout_sec, cap) = payload_limits(task);
    let timeout = Duration::from_secs(timeout_sec.max(1) as u64);
    let kind = task
        .payload
        .as_ref()
        .map(|payload| payload.kind)
        .unwrap_or(PayloadKind::Prompt);
    match kind {
        PayloadKind::Command => run_command_payload(task, timeout, cap),
        PayloadKind::Prompt => {
            let session = session_path(store_path, &task.id, run_id);
            if let Some(parent) = session.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create the run directory: {error}"))?;
            }
            run_prompt_payload(task, &session, timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_bytes_clips_at_the_byte_cap_and_marks_it() {
        let text = "a".repeat(100);
        let capped = cap_bytes(text.clone(), 40);
        assert!(capped.starts_with("aaaa"));
        assert!(capped.ends_with("\n… [truncated by outputMaxBytes]"));
        assert_eq!(capped.len(), 40 + "\n… [truncated by outputMaxBytes]".len());
        assert_eq!(cap_bytes(text, 1000), "a".repeat(100));
    }

    #[test]
    fn cap_bytes_never_splits_a_utf8_character() {
        let text = "가".repeat(20); // 3 bytes each
        let capped = cap_bytes(text, 10);
        assert!(capped.starts_with("가가가"));
        assert!(capped.ends_with("[truncated by outputMaxBytes]"));
    }

    #[test]
    fn payload_limits_apply_defaults_and_clamps() {
        let mut task: crate::cron::store::Task = serde_json::from_value(serde_json::json!({
            "id": "t",
            "schedule": "@daily",
            "prompt": "hi",
            "enabled": true,
        }))
        .expect("task");
        assert_eq!(payload_limits(&task), (3600, 65536));
        task.payload = Some(
            serde_json::from_value(serde_json::json!({
                "kind": "command",
                "argv": ["/bin/true"],
                "timeoutSec": 0,
                "outputMaxBytes": 5,
            }))
            .expect("payload"),
        );
        assert_eq!(payload_limits(&task), (600, 65536));
        task.payload = Some(
            serde_json::from_value(serde_json::json!({
                "kind": "command",
                "argv": ["/bin/true"],
                "timeoutSec": 3,
                "outputMaxBytes": 2048,
            }))
            .expect("payload"),
        );
        assert_eq!(payload_limits(&task), (3, 2048));
    }
}
