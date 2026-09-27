//! Read-only reader for the wiki-agent semantic index
//! (`docs/unified-design.md` §3.1 `danso-wiki`, issue #121 PR slice 1).
//!
//! Charter: parse `meta.json` / `manifest.jsonl` / `chunks.jsonl` exactly as
//! the (bash) wiki-agent writes them, keep a local pre-parsed `index.cache`
//! so a query does not re-parse the 143 MB JSONL, and report status. This
//! crate never builds an index, never writes into the synced wiki cache, and
//! never touches the network. Termux is a supported host: no subprocesses, no
//! Python, std fs only.

pub mod abstention;
pub mod alias;
pub mod authority;
pub mod cache;
pub mod chunk;
pub mod corpus;
pub mod embedding;
pub mod find;
pub mod manifest;
pub mod meta;
mod preserve;
pub mod status;
pub mod text;

/// The only index layout this build reads. Both spellings of the version must
/// carry it — `meta.json` calls it `version`, `manifest.jsonl` calls it
/// `indexVersion` (issue #121 decision 3) — and anything else is fail-closed:
/// a reader that guesses at an index it does not understand would hand queries
/// confidently wrong answers.
pub const SUPPORTED_INDEX_VERSION: u32 = 3;
