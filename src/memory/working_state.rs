//! Harness-written working state (issue #52 §5.3, M3): the harness renders
//! the compaction checkpoint's five fields (objective / constraints /
//! changes / tests / pending) into `state/working-state.md`, preserves a
//! PreCompact copy per compaction under `state/checkpoints/` (newest 30 by
//! mtime), and on a finished run records the first 2048 bytes of the final
//! answer plus an archived copy under `state/session-archive/` named by
//! content hash. Every write is fail-closed and passes the injection
//! scanner; the session journal is never touched.

use anyhow::{Result, ensure};
use chrono::Utc;
use sha2::{Digest, Sha256};

use super::facts;
use super::paths::{self, Route};
use super::scan;
use crate::contracts::{Event, EventSink};

/// Final-answer bytes recorded in the working state (§5.3).
pub const FINAL_ANSWER_BYTES: usize = 2048;
/// PreCompact copies retained under `state/checkpoints/` (§5.3).
pub const CHECKPOINTS_KEPT: usize = 30;

/// Records working state on compaction and run end for one memory route.
pub struct Recorder {
    route: Route,
    final_answer: Option<String>,
    last_summary: Option<serde_json::Value>,
    last_prompt: Option<String>,
}

impl Recorder {
    pub fn new(route: Route, prompt: &str) -> Self {
        Self {
            route,
            final_answer: None,
            last_summary: None,
            last_prompt: Some(prompt.chars().take(400).collect()),
        }
    }

    /// Called once per `Event::Compaction` with the recorded journal entry;
    /// renders the checkpoint's five fields and preserves the PreCompact
    /// copy before overwriting the working state.
    pub fn on_compaction(&mut self, entry: &serde_json::Value) -> Result<()> {
        let summary = entry["data"]["summary"].clone();
        self.last_summary = Some(summary.clone());
        self.write_state(&render(&summary, None)?, true)?;
        Ok(())
    }

    /// Capture the final answer text (first [`FINAL_ANSWER_BYTES`] bytes)
    /// from `Event::FinalAnswer`.
    pub fn capture_final_answer(&mut self, message: &serde_json::Value) {
        let text: String = message["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        self.final_answer = Some(scan::truncate_utf8(&text, FINAL_ANSWER_BYTES).to_string());
    }

    /// Run finished with a final answer: refresh the working state with the
    /// captured answer (pending resolved by the delivered answer) and file
    /// an archived copy under `session-archive/`.
    pub fn on_run_end(&mut self) -> Result<()> {
        let summary = self
            .last_summary
            .clone()
            .unwrap_or_else(|| self.synthesized_summary());
        let summary = self.resolve_pending(summary);
        self.write_state(&render(&summary, self.final_answer.as_deref())?, false)?;

        // Archived copy named by content hash (§3: sha256[:24]).
        let state = std::fs::read(route_working_state(&self.route))?;
        let digest = Sha256::digest(&state);
        let name = format!("working-state-{}.md", facts::hex_encode(&digest[..12]));
        let archive_dir = self.route.state_dir().join("session-archive");
        paths::require_private_dir(&archive_dir)?;
        let archive = archive_dir.join(&name);
        if !paths::validate_regular(&archive, "session archive")? {
            write_private(&archive, &state)?;
        }
        Ok(())
    }

    /// No compaction happened this run: a minimal checkpoint from the prompt
    /// so the next run still resumes from something.
    fn synthesized_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "objective": self.last_prompt.as_deref().unwrap_or("(run)"),
            "constraints": [],
            "changes": [],
            "tests": [],
            "pending": [],
        })
    }

    /// A delivered final answer resolves the checkpoint's pending list (the
    /// wrap-up lives in the recorded final answer section).
    fn resolve_pending(&self, mut summary: serde_json::Value) -> serde_json::Value {
        if self.final_answer.is_some()
            && let Some(object) = summary.as_object_mut()
        {
            object.insert("pending".into(), serde_json::Value::Array(Vec::new()));
        }
        summary
    }

    fn write_state(&self, rendered: &str, preserve_previous: bool) -> Result<()> {
        let checkpoints = self.route.state_dir().join("checkpoints");
        paths::require_private_dir(&checkpoints)?;
        let state_path = route_working_state(&self.route);

        // PreCompact copy of the previous working state, newest 30 kept.
        if preserve_previous && paths::validate_regular(&state_path, "working state")? {
            let previous = std::fs::read(&state_path)?;
            write_private(&checkpoints.join(&unique_name(&checkpoints)?), &previous)?;
        }
        prune_checkpoints(&checkpoints, CHECKPOINTS_KEPT)?;

        let scanned = scan::scan("working-state", rendered, None);
        let mut payload = scanned.text.into_bytes();
        if !payload.ends_with(b"\n") {
            payload.push(b'\n');
        }
        if paths::validate_regular(&state_path, "working state")? {
            paths::atomic_write(&state_path, &payload, "working state")?;
        } else {
            write_private(&state_path, &payload)?;
        }
        Ok(())
    }
}

