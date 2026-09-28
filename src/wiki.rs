//! `danso wiki` — the read-only wiki-agent index surface (§3.1 `danso-wiki`,
//! issue #121). Slice 1 carried `status`; slice 2 added `find`; slice 3 adds
//! `load` and `prefetch`, the read/verify half of the bash tool. `sync`
//! lands behind the same gate in a later slice.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
pub struct WikiArgs {
    #[command(subcommand)]
    pub command: WikiCommand,
}

#[derive(Subcommand)]
pub enum WikiCommand {
    /// Report the wiki index and the local query cache. Read-only: it never
    /// builds an index, hashes a cache tree, or touches the network.
    Status {
        /// Directory holding meta.json / manifest.jsonl / chunks.jsonl.
        #[arg(long, value_name = "DIR")]
        index_dir: PathBuf,
        /// Node-local cache directory holding (or destined for) index.cache.
        #[arg(long, value_name = "DIR")]
        cache_dir: PathBuf,
        /// Emit the machine-readable report instead of the text rendering.
        #[arg(long)]
        json: bool,
    },
    /// One practical discovery pass: semantic candidates from the local
    /// index plus an exact-text fallback over the read cache, every result
    /// carrying its `wiki-agent load --lines` verification command.
    /// Read-only — results are candidates, not evidence.
    Find {
        /// Directory holding meta.json / manifest.jsonl / chunks.jsonl.
        #[arg(long, value_name = "DIR")]
        index_dir: PathBuf,
        /// Node-local cache directory holding (or destined for) index.cache.
        #[arg(long, value_name = "DIR")]
        cache_dir: PathBuf,
        /// The query words. `--` ends option parsing, so a query may contain
        /// anything; like the bash original, words are joined with spaces.
        #[arg(required = true, value_name = "WORDS")]
        query: Vec<String>,
        /// Emit the machine-readable report instead of the text rendering.
        #[arg(long)]
        json: bool,
        /// Re-parse the index files into the local query cache first
        /// (reader-local; never syncs, never rebuilds, never dials out).
        #[arg(long)]
        refresh: bool,
        /// Suppress candidates below calibrated cross-signal confidence.
        #[arg(long)]
        abstention: bool,
        /// Keep candidates regardless of calibrated confidence.
        #[arg(long, overrides_with = "abstention")]
        no_abstention: bool,
        /// Maximum semantic candidates (>= 1). `--limit` is the bash
        /// compatibility alias.
        #[arg(long, visible_alias = "limit", default_value_t = 5, value_name = "N")]
        top: usize,
        /// Maximum text-fallback matches in the text rendering (>= 1).
        #[arg(long, default_value_t = 8, value_name = "N")]
        grep_top: usize,
        /// Include indexed/cache paths matching a glob; repeatable.
        #[arg(long = "include", value_name = "GLOB")]
        include: Vec<String>,
        /// Exclude indexed/cache paths matching a glob; repeatable.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude: Vec<String>,
        /// Include pages/nodes/NAME/**.
        #[arg(long, value_name = "NAME")]
        node: Option<String>,
        /// Include pages/team/NAME/**.
        #[arg(long, value_name = "NAME")]
        team: Option<String>,
        /// Include pages/runbooks/**.
        #[arg(long)]
        runbook: bool,
    },
    /// Read the synced Wiki cache: full files, `--lines START:END` ranges,
    /// or `--id` anchor sections (TM-509, DOC-110, LOG-…). Read-only: it
    /// never syncs — a page that drifted from the indexed snapshot earns a
    /// staleness warning, and the real sync is a later slice.
    Load {
        /// Directory holding meta.json / manifest.jsonl / chunks.jsonl (the
        /// staleness warning compares loaded pages against the manifest).
        #[arg(long, value_name = "DIR")]
        index_dir: PathBuf,
        /// Synced wiki cache directory holding pages/ and friends.
        #[arg(long, value_name = "DIR")]
        cache_dir: PathBuf,
        /// 1-indexed inclusive line range; `START:END` (`--lines 5` means
        /// `5:5`, the bash parameter-expansion default).
        #[arg(long, value_name = "START:END")]
        lines: Option<String>,
        /// Section anchor to resolve against the canonical cache.
        #[arg(long, value_name = "ANCHOR")]
        id: Option<String>,
        /// Cache-relative paths to read, in order.
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Budget-capped Wiki context prefetch for agent runtimes: one find
    /// pass, at most 3 snippet candidates inside a hard character budget.
    /// Fail-open — retrieval failures still print a payload and exit 0.
    Prefetch {
        /// Directory holding meta.json / manifest.jsonl / chunks.jsonl.
        #[arg(long, value_name = "DIR")]
        index_dir: PathBuf,
        /// Node-local cache directory holding (or destined for) index.cache.
        #[arg(long, value_name = "DIR")]
        cache_dir: PathBuf,
        /// Emit the machine-readable wiki-agent-prefetch-v1 payload instead
        /// of the text rendering.
        #[arg(long)]
        json: bool,
        /// Max snippets (digits; clamped to 3, the bash default).
        #[arg(long, value_name = "N", default_value = "3")]
        top: String,
        /// Hard snippet budget in characters (digits; default 3200).
        #[arg(long, value_name = "N", default_value = "3200")]
        budget_chars: String,
        /// Suppress low-confidence semantic and text candidates.
        #[arg(long)]
        abstention: bool,
        /// Keep candidates regardless of calibrated confidence.
        #[arg(long, overrides_with = "abstention")]
        no_abstention: bool,
        /// The query words; `--` ends option parsing.
        #[arg(value_name = "WORDS")]
        query: Vec<String>,
    },
}

/// Exit codes: 0 — report produced. 2 — no trustworthy report possible; an
/// index this build cannot read is fail-closed, not a warning, because a
/// status that guessed would be worse than a status that refused.
pub fn run(args: WikiArgs) -> i32 {
    match args.command {
        WikiCommand::Status {
            index_dir,
            cache_dir,
            json,
        } => match danso_wiki::status::status(&index_dir, &cache_dir) {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string(&report).expect("serializable"));
                } else {
                    print!("{}", report.to_text());
                }
                0
            }
            Err(error) => {
                // A configuration problem has to name itself or nobody can fix
                // it; the paths here are the operator's own `[wiki]` settings.
                eprintln!("wiki status failed: {error:#}");
                2
            }
        },
        WikiCommand::Find {
            index_dir,
            cache_dir,
            query,
            json,
            refresh,
            abstention,
            no_abstention,
            top,
            grep_top,
            include,
            exclude,
            node,
            team,
            runbook,
        } => {
            if top == 0 || grep_top == 0 {
                eprintln!("wiki find failed: --top/--grep-top require a positive integer");
                return 64;
            }
            // The bash scope flags are just include globs.
            let mut include = include;
            if let Some(name) = node {
                include.push(format!("pages/nodes/{name}/**"));
            }
            if let Some(name) = team {
                include.push(format!("pages/team/{name}/**"));
            }
            if runbook {
                include.push("pages/runbooks/**".to_string());
            }
            // The last flag on the command line wins, matching the bash
            // parser; neither flag given means the env/default decides.
            let abstention = if abstention {
                Some(true)
            } else if no_abstention {
                Some(false)
            } else {
                None
            };
            let options = danso_wiki::find::FindOptions {
                query: query.join(" "),
                index_dir,
                cache_dir,
                json,
                refresh,
                abstention,
                top,
                grep_top,
                include,
                exclude,
            };
            danso_wiki::find::run(&options)
        }
        WikiCommand::Load {
            index_dir,
            cache_dir,
            lines,
            id,
            paths,
        } => danso_wiki::load::run(&danso_wiki::load::LoadOptions {
            index_dir,
            cache_dir,
            lines,
            id,
            paths,
        }),
        WikiCommand::Prefetch {
            index_dir,
            cache_dir,
            json,
            top,
            budget_chars,
            abstention,
            no_abstention,
            query,
        } => {
            if query.is_empty() {
                eprintln!("wiki-agent: prefetch requires a query");
                return 64;
            }
            let Some(top) = parse_digit_option(&top) else {
                eprintln!("wiki-agent: --top requires a positive integer");
                return 64;
            };
            let Some(budget) = parse_digit_option(&budget_chars) else {
                eprintln!("wiki-agent: --budget-chars requires a positive integer");
                return 64;
            };
            // The last flag on the command line wins, matching the bash
            // parser; neither flag given means the env/default decides.
            let abstention = if abstention {
                Some(true)
            } else if no_abstention {
                Some(false)
            } else {
                None
            };
            danso_wiki::prefetch::run(&danso_wiki::prefetch::PrefetchOptions {
                query: query.join(" "),
                index_dir,
                cache_dir,
                json,
                top: top as usize,
                budget_chars: budget as usize,
                abstention,
            })
        }
    }
}

/// The bash digit-string check for `--top`/`--budget-chars`: empty or
/// non-digit input is the usage exit 64. `0` is a valid digit string — the
/// prefetch clamps it, exactly like the bash `max(1, min(3, top))`.
fn parse_digit_option(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok()
}
