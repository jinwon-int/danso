//! `load` — read the synced Wiki cache (issue #121 slice 3), ported from the
//! bash `cmd_load`, `resolve_anchor_id`, and `record_load_access` python.
//!
//! Contract kept 1:1 with the bash tool: full-file and `--lines START:END`
//! reads with the same headers, the same exit codes (64 usage · 2 unknown id
//! · 3 ambiguous id · 65 resolver misbehavior · 66 missing path or range past
//! EOF), the `.md` fallback, multi-path reads, traversal refusal, `--id`
//! anchor resolution over the canonical cache, and the awk-NR line count
//! (#128: the last line of a file without a trailing newline is reachable).
//! One deliberate deviation: the bash `load` rsync-syncs the cache first;
//! this read path never syncs (the danso-wiki charter — the real sync is
//! slice 4) and instead warns when a loaded page differs from the indexed
//! snapshot. The shared cache lock is kept, so a concurrent bash sync cannot
//! swap pages in the middle of a multi-path read.

use crate::find;
use crate::manifest;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub struct LoadOptions {
    /// Directory holding `meta.json` / `manifest.jsonl` / `chunks.jsonl`.
    /// Only read for the staleness warning; nothing here is ever written.
    pub index_dir: PathBuf,
    /// The synced wiki cache directory (`pages/`, `aliases.md`, ...).
    pub cache_dir: PathBuf,
    /// Raw `START:END`, parsed with the bash grammar (`--lines 5` means
    /// `5:5`; anything malformed is exit 64).
    pub lines: Option<String>,
    /// Section anchor (`TM-509`, `DOC-110`, `LOG-YYYYMMDD-<node>-<seq>`).
    pub id: Option<String>,
    /// Cache-relative paths, in the order given.
    pub paths: Vec<String>,
}

/// Entry point: prints the pages (or the no-arguments hint block) and returns
/// the bash exit code.
pub fn run(options: &LoadOptions) -> i32 {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    run_into(options, &mut out)
}

fn run_into(options: &LoadOptions, out: &mut impl Write) -> i32 {
    let mut lines = match &options.lines {
        Some(raw) => match parse_load_lines(raw) {
            Ok(range) => Some(range),
            Err(message) => {
                eprintln!("{message}");
                return 64;
            }
        },
        None => None,
    };

    if options.id.is_some() && options.lines.is_some() {
        eprintln!("wiki-agent: --id and --lines are mutually exclusive");
        return 64;
    }
    if options.id.is_some() && !options.paths.is_empty() {
        eprintln!("wiki-agent: --id does not take explicit paths (it resolves the page itself)");
        return 64;
    }

    // The bash load runs `cmd_sync_cache` here. The Rust read path never
    // syncs (charter; slice 4) — the staleness warning after the read keeps
    // the freshness contract visible instead of pretending.
    let _lock = match shared_cache_lock(60) {
        Ok(guard) => guard,
        Err(()) => return 75,
    };

    let mut rel_paths = options.paths.clone();
    if let Some(raw_id) = &options.id {
        let (path, start, end) = match resolve_anchor_id(raw_id, &options.cache_dir) {
            Ok(resolved) => resolved,
            Err(code) => return code,
        };
        // The resolver output is re-validated before it may address a read,
        // exactly like the bash shape check.
        if !resolver_shape_ok(&path, start, end) {
            eprintln!(
                "wiki-agent: --id resolver returned an unexpected result: {path} {start} {end}"
            );
            return 65;
        }
        if parse_load_lines(&format!("{start}:{end}")).is_err() {
            return 65;
        }
        lines = Some((start, end));
        eprintln!("===== [id:{raw_id}] {path} (lines {start}:{end}) =====");
        rel_paths = vec![path];
    }

    if rel_paths.is_empty() {
        // The bash block names the node and the write worktree; this read
        // path has neither (no node identity, and the worktree is a sync
        // concern that lands with slice 4).
        let _ = write!(
            out,
            "read cache: {}

Suggested starting points:
  {}/pages/index.md
  {}/pages/rules.md
  {}/pages/notices/wiki-agent-cache-worktree-rules.md

To read a file:
  wiki-agent load pages/index.md
  wiki-agent load --lines 10:25 pages/notices/wiki-agent-cache-worktree-rules.md
",
            options.cache_dir.display(),
            options.cache_dir.display(),
            options.cache_dir.display(),
            options.cache_dir.display(),
        );
        return 0;
    }

    let mut loaded: Vec<String> = Vec::new();
    for rel in &rel_paths {
        let mut path = match safe_cache_path(&options.cache_dir, rel) {
            Ok(path) => path,
            Err(code) => return code,
        };
        let mut display = rel.clone();
        // Whether the `.md` fallback was even tried — the bash error message
        // names it for every path that could have one, not only when it
        // existed.
        let tried_fallback = !rel.ends_with(".md");
        if !path.is_file() && tried_fallback {
            let fallback = safe_cache_path(&options.cache_dir, &format!("{rel}.md"));
            if let Ok(fallback) = fallback
                && fallback.is_file()
            {
                path = fallback;
                display = format!("{rel}.md");
            }
        }
        if !path.is_file() {
            if tried_fallback {
                eprintln!("wiki-agent: not found in cache: {rel} (also tried: {rel}.md)");
            } else {
                eprintln!("wiki-agent: not found in cache: {rel}");
            }
            return 66;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // Unreadable is un-readable-as-content: the bash awk line count
            // degrades to 0 and every START fails; saying "not found" is the
            // closest honest code (66) without inventing a new one.
            Err(_) => {
                eprintln!("wiki-agent: not found in cache: {rel}");
                return 66;
            }
        };

        if let Some((start, end)) = lines {
            // awk `END {print NR}` counts records separated by `\n`, so the
            // final line of a file without a trailing newline stays
            // reachable (#128). `split_inclusive` has exactly that shape.
            let line_count = bytes.split_inclusive(|&byte| byte == b'\n').count() as u64;
            if start > line_count {
                eprintln!(
                    "wiki-agent: --lines START={start} exceeds file line count ({line_count}): {display}"
                );
                return 66;
            }
            let line_end = end.min(line_count);
            let _ = write!(out, "\n===== {display} (lines {start}:{line_end}) =====\n");
            let _ = out.write_all(&slice_lines(&bytes, start, line_end));
        } else {
            let _ = write!(out, "\n===== {display} =====\n");
            let _ = out.write_all(&bytes);
        }
        loaded.push(display);
    }

    warn_stale_against_manifest(&options.index_dir, &options.cache_dir, &loaded);
    record_load_access(&loaded);
    0
}

