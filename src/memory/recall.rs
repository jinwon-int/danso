//! In-process recall index (issue #52 §4.3): the derived index is rebuilt on
//! every call — the sources are one ≤1000-line JSONL file plus a few small
//! markdown files, so there is no on-disk index, no SQLite dependency, and
//! no index/source drift. Scoring, valid-time semantics and the NL as-of
//! timeparse port the audited ccc-node rules (`ccc_memory_search.py`,
//! `ccc_memory_timeparse.py`) without the SQLite-only pieces (no BM25
//! tiebreak, no usage boost); the lexical and fuzzy lanes fuse with RRF
//! (k=60). BM25/usage stay out by design decision §4.3.

use anyhow::Result;
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, FixedOffset, Local, TimeZone, Timelike, Utc,
};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::OnceLock;

use super::facts::{self, FactRecord};
use super::paths::{self, Route};
use super::scan;

/// Default result limit (ccc search default).
pub const SEARCH_LIMIT_DEFAULT: usize = 5;
/// Recency window for the linear decay (ccc rule).
const RECENCY_WINDOW_DAYS: f64 = 7.0;
/// Fuzzy lane inclusion threshold (containment coefficient, ccc rule).
const FUZZY_MIN_SIM: f64 = 0.34;
/// RRF constant (ccc rule).
const RRF_K: f64 = 60.0;
/// Snippet bound (ccc display rule).
const SNIPPET_CHARS: usize = 240;
/// Per-file read bound for markdown/state documents.
const DOC_FILE_MAX_BYTES: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// Retention (§4.1 index TTLs; ccc memory-retention-policy.json)
// ---------------------------------------------------------------------------

fn retention_ttl_days(durability: &str) -> f64 {
    match durability {
        "volatile" => 14.0,
        "session-only" => 2.0,
        "week-scale" => 45.0,
        // durable and anything unknown: never age-expire (conservative).
        _ => 0.0,
    }
}

/// Guard kinds never age-expire even when mislabeled volatile (ccc rule:
/// a mislabeled volatile constraint must not silently vanish).
fn age_expiry_forbidden(kind: &str) -> bool {
    matches!(kind, "decision" | "procedure" | "constraint")
}

