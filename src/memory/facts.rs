//! Fact records and write gates (issue #52 §4.1/§4.2).
//!
//! The on-disk contract is the ccc-node `memory-facts.jsonl` superset: one
//! JSON object per line, canonical serialization (sorted keys, compact
//! separators, raw UTF-8, trailing newline). Three writer variants must be
//! readable — the ccc bridge sink (full superset), the ccc hook committer
//! (no `schema_version`/`durability`/`source_rank`), and Danso itself — so
//! loading interprets missing fields with fixed defaults instead of
//! rejecting. Unparseable lines are preserved opaquely, never dropped.
//!
//! Write gates run in a fixed order (the order changes outcomes):
//! normalize → mutable-ops filter → rank validation → decision reason →
//! auto-supersede → conflict review → dedup.

use anyhow::{Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::OnceLock;

use super::paths;

/// The only schema version Danso writes; other versions stay readable as
/// raw lines but are not interpreted.
pub const SCHEMA_VERSION: i64 = 1;
/// Fact kinds Danso can write (ccc distill contract). Unknown kinds stay
/// readable (marked needs-human) but are refused on write.
pub const KINDS: [&str; 7] = [
    "preference",
    "decision",
    "observation",
    "context",
    "task-progress",
    "procedure",
    "constraint",
];
/// Kinds that must always keep their reason (ccc required-v1 contract).
pub const REASON_KINDS: [&str; 1] = ["decision"];
/// Kinds whose text is never memory (mutable operational state, §4.2 gate 1).
pub const MUTABLE_OPS_KINDS: [&str; 2] = ["observation", "context"];
/// ccc extraction bound: fact text ≤ 4096 characters (and its widest UTF-8).
pub const MAX_FACT_TEXT_CHARS: usize = 4096;
pub const MAX_FACT_TEXT_BYTES: usize = MAX_FACT_TEXT_CHARS * 4;
/// Existing-file read bound (ccc `_MAX_EXISTING_FACT_BYTES`).
pub const MAX_FACTS_FILE_BYTES: u64 = 8 << 20;
/// Rolling tail: the file keeps the last `max_facts` lines after an append.
pub const MAX_FACTS_DEFAULT: usize = 1000;
const MAX_LABEL_BYTES: usize = 64;
const MAX_ENTITIES: usize = 16;
const MAX_TAGS: usize = 16;
const MAX_QUOTE_CHARS: usize = 120;
/// A rank-2/3 extraction must cite a quote found verbatim in the transcript
/// and at least this long (nunchi G2, §4.1).
pub const QUOTE_MIN_CHARS: usize = 8;

/// Canonical timestamp format: RFC 3339 UTC with second precision and `Z`.
pub fn format_timestamp(moment: DateTime<Utc>) -> String {
    moment.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn now_timestamp() -> String {
    format_timestamp(Utc::now())
}

/// Parse an RFC 3339 timestamp with timezone; naive timestamps are refused
/// (configuration, not data).
pub fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value.trim())
        .map_err(|_| anyhow::anyhow!("timestamp must be RFC 3339 with an offset"))?;
    Ok(parsed.with_timezone(&Utc))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    MalformedJson,
    NotAnObject,
    UnsupportedSchemaVersion,
    InvalidField(&'static str),
    AudienceMismatch,
    InvalidWindow,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedJson => write!(f, "malformed JSON"),
            Self::NotAnObject => write!(f, "record is not a JSON object"),
            Self::UnsupportedSchemaVersion => write!(f, "unsupported schema version"),
            Self::InvalidField(name) => write!(f, "invalid field {name}"),
            Self::AudienceMismatch => write!(f, "privacy and audience labels disagree"),
            Self::InvalidWindow => write!(f, "valid_until is not after valid_from"),
        }
    }
}

impl std::error::Error for RecordError {}

/// Word normalization shared with the ccc sink: `[0-9a-z가-힣]+` runs from
/// the lowercased text joined by single spaces (hanzi/kana/jamo are
/// separators). Dedup keys and distill ids are derived from this.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for ch in text.to_lowercase().chars() {
        if matches!(ch, '0'..='9' | 'a'..='z' | '\u{AC00}'..='\u{D7A3}') {
            word.push(ch);
        } else if !word.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&word);
            word.clear();
        }
    }
    if !word.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    out
}

