//! `danso memory <init|add|search|close|show|eval>` — the standalone M1 CLI
//! over the native memory store (issue #52 §8). No provider, Python, Node,
//! or ccc-node runtime is involved; errors are body-free and fail closed.
//! Unspecified flags default to memory OFF semantics: without a scope the
//! `global` tree under `$DANSO_MEMORY_DIR` (or `~/.danso/memory`) is used.

use clap::{Parser, Subcommand};
use danso::memory::distill::journal;
use danso::memory::{self, Route, eval, facts, paths, promote, recall, snapshot, transaction};
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
    /// Register a session journal for extraction (§4.7 explicit trigger).
    Distill {
        /// Pi v3 session journal path.
        #[arg(long)]
        session: PathBuf,
    },
    /// Extract and commit pending journal jobs (provider credentials required).
    Drain {
        #[arg(long, default_value_t = 1)]
        max_jobs: usize,
        #[arg(long, default_value = "anthropic")]
        provider: String,
        #[arg(long)]
        model: String,
    },
    /// Roll back the newest committed facts+resume change (§4.5).
    Rollback {
        #[arg(long)]
        action: String,
    },
    /// Body-free diagnostics for this scope (§6.4/M5).
    Check,
    /// Promote one private fact into the shared store (§7; idempotent).
    Promote {
        /// Source private scope (private-<32 hex>).
        #[arg(long)]
        from: String,
        /// Fact id (distill-<12 lowercase hex>).
        #[arg(long)]
        fact: String,
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
    // `--memory-dir` is explicit per-invocation configuration; it wins over
    // the DANSO_MEMORY_DIR environment and the ~/.danso default (§8).
    let root = args.memory_dir.clone().unwrap_or_else(default_root);
    Route::new(&root, scope)
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
        Command::Distill { session } => {
            anyhow::ensure!(session.exists(), "session journal must exist");
        }
        Command::Drain {
            max_jobs, model, ..
        } => {
            anyhow::ensure!(!model.trim().is_empty(), "drain requires --model");
            anyhow::ensure!(
                *max_jobs >= 1 && *max_jobs <= 100,
                "max-jobs must be 1..=100"
            );
        }
        Command::Rollback { action } => {
            anyhow::ensure!(
                action.len() == 32
                    && action
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "action must be 32 lowercase hex characters"
            );
        }
        Command::Promote { from, fact } => {
            anyhow::ensure!(
                from.starts_with("private-") && paths::valid_scope(from),
                "promotion source must be a private-<32 hex> scope"
            );
            anyhow::ensure!(
                fact.starts_with("distill-")
                    && fact.len() == "distill-".len() + 12
                    && fact[8..]
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "fact id must match distill-<12 lowercase hex>"
            );
        }
        Command::Init | Command::Show | Command::Check => {}
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
        Command::Distill { session } => {
            let outcome = journal::enqueue(&route, session, "explicit", chrono::Utc::now())?;
            Ok(Some(match outcome {
                journal::EnqueueOutcome::Enqueued { job_id } => {
                    json!({ "enqueued": true, "job_id": job_id })
                }
                journal::EnqueueOutcome::AlreadyPending { job_id } => {
                    json!({ "enqueued": "already", "job_id": job_id })
                }
                journal::EnqueueOutcome::Skipped { reason } => {
                    json!({ "enqueued": false, "reason": reason })
                }
            }))
        }
        Command::Drain {
            max_jobs,
            provider,
            model,
        } => drain_command(&route, provider, model, *max_jobs),
        Command::Rollback { action } => close_rollback(&route, action, args.lock_timeout_ms),
        Command::Check => check(&route),
        Command::Promote { from, fact } => {
            let result = promote::promote(
                route.root(),
                from,
                fact,
                chrono::Utc::now(),
                paths::lock_timeout(args.lock_timeout_ms),
            )?;
            Ok(Some(
                serde_json::to_value(&result).expect("promote result is serializable"),
            ))
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
    use danso::memory::transaction::{CommitMeta, Transaction};
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
    // #65 §1.5: manual facts commit through the rollback transaction under
    // the single scope lock, so a manual write and a distill commit can no
    // longer interleave on the same `memory-facts.jsonl`.
    let transaction = Transaction::new(&route.state_dir());
    let meta = CommitMeta {
        provider: "danso".into(),
        actor: "manual".into(),
        tool: "local-memory-add".into(),
        diff: "add-fact".into(),
        session: input
            .id
            .map(str::to_string)
            .unwrap_or_else(|| "manual".into()),
    };
    let audience = memory::audience_for_scope(route.scope());
    let mut report: Option<facts::GateReport> = None;
    let commit = transaction.commit(paths::lock_timeout(lock_timeout_ms), &meta, |before| {
        let mut next = before.clone();
        for (name, current) in next.iter_mut() {
            if name == memory::FACTS_FILE_NAME {
                let file = facts::read(current.clone())?;
                let output = facts::gate_and_render(
                    &file,
                    vec![candidate.clone()],
                    audience,
                    now,
                    facts::MAX_FACTS_DEFAULT,
                )?;
                if output.changed {
                    *current = Some(output.lines.concat().into_bytes());
                }
                report = Some(output.report);
            }
        }
        Ok(next)
    })?;
    let report = match report {
        Some(report) => report,
        None => anyhow::bail!("fact refused by the write gates"),
    };
    if report.saved == 0 {
        if report.skipped_mutable > 0 {
            anyhow::bail!(
                "mutable operational fact refused: measure it live instead of memorizing it"
            );
        }
        if report.skipped_missing_reason > 0 {
            anyhow::bail!("decision facts require a because reason");
        }
        anyhow::bail!("fact refused by the write gates");
    }
    if let Some(action_id) = commit.action_id {
        memory::audit::record(
            route,
            &memory::audit::Event::Commit {
                action_id,
                changed: vec![memory::FACTS_FILE_NAME.to_string()],
                facts_added: report.saved,
            },
        );
    }
    Ok(Some(json!({ "added": true, "report": report })))
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

/// Snapshot preview (§5.1): the real assembly — title, resume, status,
/// MEMORY+USER, working-state with the STALE warning, and the local-hot
/// block — exactly what `danso run --memory read` injects, without the
/// managed-block wrapper.
fn show(route: &Route) -> anyhow::Result<()> {
    let options = snapshot::SnapshotOptions {
        event: "preview",
        query: None,
        max_bytes: snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
        stale_days: snapshot::STALE_DAYS_DEFAULT,
        now: chrono::Utc::now(),
    };
    let body = snapshot::assemble(route, &options)?;
    print!("{body}");
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

fn drain_command(
    route: &Route,
    provider: &str,
    model: &str,
    max_jobs: usize,
) -> anyhow::Result<Option<Value>> {
    let max_output_tokens = danso::provider::resolve_max_output_tokens(None)?;
    let mut selected = danso::app::provider_from_parts(
        provider,
        model,
        None,
        120,
        Some(max_output_tokens),
        None,
        None,
    )?;
    selected.set_retries(danso::provider::resolve_provider_retries(None)?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let report = runtime.block_on(async {
        let mut usage = danso::usage::Usage::default();
        let mut selected = selected;
        memory::distill::extract::drain(
            route,
            &mut selected,
            &mut usage,
            max_jobs,
            120_000,
            chrono::Utc::now(),
        )
        .await
    })?;
    Ok(Some(serde_json::to_value(&report)?))
}

fn close_rollback(
    route: &Route,
    action: &str,
    lock_timeout_ms: u64,
) -> anyhow::Result<Option<Value>> {
    let transaction = transaction::Transaction::new(&route.state_dir());
    match transaction.rollback(paths::lock_timeout(lock_timeout_ms), action)? {
        transaction::RollbackOutcome::RolledBack => {
            Ok(Some(json!({ "action": action, "rolled_back": true })))
        }
        transaction::RollbackOutcome::AlreadyRolledBack => {
            Ok(Some(json!({ "action": action, "rolled_back": "already" })))
        }
    }
}

/// Body-free scope diagnostics (§6.4/M5): counts and states only.
fn check(route: &Route) -> anyhow::Result<Option<Value>> {
    let file = facts::load(route)?;
    let records: Vec<&facts::FactRecord> = file.records().collect();
    let open = records
        .iter()
        .filter(|r| {
            r.review != "superseded"
                && r.review != "rejected"
                && r.valid_until
                    .as_deref()
                    .and_then(|v| facts::parse_timestamp(v).ok())
                    .is_none_or(|until| until > chrono::Utc::now())
        })
        .count();
    let closed = records.len() - open;
    let needs_human = records.iter().filter(|r| r.review == "needs-human").count();
    let constraints = records.iter().filter(|r| r.kind == "constraint").count();
    let journal_dir = route.state_dir().join("distill-journal");
    let count_json = |dir: &std::path::Path| -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                    .count()
            })
            .unwrap_or(0)
    };
    let cooldown = std::fs::read(route.state_dir().join("distill.cooldown"))
        .ok()
        .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok());
    let transaction = transaction::Transaction::new(&route.state_dir());
    let rollback_state = transaction.status()?;
    Ok(Some(json!({
        "scope": route.scope(),
        "facts": {
            "records": records.len(),
            "open": open,
            "closed": closed,
            "needs_human": needs_human,
            "constraints": constraints,
        },
        "journal": {
            "pending": count_json(&journal_dir),
            "dead": count_json(&journal_dir.join("dead")),
            "cooldown": cooldown,
        },
        "rollback": rollback_state,
        "files": {
            "memory_md": route.memories_dir().join("MEMORY.md").exists(),
            "user_md": route.memories_dir().join("USER.md").exists(),
            "resume": route.resume_file().exists(),
            "working_state": route.working_state_file().exists(),
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `--memory-dir` flag was silently ignored (route() only read the
    /// environment/default); it is explicit per-invocation configuration.
    #[test]
    fn memory_dir_flag_overrides_env_and_default_root() {
        let args = MemoryArgs::try_parse_from([
            "danso",
            "search",
            "query",
            "--memory-dir",
            "/tmp/danso-flag-root",
        ])
        .unwrap();
        assert_eq!(
            route(&args).unwrap().root(),
            PathBuf::from("/tmp/danso-flag-root")
        );

        // SAFETY(unsafe): this test binary's only reader/writer of
        // DANSO_MEMORY_DIR.
        unsafe { std::env::set_var("DANSO_MEMORY_DIR", "/tmp/danso-env-root") };
        let args = MemoryArgs::try_parse_from(["danso", "search", "query"]).unwrap();
        assert_eq!(
            route(&args).unwrap().root(),
            PathBuf::from("/tmp/danso-env-root")
        );
        unsafe { std::env::remove_var("DANSO_MEMORY_DIR") };
    }
}