pub(crate) fn retention_keeps(record: &FactRecord, now: DateTime<Utc>) -> bool {
    if !record.known_kind {
        return true; // unknown kinds are kept conservatively
    }
    let ttl = retention_ttl_days(&record.durability);
    if ttl <= 0.0 {
        return true;
    }
    if age_expiry_forbidden(&record.kind) {
        return true;
    }
    let Some(observed_at) = &record.observed_at else {
        return true; // undated: keep conservatively
    };
    match facts::parse_timestamp(observed_at) {
        Ok(observed) => {
            let age_days = (now - observed).num_seconds() as f64 / 86400.0;
            age_days <= ttl
        }
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// One searchable document with the metadata the score formula needs.
#[derive(Clone, Debug)]
pub struct Doc {
    pub path: String,
    /// `memory` (3.0) | `structured` (2.5) | `state` (0.5) — ccc boosts.
    pub source: &'static str,
    /// Indexed text (already scanned).
    pub content: String,
    /// User-facing snippet (structured docs render the fact text itself).
    pub snippet: String,
    /// Recency reference: record observed_at for structured docs, file
    /// mtime for memory/state docs (§4.3: never path-derived noise).
    pub observed: Option<DateTime<Utc>>,
    pub durability: Option<String>,
    pub kind: Option<String>,
    pub review: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
}

impl Doc {
    fn hay(&self) -> String {
        format!("{}\n{}\n{}", self.path, self.source, self.content).to_lowercase()
    }

    fn source_boost(&self) -> f64 {
        match self.source {
            "memory" => 3.0,
            "structured" => 2.5,
            "state" => 0.5,
            _ => 1.0,
        }
    }

    fn durability_penalty(&self) -> f64 {
        let durability = self.durability.as_deref().unwrap_or("");
        if durability == "volatile" || self.kind.as_deref() == Some("task-progress") {
            return -3.0;
        }
        if self.review.as_deref() == Some("rejected") {
            return -10.0;
        }
        0.0
    }

    /// Valid-time classification (§4.3 / ccc `_temporal_tag`): valid_from is
    /// inclusive, valid_until exclusive; undated and malformed-window facts
    /// are kept conservatively with a body-free signal.
    fn temporal_status(&self, now: DateTime<Utc>) -> (TemporalStatus, Option<&'static str>) {
        let from_raw = self.valid_from.as_deref().filter(|v| !v.is_empty());
        let until_raw = self.valid_until.as_deref().filter(|v| !v.is_empty());
        if from_raw.is_none() && until_raw.is_none() {
            return (TemporalStatus::Undated, None);
        }
        let parse = |raw: &str| facts::parse_timestamp(raw).ok();
        let from = from_raw.and_then(parse);
        let until = until_raw.and_then(parse);
        if (from_raw.is_some() && from.is_none()) || (until_raw.is_some() && until.is_none()) {
            return (TemporalStatus::Undated, Some("malformed-window-kept"));
        }
        if let Some(from) = from
            && from > now
        {
            return (TemporalStatus::Future, None);
        }
        if let Some(until) = until
            && until <= now
        {
            return (TemporalStatus::Expired, None);
        }
        (TemporalStatus::Current, None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemporalStatus {
    Undated,
    Current,
    Future,
    Expired,
}

impl TemporalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Undated => "undated",
            Self::Current => "current",
            Self::Future => "future",
            Self::Expired => "expired",
        }
    }
}

/// Read one owner-only markdown/state document into a memory/state doc.
fn document(route_dir: &std::path::Path, file: &str, source: &'static str) -> Option<Doc> {
    let path = route_dir.join(file);
    let payload = paths::read_bounded(&path, DOC_FILE_MAX_BYTES, "memory document").ok()??;
    let raw = String::from_utf8_lossy(&payload).into_owned();
    if raw.trim().is_empty() {
        return None;
    }
    let outcome = scan::scan(file, &raw, None);
    let observed = std::fs::symlink_metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(DateTime::<Utc>::from);
    Some(Doc {
        path: path.display().to_string(),
        source,
        content: outcome.text.clone(),
        snippet: scan::truncate_utf8(&outcome.text, SNIPPET_CHARS).to_string(),
        observed,
        durability: None,
        kind: None,
        review: None,
        valid_from: None,
        valid_until: None,
    })
}

/// Structured docs from the facts file: one record = one document, path
/// `<facts>#L<n>:<id>` (ccc convention). Rejected and superseded records are
/// excluded (kept in the file, never in the index); volatile TTLs apply.
fn structured_docs(route: &Route, now: DateTime<Utc>) -> Result<Vec<Doc>> {
    let facts_file = route.facts_file();
    let file = facts::load(route)?;
    let mut docs = Vec::new();
    for record in file.records() {
        if record.review == "rejected" || record.review == "superseded" {
            continue;
        }
        if !retention_keeps(record, now) {
            continue;
        }
        let scanned = scan::scan("memory-facts", &record.text, None);
        let mut meta_lines = vec![format!("kind: {}", record.kind)];
        if let Some(because) = &record.because {
            meta_lines.push(format!(
                "because: {}",
                scan::scan("because", because, None).text
            ));
        }
        meta_lines.push(format!("entities: {}", record.entities.join(", ")));
        meta_lines.push(format!("tags: {}", record.tags.join(", ")));
        let mut content = scanned.text.clone();
        for line in meta_lines {
            content.push('\n');
            content.push_str(&line);
        }
        docs.push(Doc {
            path: format!("{}#L{}:{}", facts_file.display(), record.line_no, record.id),
            source: "structured",
            snippet: scan::truncate_utf8(&scanned.text, SNIPPET_CHARS).to_string(),
            content,
            observed: record
                .observed_at
                .as_deref()
                .and_then(|raw| facts::parse_timestamp(raw).ok()),
            durability: Some(record.durability.clone()),
            kind: Some(record.kind.clone()),
            review: Some(record.review.clone()),
            valid_from: record.valid_from.clone(),
            valid_until: record.valid_until.clone(),
        });
    }
    Ok(docs)
}

/// Build the index for a route. A private route additionally reads the
/// shared tree (§7 read rule); `shared` and `global` never open another
/// tree. Every source is independently fail-open (§5.1).
pub fn build_index(route: &Route, now: DateTime<Utc>) -> Result<Vec<Doc>> {
    let mut docs = Vec::new();
    let mut roots = vec![route.scope_dir()];
    if let Some(shared) = route.shared_route() {
        roots.push(shared.scope_dir());
    }
    for (index, scope_dir) in roots.iter().enumerate() {
        let memories = scope_dir.join("memories");
        let state = scope_dir.join("state");
        for file in ["MEMORY.md", "USER.md"] {
            if let Some(doc) = document(&memories, file, "memory") {
                docs.push(doc);
            }
        }
        for file in [paths::RESUME_FILE, paths::WORKING_STATE_FILE] {
            if let Some(doc) = document(&state, file, "state") {
                docs.push(doc);
            }
        }
        // Structured docs stay scoped to the route's own tree; the shared
        // tree contributes its own facts file when present. Source failures
        // are fail-open (§5.1): the block is skipped, the rest still injects.
        let facts_route = if index == 0 {
            route.clone()
        } else {
            match shared_route_for(scope_dir) {
                Ok(shared) => shared,
                Err(_) => continue,
            }
        };
        if let Ok(structured) = structured_docs(&facts_route, now) {
            docs.extend(structured);
        }
    }
    Ok(docs)
}

fn shared_route_for(scope_dir: &std::path::Path) -> Result<Route> {
    let root = scope_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("scope directory has no parent"))?;
    Route::new(root, "shared")
}

// ---------------------------------------------------------------------------
// Query tokenization and lane scoring (ccc rules)
// ---------------------------------------------------------------------------

fn stopword_tokens() -> &'static HashSet<&'static str> {
    static STOP: OnceLock<HashSet<&'static str>> = OnceLock::new();
    STOP.get_or_init(|| {
        [
            "task", "prompt", "node", "cwd", "issue", "pr", "git", "branch", "changed", "paths",
            "extra", "http", "https", "tmp", "root", "work",
        ]
        .into_iter()
        .collect()
    })
}

fn token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[0-9A-Za-z_가-힣]+").expect("token pattern"))
}

