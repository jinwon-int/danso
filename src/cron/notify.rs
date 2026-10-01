//! The notify spool write path (§6.5 → §7 `telegram/spool/`): the owner/chat
//! notification for one finished cron run, written as an `O_EXCL` 0600 JSON
//! record the bridge process consumes. A port of ccc `agent_cron.py`
//! (`write_owner_spool`, `build_owner_text`, `notification_base`,
//! `notify_allowed_chats`, `push_spool_dir`, `safe_name`) and the owner-side
//! redaction hardening of ccc `redact_for_owner`.
//!
//! Spool location (documented PR divergence): ccc resolved
//! `CCC_AGENT_CRON_PUSH_SPOOL` → `CCC_PUSH_SPOOL` → `~/.claude/state/
//! telegram-spool`; danso resolves `DANSO_AGENT_CRON_PUSH_SPOOL` →
//! `DANSO_PUSH_SPOOL` → `$DANSO_HOME/telegram/spool` (§7 layout). The chat
//! allowlist env is `DANSO_AGENT_CRON_NOTIFY_ALLOWED_CHATS` (fail-closed:
//! empty/unset means no chat delivery is permitted).

use crate::cron::store::{NotifyMode, Task};
use crate::cron::time::fmt_dt;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::sync::OnceLock;

const MARKER: &str = "[REDACTED_CREDENTIAL]";

/// ccc `_OWNER_REDACTION_HARDENING`: deliberately broader than the canonical
/// credential shapes — the owner spool masks short/near-token values even at
/// the cost of false positives.
const OWNER_HARDENING: [&str; 3] = [
    r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}",
    r"(?i)\b(?:token|secret|password|passwd|api[_-]?key|authorization)\s*[:=]\s*[^\s,;]+",
    r"(?i)\b(?:ghp|gho|ghu|ghs|github_pat|sk|xox[baprs])-?[A-Za-z0-9_./+=-]{8,}",
];

/// ccc `_OWNER_LONG_RUN_MIN`: any run of at least this many token-alphabet
/// characters is masked, except filesystem paths anchored at an allowlisted
/// root (see `owner_safe_path`). The catch-all backstop covers credential
/// shapes nobody enumerated yet while keeping diagnostic paths readable.
const OWNER_LONG_RUN_MIN: usize = 24;

fn compiled(pattern: &str) -> Regex {
    Regex::new(pattern).expect("owner redaction pattern must compile")
}

fn hardening_regexes() -> &'static Vec<Regex> {
    static COMPILED: OnceLock<Vec<Regex>> = OnceLock::new();
    COMPILED.get_or_init(|| OWNER_HARDENING.iter().map(|p| compiled(p)).collect())
}

fn long_token_run() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| compiled(&format!("[A-Za-z0-9_./+=-]{{{OWNER_LONG_RUN_MIN},}}")))
}

/// ccc `_OWNER_SAFE_PATH` semantics, implemented without lookahead (the Rust
/// regex crate has none): an absolute path anchored at an allowlisted
/// filesystem root, followed by one or more `/`-separated segments of at most
/// `OWNER_LONG_RUN_MIN - 1` token-alphabet characters each, every segment
/// ending at `/` or the end of the run. An over-long leaf therefore falls into
/// the leftover stretch and is masked — a segment is never partially consumed
/// (the smuggling channel the Python lookahead closed).
const OWNER_PATH_ROOTS: [&str; 20] = [
    "root", "home", "opt", "data", "usr", "var", "etc", "tmp", "srv", "mnt", "run", "sbin", "bin",
    "lib64", "lib", "proc", "sys", "dev", "media", "boot",
];

fn is_segment_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

