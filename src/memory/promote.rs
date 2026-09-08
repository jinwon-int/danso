//! Audited, idempotent private→shared promotion (issue #52 §7 follow-up;
//! ccc `promotion.py` contract): a private fact is copied into the shared
//! store as `review: "explicit-promotion"` with a deterministic
//! promotion id (`sha256("ccc-memory-promotion-v1" ‖ NUL ‖ scope ‖ NUL ‖
//! fact_id)`), an audit trail under the shared state, and idempotence by
//! destination id + audit id. Never automatic; explicit operator action.

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::facts;
use super::paths::{self, Route};

pub const MAX_FACTS_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_AUDIT_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_AUDITS: usize = 2000;

/// Outcome of one promotion request.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PromotionResult {
    pub promotion_id: String,
    pub destination_fact_id: String,
    pub promoted: bool,
}

fn sha256_hex(bytes: &[u8]) -> String {
    facts::hex_encode(&Sha256::digest(bytes))
}

/// Read an owner-only JSONL file into parsed objects (bounded, fail-closed).
fn read_lines(path: &std::path::Path, max_bytes: u64, what: &str) -> Result<Vec<Value>> {
    let Some(payload) = paths::read_bounded(path, max_bytes, what)? else {
        return Ok(Vec::new());
    };
    let text =
        String::from_utf8(payload).map_err(|_| anyhow::anyhow!("{what} contains invalid UTF-8"))?;
    let mut records = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|_| anyhow::anyhow!("{what} contains an invalid JSON line"))?;
        ensure!(value.is_object(), "{what} line must be an object");
        records.push(value);
    }
    Ok(records)
}

/// Promote one private fact into the shared store (§7). Idempotent: a
/// repeated request verifies the existing records and reports
/// `promoted: false`.
pub fn promote(
    audience_root: &std::path::Path,
    source_scope: &str,
    fact_id: &str,
    now: DateTime<Utc>,
    lock_timeout_ms: u64,
) -> Result<PromotionResult> {
    ensure!(paths::valid_scope(source_scope), "source scope is invalid");
    ensure!(
        source_scope.starts_with("private-"),
        "promotion source must be a private scope"
    );
    ensure!(
        fact_id.starts_with("distill-")
            && fact_id[8..].len() == 12
            && fact_id[8..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "fact id must match distill-<12 lowercase hex>"
    );

    let source_route = Route::new(audience_root, source_scope)?;
    let shared_route = Route::new(audience_root, "shared")?;
    let source_facts = facts::load(&source_route)?;
    let matches: Vec<&facts::FactRecord> =
        source_facts.records().filter(|r| r.id == fact_id).collect();
    ensure!(!matches.is_empty(), "private memory fact not found");
    ensure!(matches.len() == 1, "private memory fact id is not unique");
    let source = matches[0];

    let canonical_source = canonical_json_value(&source.raw);
    let source_fact_hash = sha256_hex(canonical_source.as_bytes());
    let source_scope_hash = sha256_hex(source_scope.as_bytes());
    let stable =
        sha256_hex(format!("ccc-memory-promotion-v1\0{source_scope}\0{fact_id}").as_bytes());
    let promotion_id = format!("promotion-{}", &stable[..24]);
    let destination_fact_id = format!("promoted-{}", &stable[..16]);
    let shared_state = shared_route.state_dir();

    let _lock = paths::ExclusiveLock::acquire(
        &shared_route.scope_lock(),
        paths::lock_timeout(lock_timeout_ms),
    )?;
    let shared_records = read_lines(
        &shared_route.facts_file(),
        MAX_FACTS_FILE_BYTES,
        "shared facts",
    )?;
    let audit_path = shared_state.join("memory-promotion-audit.jsonl");
    let audit_records = read_lines(&audit_path, MAX_AUDIT_BYTES, "promotion audit")?;

    let existing_fact = shared_records
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(destination_fact_id.as_str()));
    let existing_audit = audit_records
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(promotion_id.as_str()));

    let completed_at = existing_audit
        .and_then(|a| {
            a.get("completed_at")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| facts::format_timestamp(now));

    let mut destination_value = source.raw.clone();
    let destination = destination_value
        .as_object_mut()
        .expect("records are objects");
    destination.insert("id".into(), Value::from(destination_fact_id.as_str()));
    destination.insert("review".into(), Value::from("explicit-promotion"));
    destination.insert("privacy".into(), Value::from("shared"));
    destination.insert("audience".into(), Value::from("shared"));
    let promoted_at = Value::from(completed_at.as_str());
    destination.insert("promoted_at".into(), promoted_at.clone());
    {
        let tags = destination
            .entry("tags")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(items) = tags.as_array_mut() {
            for tag in ["promoted", "private-to-shared"] {
                if !items.iter().any(|t| t.as_str() == Some(tag)) {
                    items.push(Value::from(tag));
                }
            }
        }
    }
    destination.insert(
        "source".into(),
        json!({
            "type": "private-to-shared-promotion",
            "promotion_id": promotion_id,
            "source_fact_id": fact_id,
            "source_scope_hash": source_scope_hash,
            "source_fact_hash": source_fact_hash,
        }),
    );
    let audit = json!({
        "id": promotion_id,
        "destination_fact_id": destination_fact_id,
        "source_fact_id": fact_id,
        "source_scope_hash": source_scope_hash,
        "source_fact_hash": source_fact_hash,
        "completed_at": completed_at,
    });

    if let Some(existing) = existing_fact {
        ensure!(
            existing == &destination_value,
            "existing promoted fact does not match its source"
        );
    }
    if let Some(existing) = existing_audit {
        ensure!(
            existing == &audit,
            "existing promotion audit does not match its source"
        );
    }

    let first_promotion = existing_fact.is_none() && existing_audit.is_none();
    if first_promotion {
        let mut lines: Vec<String> = shared_records.iter().map(canonical_json_value).collect();
        lines.push(canonical_json_value(&destination_value));
        let payload = lines.join("\n") + "\n";
        paths::atomic_write(
            &shared_route.facts_file(),
            payload.as_bytes(),
            "shared facts",
        )?;

        let mut audit_lines: Vec<String> = audit_records.iter().map(canonical_json_value).collect();
        audit_lines.push(canonical_json_value(&audit));
        let total = |l: &[String]| l.iter().map(|l| l.len() + 1).sum::<usize>();
        while total(&audit_lines) > MAX_AUDIT_BYTES as usize && audit_lines.len() > 1 {
            audit_lines.remove(0);
        }
        if audit_lines.len() > MAX_AUDITS {
            let drop = audit_lines.len() - MAX_AUDITS;
            audit_lines.drain(..drop);
        }
        paths::atomic_write(
            &audit_path,
            (audit_lines.join("\n") + "\n").as_bytes(),
            "promotion audit",
        )?;
    }

    Ok(PromotionResult {
        promotion_id,
        destination_fact_id,
        promoted: first_promotion,
    })
}

fn canonical_json_value(value: &Value) -> String {
    Value::Object(value.as_object().cloned().unwrap_or_default()).to_string()
}