/// Deterministic distill id: `distill-` + sha256(job_id ‖ NUL ‖ normalized)
/// truncated to 12 hex chars — converges across retries (§4.1).
pub fn derive_distill_id(job_id: &str, normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(job_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    format!("distill-{}", hex_encode(&digest[..6]))
}

/// Manual CLI fact id (stable across retries of the same text).
pub fn derive_manual_id(normalized: &str) -> String {
    let digest = Sha256::digest(normalized.as_bytes());
    format!("manual-{}", hex_encode(&digest[..6]))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// One interpreted facts-file line.
#[derive(Clone, Debug)]
pub enum Line {
    /// A parsed, defaults-filled record plus its raw JSON (for lossless
    /// field-level edits such as close/supersede).
    Good(Box<FactRecord>),
    /// A line that failed to parse, preserved byte-for-byte (§4.1 file rule).
    Opaque(String),
}

/// One validated, defaults-filled fact record. Unknown extra JSON fields are
/// tolerated on load and preserved through the `raw` value.
#[derive(Clone, Debug)]
pub struct FactRecord {
    pub line_no: usize,
    pub schema_version: i64,
    pub id: String,
    pub kind: String,
    /// True only when `kind` is one of the seven writable kinds.
    pub known_kind: bool,
    pub text: String,
    pub because: Option<String>,
    pub quote: Option<String>,
    pub supersedes: Option<String>,
    /// `private` or `shared` after defaulting (`privacy`, else `audience`,
    /// else `private`; the scope tree is the real boundary).
    pub audience: String,
    pub durability: String,
    pub confidence: f64,
    /// 1 inferred .. 3 user-stated (defaults to 1 for the hook variant).
    pub source_rank: i64,
    pub observed_at: Option<String>,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
    pub review: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    /// The original JSON object, kept for lossless re-serialization.
    pub raw: Value,
}

impl FactRecord {
    /// The fact's subject is the first entity (ccc convention, §4.2).
    pub fn subject(&self) -> Option<&str> {
        self.entities.first().map(String::as_str)
    }

    /// Default durability when a writer omitted the field.
    pub fn default_durability(kind: &str) -> &'static str {
        if kind == "task-progress" {
            "volatile"
        } else {
            "durable"
        }
    }

    /// Interpret one JSONL line. Defaults fill the ccc hook committer
    /// variant; the interpretation rules are pinned by fixtures (§4.1).
    pub fn interpret(line: &str, line_no: usize) -> Result<Self, RecordError> {
        let value: Value = serde_json::from_str(line).map_err(|_| RecordError::MalformedJson)?;
        let Some(object) = value.as_object() else {
            return Err(RecordError::NotAnObject);
        };
        let get = |field: &str| object.get(field);
        let text_of = |field: &'static str| -> Result<Option<String>, RecordError> {
            match get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(raw)) => Ok(Some(raw.clone())),
                Some(_) => Err(RecordError::InvalidField(field)),
            }
        };

        let schema_version = match get("schema_version") {
            None | Some(Value::Null) => SCHEMA_VERSION,
            Some(Value::Number(number)) => {
                let version = number
                    .as_i64()
                    .ok_or(RecordError::InvalidField("schema_version"))?;
                ensure_usable_version(version)?;
                version
            }
            Some(_) => return Err(RecordError::InvalidField("schema_version")),
        };

        let id = match text_of("id")? {
            Some(id) => id,
            None => format!("line-{line_no}"),
        };

        let kind = text_of("kind")?.unwrap_or_else(|| "observation".to_string());
        let known_kind = KINDS.contains(&kind.as_str());

        let text = text_of("text")?.ok_or(RecordError::InvalidField("text"))?;
        if text.trim().is_empty()
            || text.chars().count() > MAX_FACT_TEXT_CHARS
            || text.len() > MAX_FACT_TEXT_BYTES
        {
            return Err(RecordError::InvalidField("text"));
        }

        let privacy = text_of("privacy")?;
        let audience_raw = text_of("audience")?;
        let audience = audience_raw
            .clone()
            .or_else(|| privacy.clone())
            .unwrap_or_else(|| "private".to_string());
        if audience != "private" && audience != "shared" {
            return Err(RecordError::InvalidField("audience"));
        }
        if let (Some(privacy), Some(audience_raw)) = (&privacy, &audience_raw)
            && privacy != audience_raw
        {
            return Err(RecordError::AudienceMismatch);
        }

        let durability = match text_of("durability")? {
            Some(raw) => {
                if raw != "durable"
                    && raw != "volatile"
                    && raw != "session-only"
                    && raw != "week-scale"
                {
                    return Err(RecordError::InvalidField("durability"));
                }
                raw
            }
            None => Self::default_durability(&kind).to_string(),
        };

        let confidence = match get("confidence") {
            None | Some(Value::Null) => 0.7,
            Some(Value::Number(number)) => {
                let value = number
                    .as_f64()
                    .ok_or(RecordError::InvalidField("confidence"))?;
                if !(0.0..=1.0).contains(&value) {
                    return Err(RecordError::InvalidField("confidence"));
                }
                value
            }
            Some(_) => return Err(RecordError::InvalidField("confidence")),
        };

        let source_rank = match get("source_rank") {
            None | Some(Value::Null) => 1,
            Some(Value::Number(number)) => {
                let rank = number
                    .as_i64()
                    .ok_or(RecordError::InvalidField("source_rank"))?;
                if !(1..=3).contains(&rank) {
                    return Err(RecordError::InvalidField("source_rank"));
                }
                rank
            }
            Some(_) => return Err(RecordError::InvalidField("source_rank")),
        };

        let observed_at = match text_of("observed_at")? {
            Some(raw) => {
                parse_timestamp(&raw).map_err(|_| RecordError::InvalidField("observed_at"))?;
                Some(raw)
            }
            None => None,
        };

        let entities = parse_labels(get("entities"), "entities")?;
        let tags = parse_labels(get("tags"), "tags")?;

        let review = match text_of("review")? {
            Some(raw) => raw,
            None => "auto-local".to_string(),
        };

        let mut valid_from_raw = text_of("valid_from")?;
        let mut valid_until_raw = text_of("valid_until")?;
        if let Some(raw) = &valid_from_raw {
            parse_timestamp(raw).map_err(|_| RecordError::InvalidField("valid_from"))?;
        }
        if let Some(raw) = &valid_until_raw {
            parse_timestamp(raw).map_err(|_| RecordError::InvalidField("valid_until"))?;
        }
        if let (Some(from), Some(until)) = (&valid_from_raw, &valid_until_raw) {
            let from =
                parse_timestamp(from).map_err(|_| RecordError::InvalidField("valid_from"))?;
            let until =
                parse_timestamp(until).map_err(|_| RecordError::InvalidField("valid_until"))?;
            if from >= until {
                return Err(RecordError::InvalidWindow);
            }
        }
        // Canonicalize nulls: JSON `null` windows are the same as absent.
        if valid_from_raw.as_deref() == Some("null") {
            valid_from_raw = None;
        }
        if valid_until_raw.as_deref() == Some("null") {
            valid_until_raw = None;
        }

        let quote = match text_of("quote")? {
            Some(raw) => {
                if raw.chars().count() > MAX_QUOTE_CHARS {
                    return Err(RecordError::InvalidField("quote"));
                }
                Some(raw)
            }
            None => None,
        };

        Ok(Self {
            line_no,
            schema_version,
            id,
            known_kind,
            kind,
            text,
            because: text_of("because")?,
            quote,
            supersedes: text_of("supersedes")?,
            audience,
            durability,
            confidence,
            source_rank,
            observed_at,
            entities,
            tags,
            review,
            valid_from: valid_from_raw,
            valid_until: valid_until_raw,
            raw: value,
        })
    }

    /// Canonical serialization: sorted keys, compact separators, raw UTF-8
    /// (never ASCII-escaped), trailing newline — the byte format ccc writes
    /// with `sort_keys=True, ensure_ascii=False`. Unknown fields present in
    /// the original record are preserved (`raw` is the base object; typed
    /// fields overlay it), so rewrites never silently drop data (§4.1).
    pub fn to_line(&self) -> String {
        let mut object = self.raw.as_object().cloned().unwrap_or_default();
        object.insert("schema_version".into(), Value::from(self.schema_version));
        object.insert("id".into(), Value::from(self.id.as_str()));
        object.insert("kind".into(), Value::from(self.kind.as_str()));
        object.insert("text".into(), Value::from(self.text.as_str()));
        object.insert("review".into(), Value::from(self.review.as_str()));
        object.insert("privacy".into(), Value::from(self.audience.as_str()));
        object.insert("audience".into(), Value::from(self.audience.as_str()));
        object.insert("durability".into(), Value::from(self.durability.as_str()));
        object.insert("confidence".into(), Value::from(self.confidence));
        object.insert("source_rank".into(), Value::from(self.source_rank));
        if let Some(observed_at) = &self.observed_at {
            object.insert("observed_at".into(), Value::from(observed_at.as_str()));
        }
        if let Some(because) = &self.because {
            object.insert("because".into(), Value::from(because.as_str()));
        }
        if let Some(quote) = &self.quote {
            object.insert("quote".into(), Value::from(quote.as_str()));
        }
        if let Some(supersedes) = &self.supersedes {
            object.insert("supersedes".into(), Value::from(supersedes.as_str()));
        }
        if let Some(from) = &self.valid_from {
            object.insert("valid_from".into(), Value::from(from.as_str()));
        }
        if let Some(until) = &self.valid_until {
            object.insert("valid_until".into(), Value::from(until.as_str()));
        }
        object.insert(
            "entities".into(),
            Value::from(self.entities.iter().map(String::as_str).collect::<Vec<_>>()),
        );
        object.insert(
            "tags".into(),
            Value::from(self.tags.iter().map(String::as_str).collect::<Vec<_>>()),
        );
        // `source` is writer-specific; the raw value is preserved when set.
        match self.raw.get("source") {
            Some(source) if source.is_object() => {
                object.insert("source".into(), source.clone());
            }
            Some(source) => {
                object.insert("source".into(), source.clone());
            }
            _ => {
                let mut source = Map::new();
                source.insert("type".into(), Value::from("manual"));
                object.insert("source".into(), Value::Object(source));
            }
        }
        format!("{}\n", Value::Object(object))
    }
}

