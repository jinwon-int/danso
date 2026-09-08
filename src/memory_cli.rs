//! `danso memory <init|add|search|close|show|eval>` — the standalone M1 CLI
//! over the native memory store (issue #52 §8). No provider, Python, Node,
//! or ccc-node runtime is involved; errors are body-free and fail closed.
//! Unspecified flags default to memory OFF semantics: without a scope the
//! `global` tree under `$DANSO_MEMORY_DIR` (or `~/.danso/memory`) is used.

use clap::{Parser, Subcommand};
use danso::memory::{self, Route, eval, facts, paths, recall};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "danso memory",
    about = "Native local memory: init, add, search, close, show, eval"
)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub command: Command,
    /// Memory root directory; scope directories live below it (§3).
    #[arg(long, global = true)]
    pub memory_dir: Option<PathBuf>,
    /// Scope name: global | shared | private-<32 hex> (§7).
    #[arg(long, global = true)]
    pub scope: Option<String>,
    /// Exclusive-lock deadline in milliseconds (fail closed on timeout).
    #[arg(long, global = true, default_value_t = paths::LOCK_TIMEOUT_DEFAULT_MS)]
    pub lock_timeout_ms: u64,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create the scope tree (0700) with MEMORY.md/USER.md templates.
    Init,
    /// Store one manual fact through the write gates (review "manual").
    Add {
        /// One of the seven fact kinds.
        #[arg(long)]
        kind: String,
        /// Fact text (a single line; newlines are rejected).
        #[arg(long)]
        text: String,
        /// Required reason when kind is decision (§4.1).
        #[arg(long)]
        because: Option<String>,
        /// Subject label stored as the first entity (user|session|node|…).
        #[arg(long)]
        subject: Option<String>,
        /// Inclusive validity start (RFC 3339).
        #[arg(long)]
        valid_from: Option<String>,
        /// Exclusive validity end (RFC 3339).
        #[arg(long)]
        valid_until: Option<String>,
        /// Explicit fact id ([a-z0-9-]); default derives from the text.
        #[arg(long)]
        id: Option<String>,
    },
    /// Search the scope tree (lexical + fuzzy lanes, RRF fusion).
    Search {
        query: String,
        /// Explicit as-of instant; unparseable values degrade to current mode.
        #[arg(long)]
        as_of: Option<String>,
        #[arg(long, default_value_t = recall::SEARCH_LIMIT_DEFAULT)]
        limit: usize,
        /// Print the full ccc-compatible result JSON.
        #[arg(long)]
        json: bool,
    },
    /// Close one fact: valid_until = now (reversible, never a deletion).
    Close {
        #[arg(long)]
        fact: String,
    },
    /// Print the M1 snapshot preview (resume + MEMORY/USER + working-state).
    Show,
    /// Run the built-in fixture evaluation with the pinned clock.
    Eval {
        /// Golden suite (5 cases).
        #[arg(long)]
        golden: bool,
        /// Scenario suite (9 cases).
        #[arg(long)]
        scenario: bool,
    },
}

fn default_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("DANSO_MEMORY_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"));
    home.join(".danso/memory")
}

pub fn route(args: &MemoryArgs) -> anyhow::Result<Route> {
    let scope = args.scope.as_deref().unwrap_or("global");
    Route::new(&default_root(), scope)
}

/// Early configuration validation — failures exit 2 (configuration), while
/// runtime refusals (locks, gates) exit 1.
pub fn validate(args: &MemoryArgs) -> anyhow::Result<()> {
    route(args)?;
    match &args.command {
        Command::Add {
            kind,
            text,
            because,
            valid_from,
            valid_until,
            ..
        } => {
            anyhow::ensure!(
                facts::KINDS.contains(&kind.as_str()),
                "kind must be one of: {}",
                facts::KINDS.join(", ")
            );
            anyhow::ensure!(!text.contains('\n'), "text must be a single line");
            if kind == "decision" {
                anyhow::ensure!(
                    because
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|b| !b.is_empty()),
                    "decision facts require a --because reason (§4.1)"
                );
            }
            for (flag, raw) in [("valid-from", valid_from), ("valid-until", valid_until)] {
                if let Some(raw) = raw {
                    facts::parse_timestamp(raw)
                        .map_err(|_| anyhow::anyhow!("{flag} must be RFC 3339 with an offset"))?;
                }
            }
        }
        Command::Close { fact } => anyhow::ensure!(
            !fact.is_empty()
                && fact.len() <= 64
                && fact
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "fact id must match [a-z0-9-]"
        ),
        Command::Search { limit, .. } => {
            anyhow::ensure!(*limit > 0 && *limit <= 50, "limit must be 1..=50");
        }
        Command::Eval { golden, scenario } => {
            anyhow::ensure!(
                *golden != *scenario,
                "exactly one of --golden or --scenario is required"
            );
        }
        Command::Init | Command::Show => {}
    }
    Ok(())
}