/// Query tokens: `[0-9A-Za-z_가-힣]+`, length ≥ 2, lowercased, stopwords
/// removed, deduplicated, capped at 12 (ccc `tokens_for`).
pub fn tokens_for(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for matched in token_regex().find_iter(query) {
        let raw = matched.as_str();
        if raw.chars().count() < 2 {
            continue;
        }
        let token = raw.to_lowercase();
        if stopword_tokens().contains(token.as_str()) || tokens.contains(&token) {
            continue;
        }
        tokens.push(token);
        if tokens.len() == 12 {
            break;
        }
    }
    tokens
}

fn recency_boost(observed: Option<DateTime<Utc>>, now: DateTime<Utc>) -> f64 {
    let Some(observed) = observed else {
        return 0.0;
    };
    let age_seconds = (now - observed).num_seconds().max(0) as f64;
    let window = RECENCY_WINDOW_DAYS * 86400.0;
    (1.0 - age_seconds.min(window) / window).max(0.0)
}

/// Character 3-grams over the normalized stream (alnum + Hangul + CJK) —
/// the stdlib-style fuzzy-recall signal from ccc `char_ngrams`.
fn char_ngrams(text: &str) -> HashSet<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[0-9a-z가-힣぀-ヿ一-鿿]").expect("ngram pattern"));
    let normalized: String = re
        .find_iter(&text.to_lowercase())
        .map(|m| m.as_str())
        .collect();
    let chars: Vec<char> = normalized.chars().collect();
    if chars.len() < 3 {
        if chars.is_empty() {
            return HashSet::new();
        }
        return chars
            .iter()
            .collect::<Vec<_>>()
            .iter()
            .map(|c| c.to_string())
            .collect();
    }
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

fn fuzzy_similarity(query_grams: &HashSet<String>, doc: &Doc) -> Option<f64> {
    if query_grams.is_empty() {
        return None;
    }
    let doc_grams = char_ngrams(&format!("{} {}", doc.path, doc.content));
    if doc_grams.is_empty() {
        return None;
    }
    let shared = query_grams.intersection(&doc_grams).count();
    let sim = shared as f64 / query_grams.len() as f64;
    (sim >= FUZZY_MIN_SIM).then_some(sim)
}

#[derive(Clone, Debug)]
struct LaneItem {
    doc_index: usize,
    score: f64,
    signals: Value,
    snippet: String,
}

fn lexical_lane(docs: &[Doc], tokens: &[String], query: &str, now: DateTime<Utc>) -> Vec<LaneItem> {
    let lowered = query.to_lowercase();
    let mut items = Vec::new();
    for (index, doc) in docs.iter().enumerate() {
        let hay = doc.hay();
        let token_hits = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
        let phrase_hit = usize::from(!lowered.is_empty() && hay.contains(&lowered));
        if token_hits == 0 && phrase_hit == 0 {
            continue;
        }
        let source_boost = doc.source_boost();
        let recency = recency_boost(doc.observed, now);
        let penalty = doc.durability_penalty();
        let score =
            token_hits as f64 * 4.0 + phrase_hit as f64 * 3.0 + source_boost + recency + penalty;
        items.push(LaneItem {
            doc_index: index,
            score: round4(score),
            signals: json!({
                "token_hits": token_hits,
                "phrase_hit": phrase_hit,
                "source_boost": source_boost,
                "recency_boost": round4(recency),
                "durability_penalty": penalty,
            }),
            snippet: doc.snippet.clone(),
        });
    }
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    items
}

fn fuzzy_lane(docs: &[Doc], query: &str, now: DateTime<Utc>) -> Vec<LaneItem> {
    let grams = char_ngrams(query);
    let mut items = Vec::new();
    for (index, doc) in docs.iter().enumerate() {
        let Some(sim) = fuzzy_similarity(&grams, doc) else {
            continue;
        };
        let source_boost = doc.source_boost();
        let recency = recency_boost(doc.observed, now);
        let penalty = doc.durability_penalty();
        let score = sim * 8.0 + source_boost + recency + penalty;
        items.push(LaneItem {
            doc_index: index,
            score: round4(score),
            signals: json!({
                "fuzzy_sim": round4(sim),
                "source_boost": source_boost,
                "recency_boost": round4(recency),
                "durability_penalty": penalty,
            }),
            snippet: doc.snippet.clone(),
        });
    }
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    items
}