fn route_working_state(route: &Route) -> std::path::PathBuf {
    route.working_state_file()
}

/// Render the five checkpoint fields plus the optional final-answer section,
/// scanner-applied by the caller.
fn render(summary: &serde_json::Value, final_answer: Option<&str>) -> Result<String> {
    let objective = summary["objective"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("checkpoint lacks objective"))?;
    let mut out = String::from("# working-state (danso auto-managed)\n\n## objective\n");
    out.push_str(objective.trim());
    out.push('\n');
    for (key, label) in [
        ("constraints", "## constraints"),
        ("changes", "## changes"),
        ("tests", "## tests"),
        ("pending", "## pending"),
    ] {
        out.push_str(&format!("\n{label}\n"));
        for item in summary[key].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(text) = item.as_str() {
                out.push_str(&format!("- {text}\n"));
            }
        }
    }
    if let Some(answer) = final_answer {
        out.push_str("\n## last final answer\n");
        out.push_str(answer);
        out.push('\n');
    }
    Ok(out)
}

/// Unique `working-state-YYYYMMDD_HHMMSS[.N].md` name for this second.
fn unique_name(dir: &std::path::Path) -> Result<String> {
    let stamp = Utc::now().format("%Y%m%d_%H%M%S");
    let mut candidate = format!("working-state-{stamp}.md");
    let mut sequence = 0u32;
    while dir.join(&candidate).exists() {
        sequence += 1;
        candidate = format!("working-state-{stamp}.{sequence:02}.md");
        ensure!(sequence < 100, "checkpoint name space exhausted");
    }
    Ok(candidate)
}

/// Keep the newest `keep` checkpoint copies by mtime (§5.3: mtime order,
/// path-safe — no shell, no name parsing).
fn prune_checkpoints(dir: &std::path::Path, keep: usize) -> Result<()> {
    let mut copies: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        copies.push((entry.path(), modified));
    }
    copies.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    for (path, _) in copies.iter().skip(keep) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn write_private(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // The 0600 mode rides the open(2) call: create-then-chmod leaves a
    // window where the umask decides the file's permissions (§6.1).
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    Ok(())
}

/// An event-sink wrapper that records working state on compaction and
/// captures the final answer, delegating every event to the real renderer.
/// The runtime stays memory-agnostic — this wrapper lives in the
/// composition root.
pub struct RecordingSink<'a, S: EventSink> {
    inner: &'a mut S,
    recorder: Option<&'a mut Recorder>,
}

impl<'a, S: crate::contracts::EventSink> RecordingSink<'a, S> {
    pub fn new(inner: &'a mut S, recorder: Option<&'a mut Recorder>) -> Self {
        Self { inner, recorder }
    }
}

impl<S: EventSink> EventSink for RecordingSink<'_, S> {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        match &event {
            Event::Compaction(entry) => {
                if let Some(recorder) = self.recorder.as_deref_mut() {
                    recorder.on_compaction(entry)?;
                }
            }
            Event::FinalAnswer(message) => {
                if let Some(recorder) = self.recorder.as_deref_mut() {
                    recorder.capture_final_answer(message);
                }
            }
            _ => {}
        }
        self.inner.emit(event)
    }
}
