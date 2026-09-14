//! Provider-neutral usage aggregation; formatting belongs to the output adapter.
use anyhow::{Context, Result};
use serde_json::{Value, json};

#[derive(Default)]
pub struct Usage {
    pub attempted: bool,
    requests: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    models: Vec<String>,
    total: u64,
    /// Compaction summary requests counted by the runtime (issue #69 F);
    /// reported only via DANSO_BUDGET, never in DANSO_USAGE.
    summary_requests: u32,
    /// Memory-distill extraction requests (#52 §4.6, issue #86). Their tokens
    /// are aggregated into DANSO_USAGE like any other request; this counter
    /// only breaks out how many of those requests came from extraction. Like
    /// `summary_requests` it is reported via DANSO_BUDGET and never via
    /// DANSO_USAGE, because that record doubles as PIRI_USAGE and its field
    /// set is fixed by the Piri schema (docs/v0.md).
    memory_requests: u32,
    /// Terminal output-cap stops seen by the runtime (issue #69 F).
    length_stops: u32,
    /// Length stops continued with a follow-up request (issue #69 B/F).
    continuations: u32,
    /// Body-free timing aggregates (issue #98 e), reported via DANSO_TIMING.
    timing: Timing,
}

/// Body-free timing aggregates (issue #98 e): durations in milliseconds and
/// request/operation counts only. They never carry prompt text, file paths,
/// model identifiers or response bodies, and formatting stays in the output
/// adapter. `provider_ms`/`provider_requests` cover the action model requests
/// dispatched by the agent loop; compaction summaries are broken out
/// separately through `summary_requests`. `retry_wait_ms` is accumulated by
/// the bounded wire-retry layer; `journal_ms` by the loop's durable journal
/// appends; `startup_ms` is set once by the process entry point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    pub provider_ms: u64,
    pub provider_requests: u64,
    pub retry_wait_ms: u64,
    pub tool_ms: u64,
    pub tool_calls: u64,
    pub journal_ms: u64,
    pub startup_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

