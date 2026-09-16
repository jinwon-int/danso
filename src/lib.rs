pub mod app;
pub mod backup;
pub mod compaction;
pub mod config;
pub mod context;
pub mod contracts;
pub mod doctor;
pub mod failure;
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
pub mod telegram;
pub mod tools;
/// The self-update surface. Gated on `ops` like the service CLI.
#[cfg(feature = "ops")]
pub mod update;
pub mod usage;
