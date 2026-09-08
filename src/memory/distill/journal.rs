//! Distill pending journal (issue #52 §4.7): one durable JSON record per
//! extractable session under `state/distill-journal/<job_id>.json`, claimed
//! by an flock sidecar, dead-lettered (never deleted) on age, transcript
//! change or repeated failure, with per-failure-class retry delays and a
//! scope-wide cooldown file for hard provider classes.

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::super::Route;
use super::super::paths;
use super::facts;

pub const JOB_SCHEMA: &str = "danso.distill.pending.v1";
pub const MAX_JOB_BYTES: u64 = 4096;
pub const MAX_FAIL_COUNT: u32 = 5;
pub const MAX_AGE_HOURS: i64 = 48;

pub fn journal_dir(route: &Route) -> std::path::PathBuf {
    route.state_dir().join("distill-journal")
}

pub fn cooldown_path(route: &Route) -> std::path::PathBuf {
    route.state_dir().join("distill.cooldown")
}

/// Provider failure classes (§4.7). Classification comes from the failure
/// kind and HTTP status only — never from error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    AuthUnavailable,
    QuotaExhausted,
    RateLimited,
    ModelUnavailable,
    Timeout,
    Other,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthUnavailable => "auth_unavailable",
            Self::QuotaExhausted => "quota_exhausted",
            Self::RateLimited => "rate_limited",
            Self::ModelUnavailable => "model_unavailable",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }

    /// Retry delay seconds (§4.7 backoff table).
    pub fn retry_seconds(self, fail_count: u32, now: DateTime<Utc>) -> Option<i64> {
        let seconds = match self {
            Self::AuthUnavailable | Self::ModelUnavailable => 6 * 3600,
            Self::RateLimited => 30 * 60,
            Self::QuotaExhausted => {
                // Next 01:00 UTC.
                use chrono::Timelike;
                let now_secs = now.time().num_seconds_from_midnight() as i64;
                if now_secs < 3600 {
                    3600 - now_secs
                } else {
                    24 * 3600 - now_secs + 3600
                }
            }
            Self::Timeout | Self::Other => (900i64)
                .saturating_mul(1i64 << fail_count.min(4))
                .min(14400),
        };
        Some(seconds)
    }
}

/// Enqueue outcome (§4.7): the minimum-content gate refuses sessions with
/// fewer than three user/assistant messages.
#[derive(Clone, Debug)]
pub enum EnqueueOutcome {
    Enqueued { job_id: String },
    AlreadyPending { job_id: String },
    Skipped { reason: &'static str },
}

fn job_id(session_id: &str, transcript_sha256: &str) -> String {
    let digest = Sha256::new()
        .chain_update(session_id.as_bytes())
        .chain_update([0u8])
        .chain_update(transcript_sha256.as_bytes())
        .chain_update([0u8])
        .chain_update(b"v1")
        .finalize();
    facts::hex_encode(&digest)
}

fn count_exchanges(session_path: &std::path::Path) -> Result<usize> {
    let payload = std::fs::read(session_path)?;
    let text = String::from_utf8_lossy(&payload);
    let mut count = 0usize;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"] == "message" {
            let role = entry["message"]["role"].as_str().unwrap_or_default();
            if role == "user" || role == "assistant" {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Enqueue one extractable session (§4.7). Idempotent by job id.
pub fn enqueue(
    route: &Route,
    session_path: &std::path::Path,
    trigger: &str,
    now: DateTime<Utc>,
) -> Result<EnqueueOutcome> {
    ensure!(
        super::TRIGGERS.contains(&trigger),
        "unknown distill trigger"
    );
    if count_exchanges(session_path)? < 3 {
        return Ok(EnqueueOutcome::Skipped {
            reason: "insufficient-content",
        });
    }
    let payload = std::fs::read(session_path)?;
    let transcript_sha256 = facts::hex_encode(&Sha256::digest(&payload));
    let transcript_bytes = payload.len();
    let text = String::from_utf8_lossy(&payload);
    let session_id = text
        .lines()
        .find_map(|line| {
            let entry = serde_json::from_str::<Value>(line).ok()?;
            (entry["type"] == "session")
                .then(|| entry["id"].as_str().map(str::to_string))
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("session journal is missing its id"))?;
    let id = job_id(&session_id, &transcript_sha256);
    let dir = journal_dir(route);
    paths::require_private_dir(&dir)?;
    let job_path = dir.join(format!("{id}.json"));
    if paths::validate_regular(&job_path, "distill job")? {
        return Ok(EnqueueOutcome::AlreadyPending { job_id: id });
    }
    let record = json!({
        "schema": JOB_SCHEMA,
        "job_id": id,
        "session_path": session_path.display().to_string(),
        "transcript_sha256": transcript_sha256,
        "transcript_bytes": transcript_bytes,
        "session_id": session_id,
        "trigger": trigger,
        "scope": route.scope(),
        "created_at": facts::format_timestamp(now),
        "fail_count": 0,
        "last_error_class": "",
        "retry_after": Value::Null,
    });
    write_private(&job_path, &serde_json::to_vec(&record)?, MAX_JOB_BYTES)?;
    Ok(EnqueueOutcome::Enqueued { job_id: id })
}

fn write_private(path: &std::path::Path, contents: &[u8], max: u64) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        contents.len() as u64 <= max,
        "journal record exceeds its bound"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// One claimed job with its authenticated record and the held claim lock.
pub struct ClaimedJob {
    pub job_id: String,
    pub record: Value,
    pub session_path: std::path::PathBuf,
    pub transcript_sha256: String,
    pub trigger: String,
    /// Held for the lifetime of the claim: dropping releases the flock.
    #[allow(dead_code)]
    lock: paths::ExclusiveLock,
}

/// Claim the oldest runnable job (claim lock held, cooldown respected,
/// 48h age dead-letter, transcript-change dead-letter, 5-failure cap).
pub fn claim(route: &Route, now: DateTime<Utc>, timeout_ms: u64) -> Result<Option<ClaimedJob>> {
    let dir = journal_dir(route);
    if !dir.is_dir() {
        return Ok(None);
    }
    // Scope-wide hard cooldown.
    if let Ok(payload) = std::fs::read(cooldown_path(route))
        && let Ok(record) = serde_json::from_slice::<Value>(&payload)
        && let Some(until) = record["until"]
            .as_str()
            .and_then(|v| facts::parse_timestamp(v).ok())
        && until > now
    {
        return Ok(None);
    }
    let mut jobs: Vec<(String, Value)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") {
            continue;
        }
        let Some(payload) = paths::read_bounded(&entry.path(), MAX_JOB_BYTES, "distill job")?
        else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<Value>(&payload) else {
            continue;
        };
        let Some(job_id) = record["job_id"].as_str().map(str::to_string) else {
            continue;
        };
        let created_at = record["created_at"]
            .as_str()
            .and_then(|v| facts::parse_timestamp(v).ok())
            .unwrap_or(now);
        let age_hours = (now - created_at).num_hours();
        if age_hours > MAX_AGE_HOURS {
            dead_letter(route, &job_id, &record, "age-exceeded")?;
            continue;
        }
        if record["fail_count"].as_u64().unwrap_or(0) >= MAX_FAIL_COUNT as u64 {
            dead_letter(route, &job_id, &record, "max-attempts")?;
            continue;
        }
        if let Some(retry_after) = record["retry_after"]
            .as_str()
            .and_then(|v| facts::parse_timestamp(v).ok())
            && retry_after > now
        {
            continue;
        }
        jobs.push((created_at.to_rfc3339(), record));
    }
    jobs.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, record) in jobs {
        let job_id = record["job_id"].as_str().expect("validated").to_string();
        let session_path = record["session_path"]
            .as_str()
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("distill job lacks its session path"))?;
        let transcript_sha256 = record["transcript_sha256"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("distill job lacks its transcript hash"))?
            .to_string();
        let trigger = record["trigger"].as_str().unwrap_or("explicit").to_string();
        let lock_path = dir.join(format!("{job_id}.json.lock"));
        let Ok(_lock) = paths::ExclusiveLock::acquire(&lock_path, timeout_ms) else {
            continue; // claimed elsewhere; try the next job
        };
        // Transcript change since enqueue: the extracted claim no longer
        // matches the journal bytes — dead-letter, never extract stale text.
        if let Ok(live) = std::fs::read(&session_path) {
            let live_hash = facts::hex_encode(&Sha256::digest(&live));
            if live_hash != transcript_sha256 {
                dead_letter(route, &job_id, &record, "transcript-changed")?;
                continue;
            }
        } else {
            dead_letter(route, &job_id, &record, "session-missing")?;
            continue;
        }
        return Ok(Some(ClaimedJob {
            job_id,
            record,
            session_path,
            transcript_sha256,
            trigger,
            lock: _lock,
        }));
    }
    Ok(None)
}

