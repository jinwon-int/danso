//! Pi JSONL is interchange; custom operation records gate execution on recovery.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub struct Session {
    file: File,
    pub entries: Vec<Value>,
}

impl crate::contracts::SessionStore for Session {
    fn header(&self) -> &Value {
        &self.entries[0]
    }
    fn messages(&self) -> Result<Vec<Value>> {
        Session::messages(self)
    }
    fn check_recovery(&self) -> Result<()> {
        Session::check_recovery(self)
    }
    fn append_message(&mut self, message: Value) -> Result<Value> {
        self.message(message)
    }
    fn record_operation(
        &mut self,
        id: &str,
        state: crate::contracts::OperationState,
    ) -> Result<()> {
        self.operation(id, state.as_str())
    }
}

impl Session {
    pub fn open(path: &Path, cwd: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path).context("open session")?;
        file.try_lock_exclusive()
            .context("session is already in use")?;
        ensure!(file.metadata()?.is_file(), "session must be a regular file");
        ensure!(
            file.metadata()?.len() <= 16 * 1024 * 1024,
            "session exceeds 16 MiB import budget"
        );
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        // Never guess whether a torn record described an executed side effect.
        ensure!(
            data.is_empty() || data.ends_with('\n'),
            "incomplete session record; manual recovery required"
        );
        let entries = data
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<Value>, _>>()?;
        let mut s = Self { file, entries };
        if s.entries.is_empty() {
            s.persist(json!({"type":"session", "version":3, "id":uuid::Uuid::new_v4().to_string(), "timestamp":now(), "cwd":cwd}))?;
            // Persist the new directory entry before any tool can run.
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
        }
        s.validate()?;
        ensure!(
            s.entries[0]["cwd"].as_str() == cwd.to_str(),
            "session cwd mismatch"
        );
        Ok(s)
    }

    fn validate(&self) -> Result<()> {
        let h = &self.entries[0];
        ensure!(
            h["type"] == "session" && h["version"] == 3,
            "expected Pi session v3"
        );
        ensure!(
            h["id"].is_string() && h["timestamp"].is_string(),
            "invalid session header"
        );
        let mut ids = HashSet::new();
        for e in &self.entries[1..] {
            ensure!(e["type"] != "session", "duplicate session header");
            let id = e["id"].as_str().context("missing entry id")?;
            ensure!(!ids.contains(id), "duplicate entry id");
            ensure!(e.get("parentId").is_some(), "missing parentId");
            if let Some(parent) = e["parentId"].as_str() {
                ensure!(ids.contains(parent), "missing parent entry");
            } else {
                ensure!(e["parentId"].is_null(), "invalid parentId");
            }
            ids.insert(id);
        }
        Ok(())
    }

    fn persist(&mut self, entry: Value) -> Result<Value> {
        let mut bytes = serde_json::to_vec(&entry)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    pub fn append(&mut self, mut entry: Value) -> Result<Value> {
        let id = loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            if !self.entries.iter().any(|e| e["id"] == id) {
                break id;
            }
        };
        entry["id"] = json!(id);
        entry["parentId"] = if self.entries.len() > 1 {
            self.entries.last().unwrap()["id"].clone()
        } else {
            Value::Null
        };
        entry["timestamp"] = json!(now());
        self.persist(entry)
    }

    pub fn message(&mut self, message: Value) -> Result<Value> {
        self.append(json!({"type":"message", "message":message}))
    }

    pub fn operation(&mut self, id: &str, state: &str) -> Result<()> {
        self.append(json!({"type":"custom", "customType":"danso.operation.v1", "data":{"toolCallId":id, "state":state}}))?;
        Ok(())
    }

    pub fn messages(&self) -> Result<Vec<Value>> {
        // v0 supports linear transcripts only; preserve other Pi entries but do
        // not silently flatten a branch/compaction into incorrect model context.
        let mut prev = Value::Null;
        let mut messages = vec![];
        for e in &self.entries[1..] {
            ensure!(
                e["parentId"] == prev,
                "branched Pi session is import-only in v0"
            );
            prev = e["id"].clone();
            match e["type"].as_str() {
                Some("message") => messages.push(e["message"].clone()),
                Some(
                    "custom" | "model_change" | "thinking_level_change" | "session_info" | "label",
                ) => {}
                _ => bail!("unsupported Pi context entry; import-only in v0"),
            }
        }
        Ok(messages)
    }

    pub fn check_recovery(&self) -> Result<()> {
        let mut unresolved = HashSet::new();
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
        for e in &self.entries[1..] {
            if e["customType"] == "danso.operation.v1" {
                let id = e["data"]["toolCallId"]
                    .as_str()
                    .context("invalid operation id")?;
                match e["data"]["state"].as_str() {
                    Some("started") => {
                        unresolved.insert(id);
                    }
                    Some("settled") => {
                        unresolved.remove(id);
                    }
                    _ => bail!("invalid operation state"),
                }
            }
            let m = &e["message"];
            if m["role"] == "assistant" {
                if let Some(blocks) = m["content"].as_array() {
                    for b in blocks.iter().filter(|b| b["type"] == "toolCall") {
                        let id = b["id"].as_str().context("invalid tool call")?;
                        ensure!(calls.insert(id), "duplicate tool call id");
                    }
                }
            } else if m["role"] == "toolResult" {
                results.insert(m["toolCallId"].as_str().context("invalid result id")?);
            }
        }
        ensure!(
            unresolved.is_empty() && calls.is_subset(&results),
            "unresolved tool operation; manual recovery required (never automatically replayed)"
        );
        Ok(())
    }
}
