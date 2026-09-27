//! `manifest.jsonl` — one record per indexed file.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use crate::SUPPORTED_INDEX_VERSION;
use crate::preserve;

/// One `manifest.jsonl` line: what was indexed and under which digest, so a
/// reader can tell which cache files are stale without building anything.
/// On-disk keys are camelCase (`fileHash`, `indexVersion`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub path: String,
    pub file_hash: String,
    pub mtime: f64,
    pub bytes: u64,
    pub chunks: u64,
    pub backend: String,
    #[serde(default)]
    pub dense_model: Option<String>,
    pub index_version: u32,
    pub indexed_at: String,
    /// Keys this crate does not model, as JSON text, preserved through the
    /// cache (see `preserve`).
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

const KNOWN_KEYS: &[&str] = &[
    "path",
    "fileHash",
    "mtime",
    "bytes",
    "chunks",
    "backend",
    "denseModel",
    "indexVersion",
    "indexedAt",
];

fn parse_line(line: &str, path: &Path, number: usize) -> Result<ManifestEntry> {
    let value: serde_json::Value = serde_json::from_str(line).with_context(|| {
        format!(
            "{} line {number}: malformed manifest record",
            path.display()
        )
    })?;
    let extra = preserve::preserve(&value, KNOWN_KEYS).with_context(|| format!("line {number}"))?;
    let mut entry: ManifestEntry = serde_json::from_value(value).with_context(|| {
        format!(
            "{} line {number}: malformed manifest record",
            path.display()
        )
    })?;
    entry.extra = extra;
    if entry.index_version != SUPPORTED_INDEX_VERSION {
        bail!(
            "{} line {number}: unsupported indexVersion {} (this build reads {})",
            path.display(),
            entry.index_version,
            SUPPORTED_INDEX_VERSION
        );
    }
    Ok(entry)
}

/// Parse the whole manifest. Blank lines are skipped; a malformed line is a
/// hard error that names the line — silently dropping a manifest entry would
/// make a stale file look fresh.
pub fn load(path: &Path) -> Result<Vec<ManifestEntry>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not read {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("could not read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        entries.push(parse_line(&line, path, index + 1)?);
    }
    Ok(entries)
}
