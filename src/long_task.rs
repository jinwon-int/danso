//! Durable policy for the opt-in long-task mode.
//!
//! Long tasks are still ordinary linear Danso sessions.  This module only
//! describes the small, body-free journal records that make a long task
//! resumable at a settled boundary.  It deliberately has no CLI, environment
//! or filesystem code.

use crate::contracts::SessionStore;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const CUSTOM_TYPE: &str = "danso.long_task.v1";
pub const MAX_WALL_SECONDS: u64 = 6 * 60 * 60;
pub const DEFAULT_WALL_SECONDS: u64 = 300;
pub const MIN_STAGE_REQUESTS: u64 = 1;
pub const MAX_STAGE_REQUESTS: u64 = 1024;
pub const DEFAULT_STAGE_REQUESTS: u64 = 16;
pub const MIN_MAX_REQUESTS: u64 = 1;
pub const MAX_MAX_REQUESTS: u64 = 2048;
pub const DEFAULT_MAX_REQUESTS: u64 = 1024;
pub const MIN_MAX_TOKENS: u64 = 1;
pub const MAX_MAX_TOKENS: u64 = 25_000_000;
pub const DEFAULT_MAX_TOKENS: u64 = 10_000_000;
pub const MIN_REPEAT_LIMIT: u64 = 2;
pub const MAX_REPEAT_LIMIT: u64 = 8;
pub const DEFAULT_REPEAT_LIMIT: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub wall_seconds: u64,
    pub stage_requests: u64,
    pub max_requests: u64,
    pub max_tokens: u64,
    pub repeat_limit: u64,
}

