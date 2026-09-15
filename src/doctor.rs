//! Read-only installation diagnostics for `danso doctor` (roadmap #33 C1).
//!
//! This module deliberately has no service constructors or write-capable
//! helpers in its call graph.  It resolves the same paths as the existing
//! configuration and Telegram modules, then uses metadata, bounded reads and
//! directory scans only.  Diagnostic details use a closed vocabulary and
//! counts; source records, tokens and error text are never rendered.

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata},
    io::{self, Read},
    path::Path,
};

use crate::{config, memory, telegram};

const HEALTH_FILE_NAME: &str = "health.json";
const TOKEN_LOCK_FILE_NAME: &str = ".telegram-token.lock";
const CONVERSATIONS_DIR_NAME: &str = "conversations";
const AUDIT_FILE_NAME: &str = "audit.jsonl";
const DISTILL_DIR_NAME: &str = "distill-journal";
const HEALTH_MAX_BYTES: u64 = 64 * 1024;
const MAX_SCAN_ENTRIES: usize = 4096;
const MAX_FACTS_READ_BYTES: u64 = memory::facts::MAX_FACTS_FILE_BYTES;

/// The command has no flags: diagnostics are always JSON and always read-only.
#[derive(Debug, Parser)]
#[command(
    name = "danso doctor",
    about = "Inspect Danso state without locks, writes, providers, or network access"
)]
pub struct DoctorArgs {}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    version: u8,
    generated_at: String,
    checks: Vec<Check>,
    summary: Summary,
}

#[derive(Clone, Debug, Serialize)]
struct Check {
    id: String,
    status: &'static str,
    detail: String,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Summary {
    ok: usize,
    warn: usize,
    fail: usize,
}

impl DoctorReport {
    /// The doctor contract treats warnings as successful completion.  A
    /// failure means the report ran and found at least one failed check.
    pub fn exit_code(&self) -> i32 {
        if self.summary.fail == 0 { 0 } else { 1 }
    }
}

impl Summary {
    fn from_checks(checks: &[Check]) -> Self {
        let mut summary = Self::default();
        for check in checks {
            match check.status {
                "ok" => summary.ok += 1,
                "warn" => summary.warn += 1,
                "fail" => summary.fail += 1,
                _ => unreachable!("doctor status is closed over three values"),
            }
        }
        summary
    }
}

impl Check {
    fn ok(id: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: "ok",
            detail: detail.into(),
        }
    }

    fn warn(id: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: "warn",
            detail: detail.into(),
        }
    }

    fn fail(id: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: "fail",
            detail: detail.into(),
        }
    }
}

/// Resolve process configuration and build one complete report.
pub fn run() -> Result<DoctorReport> {
    // `config::home` is the authoritative DANSO_HOME/HOME resolver.  If it
    // cannot run, the doctor itself cannot establish the installation root;
    // the CLI maps this one condition to exit 2 without rendering the error.
    let home = config::home().map_err(|_| anyhow::anyhow!("doctor home unavailable"))?;
    let telegram_data_dir = telegram::data_dir_from_env().ok();
    Ok(inspect_at(&home, telegram_data_dir.as_deref(), Utc::now()))
}

/// Build a report for explicit fixture paths.  The production command calls
/// [`run`]; keeping this path explicit makes the read-only behavior testable
/// without mutating process-wide environment variables.
pub fn inspect_at(
    home: &Path,
    telegram_data_dir: Option<&Path>,
    generated_at: DateTime<Utc>,
) -> DoctorReport {
    let config_path = home.join(config::FILE_NAME);
    let (config_check, parsed_config) = inspect_config(&config_path);
    let memory_dir = parsed_config
        .as_ref()
        .and_then(|config| config.memory.dir.clone())
        .unwrap_or_else(memory::MemoryConfig::default_root);

    // The nine checks are emitted in this documented order (docs/doctor.md).
    let checks = vec![
        config_check,
        config_permissions(&config_path),
        home_layout(home, &memory_dir),
        telegram_data_dir_check(telegram_data_dir),
        telegram_health(telegram_data_dir, generated_at),
        telegram_token_lock(telegram_data_dir),
        telegram_token_file(parsed_config.as_ref()),
        telegram_conversations(telegram_data_dir),
        memory_store(&memory_dir),
    ];
    let summary = Summary::from_checks(&checks);

    DoctorReport {
        version: 1,
        generated_at: generated_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        summary,
        checks,
    }
}

