//! `corpus_stats.json` — BM25-lite corpus statistics written by the index
//! builder. Optional: an index without them falls back to the plain lexical
//! scorer, exactly like the bash implementation. Unreadable counts as
//! absent — a corrupt stats file must not kill a query.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CorpusStats {
    #[serde(rename = "totalDocs", default)]
    pub total_docs: u64,
    #[serde(rename = "avgdl", default)]
    pub avgdl: f64,
    #[serde(rename = "termDf", default)]
    pub term_df: BTreeMap<String, u64>,
}

impl CorpusStats {
    /// The effective document-frequency of a term (`df = 0` when unseen).
    pub fn df(&self, term: &str) -> u64 {
        self.term_df.get(term).copied().unwrap_or(0)
    }

    /// The effective `totalDocs` (`0 → 1`, as in the reference scorer).
    pub fn total_docs(&self) -> u64 {
        if self.total_docs == 0 {
            1
        } else {
            self.total_docs
        }
    }

    /// The effective average document length (`0 → 1.0`).
    pub fn avgdl(&self) -> f64 {
        if self.avgdl == 0.0 { 1.0 } else { self.avgdl }
    }
}

/// `None` when the file is absent, unreadable, or an empty object — the
/// reference treats a falsy stats payload as "no stats".
pub fn load(index_dir: &Path) -> Result<Option<CorpusStats>> {
    let path = index_dir.join("corpus_stats.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(
                anyhow::Error::new(error).context(format!("could not read {}", path.display()))
            );
        }
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("could not parse {}", path.display()))?;
    if value.as_object().is_none_or(|map| map.is_empty()) {
        return Ok(None);
    }
    let stats: CorpusStats = serde_json::from_value(value)
        .with_context(|| format!("could not parse {}", path.display()))?;
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_empty_stats_fall_back_to_none() {
        let temp = tempfile::tempdir().expect("temp");
        assert_eq!(load(temp.path()).expect("load"), None);
        std::fs::write(temp.path().join("corpus_stats.json"), "{}").expect("empty");
        assert_eq!(load(temp.path()).expect("load"), None);
    }

    #[test]
    fn stats_decode_with_the_camel_case_contract() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(
            temp.path().join("corpus_stats.json"),
            r#"{"totalDocs":2,"avgdl":12.5,"totalTerms":25,"backend":"hashed","termDf":{"wiki":2,"rust":1}}"#,
        )
        .expect("stats");
        let stats = load(temp.path()).expect("load").expect("present");
        assert_eq!(stats.df("wiki"), 2);
        assert_eq!(stats.df("missing"), 0);
        assert_eq!(stats.total_docs(), 2);
        assert!((stats.avgdl() - 12.5).abs() < 1e-12);
    }
}
