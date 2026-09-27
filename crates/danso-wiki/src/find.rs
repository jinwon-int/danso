//! `find` — the practical Wiki discovery pass (issue #121 slice 2), ported
//! from the bash `cmd_find` + `cmd_semantic_search` python.
//!
//! One pass = semantic candidates from the local index plus an exact-text
//! fallback over the read cache, every result carrying its
//! `wiki-agent load --lines` verification command. The scoring contract is
//! the bash one: `hashed = 0.75·vector + lexical + exact` with
//! `dense = 0.7·calibrated + 0.3·lexical + exact` for chunks that carry a
//! dense vector, best-by-path bounding, weighted or RRF fusion, authority
//! multipliers on policy-intent queries, and calibrated abstention that
//! suppresses both candidate lists below full cross-signal confidence.
//! Output snippets and text-match lines are masked (`text::redact`,
//! decision 2b); scoring always uses the raw index fields.

use crate::abstention::{self, DecisionInput, TopEvidence};
use crate::alias::AliasTable;
use crate::authority::{self, AUTHORITY_SCHEMA, Authority};
use crate::cache::{self, CachedIndex};
use crate::chunk::Chunk;
use crate::corpus::{self, CorpusStats};
use crate::embedding::{self, JinaConfig};
use crate::meta::IndexMeta;
use crate::text;
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// BM25-lite constants — the bash ones, verbatim.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

static EXACT_KEYS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "headingExact",
        "snippetExact",
        "pathExact",
        "aliasPathExact",
        "aliasHeadingExact",
        "aliasSnippetExact",
    ]
});

pub struct FindOptions {
    pub query: String,
    pub index_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub json: bool,
    /// Re-parse the index files into the local query cache first. This is a
    /// reader-local refresh: it never syncs, never rebuilds the index, never
    /// touches the network.
    pub refresh: bool,
    /// `None` — default (on) unless `WIKI_SEMANTIC_ABSTENTION` says otherwise.
    pub abstention: Option<bool>,
    pub top: usize,
    pub grep_top: usize,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Everything the environment can tune, resolved once with fail-closed
/// validation (a garbled number must not silently change the vector space).
struct Settings {
    dim: u32,
    backend: String,
    fusion: String,
    rrf_k: u32,
    dense_floor: f64,
    authority_ranking: bool,
    alias_expansion: bool,
    alias_file: PathBuf,
    abstention_enabled: bool,
    recency_weight: f64,
    recency_half_life_days: f64,
}

impl Settings {
    fn resolve(options: &FindOptions) -> Result<Settings> {
        let dim = match std::env::var("WIKI_AGENT_SEMANTIC_DIMS") {
            Ok(raw) => {
                let parsed: i64 = raw.trim().parse().with_context(|| {
                    format!("WIKI_AGENT_SEMANTIC_DIMS must be an integer, got {raw:?}")
                })?;
                if parsed < 1 {
                    bail!("WIKI_AGENT_SEMANTIC_DIMS must be >= 1, got {parsed}");
                }
                parsed as u32
            }
            Err(_) => 2048,
        };
        let backend = std::env::var("WIKI_AGENT_SEMANTIC_BACKEND")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "hashed".to_string());
        let fusion = std::env::var("WIKI_SEMANTIC_FUSION")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "weighted".to_string());
        if fusion != "weighted" && fusion != "rrf" {
            bail!("unknown fusion mode: {fusion}; supported: weighted, rrf");
        }
        let rrf_k = match std::env::var("WIKI_SEMANTIC_RRF_K") {
            Ok(raw) => {
                let parsed: i64 = raw.trim().parse().with_context(|| {
                    format!("WIKI_SEMANTIC_RRF_K must be an integer, got {raw:?}")
                })?;
                parsed.max(1) as u32
            }
            Err(_) => 60,
        };
        let dense_floor = match std::env::var("WIKI_SEMANTIC_DENSE_FLOOR") {
            Ok(raw) => raw.trim().parse::<f64>().with_context(|| {
                format!("WIKI_SEMANTIC_DENSE_FLOOR must be a number, got {raw:?}")
            })?,
            Err(_) => 0.0,
        };
        let authority_ranking = std::env::var("WIKI_SEMANTIC_AUTHORITY_RANKING")
            .map(|v| v == "1")
            .unwrap_or(true);
        let alias_expansion = std::env::var("WIKI_AGENT_ALIAS_EXPANSION")
            .map(|v| v == "1")
            .unwrap_or(true);
        let alias_file = match std::env::var("WIKI_AGENT_ALIAS_FILE") {
            Ok(path) if !path.is_empty() => PathBuf::from(path),
            _ => options.cache_dir.join("pages/aliases.md"),
        };
        let abstention_env = match std::env::var("WIKI_SEMANTIC_ABSTENTION") {
            Ok(raw) => match raw.as_str() {
                "1" | "true" | "on" | "yes" => Some(true),
                "0" | "false" | "off" | "no" | "" => Some(false),
                other => bail!(
                    "WIKI_SEMANTIC_ABSTENTION must be 0 or 1 (or use --abstention/--no-abstention); got: {other}"
                ),
            },
            Err(_) => None,
        };
        let recency_weight = match std::env::var("WIKI_SEMANTIC_RECENCY_WEIGHT") {
            Ok(raw) => raw.trim().parse::<f64>().with_context(|| {
                format!("WIKI_SEMANTIC_RECENCY_WEIGHT must be a number, got {raw:?}")
            })?,
            Err(_) => 0.0,
        };
        let recency_half_life_days = match std::env::var("WIKI_SEMANTIC_RECENCY_HALF_LIFE_DAYS") {
            Ok(raw) => raw.trim().parse::<f64>().with_context(|| {
                format!("WIKI_SEMANTIC_RECENCY_HALF_LIFE_DAYS must be a number, got {raw:?}")
            })?,
            Err(_) => 30.0,
        }
        .max(0.1);
        Ok(Settings {
            dim,
            backend,
            fusion,
            rrf_k,
            dense_floor,
            authority_ranking,
            alias_expansion,
            alias_file,
            abstention_enabled: options.abstention.unwrap_or(abstention_env.unwrap_or(true)),
            recency_weight,
            recency_half_life_days,
        })
    }
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