fn inspect_config(path: &Path) -> (Check, Option<config::Config>) {
    // Keep this call on the existing config check path.  Its report is already
    // a body-free projection, so it is safe to carry forward as a detail.
    match config::check(Some(path)) {
        Ok(report) => match config::Config::load(path) {
            Ok(parsed) => (
                Check::ok(
                    "config.parse",
                    format!(
                        "config valid; report={}",
                        serde_json::to_string(&report).expect("config report is serializable")
                    ),
                ),
                Some(parsed),
            ),
            Err(_) => (Check::fail("config.parse", "config invalid"), None),
        },
        Err(_) if is_missing(path) => (Check::fail("config.parse", "config file missing"), None),
        Err(_) => (Check::fail("config.parse", "config invalid"), None),
    }
}

fn config_permissions(path: &Path) -> Check {
    let id = "config.permissions";
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Check::warn(id, "config file missing");
        }
        Err(_) => return Check::warn(id, "config permissions unreadable"),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Check::warn(id, "config is not a regular file");
    }
    let Some(mode) = unix_mode(&metadata) else {
        return Check::warn(id, "config permissions unavailable");
    };
    if mode == 0o600 {
        Check::ok(id, "config mode 0600")
    } else if mode & 0o077 != 0 {
        Check::warn(id, "config group/world readable")
    } else {
        Check::warn(id, "config mode not 0600")
    }
}

fn home_layout(home: &Path, memory_dir: &Path) -> Check {
    let home_present = is_directory(home);
    let memory_present = is_directory(memory_dir);
    match (home_present, memory_present) {
        (true, true) => Check::ok("home.layout", "home layout ok"),
        (false, true) => Check::warn("home.layout", "home directory missing"),
        (true, false) => Check::warn("home.layout", "memory directory missing"),
        (false, false) => Check::warn("home.layout", "home and memory directories missing"),
    }
}

fn telegram_data_dir_check(data_dir: Option<&Path>) -> Check {
    let id = "telegram.data_dir";
    let Some(data_dir) = data_dir else {
        return Check::warn(id, "telegram data directory unavailable");
    };
    match fs::symlink_metadata(data_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Check::ok(id, "telegram data directory present")
        }
        Ok(_) => Check::warn(id, "telegram data directory unavailable"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Check::warn(id, "telegram data directory missing")
        }
        Err(_) => Check::warn(id, "telegram data directory unavailable"),
    }
}

struct HealthStats {
    schema_version: i64,
    service_pid_present: bool,
    started_age_seconds: u64,
    last_poll_age_seconds: u64,
}

