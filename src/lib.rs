pub mod app;
pub mod backup;
pub mod compaction;
pub mod config;
pub mod context;
pub mod contracts;
/// The read-only scheduler surface (`danso cron list|describe|due`, §6.5 of
/// the unified design). Gated on `ops` like service/update: a CLI-only build
/// has no cron surface, but the store schema it reads is identical either
/// way, so a store written by one build validates on every build.
#[cfg(feature = "ops")]
pub mod cron;
pub mod doctor;
pub mod failure;
/// The self-update surface. Gated on `ops` like the service CLI.
// Not behind `ops`: `config` validates `[update] source` with it, and config
// validation is part of every build. The crate it leans on (`danso-ops`) is an
// unconditional dependency; only the CLI surface is feature-gated.
pub mod fetch;
pub mod long_task;
pub mod memory;
pub mod output;
pub mod provider;
pub mod runtime;
/// The resident-service CLI. Gated on `ops`: a CLI-only build has no service
/// surface, but the `health.json` schema it shares stays identical either way.
#[cfg(feature = "ops")]
pub mod service;
pub mod session;
pub mod settings;
pub mod telegram;
pub mod tools;
#[cfg(feature = "ops")]
pub mod update;
pub mod usage;
