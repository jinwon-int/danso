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

/// The memory root override; `settings::KEY_SOURCES` names it for
/// `memory.dir` so `config check` reports the variable this reads.
pub const DIR_ENV: &str = "DANSO_MEMORY_DIR";
/// The default root's name under `config::home()` (§8, #136).
pub const DEFAULT_DIR_NAME: &str = "memory";

impl MemoryConfig {
    /// `DANSO_MEMORY_DIR`, then `memory.dir` from `config.toml`, then
    /// `$DANSO_HOME/memory` — the one order every reader uses (#136).
    /// `doctor` and `backup` used to let the file win, and then inspected a
    /// memory root the service was not writing to. Fails only the way
    /// `config::home` fails: a relative `DANSO_HOME` is a configuration
    /// error, not a root.
    pub fn resolve_root(file: Option<std::path::PathBuf>) -> anyhow::Result<std::path::PathBuf> {
        Ok(Self::root_under(&crate::config::home()?, file))
    }

    /// `$DANSO_MEMORY_DIR`, else `$DANSO_HOME/memory` (§8); the root a run
    /// or `danso memory` uses when no `--memory-dir` is given.
    pub fn default_root() -> anyhow::Result<std::path::PathBuf> {
        Self::resolve_root(None)
    }

    /// [`resolve_root`](Self::resolve_root) beneath an already-resolved home,
    /// for the explicit-path entry points (`doctor::inspect_at`,
    /// `backup::create_at`) that must not resolve the environment twice and
    /// end up looking at a different root than the one they were handed.
    pub fn root_under(
        home: &std::path::Path,
        file: Option<std::path::PathBuf>,
    ) -> std::path::PathBuf {
        if let Some(dir) = std::env::var_os(DIR_ENV) {
            return std::path::PathBuf::from(dir);
        }
        file.unwrap_or_else(|| home.join(DEFAULT_DIR_NAME))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::with_env;
    use std::path::PathBuf;

    /// Without `DANSO_HOME` the default is the pre-#136 `$HOME/.danso/memory`
    /// byte for byte; with it, the root follows `DANSO_HOME`. The variable
    /// outranks the file, the file outranks the default, and a relative
    /// `DANSO_HOME` is a configuration error rather than a guessed root.
    #[test]
    fn default_root_follows_danso_home_and_is_unchanged_without_it() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set for tests"));
        let file = || Some(PathBuf::from("/cfg/memory"));
        with_env(&[("DANSO_HOME", None), (DIR_ENV, None)], || {
            assert_eq!(
                MemoryConfig::default_root().unwrap(),
                home.join(".danso/memory")
            );
            assert_eq!(
                MemoryConfig::resolve_root(file()).unwrap(),
                PathBuf::from("/cfg/memory")
            );
        });
        with_env(
            &[("DANSO_HOME", Some("/srv/danso")), (DIR_ENV, None)],
            || {
                assert_eq!(
                    MemoryConfig::default_root().unwrap(),
                    PathBuf::from("/srv/danso/memory")
                );
                assert_eq!(
                    MemoryConfig::resolve_root(file()).unwrap(),
                    PathBuf::from("/cfg/memory")
                );
            },
        );
        with_env(
            &[
                ("DANSO_HOME", Some("/srv/danso")),
                (DIR_ENV, Some("/var/memory")),
            ],
            || {
                assert_eq!(
                    MemoryConfig::default_root().unwrap(),
                    PathBuf::from("/var/memory")
                );
                assert_eq!(
                    MemoryConfig::resolve_root(file()).unwrap(),
                    PathBuf::from("/var/memory")
                );
            },
        );
        with_env(
            &[("DANSO_HOME", Some("srv/danso")), (DIR_ENV, None)],
            || {
                let error = format!("{:#}", MemoryConfig::default_root().unwrap_err());
                assert!(
                    error.contains("DANSO_HOME must be an absolute path"),
                    "{error}"
                );
                // The file cannot rescue a broken home: the order is fixed.
                assert!(MemoryConfig::resolve_root(file()).is_err());
            },
        );
    }
}