/// `dot(a, b)` — sparse dot product; the smaller side drives the iteration.
fn dot(query: &BTreeMap<String, f64>, record: &BTreeMap<String, f32>) -> f64 {
    if query.len() <= record.len() {
        query
            .iter()
            .map(|(dim, weight)| weight * record.get(dim).copied().unwrap_or(0.0) as f64)
            .sum()
    } else {
        record
            .iter()
            .map(|(dim, weight)| query.get(dim).copied().unwrap_or(0.0) * *weight as f64)
            .sum()
    }
}

fn dense_dot(query: &[f32], record: &[f32]) -> f64 {
    query
        .iter()
        .zip(record.iter())
        .map(|(q, v)| *q as f64 * *v as f64)
        .sum()
}

/// BM25-lite over the corpus statistics; `(score, matched_terms)`.
fn bm25_score(
    query_terms: &BTreeMap<String, u32>,
    record_terms: &BTreeMap<String, f32>,
    stats: &CorpusStats,
) -> (f64, u64) {
    if query_terms.is_empty() {
        return (0.0, 0);
    }
    let total_docs = stats.total_docs() as f64;
    let avgdl = stats.avgdl();
    let doc_len: f64 = record_terms.values().map(|count| *count as f64).sum();
    let doc_len_norm = doc_len.max(1.0);
    let mut score = 0.0;
    let mut matched = 0u64;
    for (term, q_count) in query_terms {
        let tf = f64::from(record_terms.get(term).copied().unwrap_or(0.0));
        if tf == 0.0 {
            continue;
        }
        matched += 1;
        let df = stats.df(term) as f64;
        let idf = (1.0 + (total_docs - df + 0.5) / (df + 0.5)).ln();
        let tf_component = (tf * (BM25_K1 + 1.0))
            / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * doc_len_norm / avgdl));
        score += idf * tf_component * (1.0 + 0.1 * (*q_count).min(3) as f64);
    }
    (0.45f64.min(score * 0.08), matched)
}

/// Plain lexical overlap for indexes without corpus statistics.
fn lexical_score(
    query_terms: &BTreeMap<String, u32>,
    record_terms: &BTreeMap<String, f32>,
) -> (f64, u64) {
    if query_terms.is_empty() {
        return (0.0, 0);
    }
    let mut overlap = 0.0;
    let mut matched = 0u64;
    for (term, q_count) in query_terms {
        let count = record_terms.get(term).copied().unwrap_or(0.0);
        if count != 0.0 {
            matched += 1;
            overlap += (1.0 + (*q_count as f64).ln()) * (1.0 + (count as f64).ln());
        }
    }
    let coverage = matched as f64 / query_terms.len().max(1) as f64;
    (0.35f64.min(0.05 * overlap + 0.20 * coverage), matched)
}

/// The record's lexical bag, tokenizing path/heading/snippet when the index
/// stored none (`record.get("terms") or tokenize(...)`).
fn effective_terms(chunk: &Chunk) -> BTreeMap<String, f32> {
    if chunk.terms.is_empty() {
        text::tokenize(&format!(
            "{} {} {}",
            chunk.path, chunk.heading, chunk.snippet
        ))
        .into_iter()
        .map(|(term, count)| (term, count as f32))
        .collect()
    } else {
        chunk.terms.clone()
    }
}

/// One per-path candidate: the item the bash code builds, plus the rrf
/// temporaries it keeps on the item until fusion.
struct Candidate {
    id: String,
    path: String,
    heading: String,
    heading_stack: Vec<String>,
    start_line: u64,
    end_line: u64,
    snippet: String,
    score: f64,
    explain: Map<String, Value>,
    hashed_score: Option<f64>,
    dense_score: Option<f64>,
    recency_boost: f64,
}

impl Candidate {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "path": self.path,
            "heading": text::redact(&self.heading),
            "headingStack": self.heading_stack,
            "startLine": self.start_line,
            "endLine": self.end_line,
            "snippet": text::redact(&self.snippet),
            "score": self.score,
            "explain": Value::Object(self.explain.clone()),
            "loadCommand": format!(
                "wiki-agent load --lines {}:{} {}",
                self.start_line, self.end_line, self.path
            ),
        })
    }
}

fn fnmatch_regex(pattern: &str, cache: &Mutex<HashMap<String, Regex>>) -> Regex {
    let mut cache = cache.lock().expect("fnmatch cache");
    if let Some(compiled) = cache.get(pattern) {
        return compiled.clone();
    }
    let mut translated = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(current) = chars.next() {
        match current {
            '*' => translated.push_str(".*"),
            '?' => translated.push('.'),
            '[' => {
                let negated = chars.peek() == Some(&'!');
                if negated {
                    chars.next();
                }
                let mut body = String::new();
                let mut closed = false;
                let mut first = true;
                for inner in chars.by_ref() {
                    if inner == ']' && !first {
                        closed = true;
                        break;
                    }
                    first = false;
                    if inner == '\\' {
                        body.push_str("\\\\");
                        continue;
                    }
                    if body.is_empty() && inner == '^' {
                        body.push('\\');
                    }
                    body.push(inner);
                }
                if closed {
                    translated.push_str(if negated { "[^" } else { "[" });
                    translated.push_str(&body);
                    translated.push(']');
                } else {
                    translated.push_str("\\[");
                    translated.push_str(&body);
                }
            }
            other => translated.push_str(&regex::escape(&other.to_string())),
        }
    }
    translated.push('$');
    let compiled = Regex::new(&translated).expect("translated fnmatch pattern");
    cache.insert(pattern.to_string(), compiled.clone());
    compiled
}

fn path_allowed(path: &str, include: &[Regex], exclude: &[Regex]) -> bool {
    if !include.is_empty() && !include.iter().any(|re| re.is_match(path)) {
        return false;
    }
    if exclude.iter().any(|re| re.is_match(path)) {
        return false;
    }
    true
}

/// `str.splitlines()` — python splits on more boundaries than `\n`, and a
/// trailing boundary does not produce a final empty line.
fn py_splitlines(raw: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut iterator = raw.char_indices();
    while let Some((offset, current)) = iterator.next() {
        if !matches!(
            current,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            continue;
        }
        lines.push(&raw[start..offset]);
        let mut next = offset + current.len_utf8();
        if current == '\r' && raw.as_bytes().get(next) == Some(&b'\n') {
            next += 1;
            iterator.next();
        }
        start = next;
    }
    if start < raw.len() {
        lines.push(&raw[start..]);
    }
    lines
}

#[derive(Debug)]
struct TextMatch {
    path: String,
    line: u64,
    rank: u32,
    text: String,
    load_command: String,
    also_semantic_path: bool,
}

