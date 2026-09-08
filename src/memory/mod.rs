//! Native local long-term memory (issue #52): storage, search, and — from
//! M2 — bounded context injection, reimplementing the audited ccc-node
//! local-memory contract in Rust with no Python/Node/Shell runtime
//! dependency.
//!
//! M1 scope (§10): owner-only paths and scope routing (`paths`), the
//! injection scanner (`scan`), fact records and the six write gates
//! (`facts`), the in-process recall index with valid-time and NL as-of
//! semantics (`recall`), and the built-in golden/scenario evaluation
//! (`eval`). The CLI surface lives in the binary (`danso memory
//! init|add|search|close|show|eval`); snapshot assembly and run
//! integration arrive with M2.

pub mod eval;
pub mod facts;
pub mod paths;
pub mod recall;
pub mod scan;

pub use facts::{
    Candidate, CloseOutcome, FactRecord, FactsFile, GateReport, MAX_FACTS_DEFAULT,
    MAX_FACTS_FILE_BYTES,
};
pub use paths::{Route, valid_scope};
pub use recall::SearchOptions;
pub use scan::ScanOutcome;

/// Audience labels carried by every fact record (§4.1). The scope tree is
/// the enforcement boundary; the label is data.
pub const AUDIENCE_PRIVATE: &str = "private";
pub const AUDIENCE_SHARED: &str = "shared";

/// Map a scope name to its record audience label: `shared` facts are
/// shared, everything else is private (§7).
pub fn audience_for_scope(scope: &str) -> &'static str {
    if scope == "shared" {
        AUDIENCE_SHARED
    } else {
        AUDIENCE_PRIVATE
    }
}