fn ensure_usable_version(version: i64) -> Result<(), RecordError> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(RecordError::UnsupportedSchemaVersion)
    }
}

fn parse_labels(value: Option<&Value>, field: &'static str) -> Result<Vec<String>, RecordError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(RecordError::InvalidField(field));
    };
    if items.len() > MAX_ENTITIES.max(MAX_TAGS) {
        return Err(RecordError::InvalidField(field));
    }
    let mut out = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(RecordError::InvalidField(field));
        };
        if text.is_empty() || text.len() > MAX_LABEL_BYTES {
            return Err(RecordError::InvalidField(field));
        }
        out.push(text.to_string());
    }
    Ok(out)
}

/// A parsed facts file: good lines (interpreted) and opaque lines preserved
/// in order.
#[derive(Clone, Debug, Default)]
pub struct FactsFile {
    pub lines: Vec<Line>,
}

impl FactsFile {
    pub fn records(&self) -> impl Iterator<Item = &FactRecord> {
        self.lines.iter().filter_map(|line| match line {
            Line::Good(record) => Some(record.as_ref()),
            Line::Opaque(_) => None,
        })
    }

    /// Normalized texts of all good records (dedup key set).
    pub fn normalized_texts(&self) -> std::collections::HashSet<String> {
        self.records()
            .map(|r| normalize(&r.text))
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// Read and interpret a facts file. `Ok(None)` when absent; oversize files
/// are an error (≤ 8 MiB read bound); unparseable JSON lines are preserved
/// as opaque lines, but bytes that are not valid UTF-8 fail closed — a
/// lossy rewrite would permanently replace them with replacement
/// characters (§4.1 canonical bytes).
pub fn read(payload: Option<Vec<u8>>) -> Result<FactsFile> {
    let Some(payload) = payload else {
        return Ok(FactsFile::default());
    };
    let text = std::str::from_utf8(&payload)
        .map_err(|error| anyhow::anyhow!("memory facts file is not valid UTF-8: {error}"))?;
    let mut lines = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match FactRecord::interpret(raw, index + 1) {
            Ok(record) => lines.push(Line::Good(Box::new(record))),
            Err(_) => lines.push(Line::Opaque(raw.to_string())),
        }
    }
    Ok(FactsFile { lines })
}

/// Load the facts file for a route through the owner-only read path.
pub fn load(route: &paths::Route) -> Result<FactsFile> {
    read(paths::read_bounded(
        &route.facts_file(),
        MAX_FACTS_FILE_BYTES,
        "memory facts",
    )?)
}

// ---------------------------------------------------------------------------
// Write gates (§4.2) — fixed order, the order changes outcomes.
// ---------------------------------------------------------------------------

/// One incoming fact on its way into the store.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub kind: String,
    pub text: String,
    pub because: Option<String>,
    pub quote: Option<String>,
    pub source_rank: i64,
    pub observed_at: String,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    /// Transcript for rank validation (distill path). Manual adds pass `None`
    /// and keep their rank: the CLI user is the source of the statement.
    pub transcript: Option<String>,
    /// Manual adds write `review: "manual"` and skip quote validation.
    pub manual: bool,
    /// Distill job id for id derivation; manual adds use the manual id.
    pub job_id: Option<String>,
    pub explicit_id: Option<String>,
}

