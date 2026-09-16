//! Runtime generation identity (`danso.runtime-generation.v1`).
//!
//! self-update (#119) decides whether a restart actually replaced the running
//! binary. "The unit restarted" is not that evidence: a restart that re-executes
//! the same image reports success while the generation is unchanged. The digest
//! of the image this process is running is the evidence, so it is captured once
//! at startup and republished on every health write.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub const RUNTIME_GENERATION_SCHEMA: &str = "danso.runtime-generation.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeGeneration {
    pub schema: String,
    /// `None` when the running image could not be read. Absent evidence must
    /// stay absent rather than become a digest of something else.
    pub binary_sha256: Option<String>,
    pub version: String,
    pub exe_path: Option<String>,
    pub observed_at: String,
}

static CURRENT: OnceLock<RuntimeGeneration> = OnceLock::new();

/// The generation of the running process, computed once.
///
/// Digesting the image on every health write would re-read the whole binary
/// each poll; the image cannot change under a running process, so once is
/// correct as well as cheaper.
pub fn current() -> &'static RuntimeGeneration {
    CURRENT.get_or_init(|| {
        let exe = std::env::current_exe().ok();
        let binary_sha256 = exe
            .as_deref()
            .and_then(|path| std::fs::read(path).ok())
            .map(|bytes| hex(&Sha256::digest(&bytes)));
        RuntimeGeneration {
            schema: RUNTIME_GENERATION_SCHEMA.to_string(),
            binary_sha256,
            version: env!("CARGO_PKG_VERSION").to_string(),
            exe_path: exe.map(|path| path.display().to_string()),
            observed_at: chrono::Utc::now().to_rfc3339(),
        }
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_stable_and_identifies_the_test_binary() {
        let first = current();
        let second = current();
        assert_eq!(first, second);
        assert_eq!(first.schema, RUNTIME_GENERATION_SCHEMA);
        assert_eq!(first.version, env!("CARGO_PKG_VERSION"));
        let digest = first
            .binary_sha256
            .as_deref()
            .expect("the test binary is readable");
        assert_eq!(digest.len(), 64);
        let expected = hex(&Sha256::digest(
            std::fs::read(std::env::current_exe().unwrap()).unwrap(),
        ));
        assert_eq!(digest, expected);
    }
}
