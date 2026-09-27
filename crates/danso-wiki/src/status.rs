//! `danso wiki status` — read-only report on the installed wiki index and the
//! local query cache.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::cache::{self, CacheState};
use crate::manifest;
use crate::meta::IndexMeta;

/// Everything status knows, in one serializable report.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub index_dir: String,
    pub cache_dir: String,
    pub meta: IndexMeta,
    pub manifest_entries: usize,
    pub chunks_file_bytes: u64,
    pub cache: CacheState,
    pub cache_files: CacheFileCensus,
}

/// How many of the manifest's files are actually present under the local
/// cache directory. Presence only — status never hashes a cache tree (that is
/// `sync`'s contract), and a missing file is worth knowing about without
/// paying a full digest pass.
#[derive(Debug, Serialize)]
pub struct CacheFileCensus {
    pub present: u64,
    pub missing: u64,
    /// Up to five missing paths, for the operator's terminal.
    pub missing_samples: Vec<String>,
}

/// Gather the report. Fails closed on an index this build cannot read; a
/// missing or broken local cache is a *report*, not an error.
pub fn status(index_dir: &Path, cache_dir: &Path) -> Result<StatusReport> {
    let meta = IndexMeta::load(&index_dir.join("meta.json"))?;
    let entries = manifest::load(&index_dir.join("manifest.jsonl"))?;
    let chunks_path = index_dir.join("chunks.jsonl");
    let chunks_file_bytes = std::fs::metadata(&chunks_path)
        .with_context(|| format!("could not stat {}", chunks_path.display()))?
        .len();
    let cache_state = cache::inspect(index_dir, cache_dir)?;
    let cache_files = census(cache_dir, &entries);

    Ok(StatusReport {
        index_dir: index_dir.display().to_string(),
        cache_dir: cache_dir.display().to_string(),
        meta,
        manifest_entries: entries.len(),
        chunks_file_bytes,
        cache: cache_state,
        cache_files,
    })
}

impl StatusReport {
    /// The operator-facing text rendering.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("wiki index: {}\n", self.index_dir));
        let dims = match self.meta.dims {
            Some(d) => d.to_string(),
            None => "unknown".to_string(),
        };
        let dense_model = self.meta.dense_model.as_deref().unwrap_or("none");
        out.push_str(&format!(
            "  version {} backend {} dims {dims} dense model {dense_model} chunks {} files {} built {}{}\n",
            self.meta.version,
            self.meta.backend,
            self.meta.chunks,
            self.meta.files,
            self.meta.built_at,
            self.meta
                .built_on
                .as_deref()
                .map(|on| format!(" on {on}"))
                .unwrap_or_default(),
        ));
        out.push_str(&format!(
            "  manifest entries: {}\n  chunks.jsonl: {} bytes\n",
            self.manifest_entries, self.chunks_file_bytes
        ));
        match &self.cache {
            CacheState::Absent => out.push_str("  cache: absent (the first query builds it)\n"),
            CacheState::Fresh => out.push_str("  cache: fresh\n"),
            CacheState::Stale { .. } => {
                out.push_str("  cache: stale (sources changed; rebuilt on the next query)\n")
            }
            CacheState::Invalid { error } => out.push_str(&format!("  cache: invalid ({error})\n")),
        }
        let census = &self.cache_files;
        out.push_str(&format!(
            "  cache-dir files: {}/{} present\n",
            census.present,
            census.present + census.missing
        ));
        if !census.missing_samples.is_empty() {
            out.push_str(&format!(
                "  missing: {}\n",
                census.missing_samples.join(", ")
            ));
        }
        out
    }
}

fn census(cache_dir: &Path, entries: &[manifest::ManifestEntry]) -> CacheFileCensus {
    let mut present = 0;
    let mut missing = 0;
    let mut missing_samples = Vec::new();
    for entry in entries {
        if cache_dir.join(&entry.path).is_file() {
            present += 1;
        } else {
            missing += 1;
            if missing_samples.len() < 5 {
                missing_samples.push(entry.path.clone());
            }
        }
    }
    CacheFileCensus {
        present,
        missing,
        missing_samples,
    }
}