/// Body-free gate outcome counters (§6.4 audit shape).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct GateReport {
    pub saved: usize,
    pub skipped_mutable: usize,
    pub skipped_missing_reason: usize,
    pub dedup_skipped: usize,
    pub superseded: Vec<String>,
    pub needs_human: usize,
}

/// The result of gating a batch: the full new file content plus the report.
#[derive(Clone, Debug)]
pub struct GateOutput {
    pub lines: Vec<String>,
    pub changed: bool,
    pub report: GateReport,
}

fn completion_vocab() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)완료|완결|종결|머지|MERGED|병합|해소|확정|성공|폐기|설치됨|닫힘|CLOSED|배포됨",
        )
        .expect("completion vocab")
    })
}

fn progress_vocab() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // ccc nunchi progress vocabulary, optional interior spaces included.
        Regex::new(r"진행\s?중|진행중|대기\s?중|대기|실행\s?중|예정|보류|승인\s?대기|미설치|미완료|착수|작업\s?중")
            .expect("progress vocab")
    })
}

/// Mutable-ops filter (nunchi `_MUTABLE_OPS`, §4.2 gate 1): operational
/// state (commit SHAs, systemd counters, unit states, "정상 동작" claims)
/// is measured live, never memorized. The commit-SHA branch emulates the
/// original lookahead ("must contain at least one a-f hex letter") with a
/// post-filter because the Rust regex engine has no lookaround.
pub fn is_mutable_ops_text(text: &str) -> bool {
    static SHA: OnceLock<Regex> = OnceLock::new();
    static REST: OnceLock<Regex> = OnceLock::new();
    let sha = SHA.get_or_init(|| Regex::new(r"\b[0-9a-f]{7,40}\b").expect("sha pattern"));
    if sha.is_match(text) {
        for matched in sha.find_iter(text) {
            let fragment = &text[matched.start()..matched.end()];
            if fragment.bytes().any(|b| (b'a'..=b'f').contains(&b)) {
                return true;
            }
        }
    }
    let rest = REST.get_or_init(|| {
        Regex::new(
            r"(?i)\bNRestarts=\d+|\bMain\s?PID=\d+|\bactive\s*\((?:running|exited|failed|waiting|deactivating)\)|\b(?:active|enabled|disabled|inactive)\s*/\s*(?:active|enabled|disabled|inactive)\b|정상\s*(?:상태|화|으로\s*(?:기록|동작|복구|작동))",
        )
        .expect("mutable-ops pattern")
    });
    rest.is_match(text)
}

/// Whitespace tokens of length > 1 (ccc overlap tokenization, §4.2 note).
fn overlap_tokens(text: &str) -> std::collections::HashSet<String> {
    text.split_whitespace()
        .filter(|t| t.chars().count() > 1)
        .map(str::to_string)
        .collect()
}

fn overlap_ratio(new: &str, old: &str) -> f64 {
    let new_tokens = overlap_tokens(new);
    let old_tokens = overlap_tokens(old);
    let smaller = new_tokens.len().min(old_tokens.len());
    if smaller == 0 {
        return 0.0;
    }
    let shared = new_tokens.intersection(&old_tokens).count();
    shared as f64 / smaller as f64
}