fn telegram_health(data_dir: Option<&Path>, now: DateTime<Utc>) -> Check {
    let id = "telegram.health";
    let Some(data_dir) = data_dir else {
        return Check::warn(id, "health file missing");
    };
    let path = data_dir.join(HEALTH_FILE_NAME);
    let payload = match read_bounded(&path, HEALTH_MAX_BYTES) {
        Ok(Some(payload)) => payload,
        Ok(None) => return Check::warn(id, "health file missing"),
        Err(_) => return Check::warn(id, "health file unparseable"),
    };
    let value: Value = match serde_json::from_slice(&payload) {
        Ok(value) => value,
        Err(_) => return Check::warn(id, "health file unparseable"),
    };
    let Some(schema_version) = value.get("schema_version").and_then(Value::as_i64) else {
        return Check::warn(id, "health file unparseable");
    };
    if schema_version != 1 {
        return Check::warn(id, "health file unparseable");
    }
    let Some(started_at) = value
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(parse_timestamp)
    else {
        return Check::warn(id, "health file unparseable");
    };
    let Some(last_poll_at) = value
        .get("last_poll_at")
        .and_then(Value::as_str)
        .and_then(parse_timestamp)
    else {
        return Check::warn(id, "health file unparseable");
    };
    let stats = HealthStats {
        schema_version,
        service_pid_present: value
            .get("service_pid")
            .is_some_and(|value| value.is_number()),
        started_age_seconds: age_seconds(&now, &started_at),
        last_poll_age_seconds: age_seconds(&now, &last_poll_at),
    };
    let category = if stats.last_poll_age_seconds > 600 {
        "health stale"
    } else {
        "health ok"
    };
    let detail = format!(
        "{category}; schema_version={}; service_pid_present={}; started_age_seconds={}; last_poll_age_seconds={}",
        stats.schema_version,
        stats.service_pid_present,
        stats.started_age_seconds,
        stats.last_poll_age_seconds
    );
    if stats.last_poll_age_seconds > 600 {
        Check::warn(id, detail)
    } else {
        Check::ok(id, detail)
    }
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn age_seconds(now: &DateTime<Utc>, timestamp: &DateTime<Utc>) -> u64 {
    let seconds = now.timestamp().saturating_sub(timestamp.timestamp());
    seconds.max(0) as u64
}

fn telegram_token_lock(data_dir: Option<&Path>) -> Check {
    let id = "telegram.token_lock";
    let Some(data_dir) = data_dir else {
        return Check::ok(id, "token lock unavailable");
    };
    let path = data_dir.join(TOKEN_LOCK_FILE_NAME);
    match fs::symlink_metadata(path) {
        Ok(_) => Check::ok(id, "token lock present"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // The lock file is retained by normal service operation, but its
            // absence is not itself a failure and must never trigger locking.
            Check::ok(id, "token lock absent")
        }
        Err(_) => Check::warn(id, "token lock status unavailable"),
    }
}

fn telegram_token_file(parsed_config: Option<&config::Config>) -> Check {
    let id = "telegram.token_file";
    let Some(path) = parsed_config.and_then(|config| config.telegram.token_file.as_deref()) else {
        return Check::ok(id, "token file not configured");
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Check::warn(id, "token file missing");
        }
        Err(_) => return Check::warn(id, "token file unavailable"),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Check::warn(id, "token file is not a regular file");
    }
    let Some(mode) = unix_mode(&metadata) else {
        return Check::warn(id, "token file permissions unavailable");
    };
    if mode & 0o077 == 0 {
        Check::ok(id, "token file present; owner-only")
    } else {
        Check::warn(id, "token file permissions unsafe")
    }
}

fn telegram_conversations(data_dir: Option<&Path>) -> Check {
    let id = "telegram.conversations";
    let Some(data_dir) = data_dir else {
        return Check::warn(id, "conversation scan error");
    };
    let directory = data_dir.join(CONVERSATIONS_DIR_NAME);
    match scan_json_files(&directory) {
        Ok((count, bytes)) => Check::ok(
            id,
            format!("conversations ok; count={count}; bytes={bytes}"),
        ),
        Err(_) => Check::warn(id, "conversation scan error"),
    }
}

#[derive(Default)]
struct ScopeStats {
    name: String,
    layout_ok: bool,
    facts_lines: Option<u64>,
    facts_bytes: Option<u64>,
    pending_jobs: Option<u64>,
    audit_bytes: Option<u64>,
}

