//! Body-free audit ledger (issue #52 §6.4): `state/audit.jsonl` records
//! commit and distill job events with category names and counts only — no
//! bodies, no raw session ids, no paths beyond the scope tree. The file
//! rotates at 1 MiB keeping the newest half.

use anyhow::Result;
use chrono::Utc;
use serde_json::{Value, json};

use super::paths::{self, Route};

const AUDIT_FILE: &str = "audit.jsonl";
const MAX_BYTES: u64 = 1024 * 1024;

/// The auditable memory events (§6.4).
pub enum Event {
    Commit {
        action_id: String,
        changed: Vec<String>,
        facts_added: usize,
    },
    DistillJob {
        job_id: String,
        status: &'static str,
        error_class: &'static str,
    },
}

impl Event {
    fn record(&self) -> Value {
        match self {
            Self::Commit {
                action_id,
                changed,
                facts_added,
            } => json!({
                "ts": timestamp(), "event": "MemoryCommit", "action_id": action_id,
                "changed": changed, "facts_added": facts_added,
            }),
            Self::DistillJob {
                job_id,
                status,
                error_class,
            } => json!({
                "ts": timestamp(), "event": "DistillJob", "job_id": job_id,
                "status": status, "error_class": error_class,
            }),
        }
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Append one event, rotating the ledger at 1 MiB (newest half kept).
/// Best-effort by contract: audit failures are swallowed — they must never
/// break the memory operation they observe (ccc behavior).
pub fn record(route: &Route, event: &Event) {
    let _ = record_strict(route, event);
}

pub fn record_strict(route: &Route, event: &Event) -> Result<()> {
    let path = route.state_dir().join(AUDIT_FILE);
    let mut lines: Vec<String> = std::fs::read(&path)
        .map(|payload| {
            String::from_utf8_lossy(&payload)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    lines.push(serde_json::to_string(&event.record())?);
    let total = |lines: &[String]| lines.iter().map(|l| l.len() + 1).sum::<usize>();
    while total(&lines) as u64 > MAX_BYTES && lines.len() > 2 {
        let half = lines.len() / 2;
        lines.drain(..half);
    }
    let payload = lines.join(
        "
",
    ) + "\n";
    if paths::validate_regular(&path, "audit ledger")? {
        paths::atomic_write(&path, payload.as_bytes(), "audit ledger")?;
    } else {
        std::fs::write(&path, payload.as_bytes())?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