impl Limits {
    pub fn defaults() -> Self {
        Self {
            wall_seconds: DEFAULT_WALL_SECONDS,
            stage_requests: DEFAULT_STAGE_REQUESTS,
            max_requests: DEFAULT_MAX_REQUESTS,
            max_tokens: DEFAULT_MAX_TOKENS,
            repeat_limit: DEFAULT_REPEAT_LIMIT,
        }
    }

    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=MAX_WALL_SECONDS).contains(&self.wall_seconds),
            "long-task timeout must be 1..21600 seconds"
        );
        ensure!(
            (MIN_STAGE_REQUESTS..=MAX_STAGE_REQUESTS).contains(&self.stage_requests),
            "task-stage-requests must be 1..1024"
        );
        ensure!(
            (MIN_MAX_REQUESTS..=MAX_MAX_REQUESTS).contains(&self.max_requests),
            "task-max-requests must be 1..2048"
        );
        ensure!(
            (MIN_MAX_TOKENS..=MAX_MAX_TOKENS).contains(&self.max_tokens),
            "task-max-tokens must be 1..25000000"
        );
        ensure!(
            (MIN_REPEAT_LIMIT..=MAX_REPEAT_LIMIT).contains(&self.repeat_limit),
            "task-repeat-limit must be 2..8"
        );
        Ok(())
    }

    pub fn json(self, task_id: &str, user_entry_id: &str) -> Value {
        json!({
            "version": 1,
            "event": "created",
            "task_id": task_id,
            "user_entry_id": user_entry_id,
            "wall_seconds": self.wall_seconds,
            "stage_requests": self.stage_requests,
            "max_requests": self.max_requests,
            "max_tokens": self.max_tokens,
            "repeat_limit": self.repeat_limit,
        })
    }

    fn from_created(data: &Value, expected_task_id: Option<&str>) -> Result<Self> {
        exact_keys(
            data,
            &[
                "version",
                "event",
                "task_id",
                "user_entry_id",
                "wall_seconds",
                "stage_requests",
                "max_requests",
                "max_tokens",
                "repeat_limit",
            ],
        )?;
        ensure!(data["version"] == 1 && data["event"] == "created");
        let task_id = string(data, "task_id")?;
        if let Some(expected) = expected_task_id {
            ensure!(task_id == expected, "long-task session id mismatch");
        }
        ensure!(!string(data, "user_entry_id")?.is_empty());
        let limits = Self {
            wall_seconds: integer(data, "wall_seconds")?,
            stage_requests: integer(data, "stage_requests")?,
            max_requests: integer(data, "max_requests")?,
            max_tokens: integer(data, "max_tokens")?,
            repeat_limit: integer(data, "repeat_limit")?,
        };
        limits.validate()?;
        Ok(limits)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    None,
    Ready,
    PendingProvider,
    PendingTools,
    FinalPending,
    Paused,
    Completed,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "not_long_task",
            Self::Ready => "ready",
            Self::PendingProvider => "pending_provider",
            Self::PendingTools => "pending_tools",
            Self::FinalPending => "final_pending",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenDelta {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenDelta {
    pub fn total(self) -> Result<u64> {
        self.input
            .checked_add(self.output)
            .and_then(|n| n.checked_add(self.cache_read))
            .and_then(|n| n.checked_add(self.cache_write))
            .context("long-task token counter overflow")
    }
}

#[derive(Clone, Debug)]
pub struct Ledger {
    pub limits: Option<Limits>,
    pub user_entry_id: Option<String>,
    pub state: State,
    pub stage: u64,
    pub stage_requests: u64,
    pub elapsed_ms: u64,
    pub requests: u64,
    pub reported_tokens: u64,
    pub pending_sequence: Option<u64>,
    pub pending_response: Option<PendingResponse>,
    pub last_fingerprint: Option<String>,
    pub repeat_count: u64,
    pub terminal_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PendingResponse {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub tokens: TokenDelta,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            limits: None,
            user_entry_id: None,
            state: State::None,
            stage: 0,
            stage_requests: 0,
            elapsed_ms: 0,
            requests: 0,
            reported_tokens: 0,
            pending_sequence: None,
            pending_response: None,
            last_fingerprint: None,
            repeat_count: 0,
            terminal_reason: None,
        }
    }
}

impl Ledger {
    pub fn from_records(records: &[Value], expected_task_id: Option<&str>) -> Result<Self> {
        let mut ledger = Self::default();
        for record in records {
            ledger.apply(record, expected_task_id)?;
        }
        Ok(ledger)
    }

    fn apply(&mut self, data: &Value, expected_task_id: Option<&str>) -> Result<()> {
        let event = data["event"]
            .as_str()
            .context("long-task record lacks event")?;
        match event {
            "created" => {
                if self.limits.is_some() {
                    ensure!(
                        matches!(self.state, State::Completed | State::Failed),
                        "duplicate long-task creation record"
                    );
                    // A completed/failed job is terminal.  A later explicit
                    // long-task invocation may begin a fresh job in the same
                    // linear journal; unfinished jobs never take this path.
                    *self = Self::default();
                }
                let limits = Limits::from_created(data, expected_task_id)?;
                let user_entry_id = string(data, "user_entry_id")?.to_owned();
                self.limits = Some(limits);
                self.user_entry_id = Some(user_entry_id);
                self.state = State::Ready;
            }
            "request_started" => {
                exact_keys(
                    data,
                    &["version", "event", "sequence", "stage", "elapsed_ms"],
                )?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                ensure!(
                    self.pending_sequence.is_none(),
                    "duplicate pending provider request"
                );
                ensure!(matches!(self.state, State::Ready | State::Paused));
                let sequence = integer(data, "sequence")?;
                ensure!(
                    sequence == self.requests + 1,
                    "long-task request sequence mismatch"
                );
                let stage = integer(data, "stage")?;
                ensure!(stage == self.stage, "long-task request stage mismatch");
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                self.pending_sequence = Some(sequence);
                self.state = State::PendingProvider;
                self.elapsed_ms = elapsed;
            }
            "request_settled" => {
                exact_keys(
                    data,
                    &[
                        "version",
                        "event",
                        "sequence",
                        "stage",
                        "elapsed_ms",
                        "response_kind",
                        "input_tokens",
                        "output_tokens",
                        "cache_read_tokens",
                        "cache_write_tokens",
                        "total_tokens",
                    ],
                )?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                let sequence = self
                    .pending_sequence
                    .context("provider result without start")?;
                ensure!(integer(data, "sequence")? == sequence);
                ensure!(integer(data, "stage")? == self.stage);
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                let tokens = tokens(data)?;
                self.add_request(elapsed, tokens, limits)?;
                let response_kind = string(data, "response_kind")?;
                ensure!(matches!(response_kind, "summary" | "tools" | "final"));
                self.pending_sequence = None;
                self.elapsed_ms = elapsed;
                self.state = match response_kind {
                    "tools" => State::PendingTools,
                    "final" => State::FinalPending,
                    "summary" => State::Ready,
                    _ => unreachable!(),
                };
            }
            "request_failed" => {
                exact_keys(
                    data,
                    &[
                        "version",
                        "event",
                        "sequence",
                        "stage",
                        "elapsed_ms",
                        "category",
                        "input_tokens",
                        "output_tokens",
                        "cache_read_tokens",
                        "cache_write_tokens",
                        "total_tokens",
                    ],
                )?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                let sequence = self
                    .pending_sequence
                    .context("provider failure without start")?;
                ensure!(integer(data, "sequence")? == sequence);
                ensure!(integer(data, "stage")? == self.stage);
                let category = string(data, "category")?;
                ensure!(matches!(
                    category,
                    "provider" | "provider_timeout" | "compaction" | "runtime" | "request_budget"
                ));
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                let tokens = tokens(data)?;
                self.add_request(elapsed, tokens, limits)?;
                self.pending_sequence = None;
                self.state = State::Failed;
                self.terminal_reason = Some(category.to_owned());
                self.elapsed_ms = elapsed;
            }
            "stage_checkpoint" => {
                exact_keys(
                    data,
                    &[
                        "version",
                        "event",
                        "stage",
                        "stage_requests",
                        "elapsed_ms",
                        "fingerprint",
                        "repeat_count",
                    ],
                )?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                ensure!(
                    self.state == State::Ready,
                    "stage checkpoint without settled tools"
                );
                let stage = integer(data, "stage")?;
                ensure!(stage == self.stage + 1, "long-task stage sequence mismatch");
                let stage_requests = integer(data, "stage_requests")?;
                // Summary/repair requests share the cumulative budget and can
                // carry a stage past its soft request target before the next
                // settled tool boundary. The global request cap remains hard.
                ensure!(stage_requests > 0 && stage_requests <= limits.max_requests);
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                let fingerprint = string(data, "fingerprint")?;
                ensure!(
                    fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
                );
                let repeat = integer(data, "repeat_count")?;
                ensure!(self.last_fingerprint.as_deref() == Some(fingerprint));
                ensure!(repeat == self.repeat_count && repeat <= limits.repeat_limit);
                self.stage = stage;
                self.stage_requests = 0;
                self.elapsed_ms = elapsed;
                self.last_fingerprint = Some(fingerprint.to_owned());
                self.repeat_count = repeat;
                self.state = State::Ready;
            }
            "tool_batch" => {
                exact_keys(
                    data,
                    &[
                        "version",
                        "event",
                        "stage",
                        "stage_requests",
                        "elapsed_ms",
                        "fingerprint",
                        "repeat_count",
                    ],
                )?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                ensure!(
                    self.state == State::PendingTools,
                    "tool batch without pending tools"
                );
                ensure!(integer(data, "stage")? == self.stage);
                let stage_requests = integer(data, "stage_requests")?;
                ensure!(stage_requests > 0 && stage_requests <= limits.max_requests);
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                let fingerprint = string(data, "fingerprint")?;
                ensure!(
                    fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
                );
                let repeat = if self.last_fingerprint.as_deref() == Some(fingerprint) {
                    self.repeat_count.saturating_add(1)
                } else {
                    1
                };
                ensure!(integer(data, "repeat_count")? == repeat && repeat <= limits.repeat_limit);
                self.stage_requests = stage_requests;
                self.elapsed_ms = elapsed;
                self.last_fingerprint = Some(fingerprint.to_owned());
                self.repeat_count = repeat;
                self.state = State::Ready;
            }
            "paused" => {
                exact_keys(data, &["version", "event", "reason", "stage", "elapsed_ms"])?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                ensure!(
                    self.state == State::Ready,
                    "long-task pause is not at a settled boundary"
                );
                let reason = string(data, "reason")?;
                ensure!(matches!(
                    reason,
                    "operator" | "request_budget" | "wall_timeout"
                ));
                ensure!(integer(data, "stage")? == self.stage);
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                self.elapsed_ms = elapsed;
                self.state = State::Paused;
            }
            "failed" => {
                exact_keys(
                    data,
                    &["version", "event", "category", "stage", "elapsed_ms"],
                )?;
                ensure!(data["version"] == 1);
                let limits = self.limits.context("long-task record before creation")?;
                ensure!(
                    self.state == State::Ready,
                    "long-task failure is not at a safe boundary"
                );
                let category = string(data, "category")?;
                ensure!(matches!(category, "runtime" | "request_budget"));
                ensure!(integer(data, "stage")? == self.stage);
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                self.elapsed_ms = elapsed;
                self.state = State::Failed;
                self.terminal_reason = Some(category.to_owned());
            }
            "completed" => {
                exact_keys(data, &["version", "event", "stage", "elapsed_ms"])?;
                ensure!(data["version"] == 1 && self.state == State::FinalPending);
                let limits = self.limits.context("long-task record before creation")?;
                ensure!(integer(data, "stage")? == self.stage);
                let elapsed = integer(data, "elapsed_ms")?;
                self.ensure_elapsed(elapsed, limits)?;
                self.elapsed_ms = elapsed;
                self.state = State::Completed;
            }
            _ => bail!("unsupported long-task event"),
        }
        Ok(())
    }

    fn ensure_elapsed(&self, elapsed: u64, limits: Limits) -> Result<()> {
        ensure!(elapsed <= limits.wall_seconds.saturating_mul(1000));
        ensure!(
            elapsed >= self.elapsed_ms,
            "long-task elapsed time moved backwards"
        );
        Ok(())
    }

    fn add_request(&mut self, elapsed: u64, tokens: TokenDelta, limits: Limits) -> Result<()> {
        ensure!(
            self.requests < limits.max_requests,
            "long-task request budget exhausted"
        );
        self.requests = self
            .requests
            .checked_add(1)
            .context("long-task request counter overflow")?;
        self.stage_requests = self
            .stage_requests
            .checked_add(1)
            .context("long-task stage request counter overflow")?;
        self.reported_tokens = self
            .reported_tokens
            .checked_add(tokens.total()?)
            .context("long-task token counter overflow")?;
        self.elapsed_ms = elapsed;
        Ok(())
    }

    pub fn has_task(&self) -> bool {
        self.limits.is_some()
    }

    pub fn resume_allowed(&self) -> bool {
        matches!(self.state, State::Ready | State::Paused)
            && self.limits.is_some_and(|limits| {
                self.elapsed_ms < limits.wall_seconds.saturating_mul(1000)
                    && self.repeat_count < limits.repeat_limit
                    && self.reported_tokens < limits.max_tokens
                    && self.requests < limits.max_requests
            })
    }

    pub fn ensure_safe_resume(&self) -> Result<()> {
        match self.state {
            State::None => Ok(()),
            State::Ready | State::Paused => {
                ensure!(
                    self.resume_allowed(),
                    "long-task cumulative budget exhausted"
                );
                Ok(())
            }
            State::PendingProvider | State::PendingTools | State::FinalPending => {
                bail!("long-task has uncertain work; manual recovery required")
            }
            State::Completed => bail!("long-task is complete; start a new session"),
            State::Failed => bail!("long-task failed; start a new session"),
        }
    }

    pub fn next_sequence(&self) -> Result<u64> {
        self.requests
            .checked_add(1)
            .context("long-task request counter overflow")
    }

    pub fn begin_request<S: SessionStore>(
        &mut self,
        session: &mut S,
        elapsed_ms: u64,
    ) -> Result<u64> {
        let limits = self.limits.context("long-task was not created")?;
        ensure!(
            self.resume_allowed(),
            "long-task is not at a resumable boundary"
        );
        ensure!(
            self.requests < limits.max_requests,
            "long-task request budget exhausted"
        );
        ensure!(
            self.reported_tokens < limits.max_tokens,
            "long-task token budget exhausted"
        );
        self.ensure_elapsed(elapsed_ms, limits)?;
        let sequence = self.next_sequence()?;
        session.record_long_task(json!({
            "version":1,"event":"request_started","sequence":sequence,
            "stage":self.stage,"elapsed_ms":elapsed_ms
        }))?;
        self.pending_sequence = Some(sequence);
        self.state = State::PendingProvider;
        self.elapsed_ms = elapsed_ms;
        Ok(sequence)
    }

    pub fn settle_request<S: SessionStore>(
        &mut self,
        session: &mut S,
        response_kind: &str,
        elapsed_ms: u64,
        tokens: TokenDelta,
    ) -> Result<()> {
        let sequence = self
            .pending_sequence
            .context("provider result without start")?;
        let limits = self.limits.context("long-task was not created")?;
        ensure!(matches!(response_kind, "summary" | "tools" | "final"));
        self.ensure_elapsed(elapsed_ms, limits)?;
        let total = tokens.total()?;
        ensure!(self.requests < limits.max_requests);
        ensure!(self.reported_tokens.checked_add(total).is_some());
        session.record_long_task(json!({
            "version":1,"event":"request_settled","sequence":sequence,
            "stage":self.stage,"elapsed_ms":elapsed_ms,"response_kind":response_kind,
            "input_tokens":tokens.input,"output_tokens":tokens.output,
            "cache_read_tokens":tokens.cache_read,"cache_write_tokens":tokens.cache_write,
            "total_tokens":total
        }))?;
        self.pending_sequence = None;
        self.requests += 1;
        self.stage_requests += 1;
        self.reported_tokens += total;
        self.elapsed_ms = elapsed_ms;
        self.state = match response_kind {
            "tools" => State::PendingTools,
            "final" => State::FinalPending,
            "summary" => State::Ready,
            _ => unreachable!(),
        };
        Ok(())
    }

    pub fn fail_request<S: SessionStore>(
        &mut self,
        session: &mut S,
        category: &str,
        elapsed_ms: u64,
        tokens: TokenDelta,
    ) -> Result<()> {
        let sequence = self
            .pending_sequence
            .context("provider failure without start")?;
        let limits = self.limits.context("long-task was not created")?;
        ensure!(matches!(
            category,
            "provider" | "provider_timeout" | "compaction" | "runtime" | "request_budget"
        ));
        self.ensure_elapsed(elapsed_ms, limits)?;
        let total = tokens.total()?;
        ensure!(self.requests < limits.max_requests);
        ensure!(self.reported_tokens.checked_add(total).is_some());
        session.record_long_task(json!({
            "version":1,"event":"request_failed","sequence":sequence,
            "stage":self.stage,"elapsed_ms":elapsed_ms,"category":category,
            "input_tokens":tokens.input,"output_tokens":tokens.output,
            "cache_read_tokens":tokens.cache_read,"cache_write_tokens":tokens.cache_write,
            "total_tokens":total
        }))?;
        self.pending_sequence = None;
        self.requests += 1;
        self.stage_requests += 1;
        self.reported_tokens += total;
        self.elapsed_ms = elapsed_ms;
        self.state = State::Failed;
        self.terminal_reason = Some(category.to_owned());
        Ok(())
    }

    pub fn checkpoint<S: SessionStore>(
        &mut self,
        session: &mut S,
        elapsed_ms: u64,
        fingerprint: &str,
    ) -> Result<u64> {
        let limits = self.limits.context("long-task was not created")?;
        ensure!(
            self.state == State::Ready,
            "stage checkpoint without settled tools"
        );
        ensure!(self.stage_requests > 0 && self.stage_requests <= limits.max_requests);
        ensure!(fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
        self.ensure_elapsed(elapsed_ms, limits)?;
        let repeat_count = self.repeat_count;
        ensure!(self.last_fingerprint.as_deref() == Some(fingerprint));
        ensure!(repeat_count <= limits.repeat_limit);
        let stage = self.stage + 1;
        session.record_long_task(json!({
            "version":1,"event":"stage_checkpoint","stage":stage,
            "stage_requests":self.stage_requests,"elapsed_ms":elapsed_ms,
            "fingerprint":fingerprint,"repeat_count":repeat_count
        }))?;
        self.stage = stage;
        self.stage_requests = 0;
        self.elapsed_ms = elapsed_ms;
        self.last_fingerprint = Some(fingerprint.to_owned());
        self.repeat_count = repeat_count;
        self.state = State::Ready;
        Ok(repeat_count)
    }

    pub fn record_tool_batch<S: SessionStore>(
        &mut self,
        session: &mut S,
        elapsed_ms: u64,
        fingerprint: &str,
    ) -> Result<u64> {
        let limits = self.limits.context("long-task was not created")?;
        ensure!(
            self.state == State::PendingTools,
            "tool batch without pending tools"
        );
        ensure!(self.stage_requests > 0 && self.stage_requests <= limits.max_requests);
        self.ensure_elapsed(elapsed_ms, limits)?;
        ensure!(fingerprint.len() == 64 && fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
        let repeat_count = if self.last_fingerprint.as_deref() == Some(fingerprint) {
            self.repeat_count.saturating_add(1)
        } else {
            1
        };
        ensure!(repeat_count <= limits.repeat_limit);
        session.record_long_task(json!({
            "version":1,"event":"tool_batch","stage":self.stage,
            "stage_requests":self.stage_requests,"elapsed_ms":elapsed_ms,
            "fingerprint":fingerprint,"repeat_count":repeat_count
        }))?;
        self.elapsed_ms = elapsed_ms;
        self.last_fingerprint = Some(fingerprint.to_owned());
        self.repeat_count = repeat_count;
        self.state = State::Ready;
        Ok(repeat_count)
    }

    pub fn fail_terminal<S: SessionStore>(
        &mut self,
        session: &mut S,
        category: &str,
    ) -> Result<()> {
        let limits = self.limits.context("long-task was not created")?;
        ensure!(self.state == State::Ready);
        ensure!(matches!(category, "runtime" | "request_budget"));
        session.record_long_task(json!({
            "version":1,"event":"failed","category":category,
            "stage":self.stage,"elapsed_ms":self.elapsed_ms
        }))?;
        self.state = State::Failed;
        self.terminal_reason = Some(category.to_owned());
        let _ = limits;
        Ok(())
    }

    pub fn pause<S: SessionStore>(&mut self, session: &mut S, reason: &str) -> Result<()> {
        ensure!(
            self.state == State::Ready,
            "long-task pause is not at a settled boundary"
        );
        ensure!(matches!(
            reason,
            "operator" | "request_budget" | "wall_timeout"
        ));
        session.record_long_task(json!({
            "version":1,"event":"paused","reason":reason,
            "stage":self.stage,"elapsed_ms":self.elapsed_ms
        }))?;
        self.state = State::Paused;
        Ok(())
    }

    pub fn complete<S: SessionStore>(&mut self, session: &mut S) -> Result<()> {
        ensure!(
            self.state == State::FinalPending,
            "final answer is not settled"
        );
        session.record_long_task(json!({
            "version":1,"event":"completed","stage":self.stage,"elapsed_ms":self.elapsed_ms
        }))?;
        self.state = State::Completed;
        Ok(())
    }

    pub fn status(&self, task_id: &str) -> Value {
        let limits = self.limits.unwrap_or_else(Limits::defaults);
        json!({
            "version":1,
            "kind":"long_task_status",
            "state":self.state.as_str(),
            "session_id":task_id,
            "stage":self.stage,
            "elapsed_ms":self.elapsed_ms,
            "limits":{
                "wall_seconds":limits.wall_seconds,
                "stage_requests":limits.stage_requests,
                "max_requests":limits.max_requests,
                "max_tokens":limits.max_tokens,
                "repeat_limit":limits.repeat_limit
            },
            "usage":{"requests":self.requests,"reported_tokens":self.reported_tokens},
            "pending":self.pending_sequence.map(|sequence| json!({"kind":"provider","sequence":sequence})).or_else(||
                (self.state == State::PendingTools).then(|| json!({"kind":"tools"}))
            ),
            "resume_allowed":self.resume_allowed()
        })
    }
}

fn exact_keys(data: &Value, keys: &[&str]) -> Result<()> {
    let object = data
        .as_object()
        .context("long-task record must be an object")?;
    let expected = keys
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    ensure!(
        object.len() == expected.len() && object.keys().all(|key| expected.contains(key.as_str()))
    );
    Ok(())
}

fn integer(data: &Value, key: &str) -> Result<u64> {
    data[key]
        .as_u64()
        .with_context(|| format!("invalid long-task integer {key}"))
}

fn string<'a>(data: &'a Value, key: &str) -> Result<&'a str> {
    data[key]
        .as_str()
        .with_context(|| format!("invalid long-task string {key}"))
}

fn tokens(data: &Value) -> Result<TokenDelta> {
    let result = TokenDelta {
        input: integer(data, "input_tokens")?,
        output: integer(data, "output_tokens")?,
        cache_read: integer(data, "cache_read_tokens")?,
        cache_write: integer(data, "cache_write_tokens")?,
    };
    ensure!(integer(data, "total_tokens")? == result.total()?);
    Ok(result)
}

/// Fingerprint an entire settled tool batch without retaining its body.
pub fn fingerprint_batch(batch: &[(String, Value, bool, String)]) -> String {
    let mut hasher = Sha256::new();
    for (name, arguments, is_error, output) in batch {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(arguments).unwrap_or_default());
        hasher.update([0]);
        hasher.update([u8::from(*is_error)]);
        hasher.update([0]);
        hasher.update(output.as_bytes());
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

/// A bounded progress event for the opt-in CLI renderer.
pub fn progress_event(state: &str, ledger: &Ledger) -> Value {
    json!({
        "version":1,
        "state":state,
        "stage":ledger.stage,
        "requests":ledger.requests,
        "reported_tokens":ledger.reported_tokens,
        "elapsed_seconds":ledger.elapsed_ms / 1000
    })
}

/// Verify and inspect a read-only journal without creating or mutating it.
pub fn inspect_entries(entries: &[Value]) -> Result<Value> {
    ensure!(!entries.is_empty(), "empty session journal");
    let header = &entries[0];
    ensure!(header["type"] == "session" && header["version"] == 3);
    let id = header["id"].as_str().context("invalid session id")?;
    ensure!(header["id"].is_string() && header["timestamp"].is_string());
    let mut previous = Value::Null;
    let mut records = Vec::new();
    let mut calls = std::collections::HashMap::<String, String>::new();
    let mut results = std::collections::HashSet::<String>::new();
    let mut started = std::collections::HashSet::<String>::new();
    let mut settled = std::collections::HashSet::<String>::new();
    let mut entry_ids = std::collections::HashSet::<String>::new();
    for entry in &entries[1..] {
        let entry_id = entry["id"].as_str().context("entry lacks id")?;
        ensure!(!entry_id.is_empty() && entry_ids.insert(entry_id.to_owned()));
        ensure!(entry["timestamp"].is_string(), "entry lacks timestamp");
        ensure!(entry["parentId"] == previous, "branched Pi session");
        match entry["type"].as_str() {
            Some("message") => match entry["message"]["role"].as_str() {
                Some("assistant") => {
                    for call in crate::runtime::tool_calls(&entry["message"])? {
                        ensure!(
                            calls.insert(call.id, call.name).is_none(),
                            "duplicate tool call id"
                        );
                    }
                }
                Some("toolResult") => {
                    let id = entry["message"]["toolCallId"]
                        .as_str()
                        .context("invalid result id")?;
                    ensure!(
                        calls.contains_key(id) && results.insert(id.to_owned()),
                        "orphan or duplicate tool result"
                    );
                    if let Some(name) = entry["message"]["toolName"].as_str() {
                        ensure!(calls[id] == name, "tool result name mismatch");
                    }
                }
                Some("user") => {}
                _ => bail!("unsupported message role"),
            },
            Some("custom") if entry["customType"] == "danso.operation.v1" => {
                let id = entry["data"]["toolCallId"]
                    .as_str()
                    .context("invalid operation id")?;
                ensure!(calls.contains_key(id), "orphan operation record");
                match entry["data"]["state"].as_str() {
                    Some("started") => {
                        ensure!(started.insert(id.to_owned()) && !settled.contains(id))
                    }
                    Some("settled") => ensure!(
                        results.contains(id) && started.remove(id) && settled.insert(id.to_owned())
                    ),
                    _ => bail!("invalid operation state"),
                }
            }
            Some("custom") if entry["customType"] == CUSTOM_TYPE => {
                records.push(entry["data"].clone());
            }
            Some(
                "custom" | "model_change" | "thinking_level_change" | "session_info" | "label",
            ) => {}
            _ => bail!("unsupported Pi context entry"),
        }
        previous = entry["id"].clone();
    }
    validate_bindings(entries)?;
    let ledger = Ledger::from_records(&records, Some(id))?;
    let mut status = ledger.status(id);
    if !started.is_empty() || calls.keys().any(|call| !results.contains(call)) {
        status["state"] = json!("blocked");
        status["pending"] = json!({"kind":"tool"});
        status["resume_allowed"] = json!(false);
    }
    Ok(status)
}

/// A creation record must immediately follow and point at the user message
/// that started that job. This closes the crash gap between an empty ledger
/// and its first prompt, and prevents a forged record from adopting old
/// conversation context as a new objective.
pub fn validate_bindings(entries: &[Value]) -> Result<()> {
    for (index, entry) in entries.iter().enumerate() {
        if entry["type"] != "custom" || entry["customType"] != CUSTOM_TYPE {
            continue;
        }
        if entry["data"]["event"] == "created" {
            let user_id = entry["data"]["user_entry_id"]
                .as_str()
                .context("long-task creation lacks user entry")?;
            let previous = entries
                .get(
                    index
                        .checked_sub(1)
                        .context("long-task creation has no parent")?,
                )
                .context("long-task creation has no preceding entry")?;
            ensure!(previous["id"] == user_id);
            ensure!(previous["type"] == "message" && previous["message"]["role"] == "user");
        }
    }
    Ok(())
}

/// Read the durable records from a session store. Kept here so embedders can
/// use the same status projection without learning the JSON record shape.
pub fn records<S: SessionStore>(session: &S) -> Result<Vec<Value>> {
    session.long_task_records()
}
