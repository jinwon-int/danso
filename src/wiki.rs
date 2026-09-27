//! `danso wiki` — the read-only wiki-agent index surface (§3.1 `danso-wiki`,
//! issue #121). Slice 1 carries `status`; `find`/`prefetch`/`load`/`sync`
//! land behind the same gate in later slices.

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
    }
}