/// Dead-letter is a move, never a deletion (§4.7).
pub fn dead_letter(route: &Route, job_id: &str, record: &Value, reason: &str) -> Result<()> {
    let dir = journal_dir(route);
    let dead = dir.join("dead");
    paths::require_private_dir(&dead)?;
    let mut enriched = record.clone();
    if let Some(object) = enriched.as_object_mut() {
        object.insert("dead_letter_reason".into(), Value::from(reason));
    }
    let target = dead.join(format!("{job_id}.json"));
    if !paths::validate_regular(&target, "dead letter")? {
        write_private(&target, &serde_json::to_vec(&enriched)?, MAX_JOB_BYTES)?;
    }
    let _ = std::fs::remove_file(dir.join(format!("{job_id}.json")));
    Ok(())
}

/// Record one failure with its class and retry delay; the fifth failure
/// dead-letters the job. Hard classes additionally cool the scope down.
pub fn record_failure(
    route: &Route,
    job: &ClaimedJob,
    class: FailureClass,
    now: DateTime<Utc>,
) -> Result<()> {
    let fail_count = job.record["fail_count"].as_u64().unwrap_or(0) as u32 + 1;
    let retry_seconds = class.retry_seconds(fail_count, now).unwrap_or(900);
    let retry_after = facts::format_timestamp(now + chrono::Duration::seconds(retry_seconds));
    let mut record = job.record.clone();
    if let Some(object) = record.as_object_mut() {
        object["fail_count"] = json!(fail_count);
        object["last_error_class"] = json!(class.as_str());
        object["retry_after"] = json!(retry_after);
    }
    let dir = journal_dir(route);
    let job_path = dir.join(format!("{}.json", job.job_id));
    let _ = std::fs::remove_file(&job_path);
    write_private(&job_path, &serde_json::to_vec(&record)?, MAX_JOB_BYTES)?;
    if matches!(
        class,
        FailureClass::AuthUnavailable | FailureClass::QuotaExhausted
    ) {
        let cooldown = json!({
            "class": class.as_str(),
            "until": retry_after,
            "at": facts::format_timestamp(now),
        });
        let path = cooldown_path(route);
        let _ = std::fs::remove_file(&path);
        write_private(&path, &serde_json::to_vec(&cooldown)?, 512)?;
    }
    Ok(())
}

/// Remove a successfully extracted job from the queue.
pub fn complete(route: &Route, job_id: &str) -> Result<()> {
    let path = journal_dir(route).join(format!("{job_id}.json"));
    let _ = std::fs::remove_file(&path);
    Ok(())
}
