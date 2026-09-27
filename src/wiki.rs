//! `danso wiki` — the read-only wiki-agent index surface (§3.1 `danso-wiki`,
//! issue #121). Slice 1 carried `status`; slice 2 adds `find`, the practical
//! discovery pass. `prefetch`/`load`/`sync` land behind the same gate in
//! later slices.

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
    }
}
