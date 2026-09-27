//! `chunks.jsonl` — the index itself: one record per chunk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use crate::preserve;

/// One chunk record. Field names follow the on-disk format exactly
/// (`startLine`, `fileHash`, `denseVector`, … — camelCase, `rename_all`)
/// because the cache round-trips the parsed form. Unknown keys are preserved
/// (`extra`) so a future writer's fields survive without this crate knowing
/// them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub id: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    #[serde(default)]
    pub heading: String,
    #[serde(default)]
    pub heading_stack: Vec<String>,
    #[serde(default)]
    pub level: Option<u32>,
    /// The writer emits this as a small integer (0–5 in the real index), not
    /// the string the first draft assumed — a reader that only ever parsed
    /// synthetic fixtures would fail-closed on every real chunk.
    #[serde(default)]
    pub split: Option<u32>,
    pub bytes: u64,
    pub mtime: f64,
    pub file_hash: String,
    pub snippet: String,
    /// Sparse lexical weights: `term → weight`.
    #[serde(default)]
    pub terms: BTreeMap<String, f32>,
    /// Sparse hashed vector: `dim → weight`.
    #[serde(default)]
    pub vector: BTreeMap<String, f32>,
    #[serde(default)]
    pub dense_vector: Option<Vec<f32>>,
    #[serde(default)]
    pub dense_backend: Option<String>,
    #[serde(default)]
    pub dense_model: Option<String>,
    #[serde(default)]
    pub dense_dims: Option<u32>,
    #[serde(default)]
    pub dense_task: Option<String>,
    #[serde(default)]
    pub dense_input_role: Option<String>,
    /// Keys this crate does not model, as JSON text. Filled by the reader,
    /// not serde: neither `serde(flatten)` nor `serde_json::Value` survives
    /// a bincode decode (see `preserve`).
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

const KNOWN_KEYS: &[&str] = &[
    "id",
    "path",
    "startLine",
    "endLine",
    "heading",
    "headingStack",
    "level",
    "split",
    "bytes",
    "mtime",
    "fileHash",
    "snippet",
    "terms",
    "vector",
    "denseVector",
    "denseBackend",
    "denseModel",
    "denseDims",
    "denseTask",
    "denseInputRole",
];

fn parse_line(line: &str, path: &Path, number: usize) -> Result<Chunk> {
    let value: serde_json::Value = serde_json::from_str(line)
        .with_context(|| format!("{} line {number}: malformed chunk record", path.display()))?;
    let extra = preserve::preserve(&value, KNOWN_KEYS).with_context(|| format!("line {number}"))?;
    let mut chunk: Chunk = serde_json::from_value(value)
        .with_context(|| format!("{} line {number}: malformed chunk record", path.display()))?;
    chunk.extra = extra;
    Ok(chunk)
}

/// Parse the whole chunk file. This is the call the bash implementation pays
/// on every query (a full `json.loads` sweep of ~143 MB); callers go through
/// [`crate::cache`] so it is paid once per index instead.
pub fn load(path: &Path) -> Result<Vec<Chunk>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not read {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut chunks = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("could not read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        chunks.push(parse_line(&line, path, index + 1)?);
    }
    Ok(chunks)
}