/// Reciprocal Rank Fusion (ccc `rrf_fuse`, k=60): each lane contributes
/// 1/(k+rank+1); the richer per-doc record is kept.
fn rrf_fuse(lanes: &[Vec<LaneItem>], limit: usize) -> Vec<(usize, f64, Value, String)> {
    use std::collections::HashMap;
    let mut aggregate: HashMap<usize, (f64, usize, Value, String)> = HashMap::new();
    for lane in lanes {
        for (rank, item) in lane.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            let signal_count = item.signals.as_object().map(|o| o.len()).unwrap_or(0);
            match aggregate.get_mut(&item.doc_index) {
                Some((rrf, nsig, signals, snippet)) => {
                    *rrf += contribution;
                    if signal_count > *nsig {
                        *nsig = signal_count;
                        *signals = item.signals.clone();
                        *snippet = item.snippet.clone();
                    }
                }
                None => {
                    aggregate.insert(
                        item.doc_index,
                        (
                            contribution,
                            signal_count,
                            item.signals.clone(),
                            item.snippet.clone(),
                        ),
                    );
                }
            }
        }
    }
    let mut fused: Vec<(usize, f64, Value, String)> = aggregate
        .into_iter()
        .map(|(index, (rrf, _, mut signals, snippet))| {
            if let Some(object) = signals.as_object_mut() {
                object.insert("rrf".into(), json!(round6(rrf)));
            }
            (index, round6(rrf), signals, snippet)
        })
        .collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(limit);
    fused
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

// ---------------------------------------------------------------------------
// NL as-of timeparse (ccc `ccc_memory_timeparse.py` rules)
// ---------------------------------------------------------------------------

/// Why no as-of instant could be estimated (body-free reasons, ccc rule).
pub type TimeparseError = &'static str;

fn end_of_day(local: DateTime<FixedOffset>) -> DateTime<Utc> {
    local
        .with_hour(23)
        .and_then(|t| t.with_minute(59))
        .and_then(|t| t.with_second(59))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(local)
        .with_timezone(&Utc)
}

