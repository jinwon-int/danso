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
    fn tool_call_ids(&self) -> Result<HashSet<String>> {
        Ok(self.scan()?.1.calls.keys().cloned().collect())
    }
    fn supports_compaction(&self) -> bool {
        true
    }
    fn record_compaction(&mut self, summary: Value) -> Result<Value> {
        self.check_recovery()?;
        self.messages()?;
        crate::compaction::validate_summary(&summary, crate::compaction::MAX_SUMMARY_BYTES)?;
        let user_id = self
            .entries
            .iter()
            .rev()
            .find(|e| e["type"] == "message" && e["message"]["role"] == "user")
            .context("compaction requires a user request")?["id"]
            .clone();
        let through = self.entries.last().context("missing journal head")?["id"].clone();
        self.append(
            json!({"type":"custom","customType":"danso.compaction.v1","data":{
            "version":1,"throughId":through,"userEntryId":user_id,"summary":summary}}),
        )
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
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
        ensure!(
            self.file.metadata()?.len() + bytes.len() as u64 <= 16 * 1024 * 1024,
            "session reached 16 MiB journal budget; original records retained"
        );
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
        let (messages, recovery) = self.scan()?;
        recovery.complete()?;
        Ok(messages)
    }

    pub fn check_recovery(&self) -> Result<()> {
        self.scan()?.1.complete()
    }

    fn scan(&self) -> Result<(Vec<Value>, Recovery)> {
        let mut prev = Value::Null;
        let mut messages = vec![];
        let mut latest_user: Option<(&Value, &Value)> = None;
        let mut recovery = Recovery::default();
        for e in &self.entries[1..] {
            ensure!(
                e["parentId"] == prev,
                "branched Pi session is import-only in v0"
            );
            match e["type"].as_str() {
                Some("message") => {
                    let m = &e["message"];
                    recovery.message(m)?;
                    if m["role"] == "user" {
                        latest_user = Some((&e["id"], m));
                    }
                    messages.push(m.clone());
                }
                Some("custom") if e["customType"] == "danso.operation.v1" => {
                    recovery.operation(&e["data"])?
                }
                Some("custom") if e["customType"] == "danso.compaction.v1" => {
                    // Validate the prefix here, not just the journal's final state:
                    // a checkpoint must never hide an in-flight tool batch.
                    recovery.complete()?;
                    let data = &e["data"];
                    let (user_id, _) = latest_user.context("compaction lacks user request")?;
                    ensure!(
                        data["version"] == 1
                            && data["throughId"] == prev
                            && data["userEntryId"] == *user_id,
                        "invalid compaction boundary"
                    );
                    crate::compaction::validate_summary(
                        &data["summary"],
                        crate::compaction::MAX_SUMMARY_BYTES,
                    )?;
                    messages = crate::compaction::checkpoint_messages(&data["summary"], &messages)?;
                }
                Some(
                    "custom" | "model_change" | "thinking_level_change" | "session_info" | "label",
                ) => {}
                _ => bail!("unsupported Pi context entry; import-only in v0"),
            }
            prev = e["id"].clone();
        }
        Ok((messages, recovery))
    }
}

#[derive(Default)]
struct Recovery {
    calls: std::collections::HashMap<String, String>,
    results: HashSet<String>,
    started: HashSet<String>,
    settled: HashSet<String>,
}
impl Recovery {
    fn message(&mut self, m: &Value) -> Result<()> {
        match m["role"].as_str() {
            Some("user" | "toolResult") => {
                ensure!(
                    m["content"].is_string()
                        || m["content"].as_array().is_some_and(|a| a
                            .iter()
                            .all(|b| b["type"] == "text" && b["text"].is_string())),
                    "unsupported text content in journal"
                );
            }
            Some("assistant") => {
                ensure!(
                    m["content"]
                        .as_array()
                        .is_some_and(|a| a.iter().all(|b| b["type"] == "toolCall"
                            || (b["type"] == "text" && b["text"].is_string()))),
                    "unsupported assistant content in journal"
                );
            }
            _ => bail!("unsupported message role in journal"),
        }
        if m["role"] == "assistant" {
            for c in crate::runtime::tool_calls(m)? {
                ensure!(
                    self.calls.insert(c.id, c.name).is_none(),
                    "duplicate tool call id"
                );
            }
        } else if m["role"] == "toolResult" {
            let id = m["toolCallId"].as_str().context("invalid result id")?;
            ensure!(
                self.calls.contains_key(id) && self.results.insert(id.to_owned()),
                "orphan or duplicate tool result"
            );
            if let Some(name) = m["toolName"].as_str() {
                ensure!(self.calls[id] == name, "tool result name mismatch");
            }
        }
        Ok(())
    }
    fn operation(&mut self, d: &Value) -> Result<()> {
        let id = d["toolCallId"].as_str().context("invalid operation id")?;
        ensure!(self.calls.contains_key(id), "orphan operation record");
        match d["state"].as_str() {
            Some("started") => ensure!(
                !self.results.contains(id)
                    && !self.settled.contains(id)
                    && self.started.insert(id.to_owned()),
                "invalid operation start"
            ),
            Some("settled") => ensure!(
                self.results.contains(id)
                    && self.started.remove(id)
                    && self.settled.insert(id.to_owned()),
                "invalid operation settlement"
            ),
            _ => bail!("invalid operation state"),
        }
        Ok(())
    }
    fn complete(&self) -> Result<()> {
        ensure!(
            self.started.is_empty() && self.calls.keys().all(|id| self.results.contains(id)),
            "unresolved tool operation; manual recovery required (never automatically replayed)"
        );
        Ok(())
    }
}