/// Run one memory command. `Ok(None)` prints nothing (output was streamed).
pub fn run(args: &MemoryArgs) -> anyhow::Result<Option<Value>> {
    let route = route(args)?;
    match &args.command {
        Command::Init => init(&route),
        Command::Add {
            kind,
            text,
            because,
            subject,
            valid_from,
            valid_until,
            id,
        } => add(
            &route,
            args.lock_timeout_ms,
            AddInput {
                kind,
                text,
                because: because.as_deref(),
                subject: subject.as_deref(),
                valid_from: valid_from.as_deref(),
                valid_until: valid_until.as_deref(),
                id: id.as_deref(),
            },
        ),
        Command::Search {
            query,
            as_of,
            limit,
            json,
        } => search(&route, query, as_of.as_deref(), *limit, *json),
        Command::Close { fact } => close(&route, fact, args.lock_timeout_ms),
        Command::Show => show(&route).map(|()| None),
        Command::Eval { golden, .. } => {
            let mode = if *golden {
                eval::Mode::Golden
            } else {
                eval::Mode::Scenario
            };
            eval_command(mode)
        }
    }
}

fn write_private_if_absent(path: &std::path::Path, contents: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    if paths::validate_regular(path, "memory document")? {
        return Ok(false);
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(true)
}

const MEMORY_TEMPLATE: &str = "\
<!-- Durable operating memory. Human-editable. \
Never store raw secrets here — only credential locations and handling rules. -->\n";
const USER_TEMPLATE: &str = "\
<!-- Who this node works for and how. Human-editable. \
Never store raw secrets here. -->\n";

fn init(route: &Route) -> anyhow::Result<Option<Value>> {
    paths::require_private_dir(&route.memories_dir())?;
    paths::require_private_dir(&route.state_dir())?;
    write_private_if_absent(&route.memories_dir().join("MEMORY.md"), MEMORY_TEMPLATE)?;
    write_private_if_absent(&route.memories_dir().join("USER.md"), USER_TEMPLATE)?;
    write_private_if_absent(&route.scope_lock(), "")?;
    Ok(Some(json!({
        "initialized": route.scope_dir().display().to_string(),
        "scope": route.scope(),
    })))
}

struct AddInput<'a> {
    kind: &'a str,
    text: &'a str,
    because: Option<&'a str>,
    subject: Option<&'a str>,
    valid_from: Option<&'a str>,
    valid_until: Option<&'a str>,
    id: Option<&'a str>,
}

fn add(route: &Route, lock_timeout_ms: u64, input: AddInput<'_>) -> anyhow::Result<Option<Value>> {
    let now = chrono::Utc::now();
    let candidate = facts::Candidate {
        kind: input.kind.to_string(),
        text: input.text.trim().to_string(),
        because: input
            .because
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string),
        quote: None,
        source_rank: 3,
        observed_at: facts::format_timestamp(now),
        entities: input.subject.map(str::to_string).into_iter().collect(),
        tags: vec!["manual".to_string()],
        valid_from: input.valid_from.map(str::to_string),
        valid_until: input.valid_until.map(str::to_string),
        transcript: None,
        manual: true,
        job_id: None,
        explicit_id: input.id.map(str::to_string),
    };
    let _lock =
        paths::ExclusiveLock::acquire(&route.scope_lock(), paths::lock_timeout(lock_timeout_ms))?;
    let existing = facts::load(route)?;
    let output = facts::gate_and_render(
        &existing,
        vec![candidate],
        memory::audience_for_scope(route.scope()),
        now,
        facts::MAX_FACTS_DEFAULT,
    )?;
    if !output.changed {
        return Ok(Some(json!({ "added": false, "report": output.report })));
    }
    if output.report.saved == 0 {
        if output.report.skipped_mutable > 0 {
            anyhow::bail!(
                "mutable operational fact refused: measure it live instead of memorizing it"
            );
        }
        if output.report.skipped_missing_reason > 0 {
            anyhow::bail!("decision facts require a because reason");
        }
        anyhow::bail!("fact refused by the write gates");
    }
    let payload: String = output.lines.concat();
    paths::atomic_write(&route.facts_file(), payload.as_bytes(), "memory facts")?;
    Ok(Some(json!({ "added": true, "report": output.report })))
}

