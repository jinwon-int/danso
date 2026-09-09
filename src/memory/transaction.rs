//! Two-target crash-recoverable commits compatible with the ccc
//! `ccc.local-memory-rollback.v1` ledger (issue #52 §4.5): the targets are
//! exactly `memory-facts.jsonl` and `resume.md`. One exclusive scope lock
//! (`state/.memory.lock`, shared with every other scope writer), the
//! recovery state machine runs before every operation, the pre-image of
//! every target becomes the single undoable head, and a previous head is
//! superseded with its pre-images purged.

use anyhow::{Result, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::paths;

pub const SCHEMA: &str = "ccc.local-memory-rollback.v1";
pub const TARGETS: [&str; 2] = ["memory-facts.jsonl", "resume.md"];
pub const MANIFEST_STATES: [&str; 6] = [
    "prepared",
    "committed",
    "undoing",
    "rolled_back",
    "aborted",
    "superseded",
];
pub const ABSENT_HASH: &str = "44cda575e8e5f8e97091d9a207368dc972c6d2fdd48a9c43a81e8d4cd5c7b93c";
const MAX_TARGET_BYTES: u64 = 8 << 20;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_LEDGER_BYTES: u64 = 1024 * 1024;
const ABSENT: &str = "ccc-node:absent:v1";

pub fn absent_hash() -> String {
    super::facts::hex_encode(&Sha256::digest(ABSENT.as_bytes()))
}

fn now() -> String {
    super::facts::format_timestamp(chrono::Utc::now())
}

fn hash(payload: Option<&[u8]>) -> String {
    match payload {
        None => absent_hash(),
        Some(bytes) => super::facts::hex_encode(&Sha256::digest(bytes)),
    }
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn session_ref(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    super::facts::hex_encode(&digest[..8])
}

fn is_timestamp(value: &str) -> bool {
    super::facts::parse_timestamp(value).is_ok()
}

fn target_of<'a>(manifest: &'a Map<String, Value>, name: &str) -> &'a Value {
    manifest
        .get("targets")
        .and_then(|t| t.get(name))
        .unwrap_or(&Value::Null)
}

/// One commit attempt's outcome: `action_id` is `None` when the transform
/// changed nothing (the on-disk state is untouched).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitResult {
    pub action_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RollbackOutcome {
    RolledBack,
    AlreadyRolledBack,
}

/// Per-target file contents handed to a commit transform.
pub type Targets = Vec<(String, Option<Vec<u8>>)>;

pub struct Transaction {
    state_dir: PathBuf,
    rollback_dir: PathBuf,
    actions_dir: PathBuf,
    ledger_path: PathBuf,
    head_path: PathBuf,
    lock_path: PathBuf,
}

