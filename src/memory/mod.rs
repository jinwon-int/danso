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

pub mod audit;
pub mod distill;
pub mod eval;
pub mod facts;
pub mod paths;
pub mod promote;
pub mod recall;
pub mod scan;
pub mod snapshot;
pub mod transaction;
pub mod working_state;

pub use facts::{
    Candidate, CloseOutcome, FactRecord, FactsFile, GateReport, MAX_FACTS_DEFAULT,
    MAX_FACTS_FILE_BYTES,
};
pub use paths::{Route, valid_scope};
pub const FACTS_FILE_NAME: &str = paths::FACTS_FILE;
pub use recall::SearchOptions;
pub use scan::ScanOutcome;
pub use snapshot::SnapshotOptions;

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

/// Local memory injection mode for a run (§8). `ReadWrite` additionally
/// registers distill journal entries (§4.7) on top of the read injection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemoryMode {
    #[default]
    Off,
    /// Assemble the snapshot once per run and inject it into the system
    /// context (§5).
    Read,
    ReadWrite,
}

/// Context refresh cadence (§5.3): per-run assembles once at run start;
/// per-request additionally re-assembles right after each compaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefreshMode {
    #[default]
    PerRun,
    PerRequest,
}

/// Distill scheduling for a read-write run (§8): queue (default) registers
/// a pending extraction; inline additionally drains one job after the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DistillMode {
    #[default]
    Queue,
    Inline,
    Off,
}

/// Local memory injection configuration for a run (§8). Defaults reproduce
/// memory OFF exactly: without `--memory read` the system context is byte
/// identical to a non-memory build.
#[derive(Clone, Debug, Default)]
pub struct MemoryConfig {
    pub mode: MemoryMode,
    pub root: Option<std::path::PathBuf>,
    pub scope: String,
    pub query: Option<String>,
    pub max_bytes: usize,
    pub as_of: Option<String>,
    pub refresh: RefreshMode,
    /// Read-only legacy ccc tree (§9, `--memory-legacy-read`). `None` keeps
    /// the pre-#86 behaviour byte for byte.
    pub legacy_read: Option<std::path::PathBuf>,
}

impl MemoryConfig {
    /// `$DANSO_MEMORY_DIR` or `~/.danso/memory` (§8).
    pub fn default_root() -> std::path::PathBuf {
        if let Some(dir) = std::env::var_os("DANSO_MEMORY_DIR") {
            return std::path::PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/root"));
        home.join(".danso/memory")
    }

    /// Fail-closed configuration validation (§8): out-of-range values are
    /// configuration errors, never clamped.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        ensure!(
            paths::valid_scope(&self.scope),
            "memory scope must be global, shared, or private-<32 hex>"
        );
        ensure!(
            (1..=snapshot::SNAPSHOT_MAX_BYTES_MAX).contains(&self.max_bytes),
            "memory max bytes must be 1..={}",
            snapshot::SNAPSHOT_MAX_BYTES_MAX
        );
        if let Some(root) = &self.root {
            ensure!(root.is_absolute(), "memory dir must be an absolute path");
        }
        if let Some(dir) = &self.legacy_read {
            ensure!(
                dir.is_absolute(),
                "memory legacy read dir must be an absolute path"
            );
            // §7/§8 read matrix: a shared run never opens a personal or legacy
            // tree. Refuse the combination up front rather than silently
            // ignoring the flag.
            ensure!(
                self.scope != "shared",
                "--memory-legacy-read is not allowed with --memory-scope shared"
            );
        }
        Ok(())
    }
}
