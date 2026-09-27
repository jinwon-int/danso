//! The jina query-embedding cache (issue #121 decision 1b): the read path
//! reuses `wiki-embedding-cache` entries the bash implementation wrote, so a
//! warm cache means zero network. There is deliberately no fetch here — this
//! crate never touches the network, so a cache miss is fail-closed with a
//! named remedy instead of a silent dense-score collapse.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The subset of `jina_config()` the read path needs.
#[derive(Debug, Clone, PartialEq)]
pub struct JinaConfig {
    pub model: String,
    pub dims: u32,
    /// `retrieval.query` — the asymmetric query role/task.
    pub query_task: String,
    pub cache_dir: PathBuf,
}

impl JinaConfig {
    /// Defaults mirror `jina_config()`; `cache_dir` comes from the caller
    /// (`WIKI_JINA_CACHE_DIR` or `<wiki home>/wiki-embedding-cache/jina-api`).
    /// Numeric environment values fail closed — a garbled dims value must
    /// never silently fall back to a different vector space.
    pub fn from_env(cache_dir: PathBuf) -> Result<JinaConfig> {
        let dims = match std::env::var("WIKI_JINA_EMBED_DIMS") {
            Ok(raw) => raw.trim().parse::<u32>().with_context(|| {
                format!("WIKI_JINA_EMBED_DIMS must be a positive integer, got {raw:?}")
            })?,
            Err(_) => 1024,
        };
        let task = env_or("WIKI_JINA_EMBED_TASK", "retrieval");
        Ok(JinaConfig {
            model: env_or("WIKI_JINA_EMBED_MODEL", "jina-embeddings-v5-text-small"),
            dims,
            query_task: env_or("WIKI_JINA_QUERY_TASK", format!("{task}.query")),
            cache_dir,
        })
    }
}

fn env_or(name: &str, default: impl Into<String>) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.into())
}

/// `jina_cache_key`: sha256 over `model \0 dims \0 role_task \0 text`.
pub fn cache_key(config: &JinaConfig, text: &str) -> String {
    let raw = format!(
        "{}\0{}\0{}\0{}",
        config.model, config.dims, config.query_task, text
    );
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Deserialize)]
struct CachedEmbedding {
    vector: Option<Vec<f32>>,
}

/// Read one cached query embedding. `Ok(None)` — no usable entry for this
/// key (absent, malformed, or wrong dimensionality): callers fail closed.
pub fn cached_query_vector(config: &JinaConfig, text: &str) -> Result<Option<Vec<f32>>> {
    let path: PathBuf = config
        .cache_dir
        .join(format!("{}.json", cache_key(config, text)));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(
                anyhow::Error::new(error).context(format!("could not read {}", path.display()))
            );
        }
    };
    let cached: CachedEmbedding = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not a readable embedding cache entry", path.display()))?;
    let vector = cached.vector.unwrap_or_default();
    if vector.len() as u32 != config.dims {
        return Ok(None);
    }
    Ok(Some(vector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(dir: &Path) -> JinaConfig {
        JinaConfig {
            model: "jina-embeddings-v5-text-small".into(),
            dims: 1024,
            query_task: "retrieval.query".into(),
            cache_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn cache_key_matches_the_reference_construction() {
        // python: hashlib.sha256(b"m\x001024\x00retrieval.query\x00세션").hexdigest()
        let config = JinaConfig {
            model: "m".into(),
            dims: 1024,
            query_task: "retrieval.query".into(),
            cache_dir: PathBuf::new(),
        };
        let key = cache_key(&config, "세션");
        assert_eq!(key.len(), 64);
        // Computed with the reference formula.
        assert_eq!(
            key,
            "8dccb5ac0e6c50b27edfa5d941607a2c0c634db15de058ccf7dcc4154333d7ea"
        );
    }

    #[test]
    fn reads_a_cached_vector_and_rejects_wrong_dimensionality() {
        let temp = tempfile::tempdir().expect("temp");
        let config = config(temp.path());
        let key = cache_key(&config, "q");
        std::fs::write(
            temp.path().join(format!("{key}.json")),
            r#"{"model":"jina-embeddings-v5-text-small","dims":1024,"task":"retrieval.query","vector":[1.0,0.5]}"#,
        )
        .expect("cache entry");
        // dims mismatch (2 != 1024) → treated as absent.
        assert_eq!(cached_query_vector(&config, "q").expect("read"), None);
        std::fs::write(
            temp.path().join(format!("{key}.json")),
            r#"{"vector":[0.25]}"#,
        )
        .expect("rewrite");
        // Still a mismatch; then a correct entry reads back.
        assert_eq!(cached_query_vector(&config, "q").expect("read"), None);
        std::fs::write(
            temp.path().join(format!("{key}.json")),
            format!(r#"{{"vector":[{}]}}"#, vec!["0.1"; 1024].join(",")),
        )
        .expect("rewrite");
        assert_eq!(
            cached_query_vector(&config, "q")
                .expect("read")
                .map(|v| v.len()),
            Some(1024)
        );
    }

    #[test]
    fn missing_entry_is_none_not_an_error() {
        let temp = tempfile::tempdir().expect("temp");
        assert_eq!(
            cached_query_vector(&config(temp.path()), "nope").expect("read"),
            None
        );
    }
}