impl Transaction {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            rollback_dir: state_dir.join("memory-rollback"),
            actions_dir: state_dir.join("memory-rollback/actions"),
            ledger_path: state_dir.join("memory-rollback/ledger.jsonl"),
            head_path: state_dir.join("memory-rollback/HEAD"),
            // #65 §1.5: the scope has exactly one write lock. Manual facts
            // (`add`/`close`), distill commits, and recovery all serialize
            // on `state/.memory.lock` instead of a second transaction-only
            // lock that let two writers interleave on the same facts file.
            lock_path: state_dir.join(paths::SCOPE_LOCK_FILE),
        }
    }

    fn ensure_layout(&self) -> Result<()> {
        paths::require_private_dir(&self.state_dir)?;
        paths::require_private_dir(&self.rollback_dir)?;
        paths::require_private_dir(&self.actions_dir)?;
        Ok(())
    }

    fn with_lock<T>(&self, timeout_ms: u64, body: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.ensure_layout()?;
        let _lock = paths::ExclusiveLock::acquire(&self.lock_path, timeout_ms)?;
        body(self)
    }

    fn target_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }

    fn read_target(&self, name: &str) -> Result<Option<Vec<u8>>> {
        // Validate before and after the read so a raced swap never passes.
        paths::validate_regular(&self.target_path(name), "memory state")?;
        let value = paths::read_bounded(&self.target_path(name), MAX_TARGET_BYTES, "memory state")?;
        paths::validate_regular(&self.target_path(name), "memory state")?;
        Ok(value)
    }

    fn write_target(&self, name: &str, payload: Option<&[u8]>) -> Result<()> {
        let path = self.target_path(name);
        match payload {
            None => {
                if paths::validate_regular(&path, "memory state")? {
                    std::fs::remove_file(&path)?;
                    paths::fsync_dir(&self.state_dir)?;
                }
            }
            Some(bytes) => paths::atomic_write(&path, bytes, "memory state")?,
        }
        Ok(())
    }

    fn action_dir(&self, action_id: &str) -> Result<PathBuf> {
        ensure!(
            is_lower_hex(action_id, 32),
            "action_id must be 32 lowercase hex characters"
        );
        Ok(self.actions_dir.join(action_id))
    }

    fn manifest_path(&self, action_id: &str) -> Result<PathBuf> {
        Ok(self.action_dir(action_id)?.join("manifest.json"))
    }

    fn preimage_path(&self, action_id: &str, name: &str) -> Result<PathBuf> {
        Ok(self.action_dir(action_id)?.join(format!("before-{name}")))
    }

    /// Read and fully authenticate one manifest, its action directory, and
    /// its pre-image presence (ccc `_read_manifest`).
    fn read_manifest(&self, action_id: &str) -> Result<Map<String, Value>> {
        let action_dir = self.action_dir(action_id)?;
        let meta = std::fs::symlink_metadata(&action_dir)?;
        {
            use std::os::unix::fs::MetadataExt;
            if meta.file_type().is_symlink()
                || !meta.is_dir()
                || meta.mode() & 0o777 != 0o700
                || meta.uid() != unsafe { libc::geteuid() }
            {
                anyhow::bail!("rollback action directory is unsafe");
            }
        }
        let payload = paths::read_bounded(
            &self.manifest_path(action_id)?,
            MAX_MANIFEST_BYTES,
            "rollback manifest",
        )?
        .ok_or_else(|| anyhow::anyhow!("rollback manifest is missing"))?;
        let manifest: Value = serde_json::from_slice(&payload)
            .map_err(|_| anyhow::anyhow!("rollback manifest schema is invalid"))?;
        let Some(manifest) = manifest.as_object() else {
            anyhow::bail!("rollback manifest schema is invalid");
        };
        if manifest.get("schema").and_then(Value::as_str) != Some(SCHEMA)
            || manifest.get("action_id").and_then(Value::as_str) != Some(action_id)
        {
            anyhow::bail!("rollback manifest schema is invalid");
        }
        let state = manifest
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback manifest state is invalid"))?;
        if !MANIFEST_STATES.contains(&state) {
            anyhow::bail!("rollback manifest state is invalid");
        }
        match manifest.get("parent") {
            None => anyhow::bail!("rollback manifest parent is missing"),
            Some(Value::Null) => {}
            Some(Value::String(parent)) => {
                if !is_lower_hex(parent, 32) || parent == action_id {
                    anyhow::bail!("rollback manifest parent is invalid");
                }
            }
            Some(_) => anyhow::bail!("rollback manifest parent is invalid"),
        }
        for field in ["provider", "actor", "tool", "diff"] {
            let value = manifest
                .get(field)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("rollback manifest label is invalid"))?;
            ensure!(is_safe_label(value), "rollback manifest label is invalid");
        }
        let session = manifest
            .get("session")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback manifest session is invalid"))?;
        ensure!(
            is_lower_hex(session, 16),
            "rollback manifest session is invalid"
        );
        let created_at = manifest
            .get("created_at")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback manifest timestamp is invalid"))?;
        ensure!(
            is_timestamp(created_at),
            "rollback manifest timestamp is invalid"
        );
        let Some(targets) = manifest.get("targets").and_then(Value::as_object) else {
            anyhow::bail!("rollback manifest target set is invalid");
        };
        if targets.len() != TARGETS.len() || TARGETS.iter().any(|name| !targets.contains_key(*name))
        {
            anyhow::bail!("rollback manifest target set is invalid");
        }
        for name in TARGETS {
            let Some(target) = targets.get(name).and_then(Value::as_object) else {
                anyhow::bail!("rollback manifest target is invalid");
            };
            if target.len() != 4
                || !target.contains_key("before_exists")
                || !target.contains_key("after_exists")
                || !target.contains_key("before_hash")
                || !target.contains_key("after_hash")
            {
                anyhow::bail!("rollback manifest target is invalid");
            }
            let (Some(before_exists), Some(after_exists)) = (
                target.get("before_exists").and_then(Value::as_bool),
                target.get("after_exists").and_then(Value::as_bool),
            ) else {
                anyhow::bail!("rollback manifest existence flag is invalid");
            };
            for (side, exists) in [("before", before_exists), ("after", after_exists)] {
                let Some(value) = target.get(&format!("{side}_hash")).and_then(Value::as_str)
                else {
                    anyhow::bail!("rollback manifest hash is invalid");
                };
                if !is_lower_hex(value, 64) {
                    anyhow::bail!("rollback manifest hash is invalid");
                }
                // §4.5: an absent target's hash must be exactly the absent
                // constant — an empty-block check here once let any 64-hex
                // value stand in for a missing pre/post-image.
                if !exists {
                    ensure!(value == absent_hash(), "rollback manifest hash is invalid");
                }
            }
        }
        // Action-directory entry allowlist and pre-image presence rules.
        let entries = std::fs::read_dir(&action_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let allowed = name == "manifest.json"
                || TARGETS
                    .iter()
                    .any(|target| name == format!("before-{target}"));
            if !allowed {
                anyhow::bail!("rollback action contains an unsafe entry");
            }
        }
        for name in TARGETS {
            let before_exists = target_of(manifest, name)["before_exists"]
                .as_bool()
                .unwrap_or(false);
            let preimage = self.preimage_path(action_id, name)?;
            if paths::validate_regular(&preimage, "rollback pre-image")? {
                if !before_exists {
                    anyhow::bail!("rollback action has an unexpected pre-image");
                }
            } else if before_exists && matches!(state, "prepared" | "committed" | "undoing") {
                anyhow::bail!("rollback action pre-image is missing");
            }
        }
        Ok(manifest.clone())
    }

    fn write_manifest(&self, manifest: &Map<String, Value>) -> Result<()> {
        let action_id = manifest["action_id"].as_str().expect("validated");
        let payload = format!("{}\n", Value::Object(manifest.clone())).into_bytes();
        paths::atomic_write(
            &self.manifest_path(action_id)?,
            &payload,
            "rollback manifest",
        )?;
        Ok(())
    }

    fn read_head(&self) -> Result<Option<String>> {
        if !paths::validate_regular(&self.head_path, "rollback HEAD")? {
            return Ok(None);
        }
        let payload =
            paths::read_bounded(&self.head_path, 128, "rollback HEAD")?.unwrap_or_default();
        let value =
            String::from_utf8(payload).map_err(|_| anyhow::anyhow!("rollback HEAD is invalid"))?;
        let value = value.trim().to_string();
        if value.is_empty() {
            return Ok(None);
        }
        ensure!(is_lower_hex(&value, 32), "rollback HEAD is invalid");
        Ok(Some(value))
    }

    fn write_head(&self, action_id: Option<&str>) -> Result<()> {
        let payload = match action_id {
            None => Vec::new(),
            Some(id) => format!("{id}\n").into_bytes(),
        };
        paths::atomic_write(&self.head_path, &payload, "rollback HEAD")?;
        Ok(())
    }

    fn validate_ledger_record(record: &Map<String, Value>) -> Result<()> {
        const KEYS: [&str; 11] = [
            "schema",
            "event",
            "action_id",
            "actor",
            "tool",
            "provider",
            "scope",
            "targets",
            "diff",
            "session",
            "ts",
        ];
        if record.len() != KEYS.len() || KEYS.iter().any(|key| !record.contains_key(*key)) {
            anyhow::bail!("rollback ledger record is invalid");
        }
        if record.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
            anyhow::bail!("rollback ledger schema is invalid");
        }
        let event = record
            .get("event")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback ledger schema is invalid"))?;
        if !["commit", "abort", "rollback", "supersede"].contains(&event) {
            anyhow::bail!("rollback ledger schema is invalid");
        }
        let action_id = record
            .get("action_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback ledger action id is invalid"))?;
        ensure!(
            is_lower_hex(action_id, 32),
            "rollback ledger action id is invalid"
        );
        for field in ["provider", "actor", "tool", "diff"] {
            let value = record
                .get(field)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("rollback ledger label is invalid"))?;
            ensure!(is_safe_label(value), "rollback ledger label is invalid");
        }
        if record.get("scope").and_then(Value::as_str) != Some("local-memory") {
            anyhow::bail!("rollback ledger target scope is invalid");
        }
        let Some(targets) = record.get("targets").and_then(Value::as_array) else {
            anyhow::bail!("rollback ledger target scope is invalid");
        };
        if targets.len() != TARGETS.len()
            || TARGETS
                .iter()
                .any(|name| !targets.iter().any(|t| t.as_str() == Some(*name)))
        {
            anyhow::bail!("rollback ledger target scope is invalid");
        }
        let session = record
            .get("session")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback ledger session is invalid"))?;
        ensure!(
            is_lower_hex(session, 16),
            "rollback ledger session is invalid"
        );
        let ts = record
            .get("ts")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("rollback ledger timestamp is invalid"))?;
        ensure!(is_timestamp(ts), "rollback ledger timestamp is invalid");
        Ok(())
    }

    fn read_ledger(&self) -> Result<Vec<Map<String, Value>>> {
        let Some(payload) =
            paths::read_bounded(&self.ledger_path, MAX_LEDGER_BYTES, "rollback ledger")?
        else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(payload)
            .map_err(|_| anyhow::anyhow!("rollback ledger contains invalid JSON"))?;
        let mut records = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|_| anyhow::anyhow!("rollback ledger contains invalid JSON"))?;
            let Some(record) = value.as_object() else {
                anyhow::bail!("rollback ledger contains invalid JSON");
            };
            Self::validate_ledger_record(record)?;
            records.push(record.clone());
        }
        Ok(records)
    }

    fn record_event(&self, manifest: &Map<String, Value>, event: &str) -> Result<()> {
        let action_id = manifest["action_id"]
            .as_str()
            .expect("validated")
            .to_string();
        let records = self.read_ledger()?;
        if records.iter().any(|record| {
            record.get("action_id").and_then(Value::as_str) == Some(action_id.as_str())
                && record.get("event").and_then(Value::as_str) == Some(event)
        }) {
            return Ok(());
        }
        let record = json!({
            "schema": SCHEMA,
            "event": event,
            "action_id": action_id,
            "actor": manifest["actor"],
            "tool": manifest["tool"],
            "provider": manifest["provider"],
            "scope": "local-memory",
            "targets": TARGETS,
            "diff": manifest["diff"],
            "session": manifest["session"],
            "ts": now(),
        });
        let record = record.as_object().expect("object").clone();
        Self::validate_ledger_record(&record)?;
        let mut lines: Vec<String> = records
            .iter()
            .map(|record| Value::Object(record.clone()).to_string())
            .collect();
        lines.push(Value::Object(record).to_string());
        let total_bytes = |lines: &[String]| lines.iter().map(|l| l.len() + 1).sum::<usize>();
        while total_bytes(&lines) > MAX_LEDGER_BYTES as usize && lines.len() > 1 {
            lines.remove(0);
        }
        let payload = lines.join("\n") + "\n";
        paths::atomic_write(&self.ledger_path, payload.as_bytes(), "rollback ledger")?;
        Ok(())
    }

    fn read_preimage(&self, manifest: &Map<String, Value>, name: &str) -> Result<Option<Vec<u8>>> {
        let action_id = manifest["action_id"].as_str().expect("validated");
        if !target_of(manifest, name)["before_exists"]
            .as_bool()
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let payload = paths::read_bounded(
            &self.preimage_path(action_id, name)?,
            MAX_TARGET_BYTES,
            "rollback pre-image",
        )?
        .ok_or_else(|| anyhow::anyhow!("rollback pre-image is missing"))?;
        if hash(Some(&payload))
            != target_of(manifest, name)["before_hash"]
                .as_str()
                .unwrap_or("")
        {
            anyhow::bail!("rollback pre-image hash is invalid");
        }
        Ok(Some(payload))
    }

    fn target_matches(
        manifest: &Map<String, Value>,
        name: &str,
        current: Option<&[u8]>,
        side: &str,
    ) -> bool {
        let target = target_of(manifest, name);
        let exists = target[format!("{side}_exists")].as_bool().unwrap_or(false);
        let expected = target[format!("{side}_hash")].as_str().unwrap_or("");
        (current.is_some() == exists) && hash(current) == expected
    }

    /// Restore every target to its authenticated pre-image, then verify.
    fn restore_before(&self, manifest: &Map<String, Value>) -> Result<()> {
        let mut preimages: Vec<(String, Option<Vec<u8>>)> = Vec::new();
        for name in TARGETS {
            preimages.push((name.to_string(), self.read_preimage(manifest, name)?));
        }
        for (name, payload) in &preimages {
            self.write_target(name, payload.as_deref())?;
        }
        for (name, _) in &preimages {
            let current = self.read_target(name)?;
            if !Self::target_matches(manifest, name, current.as_deref(), "before") {
                anyhow::bail!("rollback restore verification failed");
            }
        }
        Ok(())
    }

    fn purge_preimages(&self, manifest: &Map<String, Value>) -> Result<()> {
        let action_id = manifest["action_id"].as_str().expect("validated");
        for name in TARGETS {
            let preimage = self.preimage_path(action_id, name)?;
            if paths::validate_regular(&preimage, "rollback pre-image")? {
                std::fs::remove_file(&preimage)?;
            }
        }
        paths::fsync_dir(&self.action_dir(action_id)?)?;
        Ok(())
    }

    /// An interrupted prepared commit: when every target already sits at its
    /// post-image the commit completes forward; when every target still sits
    /// at its pre-image (or a mix that matches one side each) the restore
    /// aborts; an unknown target state stops without overwriting anything.
    fn finish_prepared(&self, manifest: &mut Map<String, Value>) -> Result<()> {
        let mut all_after = true;
        for name in TARGETS {
            let current = self.read_target(name)?;
            let after = Self::target_matches(manifest, name, current.as_deref(), "after");
            let before = Self::target_matches(manifest, name, current.as_deref(), "before");
            if !after && !before {
                anyhow::bail!("prepared action encountered an unknown target state");
            }
            if !after {
                all_after = false;
            }
        }
        let action_id = manifest["action_id"]
            .as_str()
            .expect("validated")
            .to_string();
        if all_after {
            self.write_head(Some(&action_id))?;
            manifest.insert("state".into(), Value::from("committed"));
            self.write_manifest(manifest)?;
            self.record_event(manifest, "commit")?;
            return Ok(());
        }
        self.restore_before(manifest)?;
        self.record_event(manifest, "abort")?;
        manifest.insert("state".into(), Value::from("aborted"));
        self.write_manifest(manifest)?;
        self.purge_preimages(manifest)?;
        Ok(())
    }

    fn finish_undo(&self, manifest: &mut Map<String, Value>) -> Result<()> {
        for name in TARGETS {
            let current = self.read_target(name)?;
            let after = Self::target_matches(manifest, name, current.as_deref(), "after");
            let before = Self::target_matches(manifest, name, current.as_deref(), "before");
            if !after && !before {
                anyhow::bail!("undo recovery encountered an unknown target state");
            }
        }
        self.restore_before(manifest)?;
        self.write_head(None)?;
        self.record_event(manifest, "rollback")?;
        manifest.insert("state".into(), Value::from("rolled_back"));
        self.write_manifest(manifest)?;
        self.purge_preimages(manifest)?;
        Ok(())
    }

    fn discard_unprepared_action(&self, path: &Path) -> Result<()> {
        let entries = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        for entry in &entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let allowed = TARGETS
                .iter()
                .any(|target| name == format!("before-{target}"));
            if !allowed {
                anyhow::bail!("rollback action has no manifest");
            }
            paths::validate_regular(&entry.path(), "rollback pre-image")?;
        }
        for entry in &entries {
            std::fs::remove_file(entry.path())?;
        }
        std::fs::remove_dir(path)?;
        paths::fsync_dir(&self.actions_dir)?;
        Ok(())
    }

    /// Recovery state machine: an interrupted prepared commit completes when
    /// every target is already at its post-image, otherwise restores to the
    /// pre-images; an interrupted undo resumes; two or more incomplete
    /// actions refuse; a HEAD that is not committed stops recovery.
    fn recover(&self) -> Result<()> {
        let mut candidates: Vec<Map<String, Value>> = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&self.actions_dir)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for path in entries {
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                anyhow::bail!("rollback action entry is unsafe");
            }
            let name = path
                .file_name()
                .expect("entry")
                .to_string_lossy()
                .to_string();
            if !self.manifest_path(&name)?.exists() {
                self.discard_unprepared_action(&path)?;
                continue;
            }
            let manifest = self.read_manifest(&name)?;
            match manifest.get("state").and_then(Value::as_str) {
                Some("prepared") | Some("undoing") => candidates.push(manifest),
                // The live head stays on disk as the undoable action.
                Some("committed") => {}
                Some("aborted") | Some("rolled_back") | Some("superseded") => {
                    self.purge_preimages(&manifest)?
                }
                _ => anyhow::bail!("rollback manifest state is invalid"),
            }
        }
        if candidates.len() > 1 {
            anyhow::bail!("multiple incomplete rollback actions exist");
        }
        if let Some(mut manifest) = candidates.pop() {
            if manifest.get("state").and_then(Value::as_str) == Some("prepared") {
                self.finish_prepared(&mut manifest)?;
            } else {
                self.finish_undo(&mut manifest)?;
            }
        }
        if let Some(head) = self.read_head()? {
            let current = self.read_manifest(&head)?;
            if current.get("state").and_then(Value::as_str) != Some("committed") {
                anyhow::bail!("rollback HEAD is not committed");
            }
            self.record_event(&current, "commit")?;
            if let Some(parent) = current.get("parent").and_then(Value::as_str) {
                self.supersede(parent)?;
            }
        }
        Ok(())
    }

    fn supersede(&self, action_id: &str) -> Result<()> {
        if !is_lower_hex(action_id, 32) {
            anyhow::bail!("rollback manifest action id is invalid");
        }
        let mut manifest = self.read_manifest(action_id)?;
        match manifest.get("state").and_then(Value::as_str) {
            Some("committed") => {
                self.record_event(&manifest, "supersede")?;
                manifest.insert("state".into(), Value::from("superseded"));
                self.write_manifest(&manifest)?;
            }
            Some("superseded") => {}
            _ => return Ok(()),
        }
        self.purge_preimages(&manifest)
    }

    /// Apply one transform to both targets under the exclusive lock. The
    /// pre-images become the single undoable head; a previous head is
    /// superseded and its pre-images purged, so retained rollback bodies
    /// stay bounded to one action per state directory.
    pub fn commit(
        &self,
        timeout_ms: u64,
        meta: &CommitMeta,
        transform: impl FnOnce(&Targets) -> Result<Targets>,
    ) -> Result<CommitResult> {
        for label in [&meta.provider, &meta.actor, &meta.tool, &meta.diff] {
            ensure!(
                is_safe_label(label),
                "commit metadata must be bounded machine labels"
            );
        }
        self.with_lock(timeout_ms, |tx| {
            tx.recover()?;
            let befores: Targets = TARGETS
                .iter()
                .map(|name| (name.to_string(), tx.read_target(name).unwrap_or(None)))
                .collect();
            let targets = transform(&befores)?;
            if targets == befores {
                // Nothing changed: no action, no file touched.
                return Ok(CommitResult { action_id: None });
            }
            let previous_head = tx.read_head()?;
            let bytes = *uuid::Uuid::new_v4().as_bytes();
            let action_id = super::facts::hex_encode(&bytes);
            let action_dir = tx.action_dir(&action_id)?;
            std::fs::create_dir(&action_dir)?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&action_dir, std::fs::Permissions::from_mode(0o700))?;
            }
            paths::fsync_dir(&tx.actions_dir)?;
            let mut targets_manifest = Map::new();
            for (name, before) in &befores {
                if let Some(bytes) = before {
                    paths::atomic_write(
                        &tx.preimage_path(&action_id, name)?,
                        bytes,
                        "rollback pre-image",
                    )?;
                }
                let after = targets
                    .iter()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, value)| value.as_deref());
                let mut entry = Map::new();
                entry.insert("before_exists".into(), Value::from(before.is_some()));
                entry.insert("after_exists".into(), Value::from(after.is_some()));
                entry.insert("before_hash".into(), Value::from(hash(before.as_deref())));
                entry.insert("after_hash".into(), Value::from(hash(after)));
                targets_manifest.insert(name.clone(), Value::Object(entry));
            }
            let mut manifest = Map::new();
            manifest.insert("schema".into(), Value::from(SCHEMA));
            manifest.insert("action_id".into(), Value::from(action_id.as_str()));
            manifest.insert("state".into(), Value::from("prepared"));
            manifest.insert(
                "parent".into(),
                previous_head
                    .as_ref()
                    .map(|head| Value::from(head.as_str()))
                    .unwrap_or(Value::Null),
            );
            manifest.insert("provider".into(), Value::from(meta.provider.as_str()));
            manifest.insert("actor".into(), Value::from(meta.actor.as_str()));
            manifest.insert("tool".into(), Value::from(meta.tool.as_str()));
            manifest.insert("diff".into(), Value::from(meta.diff.as_str()));
            manifest.insert("session".into(), Value::from(session_ref(&meta.session)));
            manifest.insert("created_at".into(), Value::from(now()));
            manifest.insert("targets".into(), Value::Object(targets_manifest));
            tx.write_manifest(&manifest)?;
            for (name, after) in &targets {
                tx.write_target(name, after.as_deref())?;
            }
            for (name, _) in &targets {
                let current = tx.read_target(name)?;
                if !Self::target_matches(&manifest, name, current.as_deref(), "after") {
                    anyhow::bail!("local-memory commit verification failed");
                }
            }
            tx.write_head(Some(&action_id))?;
            manifest.insert("state".into(), Value::from("committed"));
            tx.write_manifest(&manifest)?;
            tx.record_event(&manifest, "commit")?;
            if let Some(head) = previous_head {
                tx.supersede(&head)?;
            }
            Ok(CommitResult {
                action_id: Some(action_id),
            })
        })
    }

    /// Restore the latest committed head after a full post-image CAS check.
    /// A repeated request is an idempotent no-op.
    pub fn rollback(&self, timeout_ms: u64, action_id: &str) -> Result<RollbackOutcome> {
        ensure!(
            is_lower_hex(action_id, 32),
            "action_id must be 32 lowercase hex characters"
        );
        self.with_lock(timeout_ms, |tx| {
            tx.recover()?;
            let Some(head) = tx.read_head()? else {
                // HEAD cleared by an earlier rollback: the request is
                // idempotent only when that action is already rolled back.
                let manifest = tx.read_manifest(action_id)?;
                if manifest.get("state").and_then(Value::as_str) == Some("rolled_back") {
                    return Ok(RollbackOutcome::AlreadyRolledBack);
                }
                anyhow::bail!("no committed rollback head exists");
            };
            if head != action_id {
                anyhow::bail!("only the newest committed action can be rolled back");
            }
            let mut manifest = tx.read_manifest(&head)?;
            if manifest.get("state").and_then(Value::as_str) == Some("rolled_back") {
                return Ok(RollbackOutcome::AlreadyRolledBack);
            }
            for name in TARGETS {
                let current = tx.read_target(name)?;
                if !Self::target_matches(&manifest, name, current.as_deref(), "after") {
                    anyhow::bail!("targets changed since the commit; manual recovery is required");
                }
            }
            manifest.insert("state".into(), Value::from("undoing"));
            tx.write_manifest(&manifest)?;
            tx.restore_before(&manifest)?;
            tx.write_head(None)?;
            tx.record_event(&manifest, "rollback")?;
            manifest.insert("state".into(), Value::from("rolled_back"));
            tx.write_manifest(&manifest)?;
            tx.purge_preimages(&manifest)?;
            Ok(RollbackOutcome::RolledBack)
        })
    }

    /// Body-free rollback state for diagnostics (§6.4/M5).
    pub fn status(&self) -> Result<(Option<String>, usize)> {
        let head = self.read_head()?;
        let mut actions = 0usize;
        if self.actions_dir.is_dir() {
            for entry in std::fs::read_dir(&self.actions_dir)? {
                let entry = entry?;
                if entry.metadata()?.is_dir() {
                    actions += 1;
                }
            }
        }
        Ok((head, actions))
    }
}

/// Commit metadata: bounded machine labels only (§4.5).
pub struct CommitMeta {
    pub provider: String,
    pub actor: String,
    pub tool: String,
    pub diff: String,
    pub session: String,
}