fn parse_load_lines(raw: &str) -> Result<(u64, u64), String> {
    if raw.is_empty() {
        return Err("wiki-agent: --lines requires START:END".to_string());
    }
    let malformed =
        || format!("wiki-agent: --lines requires START:END with positive integers; got: {raw}");
    let digits_or_colon = raw.chars().all(|c| c.is_ascii_digit() || c == ':');
    if !digits_or_colon
        || raw.starts_with(':')
        || raw.ends_with(':')
        || raw.matches(':').count() > 1
    {
        return Err(malformed());
    }
    // The bash default when no colon is present: START:START.
    let (start_raw, end_raw) = match raw.split_once(':') {
        Some((start, end)) => (start, end),
        None => (raw, raw),
    };
    let (Ok(start), Ok(end)) = (start_raw.parse::<u64>(), end_raw.parse::<u64>()) else {
        // bash integers are unbounded; this build refuses to guess past u64.
        return Err(malformed());
    };
    if start < 1 || end < start {
        return Err(format!(
            "wiki-agent: --lines START:END must have START >= 1 and END >= START; got: {raw}"
        ));
    }
    Ok((start, end))
}

/// The bash `is_traversal_path` case patterns, verbatim. A plain `.` is
/// intentionally not traversal (bash lets it fall through to not-found).
fn is_traversal_path(rel: &str) -> bool {
    rel.starts_with('/')
        || rel == ".."
        || rel.starts_with("../")
        || rel.contains("/../")
        || rel.ends_with("/..")
        || rel.starts_with("./")
        || rel.contains("/./")
        || rel.ends_with("/.")
}

fn safe_cache_path(cache_dir: &Path, rel: &str) -> Result<PathBuf, i32> {
    if is_traversal_path(rel) {
        eprintln!("wiki-agent: unsafe relative path: {rel}");
        return Err(64);
    }
    Ok(cache_dir.join(rel))
}

/// 1-indexed inclusive byte slice of the `\n`-separated lines, preserving the
/// presence or absence of the final newline exactly like `sed -n a,bp`.
fn slice_lines(bytes: &[u8], start: u64, end: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, line) in bytes.split_inclusive(|&byte| byte == b'\n').enumerate() {
        let number = index as u64 + 1;
        if number < start {
            continue;
        }
        if number > end {
            break;
        }
        out.extend_from_slice(line);
    }
    out
}