fn search(
    route: &Route,
    query: &str,
    as_of: Option<&str>,
    limit: usize,
    json_output: bool,
) -> anyhow::Result<Option<Value>> {
    let options = recall::SearchOptions {
        query,
        as_of,
        limit,
        now: chrono::Utc::now(),
    };
    let output = recall::search(route, &options)?;
    if json_output {
        return Ok(Some(output));
    }
    println!(
        "query: {} | mode: {} | lanes: {}",
        query,
        output["retrievalMode"].as_str().unwrap_or("?"),
        output["lanes"]
            .as_array()
            .map(|lanes| lanes
                .iter()
                .filter_map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("+"))
            .unwrap_or_default()
    );
    for (index, result) in output["results"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .enumerate()
    {
        println!(
            "{}. {}\t{}\n   {}",
            index + 1,
            result["score"],
            result["path"].as_str().unwrap_or("?"),
            result["snippet"].as_str().unwrap_or("?").replace('\n', " ")
        );
    }
    Ok(None)
}

fn close(route: &Route, fact: &str, lock_timeout_ms: u64) -> anyhow::Result<Option<Value>> {
    match facts::close(
        route,
        fact,
        chrono::Utc::now(),
        paths::lock_timeout(lock_timeout_ms),
    )? {
        facts::CloseOutcome::Closed => Ok(Some(json!({ "fact": fact, "closed": true }))),
        facts::CloseOutcome::AlreadyClosed => {
            Ok(Some(json!({ "fact": fact, "closed": "already" })))
        }
        facts::CloseOutcome::NotFound => anyhow::bail!("fact not found in this scope"),
    }
}

/// M1 snapshot preview (§5.1 partial): the three markdown/state blocks in
/// the ccc load-memory order with fixed caps, every block scanned and
/// independently fail-open. The dynamic budget and the local-hot block land
/// with M2.
fn show(route: &Route) -> anyhow::Result<()> {
    const RESUME_CAP: usize = 2000;
    const MEMORY_CAP: usize = 4000;
    const WORKING_STATE_CAP: usize = 2048;
    let read = |path: &std::path::Path| -> Option<String> {
        paths::read_bounded(path, 64 * 1024, "memory document")
            .ok()
            .flatten()
            .map(|payload| String::from_utf8_lossy(&payload).into_owned())
            .filter(|text| !text.trim().is_empty())
    };

    println!("# danso memory snapshot (preview)");
    if let Some(resume) = read(&route.resume_file()) {
        let scanned = memory::scan::scan("resume", &resume, Some(RESUME_CAP));
        println!("\n▶ 직전 세션에서 이어서:\n{}", scanned.text.trim_end());
    }
    let mut combined = String::new();
    for file in ["MEMORY.md", "USER.md"] {
        if let Some(doc) = read(&route.memories_dir().join(file)) {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&doc);
        }
    }
    if combined.is_empty() {
        println!("\n## Built-in MEMORY + USER\n(memory files unavailable)");
    } else {
        let scanned = memory::scan::scan("memory", &combined, Some(MEMORY_CAP));
        println!("\n## Built-in MEMORY + USER\n{}", scanned.text.trim_end());
    }
    if let Some(working_state) = read(&route.working_state_file()) {
        let scanned = memory::scan::scan("working-state", &working_state, Some(WORKING_STATE_CAP));
        println!("\n## Working-state checkpoint\n{}", scanned.text.trim_end());
    } else {
        println!("\n## Working-state checkpoint\n(no working state recorded)");
    }
    Ok(())
}

fn eval_command(mode: eval::Mode) -> anyhow::Result<Option<Value>> {
    let report = eval::run(mode, &std::env::temp_dir())?;
    let ok = report["ok"].as_bool().unwrap_or(false);
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("eval report is serializable")
    );
    anyhow::ensure!(ok, "memory eval gate failed");
    Ok(None)
}
