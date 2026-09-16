//! Resident-service operations primitives (`docs/unified-design.md` §6.4, §7).
//!
//! These are the process-and-file concerns that `danso service` needs and that
//! must stay out of the runtime loop and the shared contracts (`AGENTS.md`).
//! Nothing here knows about Telegram, providers or sessions; the root package
//! wires them to the polling loop behind the `ops` feature.
//!
//! The ccc-node contracts these preserve:
//!
//! * `bridge/start.sh --status` distinguishes *serving but unbookkept* from
//!   *down*, and never reports DOWN when it could not read the evidence.
//! * The kernel lock, not the presence of a file, is the ownership signal.
//! * A zombie has exited and cannot poll or hold a lock, so it is not live.

pub mod generation;
pub mod health;
pub mod pidfile;
pub mod probe;
pub mod status;
pub mod stop;
pub mod supervise;

pub use status::{ServiceState, StatusOutcome, StatusReport};
pub use stop::{StopOutcome, TURN_DRAIN_SECS};
pub use supervise::{CrashPolicy, Decision};