static HEADING_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^(#{1,6})\s+(.+?)\s*$").expect("heading regex"));
static LOG_ENTRY_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^- \[(LOG-[A-Za-z0-9-]+)\]\s+\d{4}-\d{2}-\d{2}\s+KST\s+—")
        .expect("log entry regex")
});

struct AnchorCandidate {
    path: String,
    start: u64,
    end: u64,
    title: String,
}

/// Resolve a Wiki section anchor to `(path, start, end)` by scanning the
/// canonical cache — stale semantic-index line maps never redirect an id.
/// Exit codes: 0 unique section; 2 not found; 3 ambiguous; 64 malformed id.
fn resolve_anchor_id(raw: &str, cache_dir: &Path) -> Result<(String, u64, u64), i32> {
    let anchor = raw.strip_prefix('[').unwrap_or(raw);
    let anchor = anchor.strip_suffix(']').unwrap_or(anchor);
    if anchor.is_empty()
        || !anchor
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        eprintln!(
            "wiki-agent: --id expects an anchor like TM-509 / DOC-110 / LOG-20260720-soonwook-8; got: {raw}"
        );
        return Err(64);
    }
    let token = format!("[{anchor}]");

    let mut files = Vec::new();
    find::collect_markdown(cache_dir, &mut files);
    let mut candidates: Vec<AnchorCandidate> = Vec::new();
    for full in files {
        let Ok(content) = std::fs::read_to_string(&full) else {
            continue;
        };
        let rel = full
            .strip_prefix(cache_dir)
            .unwrap_or(&full)
            .to_string_lossy()
            .to_string();
        let lines = find::py_splitlines(&content);
        // (1-based line number, level, title)
        let mut headings: Vec<(usize, usize, &str)> = Vec::new();
        // (1-based line number, id, title)
        let mut log_entries: Vec<(usize, &str, &str)> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if let Some(matched) = HEADING_RE.captures(line) {
                let level = matched.get(1).expect("group 1").len();
                let title = matched.get(2).expect("group 2").as_str();
                headings.push((index + 1, level, title));
            }
            if let Some(matched) = LOG_ENTRY_RE.captures(line) {
                let id = matched.get(1).expect("group 1").as_str();
                // python: line[2:].strip() — drop the leading "- ".
                let title = line[2..].trim();
                log_entries.push((index + 1, id, title));
            }
        }
        for (position, (line_no, level, title)) in headings.iter().enumerate() {
            if !title.trim_start().starts_with(&token) {
                continue;
            }
            // Section end: the line before the next heading of level <= own.
            let mut end = lines.len() as u64;
            for (next_line, next_level, _title) in &headings[position + 1..] {
                if next_level <= level {
                    end = *next_line as u64 - 1;
                    break;
                }
            }
            candidates.push(AnchorCandidate {
                path: rel.clone(),
                start: *line_no as u64,
                end,
                title: (*title).to_string(),
            });
        }
        if anchor.starts_with("LOG-") {
            for (position, (line_no, entry_id, title)) in log_entries.iter().enumerate() {
                if *entry_id != anchor {
                    continue;
                }
                let next_entry = log_entries
                    .get(position + 1)
                    .map(|entry| entry.0)
                    .unwrap_or(lines.len() + 1);
                let next_heading = headings
                    .iter()
                    .map(|heading| heading.0)
                    .find(|line| *line > *line_no)
                    .unwrap_or(lines.len() + 1);
                candidates.push(AnchorCandidate {
                    path: rel.clone(),
                    start: *line_no as u64,
                    end: next_entry.min(next_heading) as u64 - 1,
                    title: (*title).to_string(),
                });
            }
        }
    }

    if candidates.is_empty() {
        eprintln!("wiki-agent: no section found for --id [{anchor}] (canonical cache scan)");
        return Err(2);
    }
    candidates.sort_by(|a, b| {
        (&a.path, a.start, a.end, &a.title).cmp(&(&b.path, b.start, b.end, &b.title))
    });
    if candidates.len() > 1 {
        eprintln!("wiki-agent: --id resolves to multiple definitions for [{anchor}]:");
        for candidate in &candidates {
            eprintln!(
                "candidate: {}:{}:{} {}",
                candidate.path, candidate.start, candidate.end, candidate.title
            );
        }
        return Err(3);
    }
    let candidate = candidates.swap_remove(0);
    Ok((candidate.path, candidate.start, candidate.end))
}

