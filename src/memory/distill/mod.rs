//! Distill extraction contract (issue #52 §4.6): build the redacted,
//! budget-bounded extraction input from a session journal, call a provider
//! for exactly one tool-free turn (one STRICT retry), and validate the
//! output against the ccc `codex-distill-extraction-v1` schema plus the
//! Danso extensions (`source`, `quote`). The session id never leaves this
//! module — only its hash does.
pub mod extract;
pub mod journal;

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::facts;
use super::scan;

pub const EXTRACTION_INPUT_BUDGET: usize = 32768;
pub const MAX_MESSAGES: usize = 50;
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
pub const MAX_FACTS: usize = 12;
pub const MAX_WIKI_CANDIDATES: usize = 3;
pub const MAX_EVIDENCE: usize = 16;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_QUOTE_CHARS: usize = 120;

/// Triggers accepted by the journal (§4.7).
pub const TRIGGERS: [&str; 5] = [
    "final_answer",
    "budget_exhausted",
    "compaction",
    "explicit",
    "shutdown",
];

/// One validated extraction, ready for the write gates.
pub struct ValidatedExtraction {
    pub summary: Value,
    pub facts: Vec<FactDraft>,
    pub wiki_candidates: Vec<Value>,
}

/// One fact draft from `honcho[]` before the write gates.
pub struct FactDraft {
    pub kind: String,
    pub text: String,
    pub because: Option<String>,
    pub quote: Option<String>,
    pub source_rank: i64,
    pub subject: Option<String>,
}

/// Build the extraction input from a session journal (§4.6): text blocks of
/// user/assistant messages only (compaction checkpoints ride along), latest
/// 50, 8 KiB per message, last 7 days, credentials redacted. Messages are
/// selected newest-first (§4.6), and the per-message 8 KiB trim happens
/// before budget math so the measured input is exactly what the model sees.
/// When the serialized input exceeds `budget_bytes` the oldest messages are
/// dropped (message-unit binary search, JSON never cut mid-payload) and
/// `truncated` is set; the returned input always fits the budget.
pub fn build_input(
    session_path: &std::path::Path,
    trigger: &str,
    budget_bytes: usize,
    now: DateTime<Utc>,
) -> Result<(Value, String, usize)> {
    ensure!(
        TRIGGERS.contains(&trigger),
        "trigger must be one of the contract triggers"
    );
    let payload = std::fs::read(session_path)?;
    let transcript_bytes = payload.len();
    let transcript_sha256 = facts::hex_encode(&Sha256::digest(&payload));
    let text = String::from_utf8(payload)?;
    let mut session_id = String::new();
    let mut messages: Vec<(i64, String, String)> = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["type"] == "session" {
            session_id = entry["id"].as_str().unwrap_or_default().to_string();
            continue;
        }
        if entry["type"] != "message" {
            continue;
        }
        let message = &entry["message"];
        let Some(role) = message["role"].as_str() else {
            continue;
        };
        if role != "user" && role != "assistant" {
            continue;
        }
        let timestamp = message["timestamp"].as_i64().unwrap_or(0);
        let content = &message["content"];
        let text: Option<String> = match content.as_str() {
            Some(text) => Some(text.to_string()),
            None => content.as_array().map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        };
        let Some(text) = text else { continue };
        if text.trim().is_empty() {
            continue;
        }
        messages.push((timestamp, role.to_string(), text));
    }
    ensure!(
        !session_id.is_empty(),
        "session journal is missing its session id"
    );
    // Last 7 days, latest 50 (§4.6).
    let now_ms = now.timestamp_millis();
    let week_ms = 7 * 24 * 3600 * 1000i64;
    messages.retain(|(timestamp, _, _)| *timestamp == 0 || now_ms - timestamp <= week_ms);
    // Newest-first selection (§4.6): the tail of the chronological session
    // is the most recent work, and the payload lists the newest message
    // first. The 8 KiB per-message trim happens before budget math so the
    // serialized input the model sees is exactly what was measured.
    let total = messages.len();
    messages.reverse();
    messages.truncate(MAX_MESSAGES);
    for (_, _, text) in messages.iter_mut() {
        if text.len() > MAX_MESSAGE_BYTES {
            *text = scan::truncate_utf8(text, MAX_MESSAGE_BYTES).to_string();
        }
    }

    // Message-unit budget fit (§4.6): binary search the largest newest-first
    // prefix whose serialized input fits `budget_bytes`. The JSON is never
    // cut mid-payload, and `truncated` records any dropped messages.
    let build = |keep: usize| -> Value {
        let message_payload: Vec<Value> = messages[..keep]
            .iter()
            .map(|(timestamp, role, text)| {
                let redacted = scan::redact_credentials(text);
                json!({"role": role, "timestamp": timestamp, "text": redacted})
            })
            .collect();
        let byte_count: usize = message_payload
            .iter()
            .map(|m| m["text"].as_str().map(str::len).unwrap_or(0))
            .sum();
        let source_thread_hash = facts::hex_encode(&Sha256::digest(session_id.as_bytes()));
        json!({
            "schema_version": 1,
            "provider": "danso",
            "content_trust": "untrusted",
            "source_thread_hash": source_thread_hash,
            "trigger": trigger,
            "captured_at": facts::format_timestamp(now),
            "truncated": keep < messages.len() || total > MAX_MESSAGES,
            "messages": message_payload,
            "message_count": message_payload.len(),
            "byte_count": byte_count,
        })
    };
    let fits = |keep: usize| -> Result<bool> {
        Ok(serde_json::to_vec(&build(keep))?.len() <= budget_bytes.max(1))
    };
    ensure!(
        fits(0)?,
        "extraction input cannot fit its budget even with no messages"
    );
    let (mut low, mut high) = (0usize, messages.len());
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid)? {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let input = build(low);
    Ok((input, transcript_sha256, transcript_bytes))
}