impl TextMatch {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "line": self.line,
            "rank": self.rank,
            "text": self.text,
            "loadCommand": self.load_command,
            "alsoSemanticPath": self.also_semantic_path,
        })
    }
}

/// The exact-text fallback: `text_matches()` in the bash implementation.
fn text_matches(
    cache_dir: &std::path::Path,
    query: &str,
    grep_top: usize,
    include: &[Regex],
    exclude: &[Regex],
    semantic_paths: &HashSet<String>,
) -> Vec<TextMatch> {
    let terms = text::tokens(query);
    let query_l = query.to_lowercase();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    collect_markdown(cache_dir, &mut files);
    files.sort();
    let mut candidates: Vec<(u32, String, u64, String)> = Vec::new();
    for path in files {
        let Ok(relative) = path.strip_prefix(cache_dir) else {
            continue;
        };
        let rel = relative.to_string_lossy();
        if !path_allowed(&rel, include, exclude) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let raw = String::from_utf8_lossy(&bytes);
        let owned = raw.into_owned();
        for (index, line) in py_splitlines(&owned).iter().enumerate() {
            let line_l = line.to_lowercase();
            let rank = if !query_l.is_empty() && line_l.contains(&query_l) {
                3
            } else if !terms.is_empty() && terms.iter().all(|term| line_l.contains(term.as_str())) {
                2
            } else if !terms.is_empty() && terms.iter().any(|term| line_l.contains(term.as_str())) {
                1
            } else {
                continue;
            };
            let stripped = line.trim();
            if stripped.is_empty() {
                continue;
            }
            candidates.push((
                rank,
                rel.clone().into_owned(),
                (index + 1) as u64,
                stripped.to_string(),
            ));
        }
    }
    // python's sort key is (-rank, path, line); stable, so ties keep the
    // file/line sweep order.
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut deduped: Vec<TextMatch> = Vec::new();
    let mut seen = HashSet::new();
    for (rank, path, line, raw_text) in candidates {
        if !seen.insert((path.clone(), line)) {
            continue;
        }
        let cut: String = raw_text.chars().take(260).collect();
        deduped.push(TextMatch {
            also_semantic_path: semantic_paths.contains(&path),
            load_command: format!("wiki-agent load --lines {line}:{line} {path}"),
            text: text::redact(&cut),
            path,
            line,
            rank,
        });
        if deduped.len() >= grep_top {
            break;
        }
    }
    deduped
}

fn collect_markdown(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => collect_markdown(&path, out),
            Ok(_) if name.ends_with(".md") => out.push(path),
            _ => {}
        }
    }
}

/// Entry point: prints the report (text or JSON) and returns the exit code.
/// 0 — report produced. 2 — fail-closed (unreadable index, unusable backend).
pub fn run(options: &FindOptions) -> i32 {
    match run_inner(options) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("wiki find failed: {error:#}");
            2
        }
    }
}

/// One rendered find report: the machine-readable payload plus the
/// text-fallback matches the text renderer needs. Split from the printing so
/// the contract tests can assert on the report instead of scraping stdout.
struct Found {
    payload: Value,
    text_matches: Vec<TextMatch>,
}