fn resolver_shape_ok(path: &str, start: u64, end: u64) -> bool {
    path.chars()
        .chain(start.to_string().chars())
        .chain(end.to_string().chars())
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
}

/// The bash `acquire_lock cache -s 60` read hold: a shared flock on
/// `<wiki-home>/run/cache.lock` so a concurrent bash sync cannot swap pages
/// mid-read. Proceeding unlocked (no wiki home, unopenable lock file,
/// unexpected flock failure) matches the bash fail-open; a lock held past the
/// timeout is the bash exit 75.
fn shared_cache_lock(timeout_secs: u64) -> Result<Option<std::fs::File>, ()> {
    let Some(home) = wiki_home() else {
        return Ok(None);
    };
    let run_dir = home.join("run");
    if std::fs::create_dir_all(&run_dir).is_err() {
        return Ok(None);
    }
    let lock_path = run_dir.join("cache.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match file.try_lock_shared() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(_) => return Ok(None),
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    eprintln!(
        "wiki-agent: another wiki-agent process is holding the 'cache' lock (waited {timeout_secs}s); retry later"
    );
    Err(())
}

/// The bash home resolution: explicit `WIKI_AGENT_HOME`, then an existing
/// `~/.wiki-agent`, then an existing `~/.openclaw`, then `~/.wiki-agent`.
/// An unset `HOME` yields nothing (a real node always has one).
fn wiki_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("WIKI_AGENT_HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let neutral = home.join(".wiki-agent");
    if neutral.is_dir() {
        return Some(neutral);
    }
    let legacy = home.join(".openclaw");
    if legacy.is_dir() {
        return Some(legacy);
    }
    Some(neutral)
}

/// Warn when a loaded page differs from the snapshot the index searched —
/// the honest substitute for the bash entry sync this path must not do.
/// Best-effort and silent when the manifest is absent or unreadable: the
/// warning must never turn a good read into a failure.
fn warn_stale_against_manifest(index_dir: &Path, cache_dir: &Path, loaded: &[String]) {
    let stale = stale_loaded_paths(index_dir, cache_dir, loaded);
    if stale.is_empty() {
        return;
    }
    let shown: Vec<&str> = stale.iter().map(String::as_str).take(3).collect();
    eprintln!(
        "wiki-agent: {} loaded page(s) differ from the indexed snapshot ({}); the read path never syncs, so search results may be staler than this text (cache sync lands in slice 4)",
        stale.len(),
        shown.join(", ")
    );
}

/// The loaded paths whose cache bytes/mtime no longer match the manifest
/// record the index was built from. Uncovered paths never warn.
fn stale_loaded_paths(index_dir: &Path, cache_dir: &Path, loaded: &[String]) -> Vec<String> {
    let entries = match manifest::load(&index_dir.join("manifest.jsonl")) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let by_path: HashMap<&str, &manifest::ManifestEntry> = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut stale: Vec<String> = Vec::new();
    for rel in loaded {
        let Some(entry) = by_path.get(rel.as_str()) else {
            continue;
        };
        let Ok(path) = safe_cache_path(cache_dir, rel) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Some(mtime) = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64())
        else {
            continue;
        };
        if meta.len() != entry.bytes || (mtime - entry.mtime).abs() > 0.001 {
            stale.push(rel.clone());
        }
    }
    stale
}

/// Access statistics for source-verification loads (issue #59 Phase 3c
/// contract): opt-in via `WIKI_AGENT_ACCESS_STATS=1`, local 0600 JSONL,
/// TTL pruning, 1MB cap, flock-serialized read-modify-write with an atomic
/// 0600 temp-file replace, and it never breaks the command.
fn record_load_access(paths: &[String]) {
    // The bash recorder runs `python3 ... 2>/dev/null || true` — every
    // failure here is swallowed the same way.
    let _ = record_load_access_inner(paths);
}