fn add_months(base: DateTime<FixedOffset>, months: i64) -> Option<DateTime<FixedOffset>> {
    let offset = *base.offset();
    let total = base.year() as i64 * 12 + base.month() as i64 - 1 + months;
    let year = total.div_euclid(12);
    let month = (total.rem_euclid(12) + 1) as u32;
    let day = base.day();
    let last_day = days_in_month(year as i32, month);
    offset
        .with_ymd_and_hms(
            year as i32,
            month,
            day.min(last_day),
            base.hour(),
            base.minute(),
            base.second(),
        )
        .single()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

struct AbsoluteHit {
    year: i32,
    month: u32,
    day: u32,
    /// End-of-period rule: year-only hits resolve to Dec 31, month-only to
    /// the month end (ccc `_absolute_candidates`).
    span: (usize, usize),
    rule: &'static str,
    precision: AbsolutePrecision,
}

#[derive(Clone, Copy, PartialEq)]
enum AbsolutePrecision {
    Day,
    Month,
    Year,
}

fn boundary_ok(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    let is_digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
    !is_digit(before) && !is_digit(after)
}

fn absolute_hits(text: &str, now: DateTime<FixedOffset>) -> Vec<AbsoluteHit> {
    let mut hits = Vec::new();

    // ISO numeric dates: 2026-03-15 / 2026/3/15 / 2026.3.15 (year 19xx/20xx).
    static ISO: OnceLock<Regex> = OnceLock::new();
    let iso =
        ISO.get_or_init(|| Regex::new(r"((?:19|20)\d{2})[-/.](\d{1,2})[-/.](\d{1,2})").unwrap());
    for m in iso.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        if !boundary_ok(text, whole.start(), whole.end()) {
            continue;
        }
        let (year, month, day) = (
            m[1].parse::<i32>().unwrap_or(0),
            m[2].parse::<u32>().unwrap_or(0),
            m[3].parse::<u32>().unwrap_or(0),
        );
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            hits.push(AbsoluteHit {
                year,
                month,
                day,
                span: (whole.start(), whole.end()),
                rule: "abs:iso-date",
                precision: AbsolutePrecision::Day,
            });
        }
    }

    // Korean YMD: 2026년 3월 15일.
    static KO_YMD: OnceLock<Regex> = OnceLock::new();
    let ko_ymd = KO_YMD.get_or_init(|| {
        Regex::new(r"((?:19|20)\d{2})\s*년\s*(\d{1,2})\s*월\s*(\d{1,2})\s*일").unwrap()
    });
    for m in ko_ymd.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        hits.push(AbsoluteHit {
            year: m[1].parse().unwrap_or(0),
            month: m[2].parse().unwrap_or(0),
            day: m[3].parse().unwrap_or(0),
            span: (whole.start(), whole.end()),
            rule: "abs:ko-ymd",
            precision: AbsolutePrecision::Day,
        });
    }

    // Korean YM: 2026년 3월 (not followed by a day).
    static KO_YM: OnceLock<Regex> = OnceLock::new();
    let ko_ym =
        KO_YM.get_or_init(|| Regex::new(r"((?:19|20)\d{2})\s*년\s*(\d{1,2})\s*월").unwrap());
    for m in ko_ym.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        let rest = &text[whole.end()..];
        if rest
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            continue; // a day follows; the YMD rule owns this span
        }
        hits.push(AbsoluteHit {
            year: m[1].parse().unwrap_or(0),
            month: m[2].parse().unwrap_or(0),
            day: 1,
            span: (whole.start(), whole.end()),
            rule: "abs:ko-ym",
            precision: AbsolutePrecision::Month,
        });
    }

    // Korean MD: 3월 15일 — only when not already inside a year-qualified span.
    static KO_MD: OnceLock<Regex> = OnceLock::new();
    let ko_md = KO_MD.get_or_init(|| Regex::new(r"(\d{1,2})\s*월\s*(\d{1,2})\s*일").unwrap());
    for m in ko_md.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        if absolute_span_covers(&hits, whole.start(), whole.end()) {
            continue;
        }
        let before = text[..whole.start()].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_digit() || c == '년' || c == '월') {
            continue;
        }
        hits.push(AbsoluteHit {
            year: now.year(),
            month: m[1].parse().unwrap_or(0),
            day: m[2].parse().unwrap_or(0),
            span: (whole.start(), whole.end()),
            rule: "abs:ko-md",
            precision: AbsolutePrecision::Day,
        });
    }

    // Numeric MD: 3/15 with strict boundaries.
    static NUM_MD: OnceLock<Regex> = OnceLock::new();
    let num_md = NUM_MD.get_or_init(|| Regex::new(r"(\d{1,2})/(\d{1,2})").unwrap());
    for m in num_md.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        if absolute_span_covers(&hits, whole.start(), whole.end()) {
            continue;
        }
        let before_ok = text[..whole.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_digit() || matches!(c, '/' | '.' | '-')));
        let after_ok = text[whole.end()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_digit() || matches!(c, '/' | '.' | '-')));
        if !before_ok || !after_ok {
            continue;
        }
        let (month, day) = (
            m[1].parse::<u32>().unwrap_or(0),
            m[2].parse::<u32>().unwrap_or(0),
        );
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            hits.push(AbsoluteHit {
                year: now.year(),
                month,
                day,
                span: (whole.start(), whole.end()),
                rule: "abs:num-md",
                precision: AbsolutePrecision::Day,
            });
        }
    }

    // English month-year / year: "in March 2025", "in 2025".
    static EN_MY: OnceLock<Regex> = OnceLock::new();
    let en_my = EN_MY.get_or_init(|| {
        Regex::new(r"(?i)\bin\s+(january|february|march|april|may|june|july|august|september|october|november|december)\s+((?:19|20)\d{2})\b").unwrap()
    });
    for m in en_my.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        let month = month_from_name(&m[1]).unwrap_or(1);
        hits.push(AbsoluteHit {
            year: m[2].parse().unwrap_or(0),
            month,
            day: 1,
            span: (whole.start(), whole.end()),
            rule: "abs:en-month-year",
            precision: AbsolutePrecision::Month,
        });
    }
    static EN_Y: OnceLock<Regex> = OnceLock::new();
    let en_y = EN_Y.get_or_init(|| Regex::new(r"(?i)\bin\s+((?:19|20)\d{2})\b").unwrap());
    for m in en_y.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        if absolute_span_covers(&hits, whole.start(), whole.end()) {
            continue;
        }
        hits.push(AbsoluteHit {
            year: m[1].parse().unwrap_or(0),
            month: 12,
            day: 31,
            span: (whole.start(), whole.end()),
            rule: "abs:en-year",
            precision: AbsolutePrecision::Year,
        });
    }

    hits
}

fn absolute_span_covers(hits: &[AbsoluteHit], start: usize, end: usize) -> bool {
    hits.iter()
        .any(|hit| hit.span.0 <= start && end <= hit.span.1)
}

fn month_from_name(name: &str) -> Option<u32> {
    let names = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(name))
        .map(|i| i as u32 + 1)
}