fn report(options: &FindOptions) -> Result<Found> {
    let started = std::time::Instant::now();
    let settings = Settings::resolve(options)?;

    let cached = load_index(options)?;
    let meta_raw: Value = std::fs::read_to_string(options.index_dir.join("meta.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));

    let alias_table = AliasTable::load(&settings.alias_file);
    let (expanded_query, alias_terms) =
        alias_table.expand(&options.query, settings.alias_expansion);
    let alias_exact_terms =
        alias_table.resolve_exact_terms(&options.query, settings.alias_expansion);
    let query_vec = text::vectorize(&expanded_query, settings.dim);
    let query_terms = text::tokenize(&expanded_query);
    let query_l = options.query.to_lowercase();
    let authority_intent = authority::query_intent(&options.query);

    let corpus_stats = corpus::load(&options.index_dir).unwrap_or(None);
    let lexical_label = if corpus_stats.is_some() {
        "bm25"
    } else {
        "lexical"
    };

    let filter_cache: Mutex<HashMap<String, Regex>> = Mutex::new(HashMap::new());
    let include: Vec<Regex> = options
        .include
        .iter()
        .map(|pattern| fnmatch_regex(pattern, &filter_cache))
        .collect();
    let exclude: Vec<Regex> = options
        .exclude
        .iter()
        .map(|pattern| fnmatch_regex(pattern, &filter_cache))
        .collect();

    let query_dense: Option<Vec<f32>> = match settings.backend.as_str() {
        "hashed" => None,
        "jina-api" => {
            let jina = JinaConfig::from_env(embedding_cache_dir()?)?;
            validate_jina_index(&cached.meta, &jina)?;
            match embedding::cached_query_vector(&jina, &options.query)? {
                Some(vector) => Some(vector),
                None => bail!(
                    "jina-api: query embedding is not in the local wiki-embedding-cache; \
                     warm it once with the bash wiki-agent (same model/task/text) or rollback: \
                     WIKI_AGENT_SEMANTIC_BACKEND=hashed — the Rust read path never dials out"
                ),
            }
        }
        other => bail!(
            "backend {other:?} is not implemented in the Rust read path \
             (issue #121 decision 1: hashed first, jina-api via cache); \
             rollback: WIKI_AGENT_SEMANTIC_BACKEND=hashed"
        ),
    };

    let mut best_by_path: HashMap<String, usize> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut authority_by_path: HashMap<String, Authority> = HashMap::new();
    let mut chunks_scanned = 0u64;
    let mut chunks_considered = 0u64;
    let mut positive_chunks = 0u64;
    let mut dense_chunks = 0u64;
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);

    for chunk in &cached.chunks {
        chunks_scanned += 1;
        if !path_allowed(&chunk.path, &include, &exclude) {
            continue;
        }
        let authority = authority_by_path
            .entry(chunk.path.clone())
            .or_insert_with(|| Authority::classify(&chunk.path, &chunk.heading, &chunk.snippet))
            .clone();
        chunks_considered += 1;
        let vector_score = dot(&query_vec, &chunk.vector);
        let record_terms = effective_terms(chunk);
        let (lexical, matched_terms) = match &corpus_stats {
            Some(stats) => bm25_score(&query_terms, &record_terms, stats),
            None => lexical_score(&query_terms, &record_terms),
        };

        let dense_raw = match (&query_dense, &chunk.dense_vector) {
            (Some(query), Some(record)) if !record.is_empty() => {
                dense_chunks += 1;
                Some(dense_dot(query, record))
            }
            _ => None,
        };
        let dense_component = dense_raw.map(|raw| {
            ((raw - settings.dense_floor) / (1.0 - settings.dense_floor).max(1e-9)).max(0.0)
        });

        let heading_l = chunk.heading.to_lowercase();
        let snippet_l = chunk.snippet.to_lowercase();
        let path_l = chunk.path.to_lowercase();
        let heading_exact = if !query_l.is_empty() && heading_l.contains(&query_l) {
            0.12
        } else {
            0.0
        };
        let snippet_exact = if !query_l.is_empty() && snippet_l.contains(&query_l) {
            0.02
        } else {
            0.0
        };
        let path_exact = if !query_l.is_empty() && path_l.contains(&query_l) {
            0.03
        } else {
            0.0
        };
        let (alias_path, alias_heading, alias_snippet) = alias_table.exact_boost(
            &alias_exact_terms,
            &chunk.path,
            &chunk.heading,
            &chunk.snippet,
        );
        let exact_boost =
            heading_exact + snippet_exact + path_exact + alias_path + alias_heading + alias_snippet;

        let mut boosts = Map::new();
        boosts.insert("aliasExactTerms".into(), json!(alias_exact_terms));
        boosts.insert("aliasHeadingExact".into(), json!(alias_heading));
        boosts.insert("aliasPathExact".into(), json!(alias_path));
        boosts.insert("aliasSnippetExact".into(), json!(alias_snippet));
        boosts.insert(
            "authority".into(),
            serde_json::to_value(&authority).expect("serializable authority"),
        );
        boosts.insert("authorityIntent".into(), json!(authority_intent));
        boosts.insert("authorityRanking".into(), json!(settings.authority_ranking));
        if let Some(raw) = dense_raw {
            boosts.insert("dense".into(), json!(round6(raw)));
            boosts.insert(
                "denseCalibrated".into(),
                json!(round6(dense_component.unwrap_or(0.0))),
            );
        }
        boosts.insert("headingExact".into(), json!(heading_exact));
        boosts.insert("lexical".into(), json!(round6(lexical)));
        boosts.insert("matchedTerms".into(), json!(matched_terms));
        boosts.insert("pathExact".into(), json!(path_exact));
        boosts.insert("snippetExact".into(), json!(snippet_exact));
        boosts.insert("vector".into(), json!(round6(vector_score)));
        boosts.insert(lexical_label.to_string(), json!(round6(lexical)));

        let hashed_score = 0.75 * vector_score + lexical + exact_boost;
        let recency = recency_boost(chunk, &settings, now_ts);

        if settings.fusion == "rrf" {
            // Rank fusion needs both signals as separate rank lists; admit a
            // path on either lexical evidence or an available dense vector.
            if hashed_score <= 0.0 && dense_raw.is_none() {
                continue;
            }
            if hashed_score > 0.0 {
                positive_chunks += 1;
            }
            let item = Candidate {
                id: chunk.id.clone(),
                path: chunk.path.clone(),
                heading: chunk.heading.clone(),
                heading_stack: chunk.heading_stack.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                snippet: chunk.snippet.clone(),
                score: round6(hashed_score.max(0.0)),
                explain: boosts,
                hashed_score: Some(hashed_score),
                dense_score: dense_raw,
                recency_boost: recency,
            };
            merge_best_by_path_rrf(&mut best_by_path, &mut candidates, item);
            continue;
        }

        let score = match dense_component {
            Some(component) => 0.7 * component + 0.3 * lexical + exact_boost,
            None => hashed_score,
        };
        // Freshness boosts ties between RELEVANT chunks only; boosting
        // zero-evidence chunks would make negative queries return results.
        if score <= 0.0 {
            continue;
        }
        let mut score = score;
        if recency > 0.0 {
            boosts.insert("recencyBoost".into(), json!(round6(recency)));
            score += recency;
        }
        let multiplier =
            authority::score_multiplier(&authority, authority_intent, settings.authority_ranking);
        boosts.insert("authorityMultiplier".into(), json!(multiplier));
        if multiplier != 1.0 {
            boosts.insert("preAuthorityScore".into(), json!(round6(score)));
            score *= multiplier;
        }
        positive_chunks += 1;
        let item = Candidate {
            id: chunk.id.clone(),
            path: chunk.path.clone(),
            heading: chunk.heading.clone(),
            heading_stack: chunk.heading_stack.clone(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            snippet: chunk.snippet.clone(),
            score,
            explain: boosts,
            hashed_score: None,
            dense_score: None,
            recency_boost: 0.0,
        };
        match best_by_path.get(&chunk.path).copied() {
            Some(index) => {
                if item.score > candidates[index].score {
                    candidates[index] = item;
                }
            }
            None => {
                best_by_path.insert(chunk.path.clone(), candidates.len());
                candidates.push(item);
            }
        }
    }

    let ranked = if settings.fusion == "rrf" {
        fuse_rrf(
            candidates,
            settings.rrf_k,
            authority_intent,
            settings.authority_ranking,
        )
    } else {
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates
    };

    let candidate_results: Vec<&Candidate> = ranked.iter().take(options.top).collect();
    let top_evidence = candidate_results.first().map(|top| TopEvidence {
        matched_terms: top
            .explain
            .get("matchedTerms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        sparse_vector: top
            .explain
            .get("vector")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        dense_cosine: top.explain.get("dense").and_then(Value::as_f64),
        authority_tier: top
            .explain
            .get("authority")
            .and_then(|authority| authority.get("tier"))
            .and_then(Value::as_str)
            .unwrap_or("standard")
            .to_string(),
        exact_boost: EXACT_KEYS
            .iter()
            .map(|key| top.explain.get(*key).and_then(Value::as_f64).unwrap_or(0.0))
            .sum(),
    });
    let decision = abstention::decide(
        candidate_results.is_empty(),
        top_evidence.as_ref(),
        query_terms.len(),
        &DecisionInput {
            backend: &settings.backend,
            fusion: &settings.fusion,
            enabled: settings.abstention_enabled,
            dense_model: cached.meta.dense_model.as_deref().unwrap_or(""),
            vectorizer: cached.meta.vectorizer.as_deref().unwrap_or(""),
            reranked: false,
        },
    );

    let results: Vec<Value> = if decision.abstained {
        Vec::new()
    } else {
        candidate_results
            .iter()
            .map(|candidate| candidate.to_json())
            .collect()
    };

    let abstention_json = serde_json::to_value(&decision).expect("serializable abstention");
    let mut metrics = Map::new();
    metrics.insert(
        "abstention".into(),
        json!({
            "abstained": decision.abstained,
            "calibrated": decision.calibrated,
            "calibration": decision.calibration,
            "confidence": decision.confidence,
            "enabled": decision.enabled,
            "reason": decision.reason,
        }),
    );
    metrics.insert(
        "authorityRanking".into(),
        json!({
            "enabled": settings.authority_ranking,
            "intent": authority_intent,
            "schema": AUTHORITY_SCHEMA,
        }),
    );
    metrics.insert("backend".into(), json!(settings.backend));
    metrics.insert("boundedBy".into(), json!("bestByPath"));
    metrics.insert("chunksConsidered".into(), json!(chunks_considered));
    metrics.insert("chunksScanned".into(), json!(chunks_scanned));
    metrics.insert("denseChunks".into(), json!(dense_chunks));
    metrics.insert(
        "elapsedMs".into(),
        json!(round3(started.elapsed().as_secs_f64() * 1000.0)),
    );
    metrics.insert("fusion".into(), json!(settings.fusion));
    if let Some(rss) = max_rss_kb() {
        metrics.insert("maxRssKb".into(), json!(rss));
    }
    metrics.insert("positiveChunks".into(), json!(positive_chunks));
    metrics.insert("returned".into(), json!(results.len()));
    metrics.insert("uniqueCandidatePaths".into(), json!(best_by_path.len()));
    if !alias_terms.is_empty() {
        metrics.insert("queryExpansion".into(), json!(alias_terms));
    }
    if settings.recency_weight > 0.0 {
        metrics.insert("recencyWeight".into(), json!(settings.recency_weight));
        metrics.insert(
            "recencyHalfLifeDays".into(),
            json!(settings.recency_half_life_days),
        );
    }

    let semantic_payload = json!({
        "abstained": decision.abstained,
        "abstention": abstention_json,
        "abstentionReason": decision.reason,
        "confidence": decision.confidence,
        "filters": {
            "exclude": options.exclude,
            "include": options.include,
        },
        "index": meta_raw,
        "metrics": Value::Object(metrics),
        "query": options.query,
        "results": results,
        "schema": "semantic-search-v1",
    });

    let semantic_paths: HashSet<String> = if decision.abstained {
        HashSet::new()
    } else {
        candidate_results
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect()
    };
    let matches = if decision.abstained {
        Vec::new()
    } else {
        text_matches(
            &options.cache_dir,
            &options.query,
            options.grep_top,
            &include,
            &exclude,
            &semantic_paths,
        )
    };

    let find_payload = json!({
        "abstained": decision.abstained,
        "abstention": abstention_json,
        "abstentionReason": decision.reason,
        "confidence": decision.confidence,
        "query": options.query,
        "schema": "wiki-agent-find-v1",
        "semantic": semantic_payload,
        "textMatches": matches.iter().map(TextMatch::to_json).collect::<Vec<_>>(),
    });

    Ok(Found {
        payload: find_payload,
        text_matches: matches,
    })
}

fn run_inner(options: &FindOptions) -> Result<()> {
    // Decision 1a: the reranker is out of the first contract. Warn once and
    // search with first-stage order; the A/B procedure fixes the env empty.
    if let Ok(reranker) = std::env::var("WIKI_AGENT_RERANKER")
        && !reranker.is_empty()
    {
        eprintln!(
            "wiki-agent: reranker {reranker} is not implemented in the Rust read path; \
             ignoring it (issue #121 decision 1a)"
        );
    }

    let found = report(options)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if options.json {
        writeln!(out, "{}", found.payload).context("could not write the find report")?;
        return Ok(());
    }
    print_text_report(&mut out, options, &found.payload, &found.text_matches)
}

/// Cache-first index load: fresh cache is used as-is; stale/invalid/absent
/// (or `--refresh`) re-parses the source files into the node-local cache.
fn load_index(options: &FindOptions) -> Result<CachedIndex> {
    let current = cache::source_key(&options.index_dir)
        .context("semantic index unavailable; build it with: wiki-agent semantic-index")?;
    if !options.refresh
        && let Ok(cached) = cache::load(&options.cache_dir)
        && cached.key == current
    {
        return Ok(cached);
    }
    cache::build(&options.index_dir, &options.cache_dir)
}

fn validate_jina_index(meta: &IndexMeta, jina: &JinaConfig) -> Result<()> {
    if meta.backend != "jina-api" || meta.dense_model.as_deref().unwrap_or("").is_empty() {
        bail!(
            "jina-api: dense index absent for this backend; build it with \
             wiki-agent semantic-index --backend jina-api --rebuild, or rollback: \
             WIKI_AGENT_SEMANTIC_BACKEND=hashed"
        );
    }
    if meta.dense_model.as_deref() != Some(jina.model.as_str())
        || meta.dense_dims != Some(jina.dims)
    {
        bail!(
            "jina-api: dense index incompatible (index model={} dims={:?}; configured \
             model={} dims={}); rebuild with --backend jina-api or rollback: \
             WIKI_AGENT_SEMANTIC_BACKEND=hashed",
            meta.dense_model.as_deref().unwrap_or(""),
            meta.dense_dims,
            jina.model,
            jina.dims
        );
    }
    Ok(())
}

fn embedding_cache_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("WIKI_JINA_CACHE_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(home) = std::env::var("WIKI_AGENT_HOME_RESOLVED")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home)
            .join("wiki-embedding-cache")
            .join("jina-api"));
    }
    let home =
        std::env::var("HOME").context("cannot locate the wiki home for the embedding cache")?;
    Ok(PathBuf::from(home)
        .join(".wiki-agent")
        .join("wiki-embedding-cache")
        .join("jina-api"))
}

fn recency_boost(chunk: &Chunk, settings: &Settings, now_ts: f64) -> f64 {
    if settings.recency_weight <= 0.0 || chunk.mtime <= 0.0 {
        return 0.0;
    }
    let age_days = (now_ts - chunk.mtime).max(0.0);
    settings.recency_weight * 0.5f64.powf(age_days / settings.recency_half_life_days)
}

/// The rrf scan-phase best-by-path bookkeeping: keep the highest-hash item
/// per path while tracking the path's best dense score and recency boost.
fn merge_best_by_path_rrf(
    best_by_path: &mut HashMap<String, usize>,
    candidates: &mut Vec<Candidate>,
    item: Candidate,
) {
    let Some(index) = best_by_path.get(&item.path).copied() else {
        best_by_path.insert(item.path.clone(), candidates.len());
        candidates.push(item);
        return;
    };
    let previous_hashed = candidates[index].hashed_score.unwrap_or(f64::NEG_INFINITY);
    let mut dense_of_path = candidates[index].dense_score;
    let mut recency_of_path = candidates[index].recency_boost;
    if item.hashed_score.unwrap_or(f64::NEG_INFINITY) > previous_hashed {
        let slot = &mut candidates[index];
        slot.id = item.id;
        slot.heading = item.heading;
        slot.heading_stack = item.heading_stack;
        slot.start_line = item.start_line;
        slot.end_line = item.end_line;
        slot.snippet = item.snippet;
        slot.score = item.score;
        slot.explain = item.explain;
        slot.hashed_score = item.hashed_score;
        slot.dense_score = dense_of_path;
        slot.recency_boost = item.recency_boost.max(recency_of_path);
        dense_of_path = slot.dense_score;
        recency_of_path = slot.recency_boost;
    }
    let slot = &mut candidates[index];
    if item.dense_score.is_some()
        && (slot.dense_score.is_none()
            || item.dense_score.unwrap_or(0.0) > slot.dense_score.unwrap_or(0.0))
    {
        slot.dense_score = item.dense_score;
    }
    if item.recency_boost > slot.recency_boost {
        slot.recency_boost = item.recency_boost;
    }
    let _ = (dense_of_path, recency_of_path);
}

/// The rrf fusion phase: rank lists over hashed and dense evidence, rank-sum
/// fusion, then the recency multiplier and the authority multiplier.
fn fuse_rrf(
    mut candidates: Vec<Candidate>,
    rrf_k: u32,
    authority_intent: bool,
    authority_ranking: bool,
) -> Vec<Candidate> {
    let mut lex_order: Vec<usize> = (0..candidates.len())
        .filter(|&index| candidates[index].hashed_score.unwrap_or(0.0) > 0.0)
        .collect();
    lex_order.sort_by(|&a, &b| {
        candidates[b]
            .hashed_score
            .partial_cmp(&candidates[a].hashed_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut dense_order: Vec<usize> = (0..candidates.len())
        .filter(|&index| candidates[index].dense_score.is_some())
        .collect();
    dense_order.sort_by(|&a, &b| {
        candidates[b]
            .dense_score
            .partial_cmp(&candidates[a].dense_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let lex_rank: HashMap<usize, usize> = lex_order
        .iter()
        .enumerate()
        .map(|(rank, &index)| (index, rank + 1))
        .collect();
    let dense_rank: HashMap<usize, usize> = dense_order
        .iter()
        .enumerate()
        .map(|(rank, &index)| (index, rank + 1))
        .collect();
    for (index, item) in candidates.iter_mut().enumerate() {
        let mut fused = 0.0;
        if let Some(rank) = lex_rank.get(&index) {
            fused += 1.0 / (rrf_k as f64 + *rank as f64);
        }
        if let Some(rank) = dense_rank.get(&index) {
            fused += 1.0 / (rrf_k as f64 + *rank as f64);
        }
        fused *= 1.0 + item.recency_boost;
        let explain = &mut item.explain;
        explain.insert(
            "denseRank".into(),
            json!(dense_rank.get(&index).copied().unwrap_or(0)),
        );
        explain.insert(
            "denseScore".into(),
            item.dense_score
                .map(round6)
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        );
        explain.insert("fusion".into(), json!("rrf"));
        explain.insert(
            "hashedScore".into(),
            json!(round6(item.hashed_score.unwrap_or(0.0))),
        );
        explain.insert(
            "lexRank".into(),
            json!(lex_rank.get(&index).copied().unwrap_or(0)),
        );
        if item.recency_boost > 0.0 {
            explain.insert("recencyBoost".into(), json!(round6(item.recency_boost)));
        }
        let tier = explain
            .get("authority")
            .and_then(|authority| authority.get("tier"))
            .and_then(Value::as_str)
            .unwrap_or("standard")
            .to_string();
        let multiplier = if authority_intent && authority_ranking {
            authority::multiplier_for(&tier)
        } else {
            1.0
        };
        explain.insert("authorityMultiplier".into(), json!(multiplier));
        if multiplier != 1.0 {
            explain.insert("preAuthorityScore".into(), json!(round6(fused)));
            fused *= multiplier;
        }
        item.score = round6(fused);
        item.hashed_score = None;
        item.dense_score = None;
        item.recency_boost = 0.0;
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

fn max_rss_kb() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage only fills the provided struct.
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        if result == 0 {
            return Some(usage.ru_maxrss.max(0) as u64);
        }
    }
    None
}

fn print_text_report<W: Write>(
    out: &mut W,
    options: &FindOptions,
    payload: &Value,
    matches: &[TextMatch],
) -> Result<()> {
    let semantic = payload.get("semantic").expect("payload carries semantic");
    let meta = semantic.get("index").cloned().unwrap_or_else(|| json!({}));
    writeln!(out, "find: {}", options.query)?;
    let built_at = meta.get("builtAt").and_then(Value::as_str).unwrap_or("");
    if !built_at.is_empty() {
        writeln!(
            out,
            "index built: {} chunks={} backend={}",
            built_at,
            meta.get("chunks")
                .map(Value::to_string)
                .unwrap_or_else(|| "null".into()),
            meta.get("backend")
                .and_then(Value::as_str)
                .unwrap_or("hashed"),
        )?;
    }
    if payload
        .get("abstained")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        writeln!(
            out,
            "low confidence: candidates suppressed confidence={:.4} reason={}",
            payload
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            payload
                .get("abstentionReason")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )?;
    }
    writeln!(out)?;
    writeln!(out, "Semantic candidates:")?;
    let results = semantic
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        writeln!(out, "  no semantic candidates")?;
    } else {
        for (index, item) in results.iter().enumerate() {
            writeln!(
                out,
                "{}. score={:.4} {}:{}-{}",
                index + 1,
                item.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                item.get("path").and_then(Value::as_str).unwrap_or(""),
                item.get("startLine").and_then(Value::as_u64).unwrap_or(0),
                item.get("endLine").and_then(Value::as_u64).unwrap_or(0),
            )?;
            let heading = item.get("heading").and_then(Value::as_str).unwrap_or("");
            if !heading.is_empty() {
                writeln!(out, "   {heading}")?;
            }
            writeln!(
                out,
                "   load: {}",
                item.get("loadCommand")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )?;
            let snippet = item.get("snippet").and_then(Value::as_str).unwrap_or("");
            if !snippet.is_empty() {
                writeln!(out, "   {snippet}")?;
            }
        }
    }
    writeln!(out)?;
    writeln!(out, "Text matches:")?;
    if matches.is_empty() {
        writeln!(out, "  no text matches")?;
    } else {
        for (index, item) in matches.iter().enumerate() {
            let marker = if item.also_semantic_path {
                " semantic-path"
            } else {
                ""
            };
            writeln!(
                out,
                "{}. rank={}{} {}:{}",
                index + 1,
                item.rank,
                marker,
                item.path,
                item.line,
            )?;
            writeln!(out, "   load: {}", item.load_command)?;
            writeln!(out, "   {}", item.text)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn splitlines_matches_the_python_boundary_set() {
        assert_eq!(py_splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\u{2028}b"), vec!["a", "b"]);
        assert_eq!(py_splitlines("trailing\n"), vec!["trailing"]);
        assert_eq!(py_splitlines("one"), vec!["one"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
    }

    #[test]
    fn fnmatch_crosses_path_separators_like_python() {
        let cache = Mutex::new(HashMap::new());
        let re = fnmatch_regex("pages/nodes/vps6/**", &cache);
        assert!(re.is_match("pages/nodes/vps6/deep/file.md"));
        assert!(!re.is_match("pages/nodes/other.md"));
        let question = fnmatch_regex("pages/log-?.md", &cache);
        assert!(question.is_match("pages/log-1.md"));
        assert!(!question.is_match("pages/log-12.md"));
        let class = fnmatch_regex("pages/[ab]*.md", &cache);
        assert!(class.is_match("pages/alpha.md"));
        assert!(!class.is_match("pages/gamma.md"));
        let negated = fnmatch_regex("pages/[!a]*.md", &cache);
        assert!(negated.is_match("pages/beta.md"));
        assert!(!negated.is_match("pages/alpha.md"));
    }

    #[test]
    fn lexical_and_bm25_scorers_match_the_reference_formulas() {
        let mut terms = BTreeMap::new();
        terms.insert("wiki".to_string(), 3.0f32);
        terms.insert("rust".to_string(), 1.0f32);
        let mut query = BTreeMap::new();
        query.insert("wiki".to_string(), 1u32);
        query.insert("rust".to_string(), 1u32);
        let stats = CorpusStats {
            total_docs: 100,
            avgdl: 20.0,
            term_df: BTreeMap::from([("wiki".to_string(), 50), ("rust".to_string(), 10)]),
        };
        let (score, matched) = bm25_score(&query, &terms, &stats);
        // Hand-computed reference value (python):
        // idf(wiki)=ln(1+50.5/50.5)=ln2, idf(rust)=ln(1+90.5/10.5)
        // tfc(wiki)=3*2.2/(3+1.2*0.4), tfc(rust)=2.2/(1+1.2*0.4)
        let expected = (2.0f64.ln() * (3.0 * 2.2) / (3.0 + 1.2 * (1.0 - 0.75 + 0.75 * 4.0 / 20.0))
            + (1.0 + (100.0f64 - 10.0 + 0.5) / (10.0 + 0.5)).ln() * (1.0 * 2.2)
                / (1.0 + 1.2 * (1.0 - 0.75 + 0.75 * 4.0 / 20.0)))
            * 1.1
            * 0.08;
        assert_eq!(matched, 2);
        assert!(
            (score - expected.min(0.45)).abs() < 1e-12,
            "{score} vs {expected}"
        );
        let (lexical, matched) = lexical_score(&query, &terms);
        // overlap = (1+ln1)(1+ln3) + (1+ln1)(1+ln1); coverage 1.0
        let expected_lexical = 0.05 * ((1.0 + 3.0f64.ln()) + 1.0) + 0.20 * 1.0;
        assert_eq!(matched, 2);
        assert!((lexical - expected_lexical.min(0.35)).abs() < 1e-12);
    }

    #[test]
    fn sparse_dot_uses_the_smaller_side() {
        let query = BTreeMap::from([("1".to_string(), 0.5), ("2".to_string(), 0.25)]);
        let record = BTreeMap::from([("2".to_string(), 4.0f32)]);
        assert!((dot(&query, &record) - 1.0).abs() < 1e-12);
        assert!((dense_dot(&[0.5, 0.5], &[2.0f32, 4.0]) - 3.0).abs() < 1e-12);
    }

    // --- slice-2 contract: the whole find pass over a tiny fixture index ---

    /// A minimal on-disk index shaped like the real one: meta v3 with the
    /// calibrated vectorizer identity, integer `split`, hashed sparse
    /// vectors, one cached page for the exact-text fallback, and no
    /// aliases.md (alias loading must fail open).
    fn write_find_fixture(index: &Path, cache: &Path) {
        std::fs::create_dir_all(index).expect("index dir");
        std::fs::write(
            index.join("meta.json"),
            r#"{"version":3,"backend":"hashed","dims":2048,"chunks":2,"files":2,"builtAt":"2026-09-27T00:00:00Z","builtOn":"test","vectorizer":"hashed-v3-d2048"}"#,
        )
        .expect("meta");
        std::fs::write(
            index.join("manifest.jsonl"),
            concat!(
                r#"{"path":"pages/a.md","fileHash":"hash-a","mtime":1758900000.0,"bytes":120,"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"2026-09-27T00:00:00Z"}"#,
                "\n",
                r#"{"path":"pages/b.md","fileHash":"hash-b","mtime":1758900001.0,"bytes":90,"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"2026-09-27T00:00:00Z"}"#,
                "\n",
            ),
        )
        .expect("manifest");
        std::fs::write(
            index.join("chunks.jsonl"),
            concat!(
                r#"{"id":"a-0","path":"pages/a.md","startLine":1,"endLine":6,"heading":"Session keys","headingStack":["Session keys"],"level":1,"split":0,"bytes":120,"mtime":1758900000.0,"fileHash":"hash-a","snippet":"session key rotation policy, API_KEY = sk-abcdefghijklmnopqrst inside","terms":{"session":2.0,"key":2.0,"rotation":1.0,"policy":1.0},"vector":{"11":0.25}}"#,
                "\n",
                r#"{"id":"b-0","path":"pages/b.md","startLine":3,"endLine":4,"heading":"Bread","headingStack":["Bread"],"level":1,"split":1,"bytes":90,"mtime":1758900001.0,"fileHash":"hash-b","snippet":"bread baking recipes","terms":{"bread":1.0,"baking":1.0},"vector":{}}"#,
                "\n",
            ),
        )
        .expect("chunks");
        std::fs::create_dir_all(cache.join("pages")).expect("cache tree");
        std::fs::write(
            cache.join("pages/a.md"),
            "# Session keys\n\nrotate the session key every 30 days\nAPI_KEY = sk-abcdefghijklmnopqrst\n",
        )
        .expect("cached page");
        std::fs::write(cache.join("pages/b.md"), "bread baking recipes\n").expect("cached page");
        crate::cache::build(index, cache).expect("cache build");
    }

    fn find_options(query: &str, index: &Path, cache: &Path) -> FindOptions {
        FindOptions {
            query: query.to_string(),
            index_dir: index.to_path_buf(),
            cache_dir: cache.to_path_buf(),
            json: false,
            refresh: false,
            // The default abstention gate depends on operator env; the
            // ranking and masking tests pin their own decision explicitly.
            abstention: Some(false),
            top: 5,
            grep_top: 8,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    fn find_fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let index = temp.path().join("index");
        let cache = temp.path().join("cache");
        write_find_fixture(&index, &cache);
        (temp, index, cache)
    }

    #[test]
    fn find_ranks_the_matching_page_and_masks_its_output() {
        let (_temp, index, cache) = find_fixture();
        let found =
            report(&find_options("session key rotation", &index, &cache)).expect("find report");
        assert_eq!(
            found.payload.get("schema").and_then(Value::as_str),
            Some("wiki-agent-find-v1")
        );
        assert_eq!(
            found.payload.get("query").and_then(Value::as_str),
            Some("session key rotation")
        );
        let results = found
            .payload
            .pointer("/semantic/results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !results.is_empty(),
            "a term-exact query must surface candidates: {:#}",
            found.payload
        );
        assert_eq!(
            results[0].get("path").and_then(Value::as_str),
            Some("pages/a.md"),
            "the chunk carrying the query terms ranks first"
        );
        assert_eq!(
            results[0].get("loadCommand").and_then(Value::as_str),
            Some("wiki-agent load --lines 1:6 pages/a.md"),
            "every result carries its verification command"
        );
        // Decision 2b: the read path masks its output; the raw index fields
        // feed scoring untouched.
        let snippet = results[0]
            .get("snippet")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(snippet.contains("API_KEY=[REDACTED]"), "{snippet}");
        assert!(!snippet.contains("sk-abc"), "{snippet}");
    }

    #[test]
    fn find_text_fallback_matches_the_read_cache_and_masks() {
        let (_temp, index, cache) = find_fixture();
        // The query token only reaches the indexed terms through the split
        // bits (`api_key` → `key`), so the exact-text pass is what proves the
        // literal line evidence — and it must be masked like everything else.
        let found = report(&find_options("API_KEY", &index, &cache)).expect("find report");
        assert_eq!(found.text_matches.len(), 1, "{:?}", found.text_matches);
        let line = &found.text_matches[0];
        assert_eq!(line.path, "pages/a.md");
        assert_eq!(line.line, 4);
        assert_eq!(
            line.rank, 3,
            "the literal query substring outranks term hits"
        );
        assert_eq!(
            line.text, "API_KEY=[REDACTED]",
            "the cached line is masked too"
        );
        assert_eq!(line.load_command, "wiki-agent load --lines 4:4 pages/a.md");
    }

    #[test]
    fn find_include_globs_bound_both_candidate_lists() {
        let (_temp, index, cache) = find_fixture();

        // `pages/a*` admits pages/a.md only, so the bread page disappears
        // from both candidate lists.
        let mut options = find_options("bread", &index, &cache);
        options.include = vec!["pages/a*".to_string()];
        let found = report(&options).expect("find report");
        assert!(
            found
                .payload
                .pointer("/semantic/results")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty),
            "pages/b is filtered out of the scan: {:#}",
            found.payload
        );
        assert!(
            found.text_matches.is_empty(),
            "the filter bounds the text pass too"
        );

        // The admitting glob surfaces both lists for the same query.
        let mut options = find_options("bread", &index, &cache);
        options.include = vec!["pages/b*".to_string()];
        let found = report(&options).expect("find report");
        assert_eq!(
            found
                .payload
                .pointer("/semantic/results/0/path")
                .and_then(Value::as_str),
            Some("pages/b.md")
        );
        assert_eq!(found.text_matches.len(), 1);
        assert_eq!(found.text_matches[0].path, "pages/b.md");
        assert_eq!(found.text_matches[0].line, 1);
    }

    #[test]
    fn find_abstention_flag_flows_into_the_decision() {
        let (_temp, index, cache) = find_fixture();
        // 1 of 3 query terms matches: calibrated coverage is below full
        // confidence, so an enabled gate must suppress both lists.
        let mut options = find_options("session gardening workshops", &index, &cache);
        options.abstention = Some(true);
        let found = report(&options).expect("find report");
        assert_eq!(
            found.payload.get("abstained").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            found
                .payload
                .pointer("/abstention/reason")
                .and_then(Value::as_str),
            Some("insufficient-cross-signal-evidence")
        );
        assert_eq!(
            found
                .payload
                .pointer("/abstention/calibrated")
                .and_then(Value::as_bool),
            Some(true),
            "hashed + hashed-v3-d2048 is the calibrated arm"
        );
        assert!(
            found
                .payload
                .pointer("/semantic/results")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty),
            "abstention suppresses the candidate list"
        );
        assert!(
            found.text_matches.is_empty(),
            "abstention suppresses the text fallback"
        );

        let mut options = find_options("session gardening workshops", &index, &cache);
        options.abstention = Some(false);
        let found = report(&options).expect("find report");
        assert_eq!(
            found.payload.get("abstained").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            found
                .payload
                .pointer("/abstention/reason")
                .and_then(Value::as_str),
            Some("disabled")
        );
        assert_eq!(
            found
                .payload
                .pointer("/semantic/results/0/path")
                .and_then(Value::as_str),
            Some("pages/a.md"),
            "with the gate off, the weak match is still reported"
        );
    }
}
