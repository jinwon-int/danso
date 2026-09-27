//! `index.cache` — the local pre-parsed index.
//!
//! Re-parsing ~143 MB of JSONL on every query is the single biggest cost of
//! the bash read path. The cache stores the parsed form once per index
//! version and is keyed on what can change underneath it: the index's own
//! `built_at` and the byte sizes of the three source files (an rsync can
//! replace files without a rebuild). A key mismatch means stale, and stale
//! means rebuild — never guess.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::chunk;
use crate::chunk::Chunk;
use crate::manifest::{self, ManifestEntry};
use crate::meta::IndexMeta;

/// Bump when the cached representation changes shape. Old caches with a
/// different `format` are stale by definition and rebuilt, not decoded.
pub const CACHE_FORMAT: u32 = 1;

const CACHE_FILE_NAME: &str = "index.cache";

/// What makes a cached parse reusable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheKey {
    pub format: u32,
    pub built_at: String,
    pub meta_bytes: u64,
    pub manifest_bytes: u64,
    pub chunks_bytes: u64,
}

/// The parsed index, ready for queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedIndex {
    pub key: CacheKey,
    pub meta: IndexMeta,
    pub manifest: Vec<ManifestEntry>,
    pub chunks: Vec<Chunk>,
}

/// Where the cache lives: under the node-local cache directory, never inside
/// the synced index directory (an rsync with `--delete` would eat it).
pub fn cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(CACHE_FILE_NAME)
}

/// The key the current source files produce.
pub fn source_key(index_dir: &Path) -> Result<CacheKey> {
    let meta_path = index_dir.join("meta.json");
    let manifest_path = index_dir.join("manifest.jsonl");
    let chunks_path = index_dir.join("chunks.jsonl");
    let meta = IndexMeta::load(&meta_path)?;
    Ok(CacheKey {
        format: CACHE_FORMAT,
        built_at: meta.built_at,
        meta_bytes: file_size(&meta_path)?,
        manifest_bytes: file_size(&manifest_path)?,
        chunks_bytes: file_size(&chunks_path)?,
    })
}

/// Decode an existing cache. Fails if it is absent, unreadable, or not a
/// cache this build understands — callers treat an error as stale.
pub fn load(cache_dir: &Path) -> Result<CachedIndex> {
    let path = cache_path(cache_dir);
    let raw = std::fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
    bincode::deserialize::<CachedIndex>(&raw)
        .with_context(|| format!("{} is not a readable index cache", path.display()))
}

/// Parse the source files and write the cache (0600, atomic rename). The
/// cache directory is created owner-only if missing, and refused if it
/// already exists with wider permissions — a query cache holds wiki content
/// and belongs to the operator.
pub fn build(index_dir: &Path, cache_dir: &Path) -> Result<CachedIndex> {
    let key = source_key(index_dir)?;
    let meta = IndexMeta::load(&index_dir.join("meta.json"))?;
    let manifest = manifest::load(&index_dir.join("manifest.jsonl"))?;
    let chunks = chunk::load(&index_dir.join("chunks.jsonl"))?;
    let cached = CachedIndex {
        key,
        meta,
        manifest,
        chunks,
    };
    ensure_private_dir(cache_dir)?;
    let path = cache_path(cache_dir);
    let tmp = cache_dir.join(format!("{}.tmp.{}", CACHE_FILE_NAME, std::process::id()));
    let encoded = bincode::serialize(&cached).context("could not encode the index cache")?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("could not create {}", tmp.display()))?;
        file.write_all(&encoded)
            .with_context(|| format!("could not write {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("could not put {} in place", path.display()))?;
    Ok(cached)
}

/// Read the cache and say whether it matches the current sources. Decodes the
/// whole file — status runs occasionally, queries go through `load`, so this
/// is not the hot path.
pub fn inspect(index_dir: &Path, cache_dir: &Path) -> Result<CacheState> {
    let path = cache_path(cache_dir);
    if !path.exists() {
        return Ok(CacheState::Absent);
    }
    let current = source_key(index_dir)?;
    match load(cache_dir) {
        Ok(cached) if cached.key == current => Ok(CacheState::Fresh),
        Ok(cached) => Ok(CacheState::Stale {
            cached: cached.key,
            current,
        }),
        Err(error) => Ok(CacheState::Invalid {
            error: format!("{error:#}"),
        }),
    }
}

/// Whether a usable cache exists, and if not, why not.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CacheState {
    /// No cache file yet — the first query builds it.
    Absent,
    /// Decoded and the key matches the current sources.
    Fresh,
    /// Decoded but built against different sources.
    Stale { cached: CacheKey, current: CacheKey },
    /// Present but not decodable by this build.
    Invalid { error: String },
}

fn file_size(path: &Path) -> Result<u64> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("could not stat {}", path.display()))?;
    Ok(metadata.len())
}

/// Create `dir` as 0700, or accept it only if it already is 0700. Never
/// widens permissions: the query cache holds wiki content and belongs to the
/// operator.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                anyhow::bail!("{} exists and is not a directory", dir.display());
            }
            let mode = metadata.permissions().mode() & 0o777;
            if mode != 0o700 {
                anyhow::bail!(
                    "{} exists with mode {:o}; the query cache needs 0700",
                    dir.display(),
                    mode
                );
            }
            Ok(())
        }
        Err(_) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("could not restrict {}", dir.display()))?;
            Ok(())
        }
    }
}