fn record_load_access_inner(paths: &[String]) -> std::io::Result<()> {
    if std::env::var("WIKI_AGENT_ACCESS_STATS").ok().as_deref() != Some("1") {
        return Ok(());
    }
    if paths.is_empty() {
        return Ok(());
    }
    let stats_path = match std::env::var("WIKI_AGENT_ACCESS_STATS_FILE") {
        Ok(file) if !file.is_empty() => PathBuf::from(file),
        _ => match wiki_home() {
            Some(home) => home.join("wiki-access-stats.jsonl"),
            None => return Ok(()),
        },
    };
    // An unparseable TTL aborts the whole recording, exactly like the bash
    // float() did inside the swallowed heredoc.
    let ttl_days = match std::env::var("WIKI_AGENT_ACCESS_STATS_TTL_DAYS") {
        Ok(raw) => match raw.trim().parse::<f64>() {
            Ok(days) => days.max(1.0),
            Err(_) => return Ok(()),
        },
        Err(_) => 30.0,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let epoch = now.as_secs();
    // bash writes `strftime("%Y-%m-%dT%H:%M:%S%z")` — local time with a
    // numeric offset, which std cannot produce without a tz database. The
    // stamp here is UTC (`Z`); `epoch` stays the authoritative ordering key.
    let entry = json!({
        "action": "load",
        "epoch": epoch,
        "paths": paths.iter().take(5).cloned().collect::<Vec<_>>(),
        "ts": iso_utc(epoch),
    });
    let entry_line = entry.to_string();
    if let Some(parent) = stats_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = stats_path.with_file_name(format!(
        "{}.lock",
        stats_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("wiki-access-stats.jsonl")
    ));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        // The fd is only a flock target — nothing is ever written through
        // it, so append mode keeps clippy's truncate question moot.
        .append(true)
        .mode(0o600)
        .open(&lock_path)?;
    lock.lock()?;
    let outcome = rewrite_stats(
        &stats_path,
        &entry_line,
        now.as_secs_f64() - ttl_days * 86400.0,
    );
    drop(lock);
    outcome
}

fn rewrite_stats(stats_path: &Path, entry_line: &str, cutoff: f64) -> std::io::Result<()> {
    let mut kept: Vec<String> = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(stats_path) {
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(old) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let epoch = old.get("epoch").and_then(Value::as_f64).unwrap_or(0.0);
            if epoch >= cutoff {
                kept.push(line.to_string());
            }
        }
    }
    kept.push(entry_line.to_string());
    while !kept.is_empty() && kept.iter().map(|line| line.len() + 1).sum::<usize>() > 1_000_000 {
        kept.remove(0);
    }
    let tmp_path = stats_path.with_file_name(format!(
        "{}.tmp.{}-{}",
        stats_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("wiki-access-stats.jsonl"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let write_result = (|| {
        let mut tmp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        tmp.write_all(kept.join("\n").as_bytes())?;
        tmp.write_all(b"\n")?;
        drop(tmp);
        std::fs::rename(&tmp_path, stats_path)
    })();
    if write_result.is_err() {
        // Do not litter the wiki home with our own temp file; the bash
        // recorder could not clean up (mkstemp lost inside the swallowed
        // heredoc), this one can.
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result
}

/// `YYYY-MM-DDTHH:MM:SSZ` from a unix epoch — Howard Hinnant's
/// `civil_from_days`, the standard no-dependency UTC conversion.
fn iso_utc(epoch: u64) -> String {
    let days = epoch / 86_400;
    let seconds = epoch % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_097) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    /// Every test in this module serializes on one mutex and points
    /// `WIKI_AGENT_HOME` at a throwaway dir: the lock and the access-stats
    /// recorder read the environment, and `set_var` is both process-global
    /// and `unsafe` in edition 2024.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
    }

    fn test_env() -> EnvGuard {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        let lock = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().expect("temp home");
        unsafe {
            std::env::set_var("WIKI_AGENT_HOME", home.path());
            std::env::remove_var("WIKI_AGENT_ACCESS_STATS");
            std::env::remove_var("WIKI_AGENT_ACCESS_STATS_FILE");
            std::env::remove_var("WIKI_AGENT_ACCESS_STATS_TTL_DAYS");
        }
        EnvGuard {
            _lock: lock,
            _home: home,
        }
    }

    fn options(index: &Path, cache: &Path) -> LoadOptions {
        LoadOptions {
            index_dir: index.to_path_buf(),
            cache_dir: cache.to_path_buf(),
            lines: None,
            id: None,
            paths: Vec::new(),
        }
    }

    /// Index (manifest only — nothing parses it here beyond the staleness
    /// check) plus a cache with a couple of pages.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let index = temp.path().join("index");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(index.join("pages")).expect("index tree");
        std::fs::create_dir_all(cache.join("pages")).expect("cache tree");
        std::fs::write(
            index.join("manifest.jsonl"),
            concat!(
                r#"{"path":"pages/a.md","fileHash":"hash-a","mtime":1758900000.0,"bytes":60,"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"2026-09-27T00:00:00Z"}"#,
                "\n",
            ),
        )
        .expect("manifest");
        std::fs::write(
            cache.join("pages/a.md"),
            "alpha line\nbeta line\ngamma line\n",
        )
        .expect("page a");
        std::fs::write(cache.join("pages/rules.md"), "# Rules\n\nbe kind\n").expect("rules");
        (temp, index, cache)
    }

    #[test]
    fn traversal_cases_match_the_bash_case_patterns() {
        assert!(is_traversal_path("/etc/passwd"));
        assert!(is_traversal_path(".."));
        assert!(is_traversal_path("../x"));
        assert!(is_traversal_path("a/../b"));
        assert!(is_traversal_path("a/.."));
        assert!(is_traversal_path("./x"));
        assert!(is_traversal_path("a/./b"));
        assert!(is_traversal_path("a/."));
        assert!(!is_traversal_path("pages/a.md"));
        assert!(!is_traversal_path("..hidden"));
        assert!(!is_traversal_path("a..b"));
        // A bare "." is not refused by bash either; it lands on not-found.
        assert!(!is_traversal_path("."));
    }

    #[test]
    fn parse_load_lines_matches_the_bash_grammar() {
        assert_eq!(parse_load_lines("10:25").ok(), Some((10, 25)));
        // No colon means START:START, exactly like the bash parameter
        // expansion default.
        assert_eq!(parse_load_lines("7").ok(), Some((7, 7)));
        assert!(parse_load_lines("").is_err());
        assert!(parse_load_lines(":10").is_err());
        assert!(parse_load_lines("10:").is_err());
        assert!(parse_load_lines("1:2:3").is_err());
        assert!(parse_load_lines("0:5").is_err());
        assert!(parse_load_lines("5:1").is_err());
        assert!(parse_load_lines("abc").is_err());
        assert!(parse_load_lines("1:2x").is_err());
        // u64 overflow is malformed here rather than silently unbounded.
        assert!(parse_load_lines("99999999999999999999999:99999999999999999999999").is_err());
    }

    #[test]
    fn load_prints_full_files_and_ranges_with_bash_headers() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut opts = options(&index, &cache);
        opts.paths = vec!["pages/a.md".to_string()];
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\n===== pages/a.md =====\nalpha line\nbeta line\ngamma line\n"));

        opts.lines = Some("2:3".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\n===== pages/a.md (lines 2:3) =====\nbeta line\ngamma line\n"
        );
    }

    #[test]
    fn load_clamps_end_and_rejects_start_past_eof_issue_128() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut opts = options(&index, &cache);
        opts.paths = vec!["pages/a.md".to_string()];

        opts.lines = Some("1:99999".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("(lines 1:3)")
        );

        opts.lines = Some("4:9".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 66);

        // awk NR semantics: the final line of a file without a trailing
        // newline is reachable (#128).
        std::fs::write(cache.join("pages/a.md"), "alpha line\nbeta line").expect("rewrite");
        opts.lines = Some("2:2".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\n===== pages/a.md (lines 2:2) =====\nbeta line"
        );
    }

    #[test]
    fn load_falls_back_to_md_and_reports_missing_paths() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut opts = options(&index, &cache);
        opts.paths = vec!["pages/rules".to_string()];
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        // The header carries the effective (.md) path, like bash display_rel.
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .starts_with("\n===== pages/rules.md =====\n")
        );

        opts.paths = vec!["pages/missing".to_string()];
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 66);

        opts.paths = vec!["pages/missing.md".to_string()];
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 66);
    }

    #[test]
    fn load_multi_path_reads_in_order_and_stops_at_first_failure() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut opts = options(&index, &cache);
        opts.paths = vec!["pages/rules.md".to_string(), "pages/a.md".to_string()];
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let text = String::from_utf8(out).expect("utf8");
        let rules = text
            .find("===== pages/rules.md =====")
            .expect("rules header");
        let a = text.find("===== pages/a.md =====").expect("a header");
        assert!(rules < a);

        opts.paths = vec![
            "pages/rules.md".to_string(),
            "pages/absent.md".to_string(),
            "pages/a.md".to_string(),
        ];
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 66);
        // The first page was already printed before the failure, like bash.
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("pages/rules.md")
        );
    }

    #[test]
    fn load_refuses_traversal_and_mutually_exclusive_flags() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut out: Vec<u8> = Vec::new();

        let mut opts = options(&index, &cache);
        opts.paths = vec!["../escape.md".to_string()];
        assert_eq!(run_into(&opts, &mut out), 64);

        let mut opts = options(&index, &cache);
        opts.lines = Some("1:2".to_string());
        opts.id = Some("TM-509".to_string());
        assert_eq!(run_into(&opts, &mut out), 64);

        let mut opts = options(&index, &cache);
        opts.id = Some("TM-509".to_string());
        opts.paths = vec!["pages/a.md".to_string()];
        assert_eq!(run_into(&opts, &mut out), 64);
    }

    #[test]
    fn load_without_paths_prints_the_hint_block() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&options(&index, &cache), &mut out), 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&format!("read cache: {}", cache.display())));
        assert!(text.contains("Suggested starting points:"));
        assert!(text.contains("wiki-agent load pages/index.md"));
        // --lines without a path is the same hint block, not an error.
        let mut opts = options(&index, &cache);
        opts.lines = Some("1:5".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("Suggested starting points:")
        );
    }

    fn id_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let index = temp.path().join("index");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::create_dir_all(&index).expect("index");
        std::fs::write(
            cache.join("tasks.md"),
            concat!(
                "# Tasks\n",
                "intro\n",
                "## [TM-509] rotate keys\n",
                "body one\n",
                "body two\n",
                "### [TM-510] nested\n",
                "deep\n",
                "## Later section\n",
                "tail\n",
            ),
        )
        .expect("tasks page");
        std::fs::write(
            cache.join("log.md"),
            concat!(
                "# Log\n",
                "\n",
                "- [LOG-20260720-soonwook-8] 2026-07-20 KST — did the thing\n",
                "detail line\n",
                "\n",
                "- [LOG-20260721-soonwook-9] 2026-07-21 KST — next thing\n",
                "more\n",
            ),
        )
        .expect("log page");
        (temp, index, cache)
    }

    #[test]
    fn id_resolves_heading_sections_with_bash_end_rules() {
        let _env = test_env();
        let (_temp, index, cache) = id_fixture();
        let mut opts = options(&index, &cache);
        opts.id = Some("TM-509".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        // Section = the heading through the line before the next heading of
        // level <= own (the level-3 [TM-510] heading does not close it).
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\n===== tasks.md (lines 3:7) =====\n## [TM-509] rotate keys\nbody one\nbody two\n### [TM-510] nested\ndeep\n"
        );

        opts.id = Some("TM-510".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\n===== tasks.md (lines 6:7) =====\n### [TM-510] nested\ndeep\n"
        );
    }

    #[test]
    fn id_resolves_log_bullets_bounded_by_next_entry_or_heading() {
        let _env = test_env();
        let (_temp, index, cache) = id_fixture();
        let mut opts = options(&index, &cache);
        opts.id = Some("LOG-20260720-soonwook-8".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        // next LOG entry line 6 bounds it before any heading would.
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\n===== log.md (lines 3:5) =====\n- [LOG-20260720-soonwook-8] 2026-07-20 KST — did the thing\ndetail line\n\n"
        );
    }

    #[test]
    fn id_exit_codes_follow_the_bash_contract() {
        let _env = test_env();
        let (_temp, index, cache) = id_fixture();
        let mut opts = options(&index, &cache);
        opts.id = Some("TM-9999".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 2);

        opts.id = Some("TM_509".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 64);

        // Two pages defining the same heading anchor are ambiguous.
        std::fs::write(
            cache.join("dupe.md"),
            "# Dupe\n\n## [DOC-9] first\none\n## [DOC-9] second\ntwo\n",
        )
        .expect("dupe page");
        std::fs::write(cache.join("dupe2.md"), "## [DOC-9] elsewhere\nthree\n").expect("dupe2");
        opts.id = Some("DOC-9".to_string());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 3);
    }

    #[test]
    fn stale_pages_are_detected_against_the_manifest() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        // pages/a.md exists in the manifest with bytes=60; the fixture page
        // is 35 bytes — differing bytes mean stale.
        let stale = stale_loaded_paths(&index, &cache, &["pages/a.md".to_string()]);
        assert_eq!(stale, vec!["pages/a.md"]);

        // Make the cache page match the manifest record exactly.
        let path = cache.join("pages/a.md");
        let mut content = std::fs::read_to_string(&path).expect("read");
        while content.len() < 60 {
            content.push('x');
        }
        std::fs::write(&path, &content).expect("rewrite");
        let mtime = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs_f64();
        let manifest_path = index.join("manifest.jsonl");
        std::fs::write(
            &manifest_path,
            format!(
                r#"{{"path":"pages/a.md","fileHash":"hash-a","mtime":{mtime},"bytes":{},"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"2026-09-27T00:00:00Z"}}"#,
                content.len()
            ),
        )
        .expect("manifest");
        let fresh = stale_loaded_paths(&index, &cache, &["pages/a.md".to_string()]);
        assert!(fresh.is_empty());

        // A page the manifest does not cover never warns.
        let uncovered = stale_loaded_paths(&index, &cache, &["pages/rules.md".to_string()]);
        assert!(uncovered.is_empty());
    }

    #[test]
    fn access_stats_recording_follows_the_opt_in_contract() {
        let _env = test_env();
        let stats = tempfile::tempdir().expect("stats dir");
        let stats_path = stats.path().join("wiki-access-stats.jsonl");
        unsafe { std::env::set_var("WIKI_AGENT_ACCESS_STATS_FILE", &stats_path) };

        // Opt-out by default: nothing is written.
        record_load_access(&["pages/a.md".to_string()]);
        assert!(!stats_path.exists());

        unsafe { std::env::set_var("WIKI_AGENT_ACCESS_STATS", "1") };
        let paths: Vec<String> = (1..=7).map(|n| format!("pages/p{n}.md")).collect();
        record_load_access(&paths);
        let raw = std::fs::read_to_string(&stats_path).expect("stats");
        let entry: Value = serde_json::from_str(raw.trim()).expect("entry json");
        assert_eq!(entry["action"], "load");
        assert_eq!(
            entry["paths"],
            json!([
                "pages/p1.md",
                "pages/p2.md",
                "pages/p3.md",
                "pages/p4.md",
                "pages/p5.md"
            ]),
            "only the first 5 paths are recorded"
        );
        assert!(entry["epoch"].as_u64().is_some());
        assert!(entry["ts"].as_str().is_some());
        // Keys come out sorted (bash `sort_keys=True`).
        assert!(raw.trim().starts_with("{\"action\":\"load\",\"epoch\":"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&stats_path)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        // TTL pruning drops older-than-cutoff entries and malformed lines.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("now")
            .as_secs();
        std::fs::write(
            &stats_path,
            format!(
                "{{\"action\":\"load\",\"epoch\":{},\"paths\":[\"pages/old\"]}}\nnot json\n{{\"action\":\"load\",\"epoch\":{},\"paths\":[\"pages/fresh\"]}}\n",
                now - 10 * 86_400,
                now
            ),
        )
        .expect("seed");
        unsafe { std::env::set_var("WIKI_AGENT_ACCESS_STATS_TTL_DAYS", "1") };
        record_load_access(&["pages/new.md".to_string()]);
        let raw = std::fs::read_to_string(&stats_path).expect("stats");
        assert!(raw.contains("pages/new.md"));
        assert!(
            raw.contains("pages/fresh"),
            "entries newer than the cutoff survive"
        );
        assert!(
            !raw.contains("pages/old"),
            "entries older than the cutoff are pruned"
        );
        assert_eq!(raw.lines().count(), 2, "malformed lines are dropped");

        // The 1MB cap drops from the front.
        let big = format!(
            "{{\"action\":\"load\",\"epoch\":{now},\"paths\":[\"{}\"]}}",
            "x".repeat(600_000)
        );
        std::fs::write(&stats_path, format!("{big}\n{big}\n")).expect("seed big");
        unsafe { std::env::remove_var("WIKI_AGENT_ACCESS_STATS_TTL_DAYS") };
        record_load_access(&["pages/cap.md".to_string()]);
        let raw = std::fs::read_to_string(&stats_path).expect("stats");
        assert!(
            raw.len() <= 1_000_000 + 2,
            "cap enforced, got {} bytes",
            raw.len()
        );
        assert_eq!(
            raw.lines().count(),
            2,
            "the oldest oversized line was dropped first"
        );
        assert!(raw.contains("pages/cap.md"));
    }

    #[test]
    fn shared_cache_lock_creates_and_takes_the_bash_lock_file() {
        let _env = test_env();
        let (_temp, index, cache) = fixture();
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&options(&index, &cache), &mut out), 0);
        let lock_path = wiki_home().expect("home").join("run").join("cache.lock");
        assert!(lock_path.exists(), "the bash-shaped lock file exists");
    }
}
