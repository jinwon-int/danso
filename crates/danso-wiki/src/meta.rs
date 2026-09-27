//! `meta.json` — what built the index and how big it is.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::SUPPORTED_INDEX_VERSION;
use crate::preserve;

/// The `meta.json` record. On-disk keys are camelCase (`builtAt`,
/// `skippedSecretChunks`, …) and that spelling is the contract. Unknown keys
/// are preserved (`extra`) so a future writer's fields survive a parse →
/// cache → query round trip without this crate having to know them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexMeta {
    pub version: u32,
    pub backend: String,
    #[serde(default)]
    pub dims: Option<u32>,
    #[serde(default)]
    pub dense_dims: Option<u32>,
    #[serde(default)]
    pub dense_model: Option<String>,
    #[serde(default)]
    pub vectorizer: Option<String>,
    pub chunks: u64,
    pub files: u64,
    pub built_at: String,
    #[serde(default)]
    pub built_on: Option<String>,
    #[serde(default)]
    pub skipped_secret_chunks: Option<u64>,
    /// Keys this crate does not model, as JSON text. Filled by the reader,
    /// not serde: neither `serde(flatten)` nor `serde_json::Value` survives
    /// a bincode decode (see [`preserve`]).
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

const KNOWN_KEYS: &[&str] = &[
    "version",
    "backend",
    "dims",
    "denseDims",
    "denseModel",
    "vectorizer",
    "chunks",
    "files",
    "builtAt",
    "builtOn",
    "skippedSecretChunks",
];

impl IndexMeta {
    /// Parse and fail closed on any layout this build cannot read.
    pub fn load(path: &Path) -> Result<IndexMeta> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("could not parse {}", path.display()))?;
        let extra =
            preserve::preserve(&value, KNOWN_KEYS).with_context(|| path.display().to_string())?;
        let mut meta: IndexMeta = serde_json::from_value(value)
            .with_context(|| format!("could not parse {}", path.display()))?;
        meta.extra = extra;
        if meta.version != SUPPORTED_INDEX_VERSION {
            bail!(
                "unsupported wiki index version {} in {} (this build reads {}); \
                 the reader never builds an index — rebuild the index with the \
                 writer that matches this reader",
                meta.version,
                path.display(),
                SUPPORTED_INDEX_VERSION
            );
        }
        Ok(meta)
    }
}