fn memory_store(memory_dir: &Path) -> Check {
    let id = "memory.store";
    let root_metadata = match fs::symlink_metadata(memory_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Check::warn(id, "memory store missing");
        }
        Err(_) => return Check::warn(id, "memory store unreadable"),
    };
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Check::warn(id, "memory store unavailable");
    }

    let entries = match fs::read_dir(memory_dir) {
        Ok(entries) => entries,
        Err(_) => return Check::warn(id, "memory store unreadable"),
    };
    let mut scope_names = BTreeSet::new();
    let mut scan_error = false;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            scan_error = true;
            break;
        }
        let Ok(entry) = entry else {
            scan_error = true;
            break;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !memory::paths::valid_scope(name) {
            continue;
        }
        let path = entry.path();
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                scope_names.insert(name.to_string());
            }
            Ok(_) => {}
            Err(_) => scan_error = true,
        }
    }
    if scan_error {
        return Check::warn(id, "memory store unreadable");
    }
    if scope_names.is_empty() {
        return Check::warn(id, "memory store layout missing");
    }

    let mut stats = Vec::with_capacity(scope_names.len());
    let mut warnings = BTreeSet::new();
    for name in scope_names {
        let scope_dir = memory_dir.join(&name);
        let state_dir = scope_dir.join("state");
        let memories_dir = scope_dir.join("memories");
        let layout_ok = is_directory(&state_dir) && is_directory(&memories_dir);
        if !layout_ok {
            warnings.insert("layout");
        }

        let facts_path = state_dir.join(memory::FACTS_FILE_NAME);
        let (facts_lines, facts_bytes) = match read_bounded(&facts_path, MAX_FACTS_READ_BYTES) {
            Ok(Some(payload)) => (Some(count_lines(&payload)), Some(payload.len() as u64)),
            Ok(None) => (None, None),
            Err(_) => {
                warnings.insert("facts");
                (None, None)
            }
        };

        let distill_dir = state_dir.join(DISTILL_DIR_NAME);
        let pending_jobs = match count_pending_jobs(&distill_dir) {
            Ok(count) => Some(count),
            Err(_) => {
                warnings.insert("distill");
                None
            }
        };

        let audit_path = state_dir.join(AUDIT_FILE_NAME);
        let audit_bytes = match optional_regular_size(&audit_path) {
            Ok(size) => size,
            Err(_) => {
                warnings.insert("audit");
                None
            }
        };

        stats.push(ScopeStats {
            name,
            layout_ok,
            facts_lines,
            facts_bytes,
            pending_jobs,
            audit_bytes,
        });
    }

    let had_warnings = !warnings.is_empty();
    let mut detail = if had_warnings {
        format!(
            "memory store warning; unreadable={}",
            warnings.into_iter().collect::<Vec<_>>().join(",")
        )
    } else {
        "memory store ok".to_string()
    };
    for scope in stats {
        let facts = match (scope.facts_lines, scope.facts_bytes) {
            (Some(lines), Some(bytes)) => format!("facts_lines={lines};facts_bytes={bytes}"),
            _ => "facts=absent".to_string(),
        };
        let pending = scope.pending_jobs.map_or_else(
            || "pending_jobs=unreadable".to_string(),
            |count| format!("pending_jobs={count}"),
        );
        let audit = scope.audit_bytes.map_or_else(
            || "audit=absent".to_string(),
            |bytes| format!("audit_bytes={bytes}"),
        );
        let layout = if scope.layout_ok {
            "layout=present"
        } else {
            "layout=incomplete"
        };
        let scope_name = scope.name;
        detail.push_str(&format!(
            "; scope={scope_name}[{layout};{facts};{pending};{audit}]"
        ));
    }
    if had_warnings {
        Check::warn(id, detail)
    } else {
        Check::ok(id, detail)
    }
}

fn count_pending_jobs(path: &Path) -> io::Result<u64> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "distill queue is not a directory",
            ));
        }
        Ok(_) => {}
    }
    let mut count = 0_u64;
    for (index, entry) in fs::read_dir(path)?.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            return Err(io::Error::other("distill queue scan bound exceeded"));
        }
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".json") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            count = count
                .checked_add(1)
                .ok_or_else(|| io::Error::other("distill count overflow"))?;
        }
    }
    Ok(count)
}