/// Duplicate-key detection: `serde_json` silently keeps the last value,
/// but the extraction contract rejects ambiguous payloads outright. A
/// minimal string-aware scanner tracks each object's seen keys.
fn contains_duplicate_keys(payload: &str) -> bool {
    enum Frame {
        Object {
            keys: Vec<String>,
            expecting_key: bool,
        },
        Array,
    }
    let bytes = payload.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => escaped = !escaped,
                b'"' if !escaped => {
                    in_string = false;
                    if let Some(Frame::Object {
                        keys,
                        expecting_key,
                    }) = stack.last_mut()
                        && *expecting_key
                    {
                        let key = String::from_utf8_lossy(&bytes[string_start + 1..i]);
                        if keys.iter().any(|k| k == &key) {
                            return true;
                        }
                        keys.push(key.into_owned());
                        *expecting_key = false;
                    }
                }
                _ => escaped = false,
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                escaped = false;
                string_start = i;
            }
            b'{' => stack.push(Frame::Object {
                keys: Vec::new(),
                expecting_key: true,
            }),
            b'[' => stack.push(Frame::Array),
            b'}' => {
                stack.pop();
            }
            b']' => {
                stack.pop();
            }
            b',' => {
                if let Some(Frame::Object { expecting_key, .. }) = stack.last_mut() {
                    *expecting_key = true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Validate one extraction response (§4.6): bounds, duplicate keys, NaN and
/// Infinity, credential and directive patterns, provenance identity, and
/// the per-fact field rules. Returns the drafts for the write gates.
pub fn validate_output(
    raw: &[u8],
    source_thread_hash: &str,
    trigger: &str,
    wiki_enabled: bool,
) -> Result<ValidatedExtraction> {
    ensure!(
        raw.len() <= MAX_RESPONSE_BYTES,
        "extraction response exceeds 64 KiB"
    );
    let text = std::str::from_utf8(raw)?;
    ensure!(
        !text.contains("NaN") && !text.contains("Infinity"),
        "extraction response contains NaN or Infinity"
    );
    ensure!(
        !contains_duplicate_keys(text),
        "extraction response contains duplicate JSON keys"
    );
    let value: Value = serde_json::from_str(text)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("extraction response must be a JSON object"))?;
    for key in object.keys() {
        ensure!(
            matches!(
                key.as_str(),
                "schema_version" | "provenance" | "honcho" | "wiki_candidates" | "resume"
            ),
            "extraction response has an unknown field"
        );
    }
    ensure!(
        object.get("schema_version").and_then(Value::as_i64) == Some(1),
        "extraction schema_version must be 1"
    );
    let provenance = object
        .get("provenance")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("extraction provenance missing"))?;
    ensure!(
        provenance.len() == 4
            && provenance.get("provider").and_then(Value::as_str) == Some("danso")
            && provenance.get("source_thread_hash").and_then(Value::as_str)
                == Some(source_thread_hash)
            && provenance.get("trigger").and_then(Value::as_str) == Some(trigger)
            && provenance
                .get("distilled_at")
                .and_then(Value::as_str)
                .is_some_and(|at| facts::parse_timestamp(at).is_ok()),
        "extraction provenance must match the input exactly"
    );
    // Credential or directive patterns reject the whole extraction (§4.6).
    let scan_outcome = scan::scan("extraction", text, None);
    ensure!(
        !scan_outcome
            .categories
            .iter()
            .any(|c| matches!(*c, "credential-pattern" | "prompt-injection")),
        "extraction response contains credential or directive patterns"
    );
    let honcho = object
        .get("honcho")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("extraction honcho missing"))?;
    ensure!(honcho.len() <= MAX_FACTS, "too many facts");
    let mut drafts = Vec::new();
    for item in honcho {
        let Some(item) = item.as_object() else {
            anyhow::bail!("fact must be an object");
        };
        for key in item.keys() {
            ensure!(
                matches!(
                    key.as_str(),
                    "kind" | "text" | "subject" | "because" | "source" | "quote"
                ),
                "fact has an unknown field"
            );
        }
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fact kind missing"))?;
        ensure!(
            facts::KINDS.contains(&kind),
            "fact kind is not one of the seven kinds"
        );
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("fact text missing"))?;
        ensure!(
            !text.trim().is_empty()
                && text.chars().count() <= facts::MAX_FACT_TEXT_CHARS
                && text.len() <= facts::MAX_FACT_TEXT_BYTES,
            "fact text out of bounds"
        );
        let because = item.get("because").and_then(Value::as_str);
        if kind == "decision" {
            ensure!(
                because.is_some_and(|b| !b.trim().is_empty()),
                "decision fact lacks a because reason"
            );
        }
        let quote = item.get("quote").and_then(Value::as_str);
        if let Some(quote) = quote {
            ensure!(
                quote.chars().count() <= MAX_QUOTE_CHARS,
                "fact quote exceeds 120 characters"
            );
        }
        let source = item.get("source").and_then(Value::as_str);
        if let Some(source) = source {
            ensure!(
                matches!(source, "user-stated" | "measured" | "inferred"),
                "fact source is invalid"
            );
        }
        let subject = item.get("subject").and_then(Value::as_str);
        if let Some(subject) = subject {
            ensure!(
                !subject.is_empty() && subject.len() <= 64,
                "fact subject is invalid"
            );
        }
        // §4.1: rank follows the source (3 user-stated / 2 measured / 1
        // inferred). The write gates demote a rank-2/3 draft to 1 unless it
        // cites a verbatim ≥8-char quote from the transcript (needs-human).
        let rank = match source {
            Some("user-stated") => 3,
            Some("measured") => 2,
            _ => 1,
        };
        drafts.push(FactDraft {
            kind: kind.to_string(),
            text: text.to_string(),
            because: because.map(str::to_string),
            quote: quote.map(str::to_string),
            source_rank: rank,
            subject: subject.map(str::to_string),
        });
    }
    let wiki_enabled_flag = wiki_enabled;
    let wiki = match object.get("wiki_candidates") {
        Some(Value::Array(items)) => {
            ensure!(
                wiki_enabled_flag || items.is_empty(),
                "wiki candidates must be empty while the wiki is disabled"
            );
            ensure!(
                items.len() <= MAX_WIKI_CANDIDATES,
                "too many wiki candidates"
            );
            items.clone()
        }
        None | Some(Value::Null) => Vec::new(),
        Some(_) => anyhow::bail!("wiki candidates must be an array"),
    };
    let summary = object.get("resume").cloned().unwrap_or_else(|| {
        json!({
            "last_activity": "", "pending_action": "", "awaiting_user": false,
            "open_question": "", "next_step": "", "evidence": []
        })
    });
    let resume = summary
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("resume must be an object"))?;
    let evidence = resume
        .get("evidence")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    ensure!(
        evidence
            .as_array()
            .is_some_and(|items| items.len() <= MAX_EVIDENCE),
        "resume evidence exceeds 16 entries"
    );
    Ok(ValidatedExtraction {
        summary,
        facts: drafts,
        wiki_candidates: wiki,
    })
}