/// Rank validation (§4.1): a rank-2/3 extraction must cite a quote of at
/// least [`QUOTE_MIN_CHARS`] characters found verbatim in the transcript;
/// otherwise the fact is demoted to rank 1 and marked needs-human. Demotion
/// is about closing authority, not confidence: lower-rank facts must never
/// supersede higher-rank ones (enforced by the supersede gate).
fn validate_rank(candidate: &mut Candidate) -> bool {
    if candidate.manual || candidate.source_rank <= 1 {
        return true;
    }
    let Some(transcript) = &candidate.transcript else {
        candidate.source_rank = 1;
        return false;
    };
    let Some(quote) = candidate.quote.as_deref() else {
        candidate.source_rank = 1;
        return false;
    };
    let quoted = quote.chars().count() >= QUOTE_MIN_CHARS && transcript.contains(quote);
    if quoted {
        true
    } else {
        candidate.source_rank = 1;
        false
    }
}

/// Build the canonical record line for a gated candidate.
fn render_candidate(
    candidate: &Candidate,
    audience: &str,
    review: &str,
    supersedes: Option<&str>,
) -> FactRecord {
    let normalized = normalize(&candidate.text);
    let id = candidate
        .explicit_id
        .clone()
        .unwrap_or_else(|| match &candidate.job_id {
            Some(job_id) => derive_distill_id(job_id, &normalized),
            None => derive_manual_id(&normalized),
        });
    let mut record = FactRecord {
        line_no: 0,
        schema_version: SCHEMA_VERSION,
        id,
        known_kind: true,
        kind: candidate.kind.clone(),
        text: candidate.text.clone(),
        because: candidate.because.clone(),
        quote: candidate.quote.clone(),
        supersedes: supersedes.map(str::to_string),
        audience: audience.to_string(),
        durability: FactRecord::default_durability(&candidate.kind).to_string(),
        confidence: 0.7,
        source_rank: candidate.source_rank,
        observed_at: Some(candidate.observed_at.clone()),
        entities: candidate.entities.clone(),
        tags: candidate.tags.clone(),
        review: review.to_string(),
        valid_from: candidate.valid_from.clone(),
        valid_until: candidate.valid_until.clone(),
        raw: Value::Object(Map::new()),
    };
    if candidate.manual {
        let mut source = Map::new();
        source.insert("type".into(), Value::from("manual"));
        record.raw = Value::Object({
            let mut object = Map::new();
            object.insert("source".into(), Value::Object(source));
            object
        });
    }
    record
}

/// Close one record in place: `valid_until = now`, `review = superseded`.
/// A field-level edit on the raw JSON, so unknown fields survive.
fn supersede_record(record: &mut FactRecord, now: &str) {
    if let Some(object) = record.raw.as_object_mut() {
        object.insert("valid_until".into(), Value::from(now));
        object.insert("review".into(), Value::from("superseded"));
    }
    record.valid_until = Some(now.to_string());
    record.review = "superseded".to_string();
}

/// Apply the write gates to a batch of candidates against the existing file
/// and render the full new file content. Nothing is written here — the
/// caller owns locking and the atomic write.
pub fn gate_and_render(
    existing: &FactsFile,
    candidates: Vec<Candidate>,
    audience: &str,
    now: DateTime<Utc>,
    max_facts: usize,
) -> Result<GateOutput> {
    let now_label = format_timestamp(now);
    let mut report = GateReport::default();
    let mut lines: Vec<Line> = existing.lines.clone();
    let mut seen: std::collections::HashSet<String> = existing.normalized_texts();
    let mut added: Vec<String> = Vec::new();

    for mut candidate in candidates {
        // normalize
        let normalized = normalize(&candidate.text);
        if normalized.is_empty() {
            report.dedup_skipped += 1;
            continue;
        }
        // mutable-ops filter
        if MUTABLE_OPS_KINDS.contains(&candidate.kind.as_str())
            && is_mutable_ops_text(&candidate.text)
        {
            report.skipped_mutable += 1;
            continue;
        }
        // rank validation
        let demoted = !validate_rank(&mut candidate);
        if demoted {
            report.needs_human += 1;
        }
        // decision reason
        if REASON_KINDS.contains(&candidate.kind.as_str())
            && candidate
                .because
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            report.skipped_missing_reason += 1;
            continue;
        }
        // auto-supersede: the candidate closes the single highest-overlap
        // open progress fact in the same subject when it announces completion
        // and outranks or equals it.
        let mut superseded_id: Option<String> = None;
        if completion_vocab().is_match(&candidate.text) {
            let subject = candidate.entities.first().map(String::as_str);
            let mut best: Option<(f64, usize)> = None;
            for (index, line) in lines.iter().enumerate() {
                let Line::Good(record) = line else { continue };
                if record.supersedes.is_some()
                    || record.review == "superseded"
                    || record.review == "rejected"
                {
                    continue;
                }
                if let Some(until) = &record.valid_until
                    && parse_timestamp(until).map(|t| t <= now).unwrap_or(false)
                {
                    continue;
                }
                if record.subject() != subject || record.source_rank > candidate.source_rank {
                    continue;
                }
                if !progress_vocab().is_match(&record.text) {
                    continue;
                }
                let ratio = overlap_ratio(&candidate.text, &record.text);
                if ratio < 0.4 {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((best_ratio, best_index)) => {
                        if ratio != best_ratio {
                            ratio > best_ratio
                        } else {
                            let best_observed = match &lines[best_index] {
                                Line::Good(other) => other.observed_at.clone().unwrap_or_default(),
                                Line::Opaque(_) => String::new(),
                            };
                            record.observed_at.clone().unwrap_or_default() > best_observed
                        }
                    }
                };
                if better {
                    best = Some((ratio, index));
                }
            }
            if let Some((_, index)) = best
                && let Line::Good(record) = &mut lines[index]
            {
                superseded_id = Some(record.id.clone());
                supersede_record(record, &now_label);
                report.superseded.push(record.id.clone());
            }
        }
        // conflict review: same kind, open, overlap ≥ 0.6 → both stay open,
        // the newcomer is marked needs-human (never auto-resolved).
        let mut review = if candidate.manual {
            "manual"
        } else if demoted {
            // §4.1: a demoted extraction is stored but flagged needs-human —
            // it must never close another fact (the supersede gate already
            // refuses it via the rank ordering).
            "needs-human"
        } else {
            "auto-local"
        };
        if superseded_id.is_none()
            && candidate.kind != "constraint"
            && candidate.kind != "observation"
        {
            for line in &lines {
                let Line::Good(record) = line else { continue };
                if record.kind != candidate.kind
                    || record.review == "superseded"
                    || record.review == "rejected"
                {
                    continue;
                }
                if let Some(until) = &record.valid_until
                    && parse_timestamp(until).map(|t| t <= now).unwrap_or(false)
                {
                    continue;
                }
                if overlap_ratio(&candidate.text, &record.text) >= 0.6 {
                    review = "needs-human";
                    report.needs_human += 1;
                    break;
                }
            }
        }
        // dedup (batch-inclusive)
        if seen.contains(&normalized) {
            report.dedup_skipped += 1;
            continue;
        }
        seen.insert(normalized);
        let record = render_candidate(&candidate, audience, review, superseded_id.as_deref());
        added.push(record.to_line());
        report.saved += 1;
    }

    let changed = !added.is_empty()
        || existing
            .lines
            .iter()
            .zip(lines.iter())
            .any(|(before, after)| match (before, after) {
                (Line::Good(b), Line::Good(a)) => {
                    b.review != a.review || b.valid_until != a.valid_until
                }
                _ => false,
            });
    if !changed {
        return Ok(GateOutput {
            lines: Vec::new(),
            changed: false,
            report,
        });
    }

    // Rolling tail over the full file (existing lines + additions).
    let mut rendered: Vec<String> = lines
        .iter()
        .map(|line| match line {
            Line::Good(record) => record.to_line(),
            Line::Opaque(raw) => format!("{raw}\n"),
        })
        .collect();
    rendered.extend(added);
    let tail_start = rendered.len().saturating_sub(max_facts);
    rendered.drain(..tail_start);
    Ok(GateOutput {
        lines: rendered,
        changed: true,
        report,
    })
}

