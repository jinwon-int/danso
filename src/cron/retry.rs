//! Retry policy clamping and the due-time retry view — a port of the pure
//! retry helpers in ccc `agent_cron_lib.py` (§6.5 file compatibility).
//!
//! A task that declared no `retryPolicy` has no retry concept and is never
//! labelled retry-exhausted; a task that explicitly declares a policy (even
//! `{maxAttempts: 1}`) opted into the retry framework.

use crate::cron::store::{RetryPolicySpec, RetryState};
use crate::cron::time::parse_utc;
use chrono::{DateTime, Utc};

/// Effective retry policy: every out-of-range or non-integer value falls back
/// to the documented default (clamped, not rejected — the store validator is
/// the fail-closed layer, this mirrors the Python runtime behavior for
/// stores that predate validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: i64,
    pub backoff_sec: i64,
    pub multiplier: i64,
    pub max_backoff_sec: i64,
}

impl RetryPolicy {
    pub fn from_spec(spec: Option<&RetryPolicySpec>) -> Self {
        fn clamp(value: Option<i64>, default: i64, low: i64, high: i64) -> i64 {
            match value {
                Some(value) if (low..=high).contains(&value) => value,
                _ => default,
            }
        }
        Self {
            max_attempts: clamp(spec.and_then(|s| s.max_attempts), 1, 1, 10),
            backoff_sec: clamp(spec.and_then(|s| s.backoff_sec), 60, 0, 86400),
            multiplier: clamp(spec.and_then(|s| s.multiplier), 2, 1, 10),
            max_backoff_sec: clamp(spec.and_then(|s| s.max_backoff_sec), 3600, 0, 86400),
        }
    }
}

/// Exponential backoff for an attempt, capped at `maxBackoffSec`.
/// `retry_delay(policy, attempt) = min(backoffSec * multiplier^(attempt-1),
/// maxBackoffSec)`.
pub fn retry_delay(policy: &RetryPolicy, attempt: i64) -> i64 {
    let attempt_index = attempt.max(1) - 1;
    let delay = policy.backoff_sec.saturating_mul(
        policy
            .multiplier
            .saturating_pow(attempt_index.min(32) as u32),
    );
    delay.min(policy.max_backoff_sec)
}

/// The due-time retry view, or `None` without a usable `retryState`.
#[derive(Debug, Clone)]
pub struct RetryView {
    pub retry_eligible_at: Option<String>,
    /// The attempt number a ready retry would run as (one past the stored
    /// attempt), else the stored attempt.
    pub retry_attempt: i64,
    pub ready: bool,
    pub waiting: bool,
    pub exhausted: bool,
    /// Set when `retryEligibleAt` exists but is not valid ISO8601.
    pub error: Option<String>,
    /// The pending run's original `scheduledAt`, for `retry-due` rows.
    pub scheduled_at: Option<String>,
}

pub fn retry_view(
    state: Option<&RetryState>,
    policy: &RetryPolicy,
    at: DateTime<Utc>,
) -> Option<RetryView> {
    let state = state?;
    let attempt = state.attempt;
    if attempt < 1 {
        return None;
    }
    let eligible = match state.retry_eligible_at.as_deref() {
        None => None,
        Some(raw) => match parse_utc(Some(raw), "retryEligibleAt") {
            Ok(parsed) => parsed,
            Err(_) => {
                return Some(RetryView {
                    retry_eligible_at: None,
                    retry_attempt: attempt,
                    ready: false,
                    waiting: false,
                    exhausted: false,
                    error: Some("invalid retryEligibleAt".to_string()),
                    scheduled_at: None,
                });
            }
        },
    };
    let ready = eligible.is_some_and(|eligible| eligible <= at) && attempt < policy.max_attempts;
    let waiting = eligible.is_some_and(|eligible| eligible > at) && attempt < policy.max_attempts;
    let exhausted = attempt >= policy.max_attempts || eligible.is_none();
    Some(RetryView {
        retry_eligible_at: state.retry_eligible_at.clone(),
        retry_attempt: if ready { attempt + 1 } else { attempt },
        ready,
        waiting,
        exhausted,
        error: None,
        scheduled_at: Some(state.scheduled_at.clone()),
    })
}