/// Estimate an as-of instant from the query (§4.3): absolute beats relative;
/// two distinct absolute dates give up rather than coin-flip; every period
/// resolves to its END instant; the conservative failure reasons surface in
/// `temporal.nl_as_of` so a wrong guess is visible, never silent.
pub fn estimate_as_of(
    query: &str,
    now: DateTime<FixedOffset>,
) -> Result<(DateTime<Utc>, String), TimeparseError> {
    let query = query.trim();
    if query.is_empty() {
        return Err("empty-query");
    }

    let hits = absolute_hits(query, now);
    if !hits.is_empty() {
        let mut distinct: Vec<(i32, u32, u32)> =
            hits.iter().map(|h| (h.year, h.month, h.day)).collect();
        distinct.sort();
        distinct.dedup();
        if distinct.len() > 1 {
            return Err("ambiguous-absolute-dates");
        }
        let first = &hits[0];
        let offset = *now.offset();
        let local = match first.precision {
            AbsolutePrecision::Day => offset
                .with_ymd_and_hms(first.year, first.month, first.day, 23, 59, 59)
                .single(),
            AbsolutePrecision::Month => offset
                .with_ymd_and_hms(first.year, first.month, 1, 0, 0, 0)
                .single()
                .and_then(|start| add_months(start, 1))
                .map(|end| end - ChronoDuration::seconds(1)),
            AbsolutePrecision::Year => offset
                .with_ymd_and_hms(first.year, 12, 31, 23, 59, 59)
                .single(),
        };
        let local = local.ok_or("impossible-calendar-date")?;
        return Ok((end_of_day(local), first.rule.to_string()));
    }

    static KO_EN_REL: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let relatives = KO_EN_REL.get_or_init(|| {
        [
            (r"그저께|그제", "ko:그제"),
            (r"어제", "ko:어제"),
            (r"(\d+)\s*일\s*전", "ko:N일전"),
            (r"(\d+)\s*주\s*전", "ko:N주전"),
            (r"(\d+)\s*개월\s*전", "ko:N개월전"),
            (r"(\d+)\s*년\s*전", "ko:N년전"),
            (r"지난주|저번주", "ko:지난주"),
            (r"지난달|저번\s*달", "ko:지난달"),
            (r"작년", "ko:작년"),
            (r"(?i)\byesterday\b", "en:yesterday"),
            (r"(?i)\b(\d+)\s*days?\s+ago\b", "en:Ndaysago"),
            (r"(?i)\b(\d+)\s*weeks?\s+ago\b", "en:Nweeksago"),
            (r"(?i)\b(\d+)\s*months?\s+ago\b", "en:Nmonthsago"),
            (r"(?i)\b(\d+)\s*years?\s+ago\b", "en:Nyearsago"),
            (r"(?i)\blast\s+week\b", "en:lastweek"),
            (r"(?i)\blast\s+month\b", "en:lastmonth"),
            (r"(?i)\blast\s+year\b", "en:lastyear"),
        ]
        .into_iter()
        .map(|(pattern, rule)| (Regex::new(pattern).expect("relative pattern"), rule))
        .collect()
    });

    for (pattern, rule) in relatives.iter() {
        let Some(captures) = pattern.captures(query) else {
            continue;
        };
        let is_n_rule = captures.get(1).is_some();
        let n: i64 = captures
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(match *rule {
                "ko:그제" => 2,
                "ko:어제" | "en:yesterday" => 1,
                _ => 0,
            });
        if is_n_rule && n > 3650 {
            return Err("relative-out-of-range");
        }
        let resolved = resolve_relative(rule, n, now);
        if let Some(local) = resolved {
            return Ok((end_of_day(local), rule.to_string()));
        }
    }
    Err("no-time-reference")
}

/// Every period resolves to its END instant (docstring rule of the ccc
/// module): "지난주 시점" asks what was true by the end of last week.
fn resolve_relative(
    rule: &str,
    n: i64,
    now: DateTime<FixedOffset>,
) -> Option<DateTime<FixedOffset>> {
    match rule {
        "ko:그제" => Some(now - ChronoDuration::days(2)),
        "ko:어제" | "en:yesterday" => Some(now - ChronoDuration::days(1)),
        "ko:N일전" | "en:Ndaysago" => Some(now - ChronoDuration::days(n)),
        "ko:N주전" | "en:Nweeksago" => Some(now - ChronoDuration::weeks(n)),
        "ko:N개월전" | "en:Nmonthsago" => add_months(now, -n),
        "ko:N년전" | "en:Nyearsago" => add_months(now, -12 * n),
        "ko:지난주" | "en:lastweek" => {
            let days_since_monday = now.weekday().num_days_from_monday() as i64;
            let monday = now - ChronoDuration::days(days_since_monday);
            Some(start_of_day(monday)? - ChronoDuration::seconds(1))
        }
        "ko:지난달" | "en:lastmonth" => {
            let first = (*now.offset())
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single()?;
            Some(first - ChronoDuration::seconds(1))
        }
        "ko:작년" | "en:lastyear" => {
            let first = (*now.offset())
                .with_ymd_and_hms(now.year(), 1, 1, 0, 0, 0)
                .single()?;
            Some(first - ChronoDuration::seconds(1))
        }
        _ => None,
    }
}

fn start_of_day(local: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    local
        .with_hour(0)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Search options. `as_of` wins over the NL estimate; an unparseable
/// explicit as_of degrades to current mode with a body-free signal.
pub struct SearchOptions<'a> {
    pub query: &'a str,
    pub as_of: Option<&'a str>,
    pub limit: usize,
    pub now: DateTime<Utc>,
}