#[derive(Default, Clone, Copy)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn snapshot(&self) -> UsageSnapshot {
        UsageSnapshot {
            requests: self.requests,
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            total: self.total,
        }
    }

    pub fn add(&mut self, provider: &str, model: &str, tokens: TokenUsage) -> Result<()> {
        // Validate every aggregate before mutating state. Error reporting must
        // still be able to print the last valid summary without wrapping/panic.
        let add = |a: u64, b: u64| a.checked_add(b).context("provider usage overflow");
        let requests = add(self.requests, 1)?;
        let input = add(self.input, tokens.input)?;
        let output = add(self.output, tokens.output)?;
        let cache_read = add(self.cache_read, tokens.cache_read)?;
        let cache_write = add(self.cache_write, tokens.cache_write)?;
        let total = add(add(add(input, output)?, cache_read)?, cache_write)?;
        self.requests = requests;
        self.input = input;
        self.output = output;
        self.cache_read = cache_read;
        self.cache_write = cache_write;
        self.total = total;
        let pair = format!("{provider}/{model}");
        if !self.models.contains(&pair) {
            self.models.push(pair);
        }
        Ok(())
    }

    pub fn summary(&self) -> Value {
        json!({"requests":self.requests,"inputTokens":self.input,"outputTokens":self.output,"cacheReadTokens":self.cache_read,"cacheWriteTokens":self.cache_write,"totalTokens":self.total,"costUsd":0,"models":self.models})
    }

    /// Run budget facts for the body-free DANSO_BUDGET record (issue #69 F).
    pub fn record_summary_request(&mut self) {
        self.summary_requests = self.summary_requests.saturating_add(1);
    }

    /// Count one memory-distill extraction request (#52 §4.6). The STRICT
    /// re-ask after a contract violation is a separate request and counts
    /// again, matching how `summary_requests` counts repair requests.
    pub fn record_memory_request(&mut self) {
        self.memory_requests = self.memory_requests.saturating_add(1);
    }

    pub fn memory_requests(&self) -> u32 {
        self.memory_requests
    }

    /// Record one completed action model request with its wall time (#98 e).
    pub fn record_provider_request(&mut self, elapsed_ms: u64) {
        self.timing.provider_requests = self.timing.provider_requests.saturating_add(1);
        self.timing.provider_ms = self.timing.provider_ms.saturating_add(elapsed_ms);
    }

    /// Accumulate backoff actually slept by the bounded wire-retry layer (#98 e).
    pub fn record_retry_wait(&mut self, waited_ms: u64) {
        self.timing.retry_wait_ms = self.timing.retry_wait_ms.saturating_add(waited_ms);
    }

    /// Record one settled tool-execution attempt with its wall time (#98 e).
    pub fn record_tool_execution(&mut self, elapsed_ms: u64) {
        self.timing.tool_calls = self.timing.tool_calls.saturating_add(1);
        self.timing.tool_ms = self.timing.tool_ms.saturating_add(elapsed_ms);
    }

    /// Accumulate time spent in durable journal appends issued by the loop (#98 e).
    pub fn record_journal_append(&mut self, elapsed_ms: u64) {
        self.timing.journal_ms = self.timing.journal_ms.saturating_add(elapsed_ms);
    }

    /// Set once by the process entry point: process start to run start (#98 e).
    pub fn record_startup(&mut self, elapsed_ms: u64) {
        self.timing.startup_ms = elapsed_ms;
    }

    /// The body-free timing snapshot for the DANSO_TIMING record (#98 e).
    pub fn timing(&self) -> Timing {
        self.timing
    }

    pub fn record_length_stop(&mut self) {
        self.length_stops = self.length_stops.saturating_add(1);
    }

    pub fn record_continuation(&mut self) {
        self.continuations = self.continuations.saturating_add(1);
    }

    pub fn budget_counts(&self) -> (u32, u32, u32) {
        (self.summary_requests, self.length_stops, self.continuations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflowing_usage_leaves_a_printable_unchanged_summary() {
        for extra in [1, 2] {
            let mut usage = Usage::default();
            usage
                .add(
                    "test",
                    "first",
                    TokenUsage {
                        input: u64::MAX - 1,
                        output: 1,
                        ..Default::default()
                    },
                )
                .unwrap();
            let before = usage.summary();
            assert!(
                usage
                    .add(
                        "test",
                        "rejected",
                        TokenUsage {
                            input: extra,
                            ..Default::default()
                        }
                    )
                    .is_err()
            );
            assert_eq!(usage.summary(), before);
            assert_eq!(usage.summary()["totalTokens"], u64::MAX);
        }
    }

    /// #98 e: the timing counters saturate independently and never leak into
    /// the Piri-fixed DANSO_USAGE summary or the budget counters.
    #[test]
    fn timing_counters_accumulate_without_touching_usage_records() {
        let mut usage = Usage::default();
        assert_eq!(usage.timing(), Timing::default());
        usage.record_startup(7);
        usage.record_provider_request(17);
        usage.record_provider_request(u64::MAX);
        usage.record_retry_wait(875);
        usage.record_tool_execution(41);
        usage.record_tool_execution(2);
        usage.record_journal_append(3);
        let timing = usage.timing();
        assert_eq!(timing.startup_ms, 7);
        assert_eq!(timing.provider_requests, 2);
        assert_eq!(timing.provider_ms, u64::MAX);
        assert_eq!(timing.retry_wait_ms, 875);
        assert_eq!(timing.tool_calls, 2);
        assert_eq!(timing.tool_ms, 43);
        assert_eq!(timing.journal_ms, 3);
        // The fixed usage/budget records do not widen.
        assert_eq!(usage.summary()["requests"], 0);
        assert_eq!(usage.budget_counts(), (0, 0, 0));
    }
}