/// Close one fact by id (`valid_until = now`), preserving every other field.
/// Idempotent: an already-closed fact returns `AlreadyClosed`. Goes through
/// the rollback transaction (#65 §1.5): the single memory-rollback lock
/// serializes manual closes with distill commits, and the write leaves an
/// undoable head with a body-free `MemoryCommit` ledger event.
pub fn close(
    route: &paths::Route,
    fact_id: &str,
    now: DateTime<Utc>,
    lock_timeout_ms: u64,
) -> Result<CloseOutcome> {
    ensure!(paths::valid_scope(route.scope()), "invalid scope");
    ensure!(
        fact_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !fact_id.is_empty()
            && fact_id.len() <= MAX_LABEL_BYTES,
        "fact id must be [a-z0-9-]"
    );
    let transaction = super::transaction::Transaction::new(&route.state_dir());
    let meta = super::transaction::CommitMeta {
        provider: "danso".into(),
        actor: "manual".into(),
        tool: "local-memory-sink".into(),
        diff: "mode-facts".into(),
        session: "manual".into(),
    };
    let mut outcome = CloseOutcome::NotFound;
    let result = transaction.commit(lock_timeout_ms, &meta, |before| {
        let mut next = before.clone();
        for (name, current) in next.iter_mut() {
            if name == super::FACTS_FILE_NAME {
                let file = read(current.clone())?;
                let now_label = format_timestamp(now);
                let mut changed = false;
                outcome = CloseOutcome::NotFound;
                let mut rendered: Vec<String> = Vec::with_capacity(file.lines.len());
                for line in &file.lines {
                    match line {
                        Line::Good(record) if record.id == fact_id => {
                            let already = record
                                .valid_until
                                .as_deref()
                                .and_then(|raw| parse_timestamp(raw).ok())
                                .map(|until| until <= now)
                                .unwrap_or(false);
                            if already {
                                outcome = CloseOutcome::AlreadyClosed;
                                rendered.push(record.to_line());
                            } else {
                                let mut closed = record.clone();
                                if let Some(object) = closed.raw.as_object_mut() {
                                    object.insert(
                                        "valid_until".into(),
                                        Value::from(now_label.as_str()),
                                    );
                                }
                                closed.valid_until = Some(now_label.clone());
                                rendered.push(closed.to_line());
                                changed = true;
                                outcome = CloseOutcome::Closed;
                            }
                        }
                        Line::Good(record) => rendered.push(record.to_line()),
                        Line::Opaque(raw) => rendered.push(format!("{raw}\n")),
                    }
                }
                if changed {
                    *current = Some(rendered.concat().into_bytes());
                }
            }
        }
        Ok(next)
    })?;
    if let Some(action_id) = result.action_id {
        super::audit::record(
            route,
            &super::audit::Event::Commit {
                action_id,
                changed: vec![super::FACTS_FILE_NAME.to_string()],
                facts_added: 0,
            },
        );
    }
    Ok(outcome)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    Closed,
    AlreadyClosed,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(text: &str) -> Candidate {
        Candidate {
            kind: "preference".into(),
            text: text.into(),
            because: None,
            quote: None,
            source_rank: 1,
            observed_at: "2026-09-08T00:00:00Z".into(),
            entities: vec!["session".into()],
            tags: vec!["test".into()],
            valid_from: None,
            valid_until: None,
            transcript: None,
            manual: false,
            job_id: None,
            explicit_id: None,
        }
    }

    fn record_from(line: &str) -> FactRecord {
        FactRecord::interpret(line, 1).unwrap()
    }

    #[test]
    fn interpret_fills_the_ccc_hook_defaults() {
        let hook = r#"{"id":"distill-abc123","kind":"decision","text":"스탠드업은 09:30 KST에 한다.","because":"팀 절반이 08시 이전에 접속하지 못함","review":"auto-local","privacy":"private","confidence":0.7,"observed_at":"2026-09-08T04:25:00Z","entities":["session"],"tags":["distilled"],"source":{"type":"distill","session":"s1","trigger":"final_answer"}}"#;
        let record = FactRecord::interpret(hook, 1).unwrap();
        assert_eq!(record.schema_version, 1);
        assert_eq!(record.durability, "durable");
        assert_eq!(record.source_rank, 1);
        assert_eq!(record.audience, "private");
        assert!(record.known_kind);
    }

    #[test]
    fn interpret_allows_unknown_kinds_readonly_and_rejects_corruption() {
        let unknown = r#"{"id":"x1","kind":"fact","text":"구형 레코드","privacy":"private"}"#;
        let record = FactRecord::interpret(unknown, 1).unwrap();
        assert!(!record.known_kind);
        assert_eq!(
            FactRecord::interpret("{broken", 1).unwrap_err(),
            RecordError::MalformedJson
        );
        assert_eq!(
            FactRecord::interpret(
                r#"{"id":"x","schema_version":2,"kind":"preference","text":"t"}"#,
                1
            )
            .unwrap_err(),
            RecordError::UnsupportedSchemaVersion
        );
        assert_eq!(
            FactRecord::interpret(
                r#"{"id":"x","kind":"preference","text":"t","privacy":"bogus"}"#,
                1
            )
            .unwrap_err(),
            RecordError::InvalidField("audience")
        );
        assert_eq!(
            FactRecord::interpret(
                r#"{"id":"x","kind":"decision","text":"t","valid_from":"2026-01-02T00:00:00Z","valid_until":"2026-01-01T00:00:00Z"}"#,
                1,
            )
            .unwrap_err(),
            RecordError::InvalidWindow
        );
    }

    #[test]
    fn canonical_line_is_sorted_compact_utf8() {
        let mut record = render_candidate(&candidate("유니온 먼저"), "private", "manual", None);
        record.raw = Value::Object(Map::new());
        let line = record.to_line();
        assert!(line.starts_with("{\"audience\":\"private\",\"confidence\":0.7,"));
        assert!(line.contains("\"text\":\"유니온 먼저\""));
        assert!(line.ends_with("}\n"));
        assert!(!line.contains("\\u"));
    }

    #[test]
    fn distill_ids_converge_on_the_job_and_text() {
        let normalized = normalize("스탠드업은 09:30 KST에 한다.");
        let first = derive_distill_id("a".repeat(64).as_str(), &normalized);
        let second = derive_distill_id("a".repeat(64).as_str(), &normalized);
        assert_eq!(first, second);
        assert!(first.starts_with("distill-") && first.len() == "distill-".len() + 12);
        let other = derive_distill_id("b".repeat(64).as_str(), &normalized);
        assert_ne!(first, other);
    }

    #[test]
    fn mutable_ops_filter_matches_operational_state() {
        assert!(is_mutable_ops_text("워커 active (running) 상태 확인"));
        assert!(is_mutable_ops_text("NRestarts=3 이후 재시작"));
        assert!(is_mutable_ops_text("Main PID=2411 로 실행 중"));
        assert!(is_mutable_ops_text("커밋 48bf5ef 적용됨"));
        assert!(is_mutable_ops_text("서비스가 정상 상태로 기록됨"));
        assert!(!is_mutable_ops_text("스탠드업은 09:30 KST에 한다"));
        assert!(!is_mutable_ops_text("1234567 순수 숫자는 SHA가 아니다"));
    }

    #[test]
    fn gates_skip_mutable_and_reasonless_then_save() {
        let existing = FactsFile::default();
        let batch = vec![
            Candidate {
                kind: "observation".into(),
                ..candidate("배포 48bf5eff 완료 후 active (running)")
            },
            Candidate {
                kind: "decision".into(),
                ..candidate("이유 없는 결정")
            },
            candidate("스탠드업은 09:30 KST에 한다"),
        ];
        let out = gate_and_render(&existing, batch, "private", Utc::now(), 100).unwrap();
        assert_eq!(out.report.skipped_mutable, 1);
        assert_eq!(out.report.skipped_missing_reason, 1);
        assert_eq!(out.report.saved, 1);
        assert_eq!(out.lines.len(), 1);
    }

    #[test]
    fn unquoted_high_ranks_demote_to_needs_human() {
        let existing = FactsFile::default();
        let batch = vec![Candidate {
            source_rank: 3,
            quote: Some("없는 인용".into()),
            transcript: Some("트랜스크립트 본문".into()),
            ..candidate("사용자가 스탠드업 시간을 정했다")
        }];
        let out = gate_and_render(&existing, batch, "private", Utc::now(), 100).unwrap();
        assert_eq!(out.report.saved, 1);
        assert_eq!(out.report.needs_human, 1);
        let record = record_from(out.lines[0].trim_end());
        assert_eq!(record.source_rank, 1);
        assert_eq!(record.review, "needs-human");
    }

    #[test]
    fn quoted_rank_three_is_kept_when_found_in_transcript() {
        let existing = FactsFile::default();
        let quote = "오늘부터 에디터는 Helix를 쓴다";
        let batch = vec![Candidate {
            source_rank: 3,
            quote: Some(quote.into()),
            transcript: Some(format!("대화 중 그가 말했다: {quote} — 이후 진행")),
            ..candidate("에디터를 Helix로 바꿨다")
        }];
        let out = gate_and_render(&existing, batch, "private", Utc::now(), 100).unwrap();
        assert_eq!(out.report.needs_human, 0);
        let record = record_from(out.lines[0].trim_end());
        assert_eq!(record.source_rank, 3);
    }

    #[test]
    fn completion_supersedes_one_progress_fact_and_marks_the_winner() {
        let existing_line = r#"{"id":"distill-old1","kind":"task-progress","text":"유니온 머지 진행 중","review":"auto-local","privacy":"private","durability":"volatile","confidence":0.7,"source_rank":1,"observed_at":"2026-09-01T00:00:00Z","entities":["session"],"tags":["distilled"],"source":{"type":"distill","session":"s","trigger":"final_answer"}}"#;
        let existing = FactsFile {
            lines: vec![Line::Good(Box::new(record_from(existing_line)))],
        };
        let batch = vec![Candidate {
            kind: "task-progress".into(),
            text: "유니온 머지 완료, --build 재생성까지 끝".into(),
            ..candidate("placeholder")
        }];
        let out = gate_and_render(
            &existing,
            batch,
            "private",
            parse_ts("2026-09-08T00:00:00Z"),
            100,
        )
        .unwrap();
        assert_eq!(out.report.saved, 1);
        assert_eq!(out.report.superseded, vec!["distill-old1"]);
        let closed = record_from(out.lines[0].trim_end());
        assert_eq!(closed.review, "superseded");
        assert_eq!(closed.valid_until.as_deref(), Some("2026-09-08T00:00:00Z"));
        let new_record = record_from(out.lines[1].trim_end());
        assert_eq!(new_record.supersedes.as_deref(), Some("distill-old1"));
    }

    #[test]
    fn conflicting_same_kind_facts_stay_open_as_needs_human() {
        let existing_line = r#"{"id":"distill-old2","kind":"preference","text":"에디터는 Vim을 쓴다","review":"auto-local","privacy":"private","durability":"durable","confidence":0.7,"source_rank":1,"observed_at":"2026-08-01T00:00:00Z","entities":["user"],"tags":[],"source":{"type":"manual"}}"#;
        let existing = FactsFile {
            lines: vec![Line::Good(Box::new(record_from(existing_line)))],
        };
        let batch = vec![candidate("에디터는 Vim 대신 Helix를 쓴다")];
        let out = gate_and_render(&existing, batch, "private", Utc::now(), 100).unwrap();
        assert_eq!(out.report.saved, 1);
        assert_eq!(out.report.needs_human, 1);
        let new_record = record_from(out.lines[1].trim_end());
        assert_eq!(new_record.review, "needs-human");
        let old_record = record_from(out.lines[0].trim_end());
        assert_eq!(old_record.review, "auto-local");
    }

    #[test]
    fn duplicates_are_skipped_batch_inclusively() {
        let existing = FactsFile::default();
        let batch = vec![candidate("같은 사실 반복"), candidate("같은 사실 반복!")];
        let out = gate_and_render(&existing, batch, "private", Utc::now(), 100).unwrap();
        assert_eq!(out.report.saved, 1);
        assert_eq!(out.report.dedup_skipped, 1);
    }

    #[test]
    fn rolling_tail_keeps_the_last_lines_and_opaque_lines_survive() {
        let opaque = FactsFile {
            lines: vec![Line::Opaque("{broken line".to_string())],
        };
        let batch: Vec<Candidate> = (0..3).map(|i| candidate(&format!("사실 {i}"))).collect();
        let out = gate_and_render(&opaque, batch, "private", Utc::now(), 2).unwrap();
        assert_eq!(out.lines.len(), 2);
        assert!(out.lines[0].contains("\"text\":\"사실 1\""));
    }

    #[test]
    fn unchanged_batches_never_touch_the_file() {
        let existing = FactsFile::default();
        let batch = vec![candidate("같은 사실 반복")];
        let first = gate_and_render(&existing, batch.clone(), "private", Utc::now(), 100).unwrap();
        assert!(first.changed);
        let after = FactsFile {
            lines: vec![Line::Good(Box::new(record_from(first.lines[0].trim_end())))],
        };
        let second = gate_and_render(&after, batch, "private", Utc::now(), 100).unwrap();
        assert!(!second.changed);
        assert!(second.lines.is_empty());
    }

    fn parse_ts(value: &str) -> DateTime<Utc> {
        parse_timestamp(value).unwrap()
    }
}
