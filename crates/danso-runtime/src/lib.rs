//! Provider-neutral agent session contract for Danso embedders.
//!
//! This crate is the seam between the core harness (`danso`) and any channel
//! or scheduler that drives it: it defines the body-free [`AgentEvent`]
//! stream, the [`AgentSession`] / [`TurnRunner`] traits, and an in-process
//! runner that executes `danso::app::run` on a dedicated thread with
//! cancellation and pause support. No Telegram, HTTP server, or scheduler
//! code lives here (docs/unified-design.md §4).
pub mod event;
pub mod inprocess;
pub mod session;
pub mod sink;

pub use event::{AgentEvent, ApprovalDecision, ErrorCode, TaskState, ToolName};
pub use inprocess::{InProcessRunner, RunTemplate};
pub use session::{
    AgentSession, ApprovalHandler, ApprovalRequest, DenyAll, EventStream, ModelInfo,
    SessionRequest, TurnInput, TurnRunner,
};
pub use sink::ChannelSink;