impl<'a> SearchOptions<'a> {
    pub fn new(query: &'a str, now: DateTime<Utc>) -> Self {
        Self {
            query,
            as_of: None,
            limit: SEARCH_LIMIT_DEFAULT,
            now,
        }
    }
}

/// Run a search over the route's index and return the ccc-compatible result
/// JSON: query, tokens, retrievalMode, lanes, temporal, results.
pub fn search(route: &Route, options: &SearchOptions) -> Result<Value> {
    let limit = options.limit.clamp(1, 50);
    let docs = build_index(route, options.now)?;
    let tokens = tokens_for(options.query);

    // Explicit as_of always wins; the NL estimate runs only without one and
    // is reported in the temporal summary (ccc #871 remaining slice).
    let mut as_of_raw: Option<String> = options.as_of.map(str::trim).map(str::to_string);
    let mut nl_as_of: Option<Value> = None;
    if as_of_raw.is_none() {
        match estimate_as_of(options.query, Local::now().fixed_offset()) {
            Ok((instant, rule)) => {
                as_of_raw = Some(facts::format_timestamp(instant));
                nl_as_of = Some(json!({ "rule": rule, "iso": as_of_raw.clone().unwrap() }));
            }
            Err(reason) => {
                if !matches!(reason, "no-time-reference" | "empty-query") {
                    nl_as_of = Some(json!({ "degraded": reason }));
                }
            }
        }
    }

    let as_of_instant = match &as_of_raw {
        Some(raw) => match facts::parse_timestamp(raw) {
            Ok(instant) => Some(instant),
            Err(_) => {
                as_of_raw = None;
                None
            }
        },
        None => None,
    };
    let as_of_parse_failed = options.as_of.is_some() && as_of_raw.is_none();

    let over = (limit * 6).max(30);
    let mut lexical = lexical_lane(&docs, &tokens, options.query, options.now);
    lexical.truncate(over);
    let mut lanes: Vec<Vec<LaneItem>> = vec![lexical];
    let mut used_lanes = vec!["lexical"];
    let mut fuzzy = fuzzy_lane(&docs, options.query, options.now);
    if !fuzzy.is_empty() {
        fuzzy.truncate(over);
        lanes.push(fuzzy);
        used_lanes.push("fuzzy");
    }

    let fused = if lanes.len() > 1 {
        rrf_fuse(&lanes, limit)
    } else {
        lanes[0]
            .iter()
            .take(limit)
            .map(|item| {
                (
                    item.doc_index,
                    round4(item.score),
                    item.signals.clone(),
                    item.snippet.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let retrieval_mode = if lanes.len() > 1 {
        "fusion-rrf"
    } else {
        "lexical"
    };

    // Valid-time semantics, applied uniformly after the lanes fused (ccc
    // `_temporal_finalize`): current mode excludes future and partitions
    // expired below still-valid; as_of keeps only then-valid facts.
    let mut current_part: Vec<Value> = Vec::new();
    let mut expired_part: Vec<Value> = Vec::new();
    let mut excluded = 0usize;
    let mut demoted = 0usize;
    let mut degraded = usize::from(as_of_parse_failed);
    for (doc_index, score, signals, snippet) in fused {
        let doc = &docs[doc_index];
        let (status, reason) = doc.temporal_status(options.now);
        let mut temporal = json!({
            "status": status.as_str(),
            "valid_from": doc.valid_from,
            "valid_until": doc.valid_until,
            "mode": if as_of_instant.is_some() { "as_of" } else { "current" },
        });
        if let Some(reason) = reason {
            temporal["reason"] = json!(reason);
            degraded += 1;
        }
        let out_of_window = match as_of_instant {
            Some(instant) => {
                let from = doc
                    .valid_from
                    .as_deref()
                    .and_then(|v| facts::parse_timestamp(v).ok());
                let until = doc
                    .valid_until
                    .as_deref()
                    .and_then(|v| facts::parse_timestamp(v).ok());
                from.is_some_and(|f| f > instant) || until.is_some_and(|u| u <= instant)
            }
            None => status == TemporalStatus::Future,
        };
        if out_of_window {
            excluded += 1;
            continue;
        }
        let mut result = json!({
            "path": doc.path,
            "source": doc.source,
            "snippet": snippet,
            "score": score,
            "signals": signals,
            "temporal": temporal,
        });
        if as_of_instant.is_some() {
            result["temporal"]["as_of"] = json!(as_of_raw.clone().unwrap_or_default());
        }
        if status == TemporalStatus::Expired && as_of_instant.is_none() {
            demoted += 1;
            expired_part.push(result);
        } else {
            current_part.push(result);
        }
    }
    current_part.extend(expired_part);

    let mut temporal_summary = json!({
        "mode": if as_of_instant.is_some() { "as_of" } else { "current" },
        "as_of": as_of_raw,
        "excluded": excluded,
        "demoted": demoted,
        "degraded": degraded,
    });
    if as_of_parse_failed {
        temporal_summary["reason"] = json!("as-of-parse-failed-using-current");
    }
    if let Some(nl) = nl_as_of {
        temporal_summary["nl_as_of"] = nl;
    }

    Ok(json!({
        "query": options.query,
        "tokens": tokens,
        "retrievalMode": retrieval_mode,
        "lanes": used_lanes,
        "temporal": temporal_summary,
        "results": current_part,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pinned +09:00 clock so the local-end-of-period rules are
    /// deterministic on any runner timezone.
    fn kst(year: i32, month: u32, day: u32, hour: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn tokens_follow_the_ccc_rule() {
        assert_eq!(
            tokens_for("task: memory PR; cwd: danso http https ok"),
            vec!["memory".to_string(), "danso".to_string(), "ok".to_string()]
        );
        assert!(tokens_for("한글 토큰").contains(&"한글".to_string()));
        assert!(tokens_for("a b c").is_empty());
        // Duplicates collapse before the 12-token cap.
        assert_eq!(tokens_for(&"word ".repeat(20)).len(), 1);
    }

    #[test]
    fn ngrams_cover_korean_variants() {
        let base = char_ngrams("메모리");
        let inflected = char_ngrams("메모리를");
        let shared = base.intersection(&inflected).count();
        assert!(shared >= 1, "morphological variants share n-grams");
    }

    #[test]
    fn timeparse_resolves_periods_to_their_end() {
        let now = kst(2026, 9, 8, 10);
        let (instant, rule) = estimate_as_of("어제 회의 결과", now).unwrap();
        assert_eq!(rule, "ko:어제");
        assert_eq!(
            facts::parse_timestamp("2026-09-07T23:59:59+09:00").unwrap(),
            instant
        );
        let (instant, rule) = estimate_as_of("2026-03-15에 무슨 일이", now).unwrap();
        assert_eq!(rule, "abs:iso-date");
        assert_eq!(
            facts::parse_timestamp("2026-03-15T23:59:59+09:00").unwrap(),
            instant
        );
    }

    #[test]
    fn timeparse_prefers_absolute_and_gives_up_on_conflict() {
        let now = kst(2026, 9, 8, 10);
        let (_, rule) = estimate_as_of("어제 말고 2026-03-15 기준", now).unwrap();
        assert_eq!(rule, "abs:iso-date");
        assert_eq!(
            estimate_as_of("2026-03-15와 2026-04-01 사이", now).unwrap_err(),
            "ambiguous-absolute-dates"
        );
        assert_eq!(
            estimate_as_of("시간 표현 없음", now).unwrap_err(),
            "no-time-reference"
        );
        assert_eq!(estimate_as_of("", now).unwrap_err(), "empty-query");
    }

    #[test]
    fn korean_ymd_suppresses_the_yearless_variant() {
        let now = kst(2026, 9, 8, 10);
        let (_, rule) = estimate_as_of("2024년 3월 15일 상황", now).unwrap();
        assert_eq!(rule, "abs:ko-ymd");
    }

    #[test]
    fn relative_ranges_and_month_ends() {
        let now = kst(2026, 9, 8, 10);
        assert_eq!(
            estimate_as_of("4000일 전", now).unwrap_err(),
            "relative-out-of-range"
        );
        let (instant, rule) = estimate_as_of("지난주 시점", now).unwrap();
        assert_eq!(rule, "ko:지난주");
        // 2026-09-08 is a Tuesday; the previous ISO week ended Sunday 23:59:59.
        assert_eq!(
            facts::parse_timestamp("2026-09-06T23:59:59+09:00").unwrap(),
            instant
        );
        let (instant, _) = estimate_as_of("지난달", now).unwrap();
        assert_eq!(
            facts::parse_timestamp("2026-08-31T23:59:59+09:00").unwrap(),
            instant
        );
    }

    #[test]
    fn rrf_fusion_accumulates_across_lanes() {
        let lanes = vec![
            vec![
                LaneItem {
                    doc_index: 1,
                    score: 10.0,
                    signals: json!({"a": 1}),
                    snippet: "one".into(),
                },
                LaneItem {
                    doc_index: 2,
                    score: 8.0,
                    signals: json!({"a": 1, "b": 2}),
                    snippet: "two".into(),
                },
            ],
            vec![LaneItem {
                doc_index: 2,
                score: 9.0,
                signals: json!({"a": 1}),
                snippet: "two".into(),
            }],
        ];
        let fused = rrf_fuse(&lanes, 5);
        assert_eq!(fused.len(), 2);
        // Doc 1 appears once (1/62); doc 2 appears twice (1/62 + 1/61).
        assert_eq!(fused[0].0, 2);
        assert!(fused[0].1 > fused[1].1);
    }
}
