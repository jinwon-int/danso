//! Unknown-key preservation without `serde(flatten)`.
//!
//! `serde(flatten)` and bincode do not compose: a flattened map serializes
//! through a buffering layer whose length bincode cannot know ahead of time.
//! `serde_json::Value` does not help either — decoding one asks the input for
//! `deserialize_any`, which bincode refuses, so a cache holding Values could
//! be written but never read back. The on-disk formats also evolve
//! independently of this reader, so dropping unknown keys is not an option —
//! the cache must round-trip them or a query would answer from a quietly
//! lossy copy of the index. Each parse therefore keeps the keys this crate
//! does not model explicitly in a plain string-valued map (each value as
//! JSON text), which bincode encodes and decodes like any other map.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

/// Copy every key not in `known` out of a parsed record, each value as JSON
/// text.
pub(crate) fn preserve(
    value: &serde_json::Value,
    known: &[&str],
) -> Result<BTreeMap<String, String>> {
    let map = match value {
        serde_json::Value::Object(map) => map,
        _ => bail!("record is not a JSON object"),
    };
    map.iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| {
            let text = serde_json::to_string(value)
                .context("preserved value is not representable as JSON text")?;
            Ok((key.clone(), text))
        })
        .collect()
}
