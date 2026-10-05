//! What every chat channel shares (#213 ①): run settings, the in-process
//! turn runner, the private data-directory rule and per-turn usage counters.
//!
//! Telegram is the first adapter (`crate::telegram`); a Matrix adapter is
//! designed in #213. Nothing here talks to a channel API: a channel turns
//! updates into turns and renders their progress, and this module runs them.
//! Error text names the channel through a `label`, so moving this code out of
//! the Telegram service changed no message Telegram prints.

pub(crate) mod runner;
pub(crate) mod settings;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// Create (owner-only, 0700) or verify a channel data directory. The lock,
/// conversation store and journals share this one filesystem boundary.
pub(crate) fn ensure_private_dir(path: &Path, label: &'static str) -> Result<()> {
    ensure!(path.is_absolute(), "{label} data paths must be absolute");
    let missing = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "{label} data path must be a directory");
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };

    if missing {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create {label} data directory: {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir(), "{label} data path must be a directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "{label} data directory must be owned by the current user"
        );
        ensure!(
            metadata.mode() & 0o777 == 0o700,
            "{label} data directory must have mode 0700"
        );
    }
    Ok(())
}

/// The bounded, provider-neutral counters a channel retains for its local
/// `/usage` view. The field names on disk follow the core usage summary, while
/// the aliases keep hand-written/early records readable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageRecord {
    #[serde(default)]
    pub requests: u64,
    #[serde(rename = "inputTokens", alias = "input_tokens", default)]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens", alias = "output_tokens", default)]
    pub output_tokens: u64,
    #[serde(rename = "cacheReadTokens", alias = "cache_read_tokens", default)]
    pub cache_read_tokens: u64,
    #[serde(rename = "cacheWriteTokens", alias = "cache_write_tokens", default)]
    pub cache_write_tokens: u64,
    #[serde(rename = "totalTokens", alias = "total_tokens", default)]
    pub total_tokens: u64,
}

impl UsageRecord {
    pub fn from_summary(summary: &Value) -> Result<Self> {
        let usage: Self = serde_json::from_value(summary.clone()).context("decode turn usage")?;
        validate_usage(&usage)?;
        Ok(usage)
    }

    pub fn add(&mut self, other: &Self) -> Result<()> {
        let add = |left: u64, right: u64| left.checked_add(right).context("usage counter overflow");
        let next = Self {
            requests: add(self.requests, other.requests)?,
            input_tokens: add(self.input_tokens, other.input_tokens)?,
            output_tokens: add(self.output_tokens, other.output_tokens)?,
            cache_read_tokens: add(self.cache_read_tokens, other.cache_read_tokens)?,
            cache_write_tokens: add(self.cache_write_tokens, other.cache_write_tokens)?,
            total_tokens: add(self.total_tokens, other.total_tokens)?,
        };
        validate_usage(&next)?;
        *self = next;
        Ok(())
    }

    pub fn is_zero(&self) -> bool {
        self == &Self::default()
    }
}

pub(crate) fn validate_usage(usage: &UsageRecord) -> Result<()> {
    let total = usage
        .input_tokens
        .checked_add(usage.output_tokens)
        .and_then(|value| value.checked_add(usage.cache_read_tokens))
        .and_then(|value| value.checked_add(usage.cache_write_tokens))
        .context("usage counter overflow")?;
    ensure!(
        total == usage.total_tokens,
        "usage total does not match its counters"
    );
    Ok(())
}