/// Byte ranges inside `run` that are allowlisted filesystem paths.
fn safe_path_ranges(run: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = run.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0usize;
    'scan: while index < bytes.len() {
        if bytes[index] == b'/' {
            for root in OWNER_PATH_ROOTS {
                let root_bytes = root.as_bytes();
                let after_root = index + 1 + root_bytes.len();
                if after_root > bytes.len() || !bytes[index + 1..after_root].starts_with(root_bytes)
                {
                    continue;
                }
                // The `+` requires at least one segment: a `/` right after
                // the root name (a bare `/root` at run end preserves nothing).
                if after_root >= bytes.len() || bytes[after_root] != b'/' {
                    continue;
                }
                let mut end = after_root;
                loop {
                    let segment_start = end + 1;
                    let mut cursor = segment_start;
                    while cursor < bytes.len()
                        && is_segment_byte(bytes[cursor])
                        && cursor - segment_start < OWNER_LONG_RUN_MIN - 1
                    {
                        cursor += 1;
                    }
                    // A segment must be terminated by `/` or the run end; a
                    // longer or badly delimited stretch is not a segment.
                    if cursor == segment_start || (cursor < bytes.len() && bytes[cursor] != b'/') {
                        break;
                    }
                    end = cursor;
                    if cursor >= bytes.len() {
                        break;
                    }
                }
                if end > after_root {
                    ranges.push(index..end);
                    index = end;
                    continue 'scan;
                }
            }
        }
        index += 1;
    }
    ranges
}

/// ccc `_mask_long_token_run`: inside one long token run, preserve only the
/// allowlisted filesystem paths; every other stretch that reaches the same
/// threshold the whole run had to reach is masked.
fn mask_long_token_run(run: &str) -> String {
    let mut pieces = String::new();
    let mut cursor = 0usize;
    for range in safe_path_ranges(run) {
        let head = &run[cursor..range.start];
        if head.chars().count() >= OWNER_LONG_RUN_MIN {
            pieces.push_str(MARKER);
        } else {
            pieces.push_str(head);
        }
        pieces.push_str(&run[range.clone()]);
        cursor = range.end;
    }
    let tail = &run[cursor..];
    if tail.chars().count() >= OWNER_LONG_RUN_MIN {
        pieces.push_str(MARKER);
    } else {
        pieces.push_str(tail);
    }
    pieces
}

/// ccc `redact_for_owner`: canonical whole-shape credential redaction, the
/// owner hardening patterns, and the long-token-run backstop, with a final
/// defense-in-depth re-verification — a canonical match must never survive
/// into a spool, so a residual match collapses the whole text to the marker.
///
/// ccc returned `None` when its dynamically imported canonical redaction
/// module could not be loaded (callers then wrote nothing,
/// `blocked-redaction-unavailable`). danso's canonical pass is statically
/// linked, so that failure mode cannot occur; the fail-closed direction is
/// kept by the residual-match collapse above (documented PR divergence).
pub fn redact_for_owner(text: &str, limit: usize) -> String {
    let mut redacted = crate::memory::scan::redact_credential_spans(text);
    for pattern in hardening_regexes() {
        redacted = pattern
            .replace_all(&redacted, regex::NoExpand(MARKER))
            .into_owned();
    }
    redacted = long_token_run()
        .replace_all(&redacted, |captures: &regex::Captures| {
            mask_long_token_run(&captures[0])
        })
        .into_owned();
    if crate::memory::scan::contains_credential(&redacted) {
        redacted = MARKER.to_string();
    }
    crate::cron::commit::short_text(&redacted, limit)
}

/// ccc `safe_name`: filesystem-safe component from an arbitrary string.
pub fn safe_name(value: &str) -> String {
    let mut cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    cleaned.truncate(120);
    if cleaned.is_empty() {
        return "unknown".to_string();
    }
    cleaned
}

/// ccc `push_spool_dir`, danso resolution order.
pub fn push_spool_dir() -> Result<PathBuf, String> {
    for name in ["DANSO_AGENT_CRON_PUSH_SPOOL", "DANSO_PUSH_SPOOL"] {
        if let Ok(raw) = std::env::var(name)
            && !raw.is_empty()
        {
            return Ok(PathBuf::from(raw));
        }
    }
    crate::config::home()
        .map(|home| home.join("telegram").join("spool"))
        .map_err(|error| error.to_string())
}