fn scan_json_files(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "conversation path is not a directory",
        ));
    }
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for (index, entry) in fs::read_dir(path)?.enumerate() {
        if index >= MAX_SCAN_ENTRIES {
            return Err(io::Error::other("conversation scan bound exceeded"));
        }
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".json") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "conversation entry is not a regular file",
            ));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("conversation count overflow"))?;
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(|| io::Error::other("conversation size overflow"))?;
    }
    Ok((count, bytes))
}

fn count_lines(payload: &[u8]) -> u64 {
    if payload.is_empty() {
        return 0;
    }
    let newline_count = payload.iter().filter(|byte| **byte == b'\n').count() as u64;
    if payload.last() == Some(&b'\n') {
        newline_count
    } else {
        newline_count + 1
    }
}

fn optional_regular_size(path: &Path) -> io::Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(metadata.len()))
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "audit path is not a regular file",
        )),
    }
}

fn read_bounded(path: &Path, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "diagnostic input is not a regular file",
        ));
    }
    let mut file = File::open(path)?;
    let mut payload = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut payload)?;
    if payload.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "diagnostic input exceeds its read bound",
        ));
    }
    Ok(Some(payload))
}

fn is_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    )
}

fn is_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn unix_mode(metadata: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_root() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("danso-doctor-{suffix}"));
        fs::create_dir_all(&path).expect("temporary doctor root");
        path
    }

    fn private_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("fixture file");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("fixture mode");
    }

    fn private_dir(path: &Path) {
        fs::create_dir_all(path).expect("fixture directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("fixture mode");
    }

    fn fixture() -> (PathBuf, PathBuf, DateTime<Utc>) {
        let root = temp_root();
        let home = root.join("home");
        let memory_dir = root.join("memory");
        let telegram_dir = root.join("telegram");
        private_dir(&home);
        private_dir(&memory_dir);
        private_dir(&telegram_dir);
        private_dir(&telegram_dir.join(CONVERSATIONS_DIR_NAME));
        private_dir(&memory_dir.join("global/memories"));
        private_dir(&memory_dir.join("global/state"));
        private_dir(&memory_dir.join("global/state/distill-journal"));
        let token_file = root.join("telegram.token");
        private_file(&token_file, b"TEST_TOKEN_SECRET_MARKER");
        let config = format!(
            "[memory]\ndir = \"{}\"\n[telegram]\ntoken_file = \"{}\"\n",
            memory_dir.display(),
            token_file.display()
        );
        private_file(&home.join(config::FILE_NAME), config.as_bytes());
        private_file(
            &memory_dir.join("global/state/memory-facts.jsonl"),
            b"{\"id\":\"fixture\",\"text\":\"TEST_MEMORY_SECRET_MARKER\"}\n{\"id\":\"fixture-2\",\"text\":\"body\"}\n",
        );
        private_file(
            &memory_dir.join("global/state/distill-journal/job.json"),
            br#"{"job_id":"fixture-job","session_path":"/private/session.jsonl"}"#,
        );
        private_file(&memory_dir.join("global/state/audit.jsonl"), b"audit\n");
        private_file(
            &telegram_dir.join(HEALTH_FILE_NAME),
            br#"{"schema_version":1,"started_at":"2026-09-15T00:00:00Z","last_poll_at":"2026-09-15T00:00:00Z","service_pid":42}"#,
        );
        private_file(&telegram_dir.join(TOKEN_LOCK_FILE_NAME), b"");
        private_file(
            &telegram_dir.join(CONVERSATIONS_DIR_NAME).join("1.json"),
            br#"{"chat_id":1}"#,
        );
        let now = DateTime::parse_from_rfc3339("2026-09-15T00:01:00Z")
            .expect("fixture time")
            .with_timezone(&Utc);
        (home, telegram_dir, now)
    }

    #[test]
    fn valid_fixture_is_body_free_and_has_counts() {
        let (home, telegram_dir, now) = fixture();
        let report = inspect_at(&home, Some(&telegram_dir), now);
        let rendered = serde_json::to_string(&report).expect("report JSON");
        assert!(!rendered.contains("TEST_TOKEN_SECRET_MARKER"));
        assert!(!rendered.contains("TEST_MEMORY_SECRET_MARKER"));
        assert_eq!(report.summary.fail, 0);
        assert!(rendered.contains("facts_lines=2"));
        assert!(rendered.contains("pending_jobs=1"));
        assert!(rendered.contains("audit_bytes=6"));
        assert!(rendered.contains("count=1"));
        assert!(rendered.contains("schema_version=1"));
        assert!(rendered.contains("service_pid_present=true"));
        let _ = fs::remove_dir_all(home.parent().expect("fixture parent"));
    }

    #[test]
    fn unsafe_config_and_token_modes_warn_without_reading_contents() {
        let (home, telegram_dir, now) = fixture();
        let config_path = home.join(config::FILE_NAME);
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o640)).expect("config mode");
        let token_path = home
            .parent()
            .expect("fixture parent")
            .join("telegram.token");
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644)).expect("token mode");
        let report = inspect_at(&home, Some(&telegram_dir), now);
        let config = report
            .checks
            .iter()
            .find(|check| check.id == "config.permissions")
            .expect("config permissions check");
        let token = report
            .checks
            .iter()
            .find(|check| check.id == "telegram.token_file")
            .expect("token check");
        assert_eq!(config.status, "warn");
        assert_eq!(token.status, "warn");
        assert_eq!(report.exit_code(), 0);
        let _ = fs::remove_dir_all(home.parent().expect("fixture parent"));
    }

    #[test]
    fn missing_and_invalid_config_are_failures_without_error_text() {
        let root = temp_root();
        let home = root.join("home");
        private_dir(&home);
        let report = inspect_at(&home, None, Utc::now());
        let rendered = serde_json::to_string(&report).expect("report JSON");
        assert!(rendered.contains("config file missing"));

        private_file(
            &home.join(config::FILE_NAME),
            b"[telegram]\ntokn = \"SECRET\"\n",
        );
        let report = inspect_at(&home, None, Utc::now());
        let rendered = serde_json::to_string(&report).expect("report JSON");
        assert!(rendered.contains("config invalid"));
        assert!(!rendered.contains("SECRET"));
        assert_eq!(report.exit_code(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn health_categories_are_fixed() {
        let (home, telegram_dir, now) = fixture();
        let health = telegram_dir.join(HEALTH_FILE_NAME);
        fs::remove_file(&health).expect("remove health fixture");
        let report = inspect_at(&home, Some(&telegram_dir), now);
        let missing = report
            .checks
            .iter()
            .find(|check| check.id == "telegram.health")
            .expect("health check");
        assert_eq!(missing.detail, "health file missing");

        private_file(&health, b"not-json");
        let report = inspect_at(&home, Some(&telegram_dir), now);
        let invalid = report
            .checks
            .iter()
            .find(|check| check.id == "telegram.health")
            .expect("health check");
        assert_eq!(invalid.detail, "health file unparseable");

        private_file(
            &health,
            br#"{"schema_version":1,"started_at":"2026-09-01T00:00:00Z","last_poll_at":"2026-09-01T00:00:00Z","service_pid":42}"#,
        );
        let report = inspect_at(&home, Some(&telegram_dir), now);
        let stale = report
            .checks
            .iter()
            .find(|check| check.id == "telegram.health")
            .expect("health check");
        assert!(stale.detail.starts_with("health stale"));
        assert_eq!(stale.status, "warn");
        let _ = fs::remove_dir_all(home.parent().expect("fixture parent"));
    }

    #[test]
    fn count_lines_handles_unterminated_last_line() {
        assert_eq!(count_lines(b"one\ntwo\n"), 2);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b""), 0);
        let now = Utc::now();
        assert_eq!(age_seconds(&now, &now), 0);
    }
}