/// ccc `notify_allowed_chats`: allowlisted chat ids for `notify=telegram-chat*`.
/// Fail-closed: an empty/unset allowlist permits nothing.
pub fn notify_allowed_chats() -> BTreeSet<String> {
    std::env::var("DANSO_AGENT_CRON_NOTIFY_ALLOWED_CHATS")
        .unwrap_or_default()
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// ccc `notification_base`: the delivery-neutral notification view.
pub fn notification_base(task: &Task) -> Value {
    let notify = task.notify;
    json!({
        "policy": notify.label(),
        "send": false,
        "delivery": if notify == NotifyMode::None { "none" } else { "not-attempted" },
        "redactProfile": task.redact_profile,
    })
}

/// A copy of `base` with `delivery` replaced — the ccc `{**base, ...}` shape.
fn with_delivery(base: &Value, delivery: &str) -> Value {
    let mut out = base.clone();
    out["delivery"] = json!(delivery);
    out
}

/// ccc `_FLEET_DIAGNOSTIC_TOKENS`: fleet command payloads (`adapter-fleet-watch`,
/// `fleet-doctor-sweep`) emit one diagnostic row per affected node. Only these
/// exact line-start tokens may be promoted into a notification title — the
/// rest of each row carries untrusted node names, paths, and free-form
/// details. The order is fixed so the title shape is deterministic.
pub const FLEET_DIAGNOSTIC_TOKENS: [&str; 8] = [
    "DOWN",
    "UNREACHABLE",
    "DRIFT",
    "BOOTPATH",
    "DUALDOMAIN",
    "NONCANONICAL",
    "DEGRADED",
    "UNVERIFIED",
];

/// ccc `_FLEET_DIAGNOSTIC_COUNT_MAX`: per-token counts are capped so the title
/// length stays bounded even if the output cap is raised.
pub const FLEET_DIAGNOSTIC_COUNT_MAX: usize = 999;

/// ccc `_VALID_TASK_ID` (the store's own id rule): only a well-formed id may
/// be promoted into the structured alert title.
fn valid_title_task_id(task_id: &str) -> bool {
    (1..=96).contains(&task_id.len())
        && task_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The diagnostic token a line starts with: the token must be followed by a
/// space, a tab, or the end of the line (ccc `^(?:TOKENS)(?=[ \t]|$)` under
/// `re.MULTILINE`, where lines are split on `\n` only).
fn line_token(line: &str) -> Option<usize> {
    FLEET_DIAGNOSTIC_TOKENS.iter().position(|token| {
        line.strip_prefix(token)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })
}

/// ccc `fleet_diagnostic_title`: an allowlist-only fleet alert title for a
/// non-success run. `stdout`/`stderr` must already have passed through
/// `redact_for_owner`. No matched line content is copied — only the fixed
/// token names and bounded occurrence counts leave this classifier.
pub fn fleet_diagnostic_title(
    task_id: &str,
    status: &str,
    stdout: &str,
    stderr: &str,
) -> Option<String> {
    if status == "success" || !valid_title_task_id(task_id) {
        return None;
    }
    let mut counts = [0usize; FLEET_DIAGNOSTIC_TOKENS.len()];
    for output in [stdout, stderr] {
        for line in output.split('\n') {
            if let Some(index) = line_token(line) {
                counts[index] = (counts[index] + 1).min(FLEET_DIAGNOSTIC_COUNT_MAX);
            }
        }
    }
    let signals: Vec<String> = FLEET_DIAGNOSTIC_TOKENS
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|(token, count)| format!("{token}={count}"))
        .collect();
    if signals.is_empty() {
        return None;
    }
    Some(format!(
        "danso cron fleet alert for task {task_id}: {}",
        signals.join(" ")
    ))
}

/// ccc `build_owner_text`. Every captured line passes through
/// `redact_for_owner` before it leaves this module.
///
/// A non-success run whose output carries line-start fleet diagnostic tokens
/// gets the `fleet_diagnostic_title` first line instead of the generic status
/// line. The classifier reads the whole captured (already `outputMaxBytes`-
/// capped) output, not the 900-character body excerpt, so a diagnostic row
/// late in a long watch matrix still raises the alert. ccc reaches the same
/// result through a `fleetDiagnosticTitle` precomputed by its headless runner,
/// because its headless fields are re-cut to 4000 characters; danso's
/// headless document keeps the capped output, so no precomputed title (and
/// no forged-title validation) is needed.
pub fn build_owner_text(
    task_id: &str,
    run_id: &str,
    scheduled_at: &str,
    status: &str,
    headless: &Value,
) -> String {
    let raw_stdout = headless["stdout"].as_str().unwrap_or_default();
    let raw_stderr = headless["stderr"].as_str().unwrap_or_default();
    let title = if status == "success" {
        None
    } else {
        fleet_diagnostic_title(
            task_id,
            status,
            &redact_for_owner(raw_stdout, raw_stdout.len() + 1024),
            &redact_for_owner(raw_stderr, raw_stderr.len() + 1024),
        )
    };
    let stdout = redact_for_owner(raw_stdout, 900);
    let stderr = redact_for_owner(raw_stderr, 900);
    let exit_code = headless["exitCode"]
        .as_i64()
        .map(|code| code.to_string())
        .unwrap_or_default();
    let mut lines = vec![
        title.unwrap_or_else(|| format!("danso cron task {task_id} finished with status={status}")),
        format!("scheduledAt={scheduled_at}"),
        format!("runId={run_id}"),
        format!("exitCode={exit_code}"),
    ];
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    if !stdout.is_empty() {
        lines.push(format!("stdout: {}", body_excerpt(stdout)));
    }
    if !stderr.is_empty() {
        lines.push(format!("stderr: {}", body_excerpt(stderr)));
    }
    lines.join("\n")
}

/// ccc `stdout.replace('\n', ' ')[:900]`: flatten to one line, then cut to
/// 900 characters again. The second cut is what drops the
/// `[truncated N chars]` marker `redact_for_owner` appended past 900 — ccc's
/// spool body never carries it, so danso's must not either (found by the
/// #120 live canary on gwakga: a 930-character danso line vs ccc's 908).
fn body_excerpt(text: &str) -> String {
    text.replace('\n', " ").chars().take(900).collect()
}

fn spool_error(base: &Value, error: String) -> Value {
    let mut out = with_delivery(base, "spool-error");
    out["redacted"] = json!(true);
    out["error"] = json!(crate::cron::commit::short_text(&error, 600));
    out
}

/// ccc `write_owner_spool` (ccc-side-effect `agent_cron.spool_notify`): write
/// one spool record for a finished run. The store schema already requires
/// `notifyChatId` for chat modes, so `blocked-missing-chat` is defense in
/// depth rather than a reachable state. `send` is always false — the bridge
/// owns actual delivery.
pub fn write_owner_spool(
    task: &Task,
    task_id: &str,
    run_id: &str,
    scheduled_at: &str,
    status: &str,
    headless: &Value,
    at: DateTime<Utc>,
) -> Value {
    let base = notification_base(task);
    if task.notify == NotifyMode::None {
        return base;
    }
    if matches!(
        task.notify,
        NotifyMode::TelegramOwnerOnFailure | NotifyMode::TelegramChatOnFailure
    ) && status == "success"
    {
        return with_delivery(&base, "skipped-success");
    }
    let mut chat_id: Option<String> = None;
    if matches!(
        task.notify,
        NotifyMode::TelegramChat | NotifyMode::TelegramChatOnFailure
    ) {
        let candidate = task.notify_chat_id.clone().unwrap_or_default();
        let candidate = candidate.trim().to_string();
        if candidate.is_empty() {
            return with_delivery(&base, "blocked-missing-chat");
        }
        if !notify_allowed_chats().contains(&candidate) {
            return with_delivery(&base, "blocked-not-allowlisted");
        }
        chat_id = Some(candidate);
    }
    let spool = match push_spool_dir() {
        Ok(dir) => dir,
        Err(error) => return spool_error(&base, error),
    };
    let text = build_owner_text(task_id, run_id, scheduled_at, status, headless);
    let ts = fmt_dt(Some(at)).unwrap_or_else(|| Utc::now().to_rfc3339());
    let mut payload = json!({
        "version": 1,
        "ts": ts,
        "event": "AgentCronRun",
        "node": crate::cron::locks::hostname(),
        "text": text,
        "dedup": format!("agent-cron:{task_id}:{run_id}:{status}"),
        "recipient": if chat_id.is_some() { "chat" } else { "owner" },
        "taskId": task_id,
        "runId": run_id,
        "scheduledAt": scheduled_at,
        "status": status,
        "redactProfile": task.redact_profile,
        "redacted": true,
        "send": false,
        "delivery": "spooled",
    });
    if let Some(chat_id) = &chat_id {
        payload["chatId"] = json!(chat_id);
    }
    if let Err(error) = std::fs::create_dir_all(&spool) {
        return spool_error(&base, error.to_string());
    }
    let path = spool.join(format!(
        "{}-{}-{}.json",
        safe_name(&ts),
        safe_name(task_id),
        safe_name(run_id)
    ));
    let mut serialized = match serde_json::to_string(&payload) {
        Ok(text) => text,
        Err(error) => return spool_error(&base, error.to_string()),
    };
    serialized.push('\n');
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| file.write_all(serialized.as_bytes()));
    match written {
        Ok(()) => {
            payload["path"] = json!(path.display().to_string());
            let mut out = with_delivery(&base, "spooled");
            out["redacted"] = json!(true);
            out["spoolPath"] = json!(path.display().to_string());
            out
        }
        Err(error) => spool_error(&base, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_redaction_replaces_whole_credential_shapes() {
        let out = redact_for_owner("token sk-abcdefghijklmnopqrst 0000", 4000);
        assert!(out.contains(MARKER), "{out}");
        assert!(!out.contains("sk-abcdefghijklmnopqrst"), "{out}");
    }

    #[test]
    fn owner_redaction_hardens_key_value_and_bearer_pairs() {
        let out = redact_for_owner("authorization: Bearer abcdefghijklmnop", 4000);
        assert_eq!(out, MARKER);
        let out = redact_for_owner("api_key=shortval00", 4000);
        assert!(out.contains(MARKER), "{out}");
        assert!(!out.contains("shortval00"), "{out}");
    }

    #[test]
    fn long_token_runs_are_masked_but_diagnostic_paths_survive() {
        // 30-char run: `runtime=` (8) + a 22-char path — the ccc 2026-07-30 case.
        let out = redact_for_owner("DRIFT n1 runtime=/home/gongmyoung/ccc-node", 4000);
        assert!(out.contains("/home/gongmyoung/ccc-node"), "{out}");
        // A high-entropy leaf over the threshold inside a path is masked while
        // the directory structure survives.
        let out = redact_for_owner("state=/var/lib/AAAABBBBCCCCDDDDEEEEFFFFGGGG1234/key", 4000);
        // The over-long leaf (and everything after it) is masked; only the
        // allowlisted directory structure survives.
        assert!(out.contains("/var/lib"), "{out}");
        assert!(!out.contains("AAAABBBBCCCCDDDDEEEEFFFFGGGG1234"), "{out}");
        // A long non-path run collapses to the marker.
        let out = redact_for_owner("x AAAABBBBCCCCDDDDEEEEFFFFGGGG", 4000);
        assert!(out.contains(MARKER), "{out}");
        assert!(!out.contains("AAAABBBBCCCCDDDDEEEEFFFF"), "{out}");
    }

    #[test]
    fn residual_canonical_matches_collapse_to_the_marker() {
        // A raw JWT is a canonical shape: the canonical pass replaces it whole
        // and nothing canonical-shaped may survive into a spool.
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_for_owner(jwt, 4000);
        assert_eq!(out, MARKER);
    }

    #[test]
    fn safe_name_rewrites_and_bounds_components() {
        assert_eq!(
            safe_name("2026-09-25T09:30:00+00:00"),
            "2026-09-25T09-30-00-00-00"
        );
        assert_eq!(safe_name(""), "unknown");
        let long = safe_name(&"a".repeat(500));
        assert_eq!(long.len(), 120);
    }

    #[test]
    fn redaction_is_byte_capped_after_masking() {
        let out = redact_for_owner(&"x".repeat(5000), 100);
        assert!(out.chars().count() <= 100 + "[truncated]".len());
    }

    /// Differential golden fixtures: every expectation below was produced by
    /// the ccc reference implementation (`bridge.utils.redaction` canonical
    /// pass + `_OWNER_REDACTION_HARDENING` + `_OWNER_LONG_TOKEN_RUN` masking
    /// with `_OWNER_SAFE_PATH`) on this node. The Rust port must match it
    /// byte-for-byte, including the cases where an over-long path leaf is
    /// masked and the directory structure survives.
    #[test]
    fn owner_redaction_matches_the_ccc_reference_byte_for_byte() {
        let goldens: &[(&str, &str)] = &[
            (
                "runtime=/home/gongmyoung/ccc-node",
                "runtime=/home/gongmyoung/ccc-node",
            ),
            (
                "state=/var/lib/AAAABBBBCCCCDDDDEEEEFFFFGGGG1234/key",
                "state=/var/lib[REDACTED_CREDENTIAL]",
            ),
            ("x AAAABBBBCCCCDDDDEEEEFFFFGGGG", "x [REDACTED_CREDENTIAL]"),
            ("/root/abc/def", "/root/abc/def"),
            ("/root", "/root"),
            ("/root/AAAA23charsAAAAAAAAAAAAA", "[REDACTED_CREDENTIAL]"),
            ("/root/AAAA24charsAAAAAAAAAAAAAB", "[REDACTED_CREDENTIAL]"),
            (
                "/opt/data/path/that/is/fairly/long/but/fine",
                "/opt/data/path/that/is/fairly/long/but/fine",
            ),
            ("/data/rooty/abc", "/data/rooty/abc"),
            ("/runtime/x", "/runtime/x"),
            (
                "prefix=/srv/mnt-=+/AAAA BBBB",
                "prefix=/srv/mnt-=+/AAAA BBBB",
            ),
            (
                "/home/u/deep/deeper/deepest/AAAABBBBCCCCDDDD",
                "/home/u/deep/deeper/deepest/AAAABBBBCCCCDDDD",
            ),
            (
                "/lib64/AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH1234567890",
                "[REDACTED_CREDENTIAL]",
            ),
            (
                "/var/lib/journal/AAAA BBBB/CCCC",
                "/var/lib/journal/AAAA BBBB/CCCC",
            ),
            ("/proc/12345/self", "/proc/12345/self"),
            ("/home/../etc/passwd", "/home/../etc/passwd"),
            ("AAAA./../BBBB/CCCC/./DDDD", "[REDACTED_CREDENTIAL]"),
            ("/tmp/=+/AAAA24charsAAAAAAAAAAAAA=", "[REDACTED_CREDENTIAL]"),
            ("/bin//sh", "/bin//sh"),
            ("/home/gongmyoung/ccc-node", "/home/gongmyoung/ccc-node"),
            (
                "doctor DRIFT n2 boot=AAAA-BBBB runtime=/root/ccc-node",
                "doctor DRIFT n2 boot=AAAA-BBBB runtime=/root/ccc-node",
            ),
        ];
        for (input, expected) in goldens {
            assert_eq!(&redact_for_owner(input, 4000), expected, "input: {input}");
        }
    }

    // --- fleet diagnostic titles: ports of ccc agent-cron.test.sh #829 ---

    fn first_line(task_id: &str, status: &str, headless: Value) -> String {
        build_owner_text(task_id, "run", "2026-01-01T00:00:00Z", status, &headless)
            .lines()
            .next()
            .expect("first line")
            .to_string()
    }

    #[test]
    fn fleet_rows_become_an_allowlist_only_alert_title() {
        let headless = json!({
            "exitCode": 1,
            "stdout": "OK healthy-node (/safe/path)\n\
                       DOWN private-node free-form detail\n\
                       UNREACHABLE secret-node credentials=should-not-title\n\
                       DOWN duplicate-node /private/path\n\
                       prefix DRIFT ignored\n \
                       DOWN ignored-leading-space\n\
                       DOWNSTREAM ignored-prefix\n",
            "stderr": "DRIFT hidden-node doctor_exit=1 runtime=/home/hidden/ccc-node\n\
                       BOOTPATH hidden-node unit=/secret/unit runtime=/secret/runtime\n\
                       DRIFT second-hidden-node api_key=SECRET_LOOKING_VALUE\n",
        });
        let text = build_owner_text(
            "adapter-fleet-watch",
            "run",
            "2026-01-01T00:00:00Z",
            "failed",
            &headless,
        );
        let title = text.lines().next().expect("title");
        assert_eq!(
            title,
            "danso cron fleet alert for task adapter-fleet-watch: \
             DOWN=2 UNREACHABLE=1 DRIFT=2 BOOTPATH=1"
        );
        for leaked in [
            "private-node",
            "/private/path",
            "SECRET_LOOKING_VALUE",
            "ignored",
        ] {
            assert!(!title.contains(leaked), "{leaked} in {title}");
        }
        assert!(!text.contains("SECRET_LOOKING_VALUE"), "{text}");
    }

    #[test]
    fn every_watcher_category_is_summarized_without_node_details() {
        for category in ["DUALDOMAIN", "NONCANONICAL", "DEGRADED", "UNVERIFIED"] {
            let title = first_line(
                "adapter-fleet-watch",
                "failed",
                json!({
                    "exitCode": 1,
                    "stdout": format!("{category} private-node /secret/path\n"),
                    "stderr": format!("{category} second-node\n{category}SUFFIX ignored\n"),
                }),
            );
            assert_eq!(
                title,
                format!("danso cron fleet alert for task adapter-fleet-watch: {category}=2")
            );
            assert!(!title.contains("private-node"), "{title}");
        }
        let combined = fleet_diagnostic_title(
            "adapter-fleet-watch",
            "failed",
            "DOWN phone1\nDOWN phone2\nDUALDOMAIN server details\n",
            "",
        )
        .expect("title");
        assert!(combined.ends_with("DOWN=2 DUALDOMAIN=1"), "{combined}");
    }

    #[test]
    fn a_late_row_past_the_body_excerpt_still_raises_the_alert() {
        for repeats in [45, 180] {
            let stdout = format!(
                "{}DOWN phone channel=matrix reason=no-process\n",
                "OK node channel=telegram\n".repeat(repeats)
            );
            let text = build_owner_text(
                "fleet-doctor-daily",
                "run",
                "2026-01-01T00:00:00Z",
                "failed",
                &json!({ "exitCode": 1, "stdout": stdout, "stderr": "" }),
            );
            let title = text.lines().next().expect("title");
            assert!(title.ends_with("DOWN=1"), "{repeats}: {title}");
            // The ordinary body excerpt stays capped and never carries the row.
            assert!(!text.contains("DOWN phone"), "{repeats}: {text}");
        }
    }

    #[test]
    fn counts_are_capped_per_token() {
        for token in FLEET_DIAGNOSTIC_TOKENS {
            let title = fleet_diagnostic_title(
                "adapter-fleet-watch",
                "failed",
                &format!("{token}\n").repeat(1001),
                "",
            )
            .expect("title");
            assert!(title.ends_with(&format!("{token}=999")), "{title}");
        }
    }

    #[test]
    fn every_non_success_state_is_classified_including_timeout() {
        let title = first_line(
            "fleet-doctor-sweep",
            "timeout",
            json!({ "exitCode": 124, "stdout": "", "stderr": "UNREACHABLE private-node\n" }),
        );
        assert_eq!(
            title,
            "danso cron fleet alert for task fleet-doctor-sweep: UNREACHABLE=1"
        );
    }

    #[test]
    fn unrecognized_or_misplaced_tokens_keep_the_generic_title() {
        let generic = "danso cron task ordinary-task finished with status=failed";
        for stdout in [
            "OK node\nprefix DOWN node",
            " DOWN hidden-node\nDRIFTED hidden-node",
            // `\r` is not a line terminator for the ccc multiline anchor.
            "DOWN\r\nDRIFT\r",
        ] {
            let title = first_line(
                "ordinary-task",
                "failed",
                json!({ "exitCode": 1, "stdout": stdout, "stderr": "ordinary error" }),
            );
            assert_eq!(title, generic, "{stdout:?}");
        }
        // A bare token on its own line (end-of-line lookahead) does count.
        assert!(
            fleet_diagnostic_title("t", "failed", "DOWN", "")
                .expect("title")
                .ends_with("DOWN=1")
        );
    }

    #[test]
    fn success_text_is_unchanged_even_when_output_carries_a_token() {
        let title = first_line(
            "adapter-fleet-watch",
            "success",
            json!({ "exitCode": 0, "stdout": "DOWN diagnostic-only", "stderr": "" }),
        );
        assert_eq!(
            title,
            "danso cron task adapter-fleet-watch finished with status=success"
        );
    }

    #[test]
    fn an_invalid_task_id_is_never_promoted_into_the_alert_title() {
        let title = first_line(
            "bad id\nINJECTED",
            "failed",
            json!({ "exitCode": 1, "stdout": "DOWN hidden-node", "stderr": "" }),
        );
        assert!(!title.contains("fleet alert"), "{title}");
        assert!(fleet_diagnostic_title(&"a".repeat(97), "failed", "DOWN x", "").is_none());
        assert!(fleet_diagnostic_title(&"a".repeat(96), "failed", "DOWN x", "").is_some());
        assert!(fleet_diagnostic_title("", "failed", "DOWN x", "").is_none());
    }
}
